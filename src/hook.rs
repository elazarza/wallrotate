//! Click forwarding for web wallpapers.
//!
//! The desktop icon layer (SHELLDLL_DefView / SysListView32) sits above our
//! surfaces and receives every mouse event, so a WebView2 wallpaper would
//! never see a click. While at least one web surface exists and
//! `web_interactive` is on, we install a WH_MOUSE_LL hook; when the cursor is
//! over bare desktop (WindowFromPoint resolves to the shell's desktop
//! windows), we re-post the event to the WebView2 child window under that
//! point. Icons still work -- clicks on an icon hit the icon, not bare
//! desktop, and we forward those too (they land on empty page area harmlessly
//! since the shell already handled them; double-activation is only possible
//! if a launcher tile sits exactly beneath an icon, which the presets avoid
//! by keeping the icon margin clear).
//!
//! The hook is only ever installed on the main thread and removed the moment
//! no web surface remains, so the rest of the time WallRotate stays invisible
//! to the input pipeline.

use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::Mutex;

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::ScreenToClient;
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, ChildWindowFromPointEx, GetAncestor, GetClassNameW, PostMessageW,
    SetWindowsHookExW, UnhookWindowsHookEx, WindowFromPoint, CWP_SKIPDISABLED,
    CWP_SKIPINVISIBLE, GA_ROOT, HHOOK, MSLLHOOKSTRUCT, WH_MOUSE_LL, WM_LBUTTONDOWN,
    WM_LBUTTONUP, WM_MOUSEMOVE,
};

/// (screen rect of the monitor, our surface hwnd) for every web surface.
static TARGETS: Mutex<Vec<(RECT, isize)>> = Mutex::new(Vec::new());
static HOOK: AtomicIsize = AtomicIsize::new(0);
static BUTTON_DOWN: AtomicBool = AtomicBool::new(false);

const MK_LBUTTON: usize = 0x0001;

/// Tell the hook which surfaces host web pages. Installs or removes the hook
/// as the set becomes non-empty / empty. Main thread only.
pub fn sync(targets: Vec<(RECT, isize)>) {
    let want = !targets.is_empty();
    if let Ok(mut t) = TARGETS.lock() {
        *t = targets;
    }
    let have = HOOK.load(Ordering::Relaxed) != 0;
    if want && !have {
        match unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), None, 0) } {
            Ok(h) => {
                HOOK.store(h.0 as isize, Ordering::Relaxed);
                crate::log::line("hook: mouse hook installed");
            }
            Err(e) => crate::log::line(format!("hook: install failed {:?}", e)),
        }
    } else if !want && have {
        let h = HHOOK(HOOK.swap(0, Ordering::Relaxed) as *mut _);
        unsafe {
            let _ = UnhookWindowsHookEx(h);
        }
        crate::log::line("hook: mouse hook removed");
    }
}

pub fn clear() {
    sync(Vec::new());
}

fn class_of(hwnd: HWND) -> String {
    let mut buf = [0u16; 64];
    let n = unsafe { GetClassNameW(hwnd, &mut buf) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

/// True when the point is over the shell's desktop (icons or empty space),
/// i.e. nothing of the user's applications is in the way.
fn over_desktop(pt: POINT) -> bool {
    let hit = unsafe { WindowFromPoint(pt) };
    if hit.0.is_null() {
        return false;
    }
    let root = unsafe { GetAncestor(hit, GA_ROOT) };
    matches!(class_of(root).as_str(), "Progman" | "WorkerW")
}

/// Descend from our surface into the WebView2 child chain at this point.
fn deepest_child_at(surface: HWND, screen_pt: POINT) -> (HWND, POINT) {
    let mut cur = surface;
    loop {
        let mut client = screen_pt;
        unsafe {
            let _ = ScreenToClient(cur, &mut client);
        }
        let child = unsafe {
            ChildWindowFromPointEx(cur, client, CWP_SKIPINVISIBLE | CWP_SKIPDISABLED)
        };
        if child.0.is_null() || child == cur {
            return (cur, client);
        }
        cur = child;
    }
}

unsafe extern "system" fn mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        let msg = wparam.0 as u32;
        if matches!(msg, WM_MOUSEMOVE | WM_LBUTTONDOWN | WM_LBUTTONUP) {
            let info = &*(lparam.0 as *const MSLLHOOKSTRUCT);
            let pt = info.pt;
            // try_lock: never stall the input pipeline.
            if let Ok(targets) = TARGETS.try_lock() {
                if let Some(&(_, surface)) = targets.iter().find(|(r, _)| {
                    pt.x >= r.left && pt.x < r.right && pt.y >= r.top && pt.y < r.bottom
                }) {
                    if over_desktop(pt) {
                        let (target, client) =
                            deepest_child_at(HWND(surface as *mut _), pt);
                        let wp = match msg {
                            WM_LBUTTONDOWN => {
                                BUTTON_DOWN.store(true, Ordering::Relaxed);
                                MK_LBUTTON
                            }
                            WM_LBUTTONUP => {
                                BUTTON_DOWN.store(false, Ordering::Relaxed);
                                0
                            }
                            _ => {
                                if BUTTON_DOWN.load(Ordering::Relaxed) {
                                    MK_LBUTTON
                                } else {
                                    0
                                }
                            }
                        };
                        let lp = ((client.y as u32) << 16) | (client.x as u32 & 0xFFFF);
                        let _ = PostMessageW(target, msg, WPARAM(wp), LPARAM(lp as isize));
                    }
                }
            }
        }
    }
    CallNextHookEx(None, code, wparam, lparam)
}
