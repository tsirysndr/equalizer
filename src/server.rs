//! gRPC control API server. A unix socket is served by default (remote
//! TUIs on the same machine, `equalizer --connect`); TCP on `0.0.0.0:port`
//! is added when a port is configured. Reflection and grpc-web are enabled
//! so `grpcurl` and browsers work out of the box.
//!
//! The server runs on its own thread with a small tokio runtime; all
//! mutations go through the process-wide [`Equalizer`], which the audio
//! thread already watches via its version counter.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::mpsc;
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream, UnixListenerStream};
use tonic::transport::Server;
use tonic::{Request, Response, Status};

use crate::api::equalizer::FILE_DESCRIPTOR_SET;
use crate::api::equalizer::v1 as pb;
use crate::api::equalizer::v1::equalizer_service_server::{
    EqualizerService, EqualizerServiceServer,
};
use crate::audio::{AudioStatus, STATE_ENDED, STATE_STREAMING};
use crate::equalizer::Equalizer;
use crate::presets;
use crate::settings::EQ_BANDS;
use crate::ui::StreamInfo;

/// Where to serve the control API, after CLI/config resolution. `token`
/// guards the TCP endpoint only — the unix socket is already restricted
/// to the local user by file permissions.
pub struct Endpoints {
    pub socket: Option<PathBuf>,
    pub tcp: Option<SocketAddr>,
    pub token: Option<String>,
}

/// Require `authorization: Bearer <token>` (or the bare token) on every
/// RPC when a token is set; a no-op otherwise.
#[derive(Clone)]
pub struct Auth {
    token: Option<Arc<str>>,
}

impl tonic::service::Interceptor for Auth {
    fn call(&mut self, request: Request<()>) -> Result<Request<()>, Status> {
        let Some(token) = &self.token else {
            return Ok(request);
        };
        let sent = request
            .metadata()
            .get("authorization")
            .and_then(|v| v.to_str().ok());
        match sent {
            Some(v) if v.strip_prefix("Bearer ").unwrap_or(v) == token.as_ref() => Ok(request),
            _ => Err(Status::unauthenticated(
                "missing or invalid token: send `authorization: Bearer <token>` \
                 (see [api].token in the server's settings.toml)",
            )),
        }
    }
}

