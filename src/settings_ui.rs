//! The "Launcher settings..." window: a normal top-level window hosting a
//! WebView2 editor page (presets/web/settings/index.html), so the launcher
//! and dashboard are configured with a real GUI instead of hand-edited JSON.
//!
//! It reuses the whole web.rs pipeline -- same environment, same virtual
//! hosts, same message handler -- which is what makes it small: the page
//! fetches /launcher.json like any preset, and saving goes through the
//! `save_launcher` message that web.rs already handles. The only piece that
//! lives here is the native side: the window itself, and the modal file
//! picker (dialogs must not run inside a COM event callback, so the picker
//! request is posted to this window and handled from its wndproc).
//!
//! Single instance: opening it again fronts the existing window.

use std::cell::{Cell, RefCell};

use webview2_com::CreateCoreWebView2ControllerCompletedHandler;
use webview2_com::Microsoft::Web::WebView2::Win32::{
    ICoreWebView2, ICoreWebView2Controller,
};
use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::WinRT::EventRegistrationToken;
use windows::Win32::UI::Controls::Dialogs::{
    GetOpenFileNameW, OFN_EXPLORER, OFN_FILEMUSTEXIST, OFN_NOCHANGEDIR, OPENFILENAMEW,
};
use windows::Win32::UI::WindowsAndMessaging::*;

const SETTINGS_CLASS: PCWSTR = w!("WallRotateSettings");
const TIMER_ENV: usize = 1;
/// Posted (wparam = request id) when the settings page asks for a file picker.
const WM_PICK_FILE: u32 = WM_APP + 30;

thread_local! {
    static CLASS_DONE: Cell<bool> = const { Cell::new(false) };
    static WINDOW: Cell<isize> = const { Cell::new(0) };
    static REQUESTED: Cell<bool> = const { Cell::new(false) };
    static VIEW: RefCell<Option<(ICoreWebView2Controller, ICoreWebView2)>> =
        const { RefCell::new(None) };
}

fn current_window() -> Option<HWND> {
    let raw = WINDOW.with(|c| c.get());
    if raw == 0 {
        return None;
    }
    let hwnd = HWND(raw as *mut _);
    if unsafe { IsWindow(hwnd).as_bool() } {
        Some(hwnd)
    } else {
        WINDOW.with(|c| c.set(0));
        None
    }
}

/// Open the settings window, or front it if it is already open.
pub fn open() {
    unsafe {
        if let Some(hwnd) = current_window() {
            let _ = ShowWindow(hwnd, SW_SHOWNORMAL);
            let _ = SetForegroundWindow(hwnd);
            return;
        }

        crate::web::write_presets();
        crate::web::ensure_environment();

        let hinstance = GetModuleHandleW(PCWSTR::null()).unwrap_or_default();
        CLASS_DONE.with(|done| {
            if !done.get() {
                let wc = WNDCLASSW {
                    lpfnWndProc: Some(wndproc),
                    hInstance: hinstance.into(),
                    lpszClassName: SETTINGS_CLASS,
                    hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
                    hIcon: LoadIconW(hinstance, PCWSTR(1 as _)).unwrap_or_default(),
                    ..Default::default()
                };
                RegisterClassW(&wc);
                done.set(true);
            }
        });

        // Centre on the primary work area.
        let mut work = RECT::default();
        let _ = SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            Some(&mut work as *mut _ as *mut _),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        );
        let (w_px, h_px) = (940, 700);
        let x = work.left + ((work.right - work.left) - w_px).max(0) / 2;
        let y = work.top + ((work.bottom - work.top) - h_px).max(0) / 2;

        let created = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            SETTINGS_CLASS,
            w!("WallRotate — Launcher settings"),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            x,
            y,
            w_px,
            h_px,
            None,
            None,
            hinstance,
            None,
        );
        let hwnd = match created {
            Ok(hwnd) if !hwnd.is_invalid() => hwnd,
            other => {
                crate::log::line(format!("settings: CreateWindowExW failed: {:?}", other));
                return;
            }
        };
        crate::log::line("settings: window created");
        WINDOW.with(|c| c.set(hwnd.0 as isize));
        REQUESTED.with(|c| c.set(false));
        // Poll until the shared WebView2 environment lands, then attach.
        SetTimer(hwnd, TIMER_ENV, 200, None);
        let _ = SetForegroundWindow(hwnd);
    }
}

/// Close the window if open (app shutdown).
pub fn close() {
    if let Some(hwnd) = current_window() {
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
    }
}

/// Called by web.rs when the settings page asks for a file picker. Posted so
/// the modal dialog runs from the wndproc, not inside the COM callback.
pub fn request_pick(id: i64) {
    if let Some(hwnd) = current_window() {
        unsafe {
            let _ = PostMessageW(hwnd, WM_PICK_FILE, WPARAM(id as usize), LPARAM(0));
        }
    }
}

