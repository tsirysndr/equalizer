//! Built-in 10-band presets (gains in whole dB, on the ISO octave bands
//! 32 Hz … 16 kHz). Cutoffs and Q are left untouched when applied.

use crate::settings::EQ_BANDS;

pub struct Preset {
    pub name: &'static str,
    pub gains_db: [i32; EQ_BANDS],
}

pub const PRESETS: &[Preset] = &[
    Preset {
        name: "flat",
        gains_db: [0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    },
    Preset {
        name: "rock",
        gains_db: [5, 4, 3, 1, -1, -1, 1, 3, 4, 5],
    },
    Preset {
        name: "pop",
        gains_db: [-1, 1, 3, 4, 3, 0, -1, -1, 1, 2],
    },
    Preset {
        name: "jazz",
        gains_db: [3, 2, 1, 2, -1, -1, 0, 1, 2, 3],
    },
    Preset {
        name: "classical",
        gains_db: [4, 3, 2, 0, -1, -1, 0, 2, 3, 4],
    },
    Preset {
        name: "electronic",
        gains_db: [5, 4, 1, 0, -2, 1, 1, 2, 4, 5],
    },
    Preset {
        name: "vocal",
        gains_db: [-2, -1, 0, 1, 3, 4, 3, 2, 1, 0],
    },
    Preset {
        name: "bass-boost",
        gains_db: [6, 5, 4, 2, 1, 0, 0, 0, 0, 0],
    },
    Preset {
        name: "treble-boost",
        gains_db: [0, 0, 0, 0, 0, 1, 2, 4, 5, 6],
    },
];

pub fn find(name: &str) -> Option<usize> {
    PRESETS.iter().position(|p| p.name == name)
}

pub fn names() -> String {
    PRESETS
        .iter()
        .map(|p| p.name)
        .collect::<Vec<_>>()
        .join(", ")
}