/// 192-bit random hex token for the TCP endpoint, generated once and
/// persisted to the settings file.
pub fn generate_token() -> Result<String> {
    use std::io::Read;
    let mut buf = [0u8; 24];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .context("cannot read /dev/urandom to generate an API token")?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

/// Build the wire snapshot the RPCs and the watch stream both return.
pub fn state_message(status: &AudioStatus, info: &StreamInfo) -> pb::State {
    use std::sync::atomic::Ordering;

    let eq = Equalizer::global();
    let playback = match status.state.load(Ordering::Acquire) {
        STATE_STREAMING => pb::PlaybackState::Streaming,
        STATE_ENDED => pb::PlaybackState::Ended,
        _ => pb::PlaybackState::Waiting,
    };
    pb::State {
        enabled: eq.is_enabled(),
        bands: eq
            .bands()
            .iter()
            .map(|b| pb::Band {
                cutoff_hz: b.cutoff,
                q_tenths: b.q,
                gain_tenths_db: b.gain,
            })
            .collect(),
        bass_db: eq.bass(),
        treble_db: eq.treble(),
        version: eq.version(),
        playback: playback.into(),
        error: status.error.lock().unwrap().clone().unwrap_or_default(),
        frames_played: status.frames_played.load(Ordering::Relaxed),
        peak_l: status.peak_l.load(Ordering::Relaxed),
        peak_r: status.peak_r.load(Ordering::Relaxed),
        stream: Some(pb::StreamInfo {
            input: info.input.clone(),
            format: info.format.clone(),
            in_rate: info.in_rate,
            out_rate: info.out_rate,
            device: info.device.clone(),
        }),
    }
}

#[derive(Clone)]
struct Service {
    status: Arc<AudioStatus>,
    info: Arc<StreamInfo>,
}

impl Service {
    fn state(&self) -> pb::State {
        state_message(&self.status, &self.info)
    }
}

#[tonic::async_trait]
impl EqualizerService for Service {
    async fn get_state(
        &self,
        _: Request<pb::GetStateRequest>,
    ) -> Result<Response<pb::State>, Status> {
        Ok(Response::new(self.state()))
    }

    type WatchStateStream = ReceiverStream<Result<pb::State, Status>>;

    async fn watch_state(
        &self,
        _: Request<pb::WatchStateRequest>,
    ) -> Result<Response<Self::WatchStateStream>, Status> {
        let (tx, rx) = mpsc::channel(4);
        let svc = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_millis(100));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                if tx.send(Ok(svc.state())).await.is_err() {
                    break; // client hung up
                }
            }
        });
        Ok(Response::new(ReceiverStream::new(rx)))
    }

    async fn set_enabled(
        &self,
        request: Request<pb::SetEnabledRequest>,
    ) -> Result<Response<pb::State>, Status> {
        Equalizer::global().set_enabled(request.into_inner().enabled);
        Ok(Response::new(self.state()))
    }

    async fn adjust_band(
        &self,
        request: Request<pb::AdjustBandRequest>,
    ) -> Result<Response<pb::State>, Status> {
        let req = request.into_inner();
        if req.band as usize >= EQ_BANDS {
            return Err(Status::invalid_argument(format!(
                "band must be 0..{}",
                EQ_BANDS - 1
            )));
        }
        Equalizer::global().adjust_band_gain(req.band as usize, req.delta_tenths_db);
        Ok(Response::new(self.state()))
    }

    async fn set_band_gains(
        &self,
        request: Request<pb::SetBandGainsRequest>,
    ) -> Result<Response<pb::State>, Status> {
        let gains = request.into_inner().gains_tenths_db;
        if gains.len() != EQ_BANDS {
            return Err(Status::invalid_argument(format!(
                "expected {EQ_BANDS} gains, got {}",
                gains.len()
            )));
        }
        Equalizer::global().set_band_gains_tenths(&gains);
        Ok(Response::new(self.state()))
    }

    async fn adjust_tone(
        &self,
        request: Request<pb::AdjustToneRequest>,
    ) -> Result<Response<pb::State>, Status> {
        let req = request.into_inner();
        let eq = Equalizer::global();
        if req.bass_delta_db != 0 {
            eq.adjust_bass(req.bass_delta_db);
        }
        if req.treble_delta_db != 0 {
            eq.adjust_treble(req.treble_delta_db);
        }
        Ok(Response::new(self.state()))
    }

    async fn reset_gains(
        &self,
        _: Request<pb::ResetGainsRequest>,
    ) -> Result<Response<pb::State>, Status> {
        Equalizer::global().reset_gains();
        Ok(Response::new(self.state()))
    }

    async fn apply_preset(
        &self,
        request: Request<pb::ApplyPresetRequest>,
    ) -> Result<Response<pb::State>, Status> {
        let name = request.into_inner().name;
        let idx = presets::find(&name).ok_or_else(|| {
            Status::not_found(format!(
                "unknown preset {name:?} (available: {})",
                presets::names()
            ))
        })?;
        let eq = Equalizer::global();
        eq.set_band_gains_db(&presets::PRESETS[idx].gains_db);
        eq.set_enabled(true);
        Ok(Response::new(self.state()))
    }

    async fn list_presets(
        &self,
        _: Request<pb::ListPresetsRequest>,
    ) -> Result<Response<pb::ListPresetsResponse>, Status> {
        Ok(Response::new(pb::ListPresetsResponse {
            presets: presets::PRESETS
                .iter()
                .map(|p| pb::Preset {
                    name: p.name.to_string(),
                    gains_db: p.gains_db.to_vec(),
                })
                .collect(),
        }))
    }

    async fn save(
        &self,
        _: Request<pb::SaveRequest>,
    ) -> Result<Response<pb::SaveResponse>, Status> {
        Equalizer::global()
            .try_save()
            .map_err(|err| Status::internal(err.to_string()))?;
        Ok(Response::new(pb::SaveResponse {}))
    }
}

