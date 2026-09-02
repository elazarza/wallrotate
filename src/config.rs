//! User-editable settings, stored as TOML next to the runtime state.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Bumped when a default changes in a way that existing files should adopt.
pub const CURRENT_VERSION: u32 = 2;

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Schema version, so old files can be migrated forward. A file written
    /// before versioning has no key and reads as 0.
    #[serde(default)]
    pub config_version: u32,

    /// Root folder scanned recursively for wallpapers.
    pub wallpaper_dir: String,
    /// Hours between automatic rotations.
    pub interval_hours: f64,
    /// Global hotkey, e.g. "win+ctrl+r".
    pub hotkey: String,
    /// fill | fit | stretch | center | tile | span
    pub fit: String,

    /// off | mixed | always
    pub animated_mode: String,
    /// Chance a given screen draws from the animated pool in "mixed" mode.
    /// Ignored when `animated_mode` is "always" or "off".
    pub animated_chance: f32,
    /// Folder names whose contents may be used as animated backgrounds.
    /// Only GIFs and videos under a folder with one of these names are eligible;
    /// an empty list allows them from anywhere in the library.
    pub animated_dirs: Vec<String>,

    /// Screen numbers (1-based, left to right) that take part in rotation.
    /// Empty means every screen rotates. Listing one screen pins the others:
    /// they keep whatever they are showing until you change it deliberately.
    pub rotate_screens: Vec<usize>,

    /// Folder names skipped entirely during the scan.
    pub exclude_dirs: Vec<String>,
    /// Windows only shows .webp wallpapers when the WebP codec is installed.
    pub include_webp: bool,

    /// Upper bound on GIF frame rate. Also thins stored frames, so lowering it
    /// cuts memory as well as CPU.
    pub max_fps: u32,
    /// Upper bound on video presentation rate. Video is decoded by the GPU and
    /// never cached, so this costs frames on screen but not memory.
    pub video_max_fps: u32,
    /// Play .mp4 and friends from the animated folder.
    pub include_video: bool,
    /// Use .gif files as animated backgrounds.
    pub include_gif: bool,
    /// GIFs bigger than this are downscaled at load; the GPU upscales for display.
    pub max_gif_width: u32,
    pub max_gif_height: u32,
    pub max_gif_frames: usize,
    /// Decoded-frame budget for a single animated screen.
    pub gif_memory_budget_mb: usize,

    /// Stop rendering while a window fully covers the screen.
    pub pause_when_covered: bool,
    /// Stop rendering while running on battery.
    pub pause_on_battery: bool,

    pub start_with_windows: bool,
    /// Show a tray balloon after each rotation.
    pub notify_on_rotate: bool,

    /// Clickable web wallpaper: "" (off), a preset name ("grid", "dock",
    /// "minimal"), or a full path to your own .html file.
    pub web_wallpaper: String,
    /// Screen numbers (1-based) that show the web wallpaper. Empty = all.
    pub web_screens: Vec<usize>,
    /// Forward desktop clicks into the page so its tiles actually launch
    /// things. Off leaves it purely decorative.
    pub web_interactive: bool,
}

