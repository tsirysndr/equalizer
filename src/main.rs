//! equalizer — pipe raw PCM through the Rockbox 10-band EQ to the sound
//! card, with a ratatui front-end for live band / bass / treble tweaking.

mod audio;
mod equalizer;
mod presets;
mod settings;
mod ui;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Parser;
use clap::builder::styling::{Color, RgbColor, Style, Styles};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::audio::{AudioStatus, PcmFormat, PipelineConfig};
use crate::equalizer::Equalizer;

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
    #[arg(short = 'd', long)]
    device: Option<String>,

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
}

fn main() -> Result<()> {
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

    let device = pick_device(&host, cli.device.as_deref())?;
    let device_name = device.name().unwrap_or_else(|_| "<unknown>".into());
    let config = device
        .default_output_config()
        .context("no default output config")?;
    let out_rate = config.sample_rate().0;

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

    if cli.no_tui {
        eprintln!(
            "input : {} ({} {}ch {} Hz)",
            input_label(&cli.input),
            cli.format.label(),
            cli.channels,
            cli.rate
        );
        eprintln!("output: {device_name} @ {out_rate} Hz");
        reader.join().ok();
        while status.queued.load(std::sync::atomic::Ordering::Acquire) > 0 {
            std::thread::sleep(Duration::from_millis(50));
        }
        std::thread::sleep(Duration::from_millis(200)); // let the device drain
        if let Some(err) = status.error.lock().unwrap().clone() {
            bail!(err);
        }
    } else {
        let info = ui::StreamInfo {
            input: input_label(&cli.input),
            format: format!("{} {}ch", cli.format.label(), cli.channels),
            in_rate: cli.rate,
            out_rate,
            device: device_name,
        };
        ui::run(Arc::clone(&status), info, preset_idx)?;
        eq.save();
        // The reader may be blocked on a FIFO open or a full channel;
        // dropping the stream/rx unblocks the latter, process exit the rest.
    }
    Ok(())
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
