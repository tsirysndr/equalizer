//! Audio pipeline: raw PCM from stdin / a FIFO / a unix socket → interleaved
//! stereo i16 → Rockbox DSP (EQ + tone + resample to device rate) → bounded
//! channel → cpal output callback.
//!
//! The `rockbox_dsp::Dsp` singleton is not `Send`, so it lives entirely on
//! the reader thread; the TUI communicates through the global
//! [`Equalizer`](crate::equalizer::Equalizer) version counter.

use std::fs::File;
use std::io::{self, Read};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, SyncSender};

use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use cpal::traits::DeviceTrait;
use rockbox_dsp::Dsp;

use crate::equalizer::Equalizer;

/// Raw PCM sample encodings accepted on the input (all little-endian,
/// matching ffmpeg's `-f s16le` / `-f f32le` … muxers).
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum PcmFormat {
    S16le,
    S24le,
    S32le,
    F32le,
    F64le,
}

impl PcmFormat {
    pub fn bytes_per_sample(self) -> usize {
        match self {
            PcmFormat::S16le => 2,
            PcmFormat::S24le => 3,
            PcmFormat::S32le => 4,
            PcmFormat::F32le => 4,
            PcmFormat::F64le => 8,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            PcmFormat::S16le => "s16le",
            PcmFormat::S24le => "s24le",
            PcmFormat::S32le => "s32le",
            PcmFormat::F32le => "f32le",
            PcmFormat::F64le => "f64le",
        }
    }
}

pub const STATE_WAITING: u8 = 0;
pub const STATE_STREAMING: u8 = 1;
pub const STATE_ENDED: u8 = 2;

/// Shared pipeline telemetry for the status line.
pub struct AudioStatus {
    /// Processed samples queued between the DSP and the output callback.
    pub queued: AtomicUsize,
    /// Post-DSP peak per channel since the UI last read it (`swap(0)`).
    pub peak_l: AtomicI32,
    pub peak_r: AtomicI32,
    /// Frames actually played (drives the elapsed-time display).
    pub frames_played: AtomicU64,
    pub state: AtomicU8,
    pub error: std::sync::Mutex<Option<String>>,
}

impl AudioStatus {
    pub fn new() -> Self {
        Self {
            queued: AtomicUsize::new(0),
            peak_l: AtomicI32::new(0),
            peak_r: AtomicI32::new(0),
            frames_played: AtomicU64::new(0),
            state: AtomicU8::new(STATE_WAITING),
            error: std::sync::Mutex::new(None),
        }
    }

    fn set_state(&self, state: u8) {
        self.state.store(state, Ordering::Release);
    }

    fn set_error(&self, msg: String) {
        *self.error.lock().unwrap() = Some(msg);
    }
}

pub struct PipelineConfig {
    /// `-` for stdin, otherwise a file / FIFO / unix-socket path. A missing
    /// path is created as a FIFO.
    pub input: String,
    pub in_rate: u32,
    pub channels: usize,
    pub format: PcmFormat,
    pub out_rate: u32,
}