impl Default for Config {
    fn default() -> Self {
        let pics = std::env::var("USERPROFILE")
            .map(|p| format!("{}\\Pictures\\backgrounds", p))
            .unwrap_or_else(|_| String::from("C:\\Wallpapers"));
        Config {
            config_version: CURRENT_VERSION,
            wallpaper_dir: pics,
            interval_hours: 12.0,
            hotkey: String::from("win+ctrl+r"),
            fit: String::from("fill"),
            animated_mode: String::from("mixed"),
            animated_chance: 0.34,
            animated_dirs: vec![String::from("animated")],
            rotate_screens: vec![],
            exclude_dirs: vec![],
            include_webp: false,
            max_fps: 50,
            video_max_fps: 60,
            include_video: true,
            include_gif: true,
            max_gif_width: 1280,
            max_gif_height: 720,
            max_gif_frames: 400,
            gif_memory_budget_mb: 128,
            pause_when_covered: true,
            pause_on_battery: false,
            start_with_windows: true,
            notify_on_rotate: false,
            web_wallpaper: String::new(),
            web_screens: vec![],
            web_interactive: true,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AnimatedMode {
    Off,
    Mixed,
    Always,
}

impl Config {
    pub fn animated_mode(&self) -> AnimatedMode {
        match self.animated_mode.to_ascii_lowercase().as_str() {
            "off" | "none" | "false" => AnimatedMode::Off,
            "always" | "all" => AnimatedMode::Always,
            _ => AnimatedMode::Mixed,
        }
    }

    pub fn set_animated_mode(&mut self, m: AnimatedMode) {
        self.animated_mode = String::from(match m {
            AnimatedMode::Off => "off",
            AnimatedMode::Mixed => "mixed",
            AnimatedMode::Always => "always",
        });
    }

    pub fn root(&self) -> PathBuf {
        PathBuf::from(&self.wallpaper_dir)
    }

    /// Does this screen (0-based) take part in rotation?
    pub fn rotates_screen(&self, index: usize) -> bool {
        self.rotate_screens.is_empty() || self.rotate_screens.contains(&(index + 1))
    }

    pub fn rotates_all_screens(&self) -> bool {
        self.rotate_screens.is_empty()
    }

    /// Clicking a screen while every screen rotates means "only this one",
    /// which is the common case. After that it is a plain toggle.
    pub fn toggle_rotate_screen(&mut self, index: usize) {
        let number = index + 1;
        if self.rotate_screens.is_empty() {
            self.rotate_screens = vec![number];
            return;
        }
        if let Some(pos) = self.rotate_screens.iter().position(|n| *n == number) {
            self.rotate_screens.remove(pos);
        } else {
            self.rotate_screens.push(number);
            self.rotate_screens.sort_unstable();
        }
    }

    /// Human-readable summary for the tray menu.
    pub fn rotate_screens_label(&self, total: usize) -> String {
        if self.rotate_screens.is_empty() {
            return String::from("all");
        }
        let listed: Vec<String> = self
            .rotate_screens
            .iter()
            .filter(|n| **n >= 1 && **n <= total.max(1))
            .map(|n| n.to_string())
            .collect();
        match listed.len() {
            0 => String::from("all"),
            1 => format!("screen {} only", listed[0]),
            _ => format!("screens {}", listed.join(", ")),
        }
    }

    /// True when the animated pool is restricted to named folders.
    pub fn animated_folder_only(&self) -> bool {
        self.animated_dirs.iter().any(|d| !d.trim().is_empty())
    }

    pub fn set_animated_folder_only(&mut self, on: bool) {
        self.animated_dirs = if on {
            vec![String::from("animated")]
        } else {
            Vec::new()
        };
    }

    /// Shortest gap we will honour between GIF frames.
    pub fn frame_floor_ms(&self) -> u32 {
        let fps = self.max_fps.clamp(1, 60);
        (1000 / fps).max(10)
    }

    /// How often the video pacing thread looks for a newly decoded frame.
    /// Half a frame-time, so a frame is picked up close to when it is ready.
    pub fn video_pace_interval_ms(&self) -> u64 {
        let fps = self.video_max_fps.clamp(1, 240) as u64;
        // A quarter of a frame-time keeps pick-up jitter well under the eye's
        // threshold without the thread spinning.
        (1000 / fps / 4).clamp(2, 33)
    }

    /// One frame-time for video, in microseconds.
    pub fn video_frame_gap_us(&self) -> u64 {
        let fps = self.video_max_fps.clamp(1, 240) as u64;
        1_000_000 / fps
    }

    /// Is the web launcher wallpaper on at all?
    pub fn web_active(&self) -> bool {
        !self.web_wallpaper.trim().is_empty()
    }

    /// Does this screen (0-based) show the web wallpaper?
    pub fn web_on_screen(&self, index: usize) -> bool {
        self.web_active()
            && (self.web_screens.is_empty() || self.web_screens.contains(&(index + 1)))
    }

    /// Same one-click semantics as toggle_rotate_screen.
    pub fn toggle_web_screen(&mut self, index: usize) {
        let number = index + 1;
        if self.web_screens.is_empty() {
            self.web_screens = vec![number];
            return;
        }
        if let Some(pos) = self.web_screens.iter().position(|n| *n == number) {
            self.web_screens.remove(pos);
        } else {
            self.web_screens.push(number);
            self.web_screens.sort_unstable();
        }
    }

    /// Short name for the tray menu: "off", "grid", or the custom file name.
    pub fn web_label(&self) -> String {
        let w = self.web_wallpaper.trim();
        if w.is_empty() {
            return String::from("off");
        }
        if matches!(w, "grid" | "dock" | "minimal" | "dashboard") {
            return w.to_string();
        }
        std::path::Path::new(w)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| String::from("custom"))
    }

    pub fn interval_secs(&self) -> u64 {
        let h = if self.interval_hours.is_finite() && self.interval_hours > 0.0 {
            self.interval_hours
        } else {
            12.0
        };
        ((h * 3600.0) as u64).max(30)
    }
}

pub fn dir() -> PathBuf {
    let base = std::env::var("APPDATA").unwrap_or_else(|_| {
        std::env::var("USERPROFILE").unwrap_or_else(|_| String::from(".")) + "\\AppData\\Roaming"
    });
    PathBuf::from(base).join("WallRotate")
}

pub fn path() -> PathBuf {
    dir().join("config.toml")
}

/// Editors on Windows love to prepend a UTF-8 byte-order mark; TOML does not.
pub fn strip_bom(text: &str) -> &str {
    text.strip_prefix('\u{feff}').unwrap_or(text)
}

/// Carry an older file forward. Returns true when something changed.
fn migrate(cfg: &mut Config) -> bool {
    if cfg.config_version >= CURRENT_VERSION {
        return false;
    }
    if cfg.config_version < 2 {
        // v1 shipped frame-rate ceilings low enough to merge GIF frames and to
        // miss video frames outright, which reads as judder. Raise them, but
        // leave a deliberately low setting alone.
        if cfg.max_fps <= 30 {
            cfg.max_fps = 50;
        }
        if cfg.video_max_fps <= 30 {
            cfg.video_max_fps = 60;
        }
    }
    cfg.config_version = CURRENT_VERSION;
    true
}

pub fn load() -> Config {
    let p = path();
    match std::fs::read_to_string(&p) {
        Ok(text) => match toml::from_str::<Config>(strip_bom(&text)) {
            Ok(mut c) => {
                if migrate(&mut c) {
                    save(&c);
                }
                c
            }
            Err(_) => {
                // Keep the unparseable file so the user can see what broke.
                let _ = std::fs::rename(&p, p.with_extension("toml.bad"));
                let c = Config::default();
                save(&c);
                c
            }
        },
        Err(_) => {
            let c = Config::default();
            save(&c);
            c
        }
    }
}

/// Rewrite the file when it predates a newer key set, so options added by an
/// upgrade become visible instead of sitting invisibly at their defaults.
/// Values already in the file are preserved; only the layout is regenerated.
pub fn upgrade_file(cfg: &Config) {
    let existing = match std::fs::read_to_string(path()) {
        Ok(t) => t,
        Err(_) => {
            save(cfg);
            return;
        }
    };
    let Ok(rendered) = toml::to_string_pretty(cfg) else {
        return;
    };
    let missing_key = rendered
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(key, _)| key.trim())
        .filter(|key| !key.is_empty() && !key.starts_with('#'))
        .any(|key| !existing.contains(key));
    if missing_key {
        save(cfg);
    }
}

pub fn save(cfg: &Config) {
    let _ = std::fs::create_dir_all(dir());
    if let Ok(text) = toml::to_string_pretty(cfg) {
        let banner = "# WallRotate settings.\n\
                      # Edit this file, then choose \"Reload settings\" from the tray menu.\n\n";
        let _ = std::fs::write(path(), format!("{}{}", banner, text));
    }
}