fn begin_controller(hwnd: HWND) {
    let Some(env) = crate::web::environment() else {
        return;
    };
    REQUESTED.with(|c| c.set(true));
    let target = hwnd.0 as isize;
    let handler = CreateCoreWebView2ControllerCompletedHandler::create(Box::new(
        move |result, controller: Option<ICoreWebView2Controller>| {
            let Ok(()) = result else {
                crate::log::line(format!("settings: controller failed {:?}", result));
                return Ok(());
            };
            let Some(controller) = controller else {
                return Ok(());
            };
            let hwnd = HWND(target as *mut core::ffi::c_void);
            unsafe {
                if !IsWindow(hwnd).as_bool() {
                    let _ = controller.Close();
                    return Ok(());
                }
                if let Err(e) = attach(hwnd, controller) {
                    crate::log::line(format!("settings: attach failed {:?}", e));
                }
            }
            Ok(())
        },
    ));
    unsafe {
        if env.CreateCoreWebView2Controller(hwnd, &handler).is_err() {
            REQUESTED.with(|c| c.set(false));
        }
    }
}

unsafe fn attach(hwnd: HWND, controller: ICoreWebView2Controller) -> windows::core::Result<()> {
    let mut rc = RECT::default();
    let _ = GetClientRect(hwnd, &mut rc);
    controller.SetBounds(rc)?;
    let webview = controller.CoreWebView2()?;
    if let Ok(settings) = webview.Settings() {
        // A form, not a wallpaper: keep the editing context menus (copy,
        // paste, spellcheck) but no devtools or zoom.
        let _ = settings.SetAreDevToolsEnabled(windows::Win32::Foundation::FALSE);
        let _ = settings.SetIsZoomControlEnabled(windows::Win32::Foundation::FALSE);
        let _ = settings.SetIsStatusBarEnabled(windows::Win32::Foundation::FALSE);
    }
    crate::web::map_hosts(&webview, &crate::web::web_root(), None)?;
    let mut token = EventRegistrationToken::default();
    crate::web::attach_message_handler(&webview, &mut token)?;
    webview.Navigate(&HSTRING::from(format!(
        "https://{}/presets/settings/index.html",
        crate::web::PAGES_HOST
    )))?;
    VIEW.with(|v| *v.borrow_mut() = Some((controller, webview)));
    Ok(())
}

/// Native "browse for a target" dialog; replies to the page with the result.
unsafe fn pick_file(hwnd: HWND, id: i64) {
    let mut buf = [0u16; 4096];
    let filter: Vec<u16> = "Programs\0*.exe;*.lnk;*.bat;*.cmd;*.msc\0All files\0*.*\0\0"
        .encode_utf16()
        .collect();
    let mut ofn = OPENFILENAMEW {
        lStructSize: std::mem::size_of::<OPENFILENAMEW>() as u32,
        hwndOwner: hwnd,
        lpstrFilter: PCWSTR(filter.as_ptr()),
        lpstrFile: windows::core::PWSTR(buf.as_mut_ptr()),
        nMaxFile: buf.len() as u32,
        Flags: OFN_EXPLORER | OFN_FILEMUSTEXIST | OFN_NOCHANGEDIR,
        ..Default::default()
    };
    let picked = GetOpenFileNameW(&mut ofn).as_bool();
    let path = if picked {
        crate::util::from_wide_ptr(buf.as_ptr())
    } else {
        String::new()
    };
    VIEW.with(|v| {
        if let Some((_, webview)) = &*v.borrow() {
            crate::web::post_json(
                webview,
                &serde_json::json!({"type": "picked_file", "id": id, "path": path}),
            );
        }
    });
}

unsafe extern "system" fn wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_TIMER if wparam.0 == TIMER_ENV => {
            if REQUESTED.with(|c| c.get()) || VIEW.with(|v| v.borrow().is_some()) {
                KillTimer(hwnd, TIMER_ENV).ok();
            } else if crate::web::environment().is_some() {
                KillTimer(hwnd, TIMER_ENV).ok();
                begin_controller(hwnd);
            } else if crate::web::environment_failed() {
                KillTimer(hwnd, TIMER_ENV).ok();
                MessageBoxW(
                    hwnd,
                    w!("The WebView2 runtime is not available, so the settings window cannot load.\n\nYou can still edit launcher.json by hand from the tray menu."),
                    w!("WallRotate"),
                    MB_OK | MB_ICONWARNING,
                );
                let _ = DestroyWindow(hwnd);
            }
            return LRESULT(0);
        }
        WM_SIZE => {
            VIEW.with(|v| {
                if let Some((controller, _)) = &*v.borrow() {
                    let mut rc = RECT::default();
                    let _ = GetClientRect(hwnd, &mut rc);
                    let _ = controller.SetBounds(rc);
                }
            });
            return LRESULT(0);
        }
        WM_PICK_FILE => {
            pick_file(hwnd, wparam.0 as i64);
            return LRESULT(0);
        }
        WM_CLOSE => {
            let _ = DestroyWindow(hwnd);
            return LRESULT(0);
        }
        WM_DESTROY => {
            KillTimer(hwnd, TIMER_ENV).ok();
            VIEW.with(|v| {
                if let Some((controller, _)) = v.borrow_mut().take() {
                    let _ = controller.Close();
                }
            });
            WINDOW.with(|c| c.set(0));
            REQUESTED.with(|c| c.set(false));
            // No PostQuitMessage: this window shares the app's message loop.
            return LRESULT(0);
        }
        _ => {}
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}
