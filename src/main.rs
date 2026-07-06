//! equalizer — pipe raw PCM through the Rockbox 10-band EQ to the sound
//! card, with a ratatui front-end for live band / bass / treble tweaking.

mod api;
mod audio;
mod control;
mod equalizer;
mod presets;
mod remote;
mod server;
mod settings;
mod ui;

use std::io::IsTerminal;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Parser;
use clap::builder::styling::{Color, RgbColor, Style, Styles};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::audio::{AudioStatus, OutputTarget, PcmFormat, PipelineConfig};
use crate::control::{Controller, LocalController};
use crate::equalizer::Equalizer;
use crate::settings::{Settings, default_socket_path};

/// Synthwave '84 palette applied to clap's help & error output — mirrors
/// the ratatui theme in `ui::theme`.
fn synthwave_styles() -> Styles {
    let pink = Color::Rgb(RgbColor(0xff, 0x7e, 0xdb)); // section titles
    let cyan = Color::Rgb(RgbColor(0x36, 0xf9, 0xf6)); // flag literals
    let yellow = Color::Rgb(RgbColor(0xfe, 0xde, 0x5d)); // <PLACEHOLDER> tokens
    let orange = Color::Rgb(RgbColor(0xff, 0x8b, 0x39)); // "Usage:" line
    let green = Color::Rgb(RgbColor(0x72, 0xf1, 0xb8)); // valid values
    let red = Color::Rgb(RgbColor(0xfe, 0x44, 0x50)); // "error:" prefix

    Styles::styled()
        .header(Style::new().bold().underline().fg_color(Some(pink)))
        .usage(Style::new().bold().fg_color(Some(orange)))
        .literal(Style::new().bold().fg_color(Some(cyan)))
        .placeholder(Style::new().fg_color(Some(yellow)))
        .valid(Style::new().bold().fg_color(Some(green)))
        .invalid(Style::new().bold().fg_color(Some(yellow)))
        .error(Style::new().bold().fg_color(Some(red)))
}

/// Real-time equalizer for raw PCM pipes.
///
/// Reads raw PCM from stdin, a FIFO, or a unix socket, applies the Rockbox
/// DSP (10-band EQ + bass/treble shelves + resampling) and plays the result
/// on the default output device. Settings are persisted to a TOML file and
/// restored on the next run.
///
/// Examples:
///
///   ffmpeg -i track.flac -f s16le -ac 2 -ar 44100 - 2>/dev/null | equalizer
///
///   equalizer /tmp/eq.fifo &
///   ffmpeg -i track.mp3 -f s16le -ac 2 -ar 44100 -y /tmp/eq.fifo
#[derive(Parser)]
#[command(version, about, verbatim_doc_comment, styles = synthwave_styles())]
struct Cli {
    /// Input: a file / FIFO / unix-socket path, or "-" for stdin.
    /// A nonexistent path is created as a FIFO.
    #[arg(default_value = "-")]
    input: String,

    /// Input sample rate in Hz (resampled to the device rate by the DSP).
    #[arg(short = 'r', long, default_value_t = 44100)]
    rate: u32,

    /// Input channel count (mono is upmixed, >2 folds to the front pair).
    #[arg(short = 'c', long, default_value_t = 2)]
    channels: usize,

    /// Input sample encoding (little-endian, as in ffmpeg -f <fmt>).
    #[arg(short = 'f', long, value_enum, default_value_t = PcmFormat::S16le)]
    format: PcmFormat,

    /// Output device name (case-insensitive substring); default device
    /// otherwise. See --list-devices.
    #[arg(short = 'd', long, conflicts_with = "output")]
    device: Option<String>,

    /// Audio output: "default" plays on the sound card; "-" writes raw
    /// s16le stereo PCM to stdout; any other value is a FIFO path
    /// (created if missing). Pipe outputs run at the input rate (no
    /// resampling) and are paced by the consumer.
    #[arg(short = 'o', long, value_name = "TARGET")]
    output: Option<String>,