/// Blocking read → DSP → send loop. Runs until the input ends (stdin /
/// regular file), the output side hangs up, or an I/O error. FIFO inputs
/// are reopened on EOF so another process can stream again later.
pub fn reader_loop(cfg: PipelineConfig, status: Arc<AudioStatus>, tx: SyncSender<Vec<i16>>) {
    let eq = Equalizer::global();
    let mut dsp = Dsp::new(cfg.out_rate);
    dsp.set_input_frequency(cfg.in_rate);
    eq.apply_to(&mut dsp);
    let mut applied_version = eq.version();

    let frame_bytes = cfg.format.bytes_per_sample() * cfg.channels;
    // ~10 ms of input per chunk: an EQ tweak is audible after at most
    // channel-bound × chunk (~30 ms) plus the device buffer.
    let chunk_bytes = (cfg.in_rate as usize / 100).max(128) * frame_bytes;
    let mut buf = vec![0u8; chunk_bytes];
    let mut pending: Vec<u8> = Vec::new();
    let mut samples: Vec<i16> = Vec::new();
    let mut stereo: Vec<i16> = Vec::new();

    'outer: loop {
        status.set_state(STATE_WAITING);
        let (mut input, is_fifo) = match open_input(&cfg.input) {
            Ok(pair) => pair,
            Err(err) => {
                status.set_error(format!("cannot open {}: {err}", cfg.input));
                break;
            }
        };

        loop {
            let n = match input.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) => {
                    status.set_error(format!("read error: {err}"));
                    break 'outer;
                }
            };
            status.set_state(STATE_STREAMING);

            let version = eq.version();
            if version != applied_version {
                eq.apply_to(&mut dsp);
                applied_version = version;
            }

            pending.extend_from_slice(&buf[..n]);
            let usable = pending.len() - pending.len() % frame_bytes;
            if usable == 0 {
                continue;
            }
            decode_samples(&pending[..usable], cfg.format, &mut samples);
            pending.drain(..usable);
            fold_stereo(&samples, cfg.channels, &mut stereo);

            let mut out = Vec::new();
            dsp.process(&stereo, &mut out);
            if out.is_empty() {
                continue;
            }
            update_peaks(&out, &status);
            status.queued.fetch_add(out.len(), Ordering::AcqRel);
            if tx.send(out).is_err() {
                break 'outer; // output stream gone
            }
        }

        if !is_fifo {
            break; // stdin / regular file / socket: EOF is final
        }
        // FIFO writer closed — drop buffered remainder, reset the DSP
        // state and wait for the next writer.
        pending.clear();
        dsp.flush();
    }
    status.set_state(STATE_ENDED);
}

/// Open the input source. Returns the reader plus whether it is a FIFO
/// (FIFOs are reopened on EOF). A nonexistent path is created as a FIFO so
/// `equalizer /tmp/eq.fifo` works before the writer side exists.
fn open_input(input: &str) -> Result<(Box<dyn Read>, bool)> {
    if input == "-" {
        return Ok((Box::new(io::stdin().lock()), false));
    }
    let path = Path::new(input);

    if !path.exists() {
        let cpath = std::ffi::CString::new(input.as_bytes()).context("invalid path")?;
        let rc = unsafe { libc::mkfifo(cpath.as_ptr(), 0o644) };
        if rc != 0 {
            bail!("mkfifo failed: {}", io::Error::last_os_error());
        }
    }

    let meta = std::fs::metadata(path).with_context(|| format!("cannot stat {input}"))?;
    use std::os::unix::fs::FileTypeExt;
    if meta.file_type().is_socket() {
        let stream = std::os::unix::net::UnixStream::connect(path)
            .with_context(|| format!("cannot connect to socket {input}"))?;
        return Ok((Box::new(stream), false));
    }
    let is_fifo = meta.file_type().is_fifo();
    // Opening a FIFO read-only blocks until a writer connects — the status
    // line shows "waiting for input" meanwhile.
    let file = File::open(path).with_context(|| format!("cannot open {input}"))?;
    Ok((Box::new(file), is_fifo))
}

/// Decode little-endian raw PCM bytes into i16 samples. `bytes` must hold
/// whole samples.
fn decode_samples(bytes: &[u8], format: PcmFormat, out: &mut Vec<i16>) {
    out.clear();
    match format {
        PcmFormat::S16le => out.extend(
            bytes
                .chunks_exact(2)
                .map(|b| i16::from_le_bytes([b[0], b[1]])),
        ),
        PcmFormat::S24le => out.extend(bytes.chunks_exact(3).map(|b| {
            let v = ((b[0] as i32) << 8 | (b[1] as i32) << 16 | (b[2] as i32) << 24) >> 8;
            (v >> 8) as i16
        })),
        PcmFormat::S32le => out.extend(
            bytes
                .chunks_exact(4)
                .map(|b| (i32::from_le_bytes([b[0], b[1], b[2], b[3]]) >> 16) as i16),
        ),
        PcmFormat::F32le => out.extend(bytes.chunks_exact(4).map(|b| {
            let s = f32::from_le_bytes([b[0], b[1], b[2], b[3]]);
            (s * 32768.0).clamp(-32768.0, 32767.0) as i16
        })),
        PcmFormat::F64le => out.extend(bytes.chunks_exact(8).map(|b| {
            let s = f64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]);
            (s * 32768.0).clamp(-32768.0, 32767.0) as i16
        })),
    }
}

