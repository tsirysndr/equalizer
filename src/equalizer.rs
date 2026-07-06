//! Process-wide 10-band equalizer state shared between the TUI (writer)
//! and the audio thread (reader).
//!
//! The UI mutates one global [`Equalizer`]; the audio thread owns the
//! `rockbox_dsp::Dsp` instance and watches the version counter, reapplying
//! the settings to the DSP only after an actual change.

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use rockbox_dsp::{Dsp, EQ_NUM_BANDS, eq_band_setting};

use crate::settings::{EQ_BANDS, EqBand, Settings};

pub const GAIN_MIN_TENTHS: i32 = -240;
pub const GAIN_MAX_TENTHS: i32 = 240;
pub const TONE_MIN_DB: i32 = -24;
pub const TONE_MAX_DB: i32 = 24;

/// Shared equalizer state. Cheap to read from the audio thread: the
/// enabled flag and version counter are atomics, so the per-chunk hot
/// path only takes the bands lock after an actual change.
pub struct Equalizer {
    enabled: AtomicBool,
    version: AtomicU64,
    bands: Mutex<Vec<EqBand>>,
    /// Bass/treble shelf gains in whole dB. Like Rockbox, the tone stage
    /// is independent of the EQ on/off switch: 0 dB means off.
    bass: AtomicI32,
    treble: AtomicI32,
    /// Shelf cutoffs in Hz, 0 = Rockbox defaults (200 / 3500). Only
    /// settable via the settings file.
    bass_cutoff: AtomicI32,
    treble_cutoff: AtomicI32,
}

static GLOBAL: OnceLock<Equalizer> = OnceLock::new();

impl Equalizer {
    /// The process-wide equalizer, seeded from the settings file on first use.
    pub fn global() -> &'static Equalizer {
        GLOBAL.get_or_init(|| {
            let settings = Settings::load();
            Equalizer {
                enabled: AtomicBool::new(settings.eq_enabled),
                version: AtomicU64::new(0),
                bands: Mutex::new(settings.eq_band_settings),
                bass: AtomicI32::new(settings.bass),
                treble: AtomicI32::new(settings.treble),
                bass_cutoff: AtomicI32::new(settings.bass_cutoff),
                treble_cutoff: AtomicI32::new(settings.treble_cutoff),
            }
        })
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    pub fn bass(&self) -> i32 {
        self.bass.load(Ordering::Relaxed)
    }

    pub fn treble(&self) -> i32 {
        self.treble.load(Ordering::Relaxed)
    }

    /// Adjust the bass shelf gain by `delta_db`, clamped to ±24 dB.
    pub fn adjust_bass(&self, delta_db: i32) {
        let new = (self.bass() + delta_db).clamp(TONE_MIN_DB, TONE_MAX_DB);
        self.bass.store(new, Ordering::Relaxed);
        self.bump();
    }

    /// Adjust the treble shelf gain by `delta_db`, clamped to ±24 dB.
    pub fn adjust_treble(&self, delta_db: i32) {
        let new = (self.treble() + delta_db).clamp(TONE_MIN_DB, TONE_MAX_DB);
        self.treble.store(new, Ordering::Relaxed);
        self.bump();
    }

    pub fn version(&self) -> u64 {
        self.version.load(Ordering::Relaxed)
    }

