//! The control surface the TUI drives, abstracted so the same UI can edit
//! either the in-process equalizer ([`LocalController`]) or a remote one
//! over gRPC (`remote::RemoteController`).

use std::sync::Arc;
use std::sync::atomic::Ordering;

use crate::audio::AudioStatus;
use crate::equalizer::Equalizer;
use crate::settings::{EQ_BANDS, EqBand};

/// Point-in-time copy of the EQ state, enough to draw one frame.
#[derive(Clone)]
pub struct EqSnapshot {
    pub enabled: bool,
    pub bands: Vec<EqBand>,
    pub bass: i32,
    pub treble: i32,
}

/// Point-in-time copy of the pipeline telemetry for the status line.
/// `state` uses the `audio::STATE_*` constants; `peak_l/r` are the latest
/// chunk peaks in i16 units.
#[derive(Clone)]
pub struct PlayStatus {
    pub state: u8,
    pub error: Option<String>,
    pub frames_played: u64,
    pub peak_l: i32,
    pub peak_r: i32,
}

pub trait Controller {
    fn snapshot(&self) -> EqSnapshot;
    fn status(&self) -> PlayStatus;
    fn set_enabled(&self, enabled: bool);
    /// Relative band gain change in tenths of dB.
    fn adjust_band(&self, band: usize, delta_tenths: i32);
    /// Relative shelf changes in whole dB.
    fn adjust_bass(&self, delta_db: i32);
    fn adjust_treble(&self, delta_db: i32);
    /// Absolute gains in whole dB (presets).
    fn set_band_gains_db(&self, gains_db: &[i32; EQ_BANDS]);
    fn reset_gains(&self);
    fn save(&self);
}

/// Direct access to the process-wide [`Equalizer`] and audio telemetry.
pub struct LocalController {
    status: Arc<AudioStatus>,
}

impl LocalController {
    pub fn new(status: Arc<AudioStatus>) -> Self {
        Self { status }
    }
}

impl Controller for LocalController {
    fn snapshot(&self) -> EqSnapshot {
        let eq = Equalizer::global();
        EqSnapshot {
            enabled: eq.is_enabled(),
            bands: eq.bands(),
            bass: eq.bass(),
            treble: eq.treble(),
        }
    }

    fn status(&self) -> PlayStatus {
        PlayStatus {
            state: self.status.state.load(Ordering::Acquire),
            error: self.status.error.lock().unwrap().clone(),
            frames_played: self.status.frames_played.load(Ordering::Relaxed),
            peak_l: self.status.peak_l.load(Ordering::Relaxed),
            peak_r: self.status.peak_r.load(Ordering::Relaxed),
        }
    }

    fn set_enabled(&self, enabled: bool) {
        Equalizer::global().set_enabled(enabled);
    }

    fn adjust_band(&self, band: usize, delta_tenths: i32) {
        Equalizer::global().adjust_band_gain(band, delta_tenths);
    }

    fn adjust_bass(&self, delta_db: i32) {
        Equalizer::global().adjust_bass(delta_db);
    }

    fn adjust_treble(&self, delta_db: i32) {
        Equalizer::global().adjust_treble(delta_db);
    }

    fn set_band_gains_db(&self, gains_db: &[i32; EQ_BANDS]) {
        Equalizer::global().set_band_gains_db(gains_db);
    }

    fn reset_gains(&self) {
        Equalizer::global().reset_gains();
    }

    fn save(&self) {
        Equalizer::global().save();
    }
}