    /// List output devices and exit.
    #[arg(long)]
    list_devices: bool,

    /// Apply a built-in preset on startup (and persist it):
    /// flat, rock, pop, jazz, classical, electronic, vocal,
    /// bass-boost, treble-boost.
    #[arg(short = 'p', long)]
    preset: Option<String>,

    /// Settings file path (default: user config dir).
    #[arg(long)]
    config: Option<PathBuf>,

    /// Headless mode: no TUI, run until the input ends.
    #[arg(long)]
    no_tui: bool,

    /// Control-API unix socket path (default: per-user runtime path, or
    /// `[api] socket` in the settings file). Served automatically.
    #[arg(long, value_name = "PATH")]
    api_socket: Option<PathBuf>,

    /// Also serve the control API over TCP on 0.0.0.0:<PORT> (or `[api]
    /// port`/`host` in the settings file). Protected by `[api] token`,
    /// auto-generated on first use.
    #[arg(long, value_name = "PORT")]
    port: Option<u16>,

    /// Do not serve the control API at all.
    #[arg(long)]
    no_api: bool,

    /// Remote TUI: control another equalizer instead of playing audio.
    /// ADDR is "host:port", "http://host:port", "unix:PATH" or a socket
    /// path; with no value, the default local socket. Plain `equalizer`
    /// with no piped input auto-connects when a local instance is running.
    #[arg(
        long,
        value_name = "ADDR",
        num_args = 0..=1,
        default_missing_value = "",
        conflicts_with_all = ["input", "rate", "channels", "format", "device", "output",
                              "list_devices", "no_tui", "api_socket", "port", "no_api"]
    )]
    connect: Option<String>,

    /// Bearer token for a remote server's TCP API (env: EQUALIZER_TOKEN;
    /// printed/stored by the server as `[api] token` in its settings file).
    #[arg(long, value_name = "TOKEN")]
    token: Option<String>,
}

