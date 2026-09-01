//! Recursive scan of the wallpaper root into a static pool and an animated pool.

use crate::config::Config;
use crate::video::is_video_ext;
use std::path::{Path, PathBuf};

#[derive(Default)]
pub struct Library {
    pub statics: Vec<PathBuf>,
    /// GIFs and video clips, in one pool: both drive the animated layer.
    pub animated: Vec<PathBuf>,
    pub gifs: usize,
    pub videos: usize,
    /// Video files found while `include_video` is off.
    pub videos_disabled: usize,
    /// GIFs found while `include_gif` is off.
    pub gifs_disabled: usize,
    /// Animated files sitting outside the folders `animated_dirs` allows.
    pub animated_out_of_scope: usize,
}

impl Library {
    pub fn is_empty(&self) -> bool {
        self.statics.is_empty() && self.animated.is_empty()
    }
}

pub fn ext_lower(p: &Path) -> String {
    p.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default()
}

pub fn is_video(p: &Path) -> bool {
    is_video_ext(&ext_lower(p))
}

pub fn scan(cfg: &Config) -> Library {
    let root = cfg.root();
    let mut lib = Library::default();
    if !root.is_dir() {
        return lib;
    }

    let excluded: Vec<String> = cfg
        .exclude_dirs
        .iter()
        .map(|s| s.to_ascii_lowercase())
        .collect();
    let animated_dirs: Vec<String> = cfg
        .animated_dirs
        .iter()
        .map(|s| s.to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect();
    // An empty list means "animated backgrounds may come from anywhere".
    let anywhere = animated_dirs.is_empty();

    // Each entry carries whether it sits inside an approved animated folder.
    let mut stack = vec![(root, anywhere)];
    while let Some((dir, in_animated)) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let file_type = match entry.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
            let path = entry.path();

            if file_type.is_dir() {
                // Skip dotfolders (including the .git of a cloned wallpaper repo).
                if name.starts_with('.') || excluded.contains(&name) {
                    continue;
                }
                let child_animated = in_animated || animated_dirs.contains(&name);
                stack.push((path, child_animated));
            } else if file_type.is_file() {
                let ext = ext_lower(&path);
                match ext.as_str() {
                    "gif" => {
                        if !cfg.include_gif {
                            lib.gifs_disabled += 1;
                        } else if in_animated {
                            lib.gifs += 1;
                            lib.animated.push(path);
                        } else {
                            lib.animated_out_of_scope += 1;
                        }
                    }
                    "jpg" | "jpeg" | "png" | "bmp" => lib.statics.push(path),
                    "webp" if cfg.include_webp => lib.statics.push(path),
                    _ if is_video_ext(&ext) => {
                        if !cfg.include_video {
                            lib.videos_disabled += 1;
                        } else if in_animated {
                            lib.videos += 1;
                            lib.animated.push(path);
                        } else {
                            lib.animated_out_of_scope += 1;
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    // Stable order so a saved playlist seed keeps producing the same sequence.
    lib.statics.sort();
    lib.animated.sort();
    lib
}
