//! Remote [`Controller`]: the TUI talking to another equalizer process
//! over its gRPC control API (unix socket or TCP).
//!
//! The TUI thread is synchronous, so the tonic client lives on a small
//! background tokio runtime. Edits are applied optimistically to a local
//! mirror (so sliders move instantly) and queued to the server in order;
//! a `WatchState` stream keeps the mirror authoritative — including edits
//! made by other clients or the server's own TUI.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use hyper_util::rt::TokioIo;
use tokio::sync::mpsc;
use tonic::Request;
use tonic::service::interceptor::InterceptedService;
use tonic::transport::{Channel, Endpoint, Uri};

use crate::api::equalizer::v1 as pb;
use crate::api::equalizer::v1::equalizer_service_client::EqualizerServiceClient;
use crate::control::{Controller, EqSnapshot, PlayStatus};
use crate::equalizer::{GAIN_MAX_TENTHS, GAIN_MIN_TENTHS, TONE_MAX_DB, TONE_MIN_DB};
use crate::settings::{EQ_BANDS, EqBand, default_socket_path};
use crate::ui::StreamInfo;

/// Adds `authorization: Bearer <token>` to every call when a token is set
/// (required by remote servers' TCP endpoint, ignored on unix sockets).
#[derive(Clone)]
struct ClientAuth {
    header: Option<tonic::metadata::MetadataValue<tonic::metadata::Ascii>>,
}

impl tonic::service::Interceptor for ClientAuth {
    fn call(&mut self, mut request: Request<()>) -> Result<Request<()>, tonic::Status> {
        if let Some(header) = &self.header {
            request
                .metadata_mut()
                .insert("authorization", header.clone());
        }
        Ok(request)
    }
}

type Client = EqualizerServiceClient<InterceptedService<Channel, ClientAuth>>;

/// Local copy of the server state the TUI reads every frame.
struct Mirror {
    enabled: bool,
    bands: Vec<EqBand>,
    bass: i32,
    treble: i32,
    play_state: u8,
    server_error: Option<String>,
    frames_played: u64,
    peak_l: i32,
    peak_r: i32,
    /// Transport problem (lost stream, failed RPC); shown in place of the
    /// server error until the connection recovers.
    conn_error: Option<String>,
}

enum Cmd {
    SetEnabled(bool),
    AdjustBand(usize, i32),
    AdjustTone(i32, i32),
    SetGainsDb([i32; EQ_BANDS]),
    Reset,
    Save,
}

pub struct RemoteController {
    mirror: Arc<Mutex<Mirror>>,
    cmds: mpsc::UnboundedSender<Cmd>,
    /// Commands sent but not yet acknowledged; while nonzero, watch
    /// updates skip the EQ fields so a stale snapshot can't briefly undo
    /// an optimistic local edit.
    pending: Arc<AtomicUsize>,
    /// Keeps the client runtime (and its background tasks) alive for the
    /// controller's lifetime.
    _rt: tokio::runtime::Runtime,
}

/// Dial `addr` and fetch the initial state. `addr` forms: "" (the default
/// local socket), "unix:PATH", a bare path, "host:port", or an
/// "http://host:port" URL.
pub fn connect(addr: &str, token: Option<String>) -> Result<(RemoteController, StreamInfo)> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .context("failed to start client runtime")?;

    let header = match &token {
        Some(t) => Some(
            format!("Bearer {t}")
                .parse()
                .context("token contains characters not allowed in a header")?,
        ),
        None => None,
    };
    let auth = ClientAuth { header };

    let channel = rt
        .block_on(dial(addr))
        .with_context(|| format!("cannot connect to {}", describe(addr)))?;
    let mut client = EqualizerServiceClient::with_interceptor(channel, auth);

    let initial = rt
        .block_on(client.get_state(pb::GetStateRequest {}))
        .with_context(|| format!("GetState failed on {}", describe(addr)))?
        .into_inner();

    let stream = initial.stream.clone().unwrap_or_default();
    let info = StreamInfo {
        input: format!("{} ⇄ {}", describe(addr), stream.input),
        format: stream.format,
        in_rate: stream.in_rate,
        out_rate: stream.out_rate,
        device: stream.device,
    };

    let mirror = Arc::new(Mutex::new(Mirror {
        enabled: false,
        bands: Vec::new(),
        bass: 0,
        treble: 0,
        play_state: 0,
        server_error: None,
        frames_played: 0,
        peak_l: 0,
        peak_r: 0,
        conn_error: None,
    }));
    apply_state(&mut mirror.lock().unwrap(), &initial, true);

    let pending = Arc::new(AtomicUsize::new(0));
    let (tx, rx) = mpsc::unbounded_channel();

    rt.spawn(watch_loop(
        client.clone(),
        Arc::clone(&mirror),
        Arc::clone(&pending),
    ));
    rt.spawn(command_loop(
        client,
        rx,
        Arc::clone(&mirror),
        Arc::clone(&pending),
    ));

    Ok((
        RemoteController {
            mirror,
            cmds: tx,
            pending,
            _rt: rt,
        },
        info,
    ))
}