fn main() -> Result<()> {
    // Logs go to stderr (stdout may be captured by scripts). RUST_LOG
    // overrides the default `info` filter.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    if let Some(path) = &cli.config {
        settings::set_path_override(path.clone());
    }
    if cli.channels == 0 || cli.channels > 32 {
        bail!("--channels must be between 1 and 32");
    }

    let host = cpal::default_host();
    if cli.list_devices {
        for device in host.output_devices()? {
            println!("{}", device.name().unwrap_or_else(|_| "<unknown>".into()));
        }
        return Ok(());
    }

    // Client mode: explicit --connect, or auto-connect when nothing is
    // piped in and another local instance already owns the control socket.
    let api_cfg = Settings::load().api;
    let socket_path = cli
        .api_socket
        .clone()
        .or_else(|| api_cfg.socket.clone())
        .unwrap_or_else(default_socket_path);
    let connect_target = cli.connect.clone().or_else(|| {
        (cli.input == "-"
            && cli.output.is_none()
            && std::io::stdin().is_terminal()
            && !cli.no_tui
            && server::socket_is_live(&socket_path))
        .then(|| socket_path.to_string_lossy().into_owned())
    });
    if let Some(addr) = connect_target {
        let token = cli
            .token
            .clone()
            .or_else(|| std::env::var("EQUALIZER_TOKEN").ok());
        let (ctrl, info) = remote::connect(&addr, token)?;
        let preset_idx = match &cli.preset {
            Some(name) => {
                let idx = presets::find(name).with_context(|| {
                    format!("unknown preset {name:?} (available: {})", presets::names())
                })?;
                ctrl.set_band_gains_db(&presets::PRESETS[idx].gains_db);
                ctrl.set_enabled(true);
                ctrl.save();
                Some(idx)
            }
            None => None,
        };
        return ui::run(&ctrl, info, preset_idx);
    }

    let eq = Equalizer::global();
    let preset_idx = match &cli.preset {
        Some(name) => {
            let idx = presets::find(name).with_context(|| {
                format!("unknown preset {name:?} (available: {})", presets::names())
            })?;
            eq.set_band_gains_db(&presets::PRESETS[idx].gains_db);
            eq.set_enabled(true);
            eq.save();
            Some(idx)
        }
        None => None,
    };

    let output = match cli.output.as_deref() {
        None | Some("default") => None,
        Some("-") => Some(OutputTarget::Stdout),
        Some(path) => Some(OutputTarget::Fifo(path.into())),
    };

    // Pipe outputs have no device clock: the DSP runs at the input rate
    // (resampler inactive) and the consumer paces the pipeline.
    let device_stuff = match &output {
        None => {
            let device = pick_device(&host, cli.device.as_deref())?;
            let config = device
                .default_output_config()
                .context("no default output config")?;
            Some((device, config))
        }
        Some(_) => None,
    };
    let out_rate = match &device_stuff {
        Some((_, config)) => config.sample_rate().0,
        None => cli.rate,
    };
    let device_name = match (&output, &device_stuff) {
        (Some(target), _) => target.label(),
        (None, Some((device, _))) => device.name().unwrap_or_else(|_| "<unknown>".into()),
        (None, None) => unreachable!(),
    };

    let status = Arc::new(AudioStatus::new());
    // Small bound: post-DSP buffering is what delays audible EQ changes
    // (3 × ~10 ms chunks in flight).
    let (tx, rx) = mpsc::sync_channel::<Vec<i16>>(3);

    let pipeline = PipelineConfig {
        input: cli.input.clone(),
        in_rate: cli.rate,
        channels: cli.channels,
        format: cli.format,
        out_rate,
    };
    let reader = std::thread::spawn({
        let status = Arc::clone(&status);
        move || audio::reader_loop(pipeline, status, tx)
    });

    // Either a live cpal stream (kept alive by the binding) or the writer
    // thread draining the channel into a pipe.
    enum Sink {
        Device(#[allow(dead_code)] cpal::Stream),
        Pipe(std::thread::JoinHandle<()>),
    }
    let sink = match (output, device_stuff) {
        (Some(target), _) => Sink::Pipe(std::thread::spawn({
            let status = Arc::clone(&status);
            move || audio::writer_loop(target, status, rx)
        })),
        (None, Some((device, config))) => {
            let stream_config: cpal::StreamConfig = config.clone().into();
            let stream = match config.sample_format() {
                cpal::SampleFormat::F32 => {
                    audio::build_stream::<f32>(&device, &stream_config, rx, Arc::clone(&status))
                }
                cpal::SampleFormat::I16 => {
                    audio::build_stream::<i16>(&device, &stream_config, rx, Arc::clone(&status))
                }
                cpal::SampleFormat::U16 => {
                    audio::build_stream::<u16>(&device, &stream_config, rx, Arc::clone(&status))
                }
                other => bail!("unsupported output sample format {other:?}"),
            }?;
            stream.play().context("failed to start output stream")?;
            Sink::Device(stream)
        }
        (None, None) => unreachable!(),
    };

    let info = ui::StreamInfo {
        input: input_label(&cli.input),
        format: format!("{} {}ch", cli.format.label(), cli.channels),
        in_rate: cli.rate,
        out_rate,
        device: device_name.clone(),
    };
    let api = serve_api(&cli, socket_path, Arc::clone(&status), info.clone())?;

    if cli.no_tui {
        tracing::info!(
            "input: {} ({} {}ch {} Hz)",
            input_label(&cli.input),
            cli.format.label(),
            cli.channels,
            cli.rate
        );
        tracing::info!("output: {device_name} @ {out_rate} Hz");
        if let Some(endpoints) = &api {
            if let Some(path) = &endpoints.socket {
                tracing::info!("api: unix {}", path.display());
            }
            if let Some(addr) = endpoints.tcp {
                tracing::info!("api: tcp {addr} (token required)");
            }
            if let Some(token) = &endpoints.token {
                // stdout on purpose (not a log): scripts starting a headless
                // server capture the token to hand to remote clients — unless
                // stdout is the PCM output, where it would corrupt the stream.
                if matches!(sink, Sink::Pipe(_)) && device_name == "stdout" {
                    tracing::info!("api token: {token}");
                } else {
                    println!("api token: {token}");
                }
            }
        }
        reader.join().ok();
        match sink {
            // The writer exits once the channel drains after the reader ends.
            Sink::Pipe(writer) => {
                writer.join().ok();
            }
            Sink::Device(_) => {
                while status.queued.load(std::sync::atomic::Ordering::Acquire) > 0 {
                    std::thread::sleep(Duration::from_millis(50));
                }
                std::thread::sleep(Duration::from_millis(200)); // let the device drain
            }
        }
        cleanup_socket(&api);
        if let Some(err) = status.error.lock().unwrap().clone() {
            bail!(err);
        }
    } else {
        let ctrl = LocalController::new(Arc::clone(&status));
        let result = ui::run(&ctrl, info, preset_idx);
        cleanup_socket(&api);
        result?;
        eq.save();
        // The reader may be blocked on a FIFO open or a full channel;
        // dropping the stream/rx unblocks the latter, process exit the rest.
    }
    Ok(())
}

