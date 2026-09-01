//! Notification-area icon.

use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_INFO, NIM_ADD, NIM_DELETE,
    NIM_MODIFY, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{HICON, WM_APP};

/// Mouse activity on the tray icon arrives here.
pub const WM_TRAY: u32 = WM_APP + 1;
const TRAY_UID: u32 = 1;

pub struct Tray {
    data: NOTIFYICONDATAW,
    added: bool,
}

impl Tray {
    pub fn new(hwnd: HWND, icon: HICON, tip: &str) -> Self {
        let mut data = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd,
            uID: TRAY_UID,
            uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
            uCallbackMessage: WM_TRAY,
            hIcon: icon,
            ..Default::default()
        };
        copy_into(&mut data.szTip, tip);
        Tray { data, added: false }
    }

    /// Idempotent: also used to re-add the icon after Explorer restarts.
    pub fn add(&mut self) {
        unsafe {
            if self.added {
                let _ = Shell_NotifyIconW(NIM_DELETE, &self.data);
            }
            self.data.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
            self.added = Shell_NotifyIconW(NIM_ADD, &self.data).as_bool();
        }
    }

    pub fn remove(&mut self) {
        if self.added {
            unsafe {
                let _ = Shell_NotifyIconW(NIM_DELETE, &self.data);
            }
            self.added = false;
        }
    }

    pub fn set_tip(&mut self, tip: &str) {
        copy_into(&mut self.data.szTip, tip);
        if self.added {
            unsafe {
                self.data.uFlags = NIF_TIP;
                let _ = Shell_NotifyIconW(NIM_MODIFY, &self.data);
            }
        }
    }

    pub fn balloon(&mut self, title: &str, text: &str) {
        if !self.added {
            return;
        }
        copy_into(&mut self.data.szInfoTitle, title);
        copy_into(&mut self.data.szInfo, text);
        self.data.dwInfoFlags = NIIF_INFO;
        unsafe {
            self.data.uFlags = NIF_INFO;
            let _ = Shell_NotifyIconW(NIM_MODIFY, &self.data);
        }
        // Leave szInfo empty afterwards so later NIM_MODIFY calls stay quiet.
        copy_into(&mut self.data.szInfo, "");
        copy_into(&mut self.data.szInfoTitle, "");
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        self.remove();
    }
}

fn copy_into(dst: &mut [u16], s: &str) {
    for slot in dst.iter_mut() {
        *slot = 0;
    }
    if dst.is_empty() {
        return;
    }
    let src: Vec<u16> = s.encode_utf16().collect();
    let n = src.len().min(dst.len() - 1);
    dst[..n].copy_from_slice(&src[..n]);
    dst[n] = 0;
}