    pub fn bands(&self) -> Vec<EqBand> {
        self.bands.lock().unwrap().clone()
    }

    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Relaxed);
        self.bump();
    }

    /// Adjust one band's gain by `delta_tenths_db`, clamped to ±24 dB
    /// (Rockbox's own limit).
    pub fn adjust_band_gain(&self, band: usize, delta_tenths_db: i32) {
        let mut bands = self.bands.lock().unwrap();
        if let Some(b) = bands.get_mut(band) {
            b.gain = (b.gain + delta_tenths_db).clamp(GAIN_MIN_TENTHS, GAIN_MAX_TENTHS);
        }
        drop(bands);
        self.bump();
    }

    /// Replace every band gain (whole dB) — used by presets. Cutoffs and Q
    /// are kept.
    pub fn set_band_gains_db(&self, gains_db: &[i32; EQ_BANDS]) {
        let mut bands = self.bands.lock().unwrap();
        for (b, g) in bands.iter_mut().zip(gains_db) {
            b.gain = (g * 10).clamp(GAIN_MIN_TENTHS, GAIN_MAX_TENTHS);
        }
        drop(bands);
        self.bump();
    }

    /// Replace band gains from absolute tenths-of-dB values (the gRPC
    /// `SetBandGains` unit). Cutoffs and Q are kept.
    pub fn set_band_gains_tenths(&self, gains_tenths: &[i32]) {
        let mut bands = self.bands.lock().unwrap();
        for (b, g) in bands.iter_mut().zip(gains_tenths) {
            b.gain = (*g).clamp(GAIN_MIN_TENTHS, GAIN_MAX_TENTHS);
        }
        drop(bands);
        self.bump();
    }

    /// Reset every band's gain and the tone shelves to 0 dB (cutoffs and
    /// Q are kept).
    pub fn reset_gains(&self) {
        let mut bands = self.bands.lock().unwrap();
        for b in bands.iter_mut() {
            b.gain = 0;
        }
        drop(bands);
        self.bass.store(0, Ordering::Relaxed);
        self.treble.store(0, Ordering::Relaxed);
        self.bump();
    }

    fn bump(&self) {
        self.version.fetch_add(1, Ordering::Relaxed);
    }

    /// Persist the current state to the settings file, warning on failure.
    pub fn save(&self) {
        if let Err(err) = self.try_save() {
            tracing::warn!("failed to save settings: {err}");
        }
    }

    /// Persist the current state to the settings file. Loads the file
    /// first so non-EQ sections (e.g. `[api]`) survive the rewrite.
    pub fn try_save(&self) -> Result<(), anyhow::Error> {
        let mut settings = Settings::load();
        settings.eq_enabled = self.is_enabled();
        settings.eq_band_settings = self.bands();
        settings.bass = self.bass();
        settings.treble = self.treble();
        settings.bass_cutoff = self.bass_cutoff.load(Ordering::Relaxed);
        settings.treble_cutoff = self.treble_cutoff.load(Ordering::Relaxed);
        settings.save()
    }

    #[cfg(test)]
    fn set_bands(&self, new_bands: Vec<EqBand>) {
        *self.bands.lock().unwrap() = new_bands;
        self.bump();
    }

    /// Push the current state into a DSP instance. Called by the audio
    /// thread on startup and whenever [`Equalizer::version`] changes.
    pub fn apply_to(&self, dsp: &mut Dsp) {
        for (i, band) in self.bands().iter().take(EQ_NUM_BANDS).enumerate() {
            dsp.set_eq_band_raw(
                i,
                eq_band_setting {
                    cutoff: band.cutoff,
                    q: band.q,
                    gain: band.gain,
                },
            );
        }
        dsp.eq_enable(self.is_enabled());

        // Cutoffs must be set BEFORE gains — set_tone runs the prescale
        // step that recomputes the shelf coefficients from the active cutoff.
        dsp.set_tone_cutoffs(
            self.bass_cutoff.load(Ordering::Relaxed),
            self.treble_cutoff.load(Ordering::Relaxed),
        );
        dsp.set_tone(self.bass(), self.treble());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::default_eq_band_settings;

    fn sine_stereo(freq_hz: f64, rate: u32, frames: usize) -> Vec<i16> {
        let mut pcm = Vec::with_capacity(frames * 2);
        for n in 0..frames {
            let t = n as f64 / rate as f64;
            let s = (0.25 * (2.0 * std::f64::consts::PI * freq_hz * t).sin() * 32767.0) as i16;
            pcm.push(s);
            pcm.push(s);
        }
        pcm
    }

    fn rms(pcm: &[i16]) -> f64 {
        let sum: f64 = pcm.iter().map(|&s| (s as f64) * (s as f64)).sum();
        (sum / pcm.len() as f64).sqrt()
    }

    fn process(dsp: &mut Dsp, input: &[i16]) -> Vec<i16> {
        let mut out = Vec::new();
        dsp.process(input, &mut out);
        out
    }

    /// One test rather than several: the Rockbox DSP is a process-wide
    /// singleton, so splitting this up would race under the parallel
    /// test runner.
    #[test]
    fn dsp_follows_equalizer_state() {
        let eq = Equalizer::global();
        eq.set_bands(default_eq_band_settings());
        eq.reset_gains();

        // Flat and disabled → levels pass through (resampler inactive at
        // equal rates, so RMS is essentially unchanged).
        eq.set_enabled(false);
        let mut dsp = Dsp::new(44100);
        eq.apply_to(&mut dsp);
        let tone = sine_stereo(1000.0, 44100, 44100);
        let flat = process(&mut dsp, &tone);
        let ratio = rms(&tone) / rms(&flat);
        assert!(
            (0.95..1.05).contains(&ratio),
            "flat pipeline should be transparent, got {ratio:.3}x"
        );

        // −12 dB on the 1 kHz band clearly attenuates a 1 kHz tone.
        eq.set_enabled(true);
        assert_eq!(eq.bands()[5].cutoff, 1000);
        eq.adjust_band_gain(5, -120);
        eq.apply_to(&mut dsp);
        dsp.flush();
        let cut = process(&mut dsp, &tone);
        let ratio = rms(&tone) / rms(&cut);
        assert!(
            ratio > 2.0 && ratio < 8.0,
            "expected ~4x attenuation at 1 kHz, got {ratio:.2}x"
        );

        // Band gains clamp at ±24 dB.
        eq.adjust_band_gain(5, -10_000);
        assert_eq!(eq.bands()[5].gain, GAIN_MIN_TENTHS);

        // Bass shelf works even with the band EQ switched off (Rockbox
        // semantics): −12 dB bass clearly attenuates a 100 Hz tone.
        eq.reset_gains();
        eq.set_enabled(false);
        eq.adjust_bass(-12);
        assert_eq!(eq.bass(), -12);
        eq.apply_to(&mut dsp);
        dsp.flush();
        let low = sine_stereo(100.0, 44100, 44100);
        let low_cut = process(&mut dsp, &low);
        let ratio = rms(&low) / rms(&low_cut);
        assert!(
            ratio > 2.0 && ratio < 8.0,
            "expected ~4x bass attenuation at 100 Hz, got {ratio:.2}x"
        );

        // Treble shelf boost raises a high tone; tone gains clamp at ±24.
        eq.reset_gains();
        eq.adjust_treble(200);
        assert_eq!(eq.treble(), TONE_MAX_DB);
        eq.adjust_treble(-6);
        assert_eq!(eq.treble(), TONE_MAX_DB - 6);

        // Presets replace gains and the version counter tracks changes.
        let before = eq.version();
        eq.set_band_gains_db(&[6, 5, 4, 2, 1, 0, 0, 0, 0, 0]);
        assert!(eq.version() > before);
        assert_eq!(eq.bands()[0].gain, 60);

        eq.reset_gains();
        eq.set_enabled(false);
    }
}