/// Resolve the control-API endpoints (CLI > settings file > defaults) and
/// start serving them. The TCP endpoint requires the persisted `[api]`
/// token, generated on first use.
fn serve_api(
    cli: &Cli,
    socket_path: PathBuf,
    status: Arc<AudioStatus>,
    info: ui::StreamInfo,
) -> Result<Option<server::Endpoints>> {
    if cli.no_api {
        return Ok(None);
    }
    let cfg = Settings::load().api;

    // An explicit --api-socket serves even when the config disables the
    // default-on socket.
    let socket = (cfg.enabled || cli.api_socket.is_some()).then_some(socket_path);

    let tcp = match cli.port.or(cfg.port) {
        Some(port) => {
            let host: IpAddr = cfg
                .host
                .parse()
                .with_context(|| format!("invalid [api] host {:?} in settings", cfg.host))?;
            Some(SocketAddr::new(host, port))
        }
        None => None,
    };

    // Only the network endpoint needs the token; don't generate one for
    // purely local (socket) use.
    let token = match (&tcp, cfg.token) {
        (None, _) => None,
        (Some(_), Some(token)) => Some(token),
        (Some(_), None) => {
            let token = server::generate_token()?;
            let mut settings = Settings::load();
            settings.api.token = Some(token.clone());
            settings
                .save()
                .context("failed to persist the generated API token")?;
            Some(token)
        }
    };

    if socket.is_none() && tcp.is_none() {
        return Ok(None);
    }
    server::spawn(server::Endpoints { socket, tcp, token }, status, info).map(Some)
}

/// Best-effort removal of the socket file we bound, so restarts never see
/// a stale path.
fn cleanup_socket(api: &Option<server::Endpoints>) {
    if let Some(path) = api.as_ref().and_then(|e| e.socket.as_ref()) {
        let _ = std::fs::remove_file(path);
    }
}

fn input_label(input: &str) -> String {
    if input == "-" {
        "stdin".to_string()
    } else {
        input.to_string()
    }
}

fn pick_device(host: &cpal::Host, name: Option<&str>) -> Result<cpal::Device> {
    match name {
        None => host
            .default_output_device()
            .context("no default output device"),
        Some(wanted) => {
            let needle = wanted.to_lowercase();
            for device in host.output_devices()? {
                if device
                    .name()
                    .is_ok_and(|n| n.to_lowercase().contains(&needle))
                {
                    return Ok(device);
                }
            }
            bail!("no output device matching {wanted:?} (see --list-devices)");
        }
    }
}