/// Human-readable target for status/error messages.
fn describe(addr: &str) -> String {
    if addr.is_empty() {
        default_socket_path().display().to_string()
    } else {
        addr.strip_prefix("unix:").unwrap_or(addr).to_string()
    }
}

async fn dial(addr: &str) -> Result<Channel> {
    let unix_path = if addr.is_empty() {
        Some(default_socket_path())
    } else if let Some(path) = addr.strip_prefix("unix:") {
        Some(path.into())
    } else if addr.contains('/') {
        Some(addr.into())
    } else {
        None
    };

    if let Some(path) = unix_path {
        // The URI is ignored for unix transports but an Endpoint needs one.
        let channel = Endpoint::try_from("http://[::1]:50051")?
            .connect_with_connector(tower::service_fn(move |_: Uri| {
                let path = path.clone();
                async move {
                    Ok::<_, std::io::Error>(TokioIo::new(
                        tokio::net::UnixStream::connect(path).await?,
                    ))
                }
            }))
            .await?;
        return Ok(channel);
    }

    let url = if addr.starts_with("http://") || addr.starts_with("https://") {
        addr.to_string()
    } else {
        format!("http://{addr}")
    };
    Ok(Endpoint::try_from(url)?
        .connect_timeout(Duration::from_secs(5))
        .connect()
        .await?)
}

/// Copy a wire state into the mirror. Telemetry is always taken; the EQ
/// fields only when `eq_too` (callers skip them while local edits are in
/// flight, so a stale snapshot can't briefly undo an optimistic edit).
fn apply_state(mirror: &mut Mirror, state: &pb::State, eq_too: bool) {
    if eq_too {
        mirror.enabled = state.enabled;
        mirror.bands = state
            .bands
            .iter()
            .map(|b| EqBand {
                cutoff: b.cutoff_hz,
                q: b.q_tenths,
                gain: b.gain_tenths_db,
            })
            .collect();
        mirror.bass = state.bass_db;
        mirror.treble = state.treble_db;
    }
    mirror.play_state = state.playback as u8;
    mirror.server_error = (!state.error.is_empty()).then(|| state.error.clone());
    mirror.frames_played = state.frames_played;
    mirror.peak_l = state.peak_l;
    mirror.peak_r = state.peak_r;
}