/// Claim the unix socket path, handling leftovers: a live socket means
/// another equalizer already serves there (skip with a warning so playback
/// still works); a dead one is removed and rebound.
fn bind_socket(path: &Path) -> Result<Option<std::os::unix::net::UnixListener>> {
    if path.exists() {
        if std::os::unix::net::UnixStream::connect(path).is_ok() {
            tracing::warn!(
                "another equalizer is already serving {}; socket API disabled for this instance",
                path.display()
            );
            return Ok(None);
        }
        std::fs::remove_file(path)
            .with_context(|| format!("cannot remove stale socket {}", path.display()))?;
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    let listener = std::os::unix::net::UnixListener::bind(path)
        .with_context(|| format!("cannot bind socket {}", path.display()))?;
    Ok(Some(listener))
}

fn router(svc: Service, auth: Auth) -> Result<tonic::transport::server::Router> {
    let reflection = tonic_reflection::server::Builder::configure()
        .register_encoded_file_descriptor_set(FILE_DESCRIPTOR_SET)
        .build_v1()
        .context("failed to build reflection service")?;
    // Reflection stays open: it only exposes the schema, and grpcurl needs
    // it before it can send authenticated calls.
    Ok(Server::builder()
        .accept_http1(true) // grpc-web
        .add_service(reflection)
        .add_service(tonic_web::enable(EqualizerServiceServer::with_interceptor(
            svc, auth,
        ))))
}

/// Is `path` a live equalizer control socket? Used by the auto-connect
/// startup path.
pub fn socket_is_live(path: &Path) -> bool {
    path.exists() && std::os::unix::net::UnixStream::connect(path).is_ok()
}

/// Bind the configured endpoints (fail-fast, on the caller's thread), then
/// serve them from a background thread. Returns what was actually bound.
pub fn spawn(
    endpoints: Endpoints,
    status: Arc<AudioStatus>,
    info: StreamInfo,
) -> Result<Endpoints> {
    let socket = match &endpoints.socket {
        Some(path) => bind_socket(path)?.map(|l| (path.clone(), l)),
        None => None,
    };
    let tcp = match endpoints.tcp {
        Some(addr) => Some((
            addr,
            std::net::TcpListener::bind(addr).with_context(|| format!("cannot bind tcp {addr}"))?,
        )),
        None => None,
    };

    let bound = Endpoints {
        socket: socket.as_ref().map(|(p, _)| p.clone()),
        tcp: tcp.as_ref().map(|(a, _)| *a),
        token: endpoints.token.clone(),
    };

    let tcp_auth = Auth {
        token: endpoints.token.map(Arc::from),
    };
    let svc = Service {
        status,
        info: Arc::new(info),
    };
    std::thread::Builder::new()
        .name("grpc-api".to_string())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(err) => {
                    tracing::warn!("control API disabled: {err}");
                    return;
                }
            };
            rt.block_on(async move {
                let mut tasks = Vec::new();
                if let Some((path, listener)) = socket {
                    listener.set_nonblocking(true).ok();
                    match (
                        tokio::net::UnixListener::from_std(listener),
                        router(svc.clone(), Auth { token: None }),
                    ) {
                        (Ok(listener), Ok(router)) => tasks.push(tokio::spawn(async move {
                            let incoming = UnixListenerStream::new(listener);
                            if let Err(err) = router.serve_with_incoming(incoming).await {
                                tracing::error!("socket API on {} died: {err}", path.display());
                            }
                        })),
                        (Err(err), _) => tracing::warn!("socket API disabled: {err}"),
                        (_, Err(err)) => tracing::warn!("socket API disabled: {err}"),
                    }
                }
                if let Some((addr, listener)) = tcp {
                    listener.set_nonblocking(true).ok();
                    match (
                        tokio::net::TcpListener::from_std(listener),
                        router(svc, tcp_auth),
                    ) {
                        (Ok(listener), Ok(router)) => tasks.push(tokio::spawn(async move {
                            let incoming = TcpListenerStream::new(listener);
                            if let Err(err) = router.serve_with_incoming(incoming).await {
                                tracing::error!("tcp API on {addr} died: {err}");
                            }
                        })),
                        (Err(err), _) => tracing::warn!("tcp API disabled: {err}"),
                        (_, Err(err)) => tracing::warn!("tcp API disabled: {err}"),
                    }
                }
                for task in tasks {
                    task.await.ok();
                }
            });
        })
        .context("failed to spawn gRPC server thread")?;

    Ok(bound)
}
