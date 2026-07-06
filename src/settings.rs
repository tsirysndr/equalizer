//! Persisted equalizer settings, stored as TOML in the user config dir
//! (override with `--config`). The band format matches Rockbox's
//! `[[eq_band_settings]]` so presets round-trip losslessly.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{Context, Error};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

/// Number of equalizer bands (matches `rockbox_dsp::EQ_NUM_BANDS`).
pub const EQ_BANDS: usize = 10;

/// One EQ band, in the exact on-disk format Rockbox uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct EqBand {
    /// Cutoff frequency in Hz.
    pub cutoff: i32,
    /// Q × 10 (e.g. `7` = Q 0.7).
    pub q: i32,
    /// Gain × 10 in dB (e.g. `-125` = −12.5 dB).
    pub gain: i32,
}

/// gRPC control API configuration (`[api]` in settings.toml). CLI flags
/// (`--api-socket`, `--port`, `--no-api`) take precedence over these.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ApiConfig {
    /// Serve the control API on a unix socket. On by default.
    pub enabled: bool,
    /// Unix socket path; unset = per-user default (see [`default_socket_path`]).
    pub socket: Option<PathBuf>,
    /// Also serve over TCP on `host:port`. Unset = no network API.
    pub port: Option<u16>,
    /// TCP bind address, used only when `port` is set.
    pub host: String,
    /// Bearer token required on the TCP endpoint. Auto-generated and
    /// written back here the first time the TCP API is enabled. The unix
    /// socket never requires it.
    pub token: Option<String>,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            socket: None,
            port: None,
            host: "0.0.0.0".to_string(),
            token: None,
        }
    }
}

/// Default unix socket the API listens on (and `--connect` dials when no
/// address is given): the per-user runtime dir when the platform has one,
/// otherwise a uid-suffixed path in the temp dir.
pub fn default_socket_path() -> PathBuf {
    if let Some(dirs) = ProjectDirs::from("io", "tsirysndr", "equalizer") {
        if let Some(runtime) = dirs.runtime_dir() {
            return runtime.join("equalizer.sock");
        }
    }
    let uid = unsafe { libc::getuid() };
    std::env::temp_dir().join(format!("equalizer-{uid}.sock"))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    /// Rockbox-style top-level equalizer on/off. On by default.
    #[serde(default = "default_true")]
    pub eq_enabled: bool,
    /// 10 EQ bands. Band 0 is a low shelf, band 9 a high shelf, the rest
    /// are peaking filters.
    #[serde(default = "default_eq_band_settings")]
    pub eq_band_settings: Vec<EqBand>,
    /// Bass shelf gain in whole dB (matches Rockbox `bass`). Range −24…+24;
    /// active regardless of `eq_enabled`, 0 = stage off.
    #[serde(default)]
    pub bass: i32,
    /// Treble shelf gain in whole dB (matches Rockbox `treble`). Range −24…+24.
    #[serde(default)]
    pub treble: i32,
    /// Bass shelf cutoff in Hz. `0` = Rockbox default 200 Hz.
    #[serde(default)]
    pub bass_cutoff: i32,
    /// Treble shelf cutoff in Hz. `0` = Rockbox default 3500 Hz.
    #[serde(default)]
    pub treble_cutoff: i32,
    /// gRPC control API (`[api]` section).
    #[serde(default)]
    pub api: ApiConfig,
}

fn default_true() -> bool {
    true
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            eq_enabled: true,
            eq_band_settings: default_eq_band_settings(),
            bass: 0,
            treble: 0,
            bass_cutoff: 0,
            treble_cutoff: 0,
            api: ApiConfig::default(),
        }
    }
}

/// The ISO-octave 10-band flat preset used when a fresh config has no
/// `[[eq_band_settings]]` section: standard ISO center frequencies, Q 0.7
/// across the board, every gain at 0 dB.
pub fn default_eq_band_settings() -> Vec<EqBand> {
    const CUTOFFS_HZ: [i32; EQ_BANDS] = [32, 63, 125, 250, 500, 1000, 2000, 4000, 8000, 16000];
    CUTOFFS_HZ
        .iter()
        .map(|&hz| EqBand {
            cutoff: hz,
            q: 7,
            gain: 0,
        })
        .collect()
}

impl Settings {
    /// Load settings from disk, falling back to defaults when the file does
    /// not exist or is corrupted.
    pub fn load() -> Self {
        let path = match settings_path() {
            Ok(path) => path,
            Err(err) => {
                tracing::warn!("{err}");
                return Self::default();
            }
        };
        let mut settings = match fs::read_to_string(&path) {
            Ok(content) => match toml::from_str::<Settings>(&content) {
                Ok(settings) => settings,
                Err(err) => {
                    tracing::warn!("settings file corrupted ({err}), using defaults");
                    Self::default()
                }
            },
            Err(_) => Self::default(),
        };
        settings.ensure_eq_bands();
        settings
    }

    pub fn save(&self) -> Result<(), Error> {
        let path = settings_path()?;
        ensure_parent(&path)?;
        let serialized = toml::to_string_pretty(self).context("failed to serialize settings")?;
        fs::write(&path, serialized).context("failed to write settings file")
    }

    /// Guarantee the full 10 bands so the DSP always has valid coefficients
    /// and the TUI can render 10 sliders regardless of prior file state.
    fn ensure_eq_bands(&mut self) {
        let defaults = default_eq_band_settings();
        if self.eq_band_settings.len() < EQ_BANDS {
            let have = self.eq_band_settings.len();
            self.eq_band_settings.extend_from_slice(&defaults[have..]);
        } else {
            self.eq_band_settings.truncate(EQ_BANDS);
        }
    }
}

static PATH_OVERRIDE: OnceLock<PathBuf> = OnceLock::new();

/// Route load/save to an explicit file (`--config`). Must be called before
/// the first [`Settings::load`].
pub fn set_path_override(path: PathBuf) {
    let _ = PATH_OVERRIDE.set(path);
}

pub fn settings_path() -> Result<PathBuf, Error> {
    if let Some(path) = PATH_OVERRIDE.get() {
        return Ok(path.clone());
    }
    let dirs = ProjectDirs::from("io", "tsirysndr", "equalizer")
        .ok_or_else(|| Error::msg("unable to determine configuration directory"))?;
    Ok(dirs.config_dir().join("settings.toml"))
}

fn ensure_parent(path: &Path) -> Result<(), Error> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).context("failed to create settings directory")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_roundtrip_toml() {
        let mut settings = Settings {
            eq_enabled: true,
            bass: 6,
            treble: -3,
            ..Settings::default()
        };
        settings.eq_band_settings[3].gain = -55;

        let text = toml::to_string_pretty(&settings).unwrap();
        let back: Settings = toml::from_str(&text).unwrap();
        assert!(back.eq_enabled);
        assert_eq!(back.eq_band_settings, settings.eq_band_settings);
        assert_eq!(back.bass, 6);
        assert_eq!(back.treble, -3);
    }

    #[test]
    fn short_band_lists_are_padded_to_ten() {
        let mut settings: Settings = toml::from_str(
            "eq_enabled = true\n[[eq_band_settings]]\ncutoff = 60\nq = 7\ngain = 30\n",
        )
        .unwrap();
        settings.ensure_eq_bands();
        assert_eq!(settings.eq_band_settings.len(), EQ_BANDS);
        assert_eq!(settings.eq_band_settings[0].cutoff, 60);
        assert_eq!(settings.eq_band_settings[9].cutoff, 16000);
    }
}