/// Fold interleaved N-channel samples to interleaved stereo (mono is
/// duplicated, >2 channels keep the front pair).
fn fold_stereo(samples: &[i16], channels: usize, out: &mut Vec<i16>) {
    out.clear();
    match channels {
        1 => {
            for &s in samples {
                out.push(s);
                out.push(s);
            }
        }
        2 => out.extend_from_slice(samples),
        n => {
            for frame in samples.chunks_exact(n) {
                out.push(frame[0]);
                out.push(frame[1]);
            }
        }
    }
}

fn update_peaks(stereo: &[i16], status: &AudioStatus) {
    let (mut pl, mut pr) = (0i32, 0i32);
    for frame in stereo.chunks_exact(2) {
        pl = pl.max((frame[0] as i32).abs());
        pr = pr.max((frame[1] as i32).abs());
    }
    status.peak_l.fetch_max(pl, Ordering::Relaxed);
    status.peak_r.fetch_max(pr, Ordering::Relaxed);
}

/// cpal output stream pulling processed stereo i16 chunks from the channel;
/// silence on underrun.
pub fn build_stream<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    rx: Receiver<Vec<i16>>,
    status: Arc<AudioStatus>,
) -> Result<cpal::Stream>
where
    T: cpal::SizedSample + cpal::FromSample<i16>,
{
    let channels = config.channels as usize;
    let mut pending: std::collections::VecDeque<i16> = std::collections::VecDeque::new();

    let stream = device.build_output_stream(
        config,
        move |data: &mut [T], _| {
            for frame in data.chunks_mut(channels) {
                while pending.len() < 2 {
                    match rx.try_recv() {
                        Ok(chunk) => pending.extend(chunk),
                        Err(_) => break,
                    }
                }
                let (l, r) = match pending.pop_front() {
                    Some(l) => {
                        let r = pending.pop_front().unwrap_or(0);
                        status
                            .queued
                            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |q| {
                                Some(q.saturating_sub(2))
                            })
                            .ok();
                        status.frames_played.fetch_add(1, Ordering::Relaxed);
                        (l, r)
                    }
                    None => (0, 0),
                };
                frame[0] = T::from_sample(l);
                if channels > 1 {
                    frame[1] = T::from_sample(r);
                    for s in frame.iter_mut().skip(2) {
                        *s = T::from_sample(0i16);
                    }
                }
            }
        },
        |e| eprintln!("stream error: {e}"),
        None,
    )?;
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn s16le_roundtrip() {
        let src: Vec<i16> = vec![0, 1, -1, i16::MAX, i16::MIN, 12345, -12345];
        let bytes: Vec<u8> = src.iter().flat_map(|s| s.to_le_bytes()).collect();
        let mut out = Vec::new();
        decode_samples(&bytes, PcmFormat::S16le, &mut out);
        assert_eq!(out, src);
    }

    #[test]
    fn f32le_full_scale_maps_to_i16_range() {
        let src = [1.0f32, -1.0, 0.0, 0.5];
        let bytes: Vec<u8> = src.iter().flat_map(|s| s.to_le_bytes()).collect();
        let mut out = Vec::new();
        decode_samples(&bytes, PcmFormat::F32le, &mut out);
        assert_eq!(out, vec![32767, -32768, 0, 16384]);
    }

    #[test]
    fn s24le_sign_extension() {
        // +1.0 (0x7FFFFF) and -1.0 (0x800000) full scale, little-endian.
        let bytes = [0xFF, 0xFF, 0x7F, 0x00, 0x00, 0x80];
        let mut out = Vec::new();
        decode_samples(&bytes, PcmFormat::S24le, &mut out);
        assert_eq!(out, vec![32767, -32768]);
    }

    #[test]
    fn fold_mono_and_multichannel() {
        let mut out = Vec::new();
        fold_stereo(&[1, 2], 1, &mut out);
        assert_eq!(out, vec![1, 1, 2, 2]);
        fold_stereo(&[1, 2, 3, 4, 5, 6], 3, &mut out);
        assert_eq!(out, vec![1, 2, 4, 5]);
    }
}
