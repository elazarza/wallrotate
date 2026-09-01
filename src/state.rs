//! Rotation bookkeeping that has to survive a restart.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Assignment {
    /// Monitor device path, as reported by IDesktopWallpaper.
    pub monitor: String,
    /// What is on screen: the GIF when `animated`, otherwise the still image.
    pub path: String,
    /// Still image set as the real wallpaper beneath an animated surface, so
    /// the desktop degrades to something sensible if the app is not running.
    pub under: String,
    pub animated: bool,
}

/// A shuffled playlist over a pool, remembered by seed rather than by value so
/// the state file stays tiny no matter how many wallpapers there are.
#[derive(Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Playlist {
    pub seed: u64,
    pub cursor: usize,
    pub pool_len: usize,
}

#[derive(Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct State {
    /// Unix seconds of the last rotation.
    pub last_rotate: u64,
    pub statics: Playlist,
    pub animated: Playlist,
    /// What each screen is currently showing.
    pub assignments: Vec<Assignment>,
}

impl State {
    pub fn assignment_for(&self, monitor: &str) -> Option<&Assignment> {
        self.assignments.iter().find(|a| a.monitor == monitor)
    }
}

pub fn path() -> PathBuf {
    crate::config::dir().join("state.toml")
}

pub fn load() -> State {
    std::fs::read_to_string(path())
        .ok()
        .and_then(|t| toml::from_str::<State>(crate::config::strip_bom(&t)).ok())
        .unwrap_or_default()
}

pub fn save(s: &State) {
    let _ = std::fs::create_dir_all(crate::config::dir());
    if let Ok(text) = toml::to_string(s) {
        let _ = std::fs::write(path(), text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression: TOML integers are i64, so a u64 seed with the high bit set
    /// used to make serialisation fail and the state file silently vanish.
    #[test]
    fn state_round_trips_with_large_seeds() {
        let s = State {
            last_rotate: 1_800_000_000,
            statics: Playlist {
                seed: 0x7FFF_FFFF_FFFF_FFFF,
                cursor: 12,
                pool_len: 1671,
            },
            animated: Playlist {
                seed: 1,
                cursor: 0,
                pool_len: 5,
            },
            assignments: vec![Assignment {
                monitor: String::from("\\\\?\\DISPLAY#XYZ"),
                path: String::from("C:\\pics\\a.gif"),
                under: String::from("C:\\pics\\b.jpg"),
                animated: true,
            }],
        };
        let text = toml::to_string(&s).expect("serialises");
        let back: State = toml::from_str(&text).expect("deserialises");
        assert_eq!(back.statics.seed, s.statics.seed);
        assert_eq!(back.statics.cursor, 12);
        assert_eq!(back.assignments.len(), 1);
        assert!(back.assignments[0].animated);
    }
}