/// Follow `WatchState`, reconnecting with a short backoff so a restarted
/// server picks the TUI back up.
async fn watch_loop(mut client: Client, mirror: Arc<Mutex<Mirror>>, pending: Arc<AtomicUsize>) {
    loop {
        match client.watch_state(pb::WatchStateRequest {}).await {
            Ok(response) => {
                let mut stream = response.into_inner();
                loop {
                    match stream.message().await {
                        Ok(Some(state)) => {
                            let mut m = mirror.lock().unwrap();
                            m.conn_error = None;
                            let settled = pending.load(Ordering::Acquire) == 0;
                            apply_state(&mut m, &state, settled);
                        }
                        Ok(None) => break,
                        Err(err) => {
                            mirror.lock().unwrap().conn_error =
                                Some(format!("connection lost: {err}"));
                            break;
                        }
                    }
                }
            }
            Err(err) => {
                mirror.lock().unwrap().conn_error = Some(format!("connection lost: {err}"));
            }
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

async fn command_loop(
    mut client: Client,
    mut rx: mpsc::UnboundedReceiver<Cmd>,
    mirror: Arc<Mutex<Mirror>>,
    pending: Arc<AtomicUsize>,
) {
    while let Some(cmd) = rx.recv().await {
        let result = match cmd {
            Cmd::SetEnabled(enabled) => client
                .set_enabled(pb::SetEnabledRequest { enabled })
                .await
                .map(drop),
            Cmd::AdjustBand(band, delta) => client
                .adjust_band(pb::AdjustBandRequest {
                    band: band as u32,
                    delta_tenths_db: delta,
                })
                .await
                .map(drop),
            Cmd::AdjustTone(bass, treble) => client
                .adjust_tone(pb::AdjustToneRequest {
                    bass_delta_db: bass,
                    treble_delta_db: treble,
                })
                .await
                .map(drop),
            Cmd::SetGainsDb(gains_db) => client
                .set_band_gains(pb::SetBandGainsRequest {
                    gains_tenths_db: gains_db.iter().map(|g| g * 10).collect(),
                })
                .await
                .map(drop),
            Cmd::Reset => client.reset_gains(pb::ResetGainsRequest {}).await.map(drop),
            Cmd::Save => client.save(pb::SaveRequest {}).await.map(drop),
        };
        pending.fetch_sub(1, Ordering::AcqRel);
        if let Err(status) = result {
            mirror.lock().unwrap().conn_error = Some(format!("rpc failed: {status}"));
        }
    }
}

impl RemoteController {
    fn send(&self, cmd: Cmd) {
        self.pending.fetch_add(1, Ordering::AcqRel);
        if self.cmds.send(cmd).is_err() {
            self.pending.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

impl Controller for RemoteController {
    fn snapshot(&self) -> EqSnapshot {
        let m = self.mirror.lock().unwrap();
        EqSnapshot {
            enabled: m.enabled,
            bands: m.bands.clone(),
            bass: m.bass,
            treble: m.treble,
        }
    }

    fn status(&self) -> PlayStatus {
        let m = self.mirror.lock().unwrap();
        PlayStatus {
            state: m.play_state,
            error: m.conn_error.clone().or_else(|| m.server_error.clone()),
            frames_played: m.frames_played,
            peak_l: m.peak_l,
            peak_r: m.peak_r,
        }
    }

    fn set_enabled(&self, enabled: bool) {
        self.mirror.lock().unwrap().enabled = enabled;
        self.send(Cmd::SetEnabled(enabled));
    }

    fn adjust_band(&self, band: usize, delta_tenths: i32) {
        {
            let mut m = self.mirror.lock().unwrap();
            if let Some(b) = m.bands.get_mut(band) {
                b.gain = (b.gain + delta_tenths).clamp(GAIN_MIN_TENTHS, GAIN_MAX_TENTHS);
            }
        }
        self.send(Cmd::AdjustBand(band, delta_tenths));
    }

    fn adjust_bass(&self, delta_db: i32) {
        {
            let mut m = self.mirror.lock().unwrap();
            m.bass = (m.bass + delta_db).clamp(TONE_MIN_DB, TONE_MAX_DB);
        }
        self.send(Cmd::AdjustTone(delta_db, 0));
    }

    fn adjust_treble(&self, delta_db: i32) {
        {
            let mut m = self.mirror.lock().unwrap();
            m.treble = (m.treble + delta_db).clamp(TONE_MIN_DB, TONE_MAX_DB);
        }
        self.send(Cmd::AdjustTone(0, delta_db));
    }

    fn set_band_gains_db(&self, gains_db: &[i32; EQ_BANDS]) {
        {
            let mut m = self.mirror.lock().unwrap();
            for (b, g) in m.bands.iter_mut().zip(gains_db) {
                b.gain = (g * 10).clamp(GAIN_MIN_TENTHS, GAIN_MAX_TENTHS);
            }
        }
        self.send(Cmd::SetGainsDb(*gains_db));
    }

    fn reset_gains(&self) {
        {
            let mut m = self.mirror.lock().unwrap();
            for b in m.bands.iter_mut() {
                b.gain = 0;
            }
            m.bass = 0;
            m.treble = 0;
        }
        self.send(Cmd::Reset);
    }

    fn save(&self) {
        self.send(Cmd::Save);
    }
}
