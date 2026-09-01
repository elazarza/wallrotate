//! Per-monitor wallpaper through the shell's IDesktopWallpaper (Windows 8+).

use crate::util::{from_wide_ptr, wide, wide_path};
use std::path::Path;
use windows::core::{Result as WResult, PCWSTR};
use windows::Win32::Foundation::RECT;
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_ALL};
use windows::Win32::UI::Shell::{
    DesktopWallpaper, IDesktopWallpaper, DWPOS_CENTER, DWPOS_FILL, DWPOS_FIT, DWPOS_SPAN,
    DWPOS_STRETCH, DWPOS_TILE,
};

#[derive(Clone)]
pub struct MonitorInfo {
    /// Device path, the handle IDesktopWallpaper wants for per-monitor calls.
    pub id: String,
    /// Bounds in virtual-screen coordinates (physical pixels).
    pub rect: RECT,
}

impl MonitorInfo {
    pub fn width(&self) -> i32 {
        self.rect.right - self.rect.left
    }
    pub fn height(&self) -> i32 {
        self.rect.bottom - self.rect.top
    }
    /// Short, human-readable label for menus.
    pub fn label(&self, index: usize) -> String {
        format!("Screen {} ({}x{})", index + 1, self.width(), self.height())
    }
}

pub struct Wallpaper {
    api: IDesktopWallpaper,
}

impl Wallpaper {
    pub fn new() -> WResult<Self> {
        let api: IDesktopWallpaper =
            unsafe { CoCreateInstance(&DesktopWallpaper, None, CLSCTX_ALL)? };
        Ok(Wallpaper { api })
    }

    /// Active monitors only, ordered left-to-right then top-to-bottom.
    pub fn monitors(&self) -> Vec<MonitorInfo> {
        let mut out: Vec<MonitorInfo> = Vec::new();
        unsafe {
            let count = match self.api.GetMonitorDevicePathCount() {
                Ok(c) => c,
                Err(_) => return out,
            };
            for i in 0..count {
                let id_ptr = match self.api.GetMonitorDevicePathAt(i) {
                    Ok(p) => p,
                    Err(_) => continue,
                };
                if id_ptr.is_null() {
                    continue;
                }
                let id = from_wide_ptr(id_ptr.0);
                // Detached monitors stay in the list; GetMonitorRECT fails for them.
                let rect = self.api.GetMonitorRECT(PCWSTR(id_ptr.0));
                CoTaskMemFree(Some(id_ptr.0 as *const core::ffi::c_void));
                if id.is_empty() {
                    continue;
                }
                if let Ok(rect) = rect {
                    if rect.right > rect.left && rect.bottom > rect.top {
                        out.push(MonitorInfo { id, rect });
                    }
                }
            }
        }
        out.sort_by_key(|m| (m.rect.left, m.rect.top));
        out
    }

    pub fn set(&self, monitor_id: &str, file: &Path) -> WResult<()> {
        let id = wide(monitor_id);
        let p = wide_path(file);
        unsafe {
            self.api
                .SetWallpaper(PCWSTR(id.as_ptr()), PCWSTR(p.as_ptr()))
        }
    }

    pub fn set_position(&self, fit: &str) {
        let pos = match fit.to_ascii_lowercase().as_str() {
            "fit" => DWPOS_FIT,
            "stretch" => DWPOS_STRETCH,
            "center" => DWPOS_CENTER,
            "tile" => DWPOS_TILE,
            "span" => DWPOS_SPAN,
            _ => DWPOS_FILL,
        };
        unsafe {
            let _ = self.api.SetPosition(pos);
        }
    }
}
