//! WallRotate -- a low-footprint multi-monitor wallpaper rotator.
//!
//! Design notes:
//!  * There is no polling anywhere. Everything is driven by window messages:
//!    one long timer for the rotation schedule, one timer per animated screen,
//!    and OS notifications for power/session/display changes. Idle cost is a
//!    blocked GetMessageW call.
//!  * Wallpapers are applied per monitor through IDesktopWallpaper, so each
//!    screen genuinely gets its own image.
//!  * Animated GIFs are drawn by a child window parented into the shell's
//!    WorkerW (see desktop.rs), above the wallpaper but below desktop icons.

#![windows_subsystem = "windows"]

mod autostart;
mod config;
mod desktop;
mod gifanim;
mod hook;
mod hotkey;
mod log;
mod scan;
mod settings_ui;
mod stats;
mod video;
mod state;
mod tray;
mod util;
mod wallpaper;
mod web;

use config::{AnimatedMode, Config};
use desktop::{Animation, DesktopLayer};
use gifanim::{GifAnim, Limits};
use scan::Library;
use state::{Assignment, Playlist, State};
use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use tray::{Tray, WM_TRAY};
use util::{human_duration, now_unix, wide, Rng};
use wallpaper::{MonitorInfo, Wallpaper};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{
    GetLastError, ERROR_ALREADY_EXISTS, HANDLE, HWND, LPARAM, LRESULT, POINT, TRUE, WPARAM,
};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Power::{
    GetSystemPowerStatus, RegisterPowerSettingNotification, POWERBROADCAST_SETTING,
    SYSTEM_POWER_STATUS,
};
use windows::Win32::System::ProcessStatus::EmptyWorkingSet;
use windows::Win32::System::RemoteDesktop::{
    WTSRegisterSessionNotification, WTSUnRegisterSessionNotification, NOTIFY_FOR_THIS_SESSION,
};
use windows::Win32::System::SystemServices::{GUID_ACDC_POWER_SOURCE, GUID_SESSION_DISPLAY_STATUS};
use windows::Win32::System::Threading::{CreateMutexW, GetCurrentProcess};
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{RegisterHotKey, UnregisterHotKey};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::*;

const MAIN_CLASS: PCWSTR = w!("WallRotateMainWindow");
const MUTEX_NAME: PCWSTR = w!("Local\\WallRotate.Singleton.v1");

const TIMER_ROTATE: usize = 100;
/// One-shot retries after an Explorer restart; the shell needs a moment before
/// Progman will hand out a WorkerW again.
const TIMER_RELAYER: usize = 101;
const RELAYER_MAX_TRIES: u32 = 6;
/// Fires shortly after attaching, because the shell sometimes retires the
/// WorkerW we just parented into and our surfaces go with it.
const TIMER_VERIFY: usize = 102;
const VERIFY_DELAY_MS: u32 = 2500;
const VERIFY_MAX_TRIES: u32 = 4;
/// Never sleep longer than an hour, so a resumed machine catches up quickly.
const MAX_TIMER_SECS: u64 = 3600;
const HOTKEY_ID: i32 = 1;

// Commands a second instance can post at the running one.
const WM_CMD_NEXT: u32 = WM_APP + 2;
const WM_CMD_PREV: u32 = WM_APP + 3;
const WM_CMD_RESCAN: u32 = WM_APP + 4;
/// wparam carries the 0-based screen index.
const WM_CMD_NEXT_SCREEN: u32 = WM_APP + 5;
/// Posted by web.rs after the settings GUI rewrites launcher.json.
pub(crate) const WM_CMD_WEB_RELOAD: u32 = WM_APP + 6;
/// `wallrotate --settings`: open the launcher settings window.
const WM_CMD_OPEN_SETTINGS: u32 = WM_APP + 7;

// Tray menu item ids.
const ID_NEXT: usize = 1001;
const ID_PREV: usize = 1002;
const ID_RESCAN: usize = 1003;
const ID_OPEN_FOLDER: usize = 1004;
const ID_OPEN_CONFIG: usize = 1005;
const ID_RELOAD: usize = 1006;
const ID_ANIM_OFF: usize = 1007;
const ID_ANIM_MIXED: usize = 1008;
const ID_ANIM_ALWAYS: usize = 1009;
const ID_AUTOSTART: usize = 1010;
const ID_CURRENT: usize = 1011;
const ID_EXIT: usize = 1012;
const ID_USE_GIF: usize = 1013;
const ID_USE_VIDEO: usize = 1014;
const ID_ANIM_FOLDER_ONLY: usize = 1015;
const ID_ROTATE_ALL: usize = 1016;
const ID_WEB_OFF: usize = 1017;
const ID_WEB_GRID: usize = 1018;
const ID_WEB_DOCK: usize = 1019;
const ID_WEB_MINIMAL: usize = 1020;
const ID_WEB_INTERACTIVE: usize = 1021;
const ID_WEB_EDIT: usize = 1022;
const ID_WEB_ALL: usize = 1023;
const ID_WEB_SETTINGS: usize = 1024;
const ID_WEB_DASHBOARD: usize = 1025;
/// One id per screen, offset by its index.
const ID_ROTATE_SCREEN_BASE: usize = 1100;
/// One-shot "change this screen now", one id per screen.
const ID_NEXT_SCREEN_BASE: usize = 1200;
/// Which screens show the web launcher, one id per screen.
const ID_WEB_SCREEN_BASE: usize = 1300;

struct App {
    hwnd: HWND,
    cfg: Config,
    st: State,
    lib: Library,
    wp: Wallpaper,
    monitors: Vec<MonitorInfo>,
    layer: DesktopLayer,
    tray: Tray,
    hotkey_label: String,
    hotkey_ok: bool,
    locked: bool,
    display_off: bool,
    /// Guards against a bogus "display off" at registration time.
    display_seen_on: bool,
    on_battery: bool,
    exe: PathBuf,
    /// Per-monitor notes about the loaded animation (size, frames, memory).
    anim_info: Vec<(String, String)>,
    /// Decoded animations currently in play, kept so the layer can be rebuilt
    /// after an Explorer restart without paying for another GIF decode.
    anims: Vec<(MonitorInfo, Animation)>,
    relayer_tries: u32,
    verify_tries: u32,
}

impl App {
    fn should_suspend(&self) -> bool {
        self.locked
            || self.display_off
            || (self.cfg.pause_on_battery && self.on_battery)
    }

    fn next_due(&self) -> u64 {
        self.st.last_rotate.saturating_add(self.cfg.interval_secs())
    }
}

thread_local! {
    static APP: Cell<*mut App> = const { Cell::new(std::ptr::null_mut()) };
}

fn app<'a>() -> Option<&'a mut App> {
    let ptr = APP.with(|c| c.get());
    if ptr.is_null() {
        None
    } else {
        Some(unsafe { &mut *ptr })
    }
}

// ------------------------------------------------------------------- main ---

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let verb = args.first().map(|s| s.to_ascii_lowercase()).unwrap_or_default();

    match verb.as_str() {
        "--install" | "/install" => {
            install();
            return;
        }
        "--uninstall" | "/uninstall" => {
            uninstall();
            return;
        }
        "--help" | "/?" | "-h" => {
            info_box(
                "WallRotate",
                "Usage:\n\
                 wallrotate.exe                 run in the tray\n\
                 wallrotate.exe --install       copy to %LOCALAPPDATA% and run at login\n\
                 wallrotate.exe --uninstall     stop, and remove the login entry\n\
                 wallrotate.exe --next          advance to the next wallpapers\n\
                 wallrotate.exe --prev          go back\n\
                 wallrotate.exe --screen N      change only screen N (1 = leftmost)\n\
                 wallrotate.exe --rescan        re-read the wallpaper folder\n\
                 wallrotate.exe --settings      open the launcher settings window\n\
                 wallrotate.exe --quit          stop the running instance",
            );
            return;
        }
        _ => {}
    }

    unsafe {
        // Physical pixels everywhere; the desktop layer depends on it.
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    // Control verbs only ever talk to a running instance. Without this they
    // would fall through and start a tray app, which is the opposite of what
    // "--quit" means.
    if is_control_verb(&verb) {
        let number = args.get(1).and_then(|s| s.parse::<usize>().ok());
        forward_command(&verb, number);
        return;
    }

    // A second plain launch just hands off to the instance already running.
    let mutex: windows::core::Result<HANDLE> =
        unsafe { CreateMutexW(None, TRUE, MUTEX_NAME) };
    let already_running = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    if already_running {
        return;
    }
    let _mutex_guard = mutex;

    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }

    let wp = match Wallpaper::new() {
        Ok(w) => w,
        Err(_) => {
            error_box(
                "WallRotate",
                "Could not reach the Windows desktop wallpaper service.\n\
                 This program needs Windows 8 or newer.",
            );
            return;
        }
    };

    let cfg = config::load();
    config::upgrade_file(&cfg);
    if cfg.web_active() {
        // Refresh the materialised preset pages so an upgraded exe's designs
        // take effect; the user's launcher.json is never overwritten.
        web::write_presets();
    }
    let st = state::load();
    let exe = std::env::current_exe().unwrap_or_default();

    let hwnd = match create_main_window() {
        Some(h) => h,
        None => return,
    };

    let icon = load_app_icon(true);
    let mut tray_icon = Tray::new(hwnd, icon, "WallRotate");
    tray_icon.add();

    let mut instance = Box::new(App {
        hwnd,
        cfg,
        st,
        lib: Library::default(),
        wp,
        monitors: Vec::new(),
        layer: DesktopLayer::new(),
        tray: tray_icon,
        hotkey_label: String::new(),
        hotkey_ok: false,
        locked: false,
        display_off: false,
        display_seen_on: false,
        on_battery: false,
        exe,
        anim_info: Vec::new(),
        anims: Vec::new(),
        relayer_tries: 0,
        verify_tries: 0,
    });
    // The box stays owned by main; the wndproc only gets a borrow handle.
    let raw: *mut App = &mut *instance;
    APP.with(|c| c.set(raw));

    unsafe {
        register_notifications(hwnd);
    }

    {
        let a = app().expect("app installed");
        a.on_battery = on_battery();
        a.lib = scan::scan(&a.cfg);
        register_hotkey(a);
        first_run_checks(a);
        startup_apply(a);
        schedule_timer(a);
        update_tip(a);
        trim_working_set();
    }

    // Message pump. Idle cost is one blocked call.
    unsafe {
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).into() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    {
        let a = app().expect("app installed");
        settings_ui::close();
        hook::clear();
        a.layer.clear();
        a.tray.remove();
        if a.hotkey_ok {
            unsafe {
                let _ = UnregisterHotKey(a.hwnd, HOTKEY_ID);
            }
        }
        unsafe {
            let _ = WTSUnRegisterSessionNotification(a.hwnd);
        }
        state::save(&a.st);
    }
    APP.with(|c| c.set(std::ptr::null_mut()));
    drop(instance);

    unsafe {
        CoUninitialize();
    }
}

// ---------------------------------------------------------------- window ---

fn create_main_window() -> Option<HWND> {
    unsafe {
        let hinstance = GetModuleHandleW(PCWSTR::null()).ok()?;
        let wc = WNDCLASSW {
            lpfnWndProc: Some(main_proc),
            hInstance: hinstance.into(),
            lpszClassName: MAIN_CLASS,
            hIcon: load_app_icon(false),
            ..Default::default()
        };
        RegisterClassW(&wc);
        // A real top-level window (never shown) rather than a message-only one,
        // because only top-level windows receive the TaskbarCreated broadcast.
        CreateWindowExW(
            WS_EX_TOOLWINDOW,
            MAIN_CLASS,
            w!("WallRotate"),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            None,
            None,
            hinstance,
            None,
        )
        .ok()
    }
}

fn load_app_icon(small: bool) -> HICON {
    unsafe {
        let hinstance = GetModuleHandleW(PCWSTR::null()).unwrap_or_default();
        let (cx, cy) = if small {
            (
                GetSystemMetrics(SM_CXSMICON),
                GetSystemMetrics(SM_CYSMICON),
            )
        } else {
            (GetSystemMetrics(SM_CXICON), GetSystemMetrics(SM_CYICON))
        };
        // Resource id 1 comes from app.rc.
        match LoadImageW(
            hinstance,
            PCWSTR(1 as *const u16),
            IMAGE_ICON,
            cx,
            cy,
            LR_DEFAULTCOLOR,
        ) {
            Ok(h) => HICON(h.0),
            Err(_) => LoadIconW(None, IDI_APPLICATION).unwrap_or_default(),
        }
    }
}

unsafe fn register_notifications(hwnd: HWND) {
    let _ = WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION);
    // GUID_SESSION_DISPLAY_STATUS is the per-session display state. Its console
    // counterpart is documented as meaningless outside the console session and
    // reports "off" on this machine while the monitors are plainly on.
    let _ = RegisterPowerSettingNotification(
        HANDLE(hwnd.0),
        &GUID_SESSION_DISPLAY_STATUS,
        DEVICE_NOTIFY_WINDOW_HANDLE,
    );
    let _ = RegisterPowerSettingNotification(
        HANDLE(hwnd.0),
        &GUID_ACDC_POWER_SOURCE,
        DEVICE_NOTIFY_WINDOW_HANDLE,
    );
}

unsafe extern "system" fn main_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // Explorer restarted: the tray icon and the WorkerW both went away.
    let taskbar_created = taskbar_created_message();
    if msg == taskbar_created && taskbar_created != 0 {
        if let Some(a) = app() {
            a.tray.add();
            update_tip(a);
            // The desktop is not ready yet, so schedule the rebuild instead of
            // trying (and failing) right now.
            a.relayer_tries = 0;
            SetTimer(hwnd, TIMER_RELAYER, 1200, None);
        }
        return LRESULT(0);
    }

    match msg {
        WM_TRAY => {
            let event = lparam.0 as u32;
            if event == WM_RBUTTONUP || event == WM_LBUTTONUP || event == WM_CONTEXTMENU {
                if let Some(a) = app() {
                    show_menu(a);
                }
            } else if event == WM_LBUTTONDBLCLK {
                if let Some(a) = app() {
                    rotate(a, 1);
                }
            }
            return LRESULT(0);
        }
        WM_HOTKEY => {
            if wparam.0 as i32 == HOTKEY_ID {
                if let Some(a) = app() {
                    rotate(a, 1);
                }
            }
            return LRESULT(0);
        }
        WM_TIMER => {
            if wparam.0 == TIMER_ROTATE {
                if let Some(a) = app() {
                    if now_unix() >= a.next_due() {
                        rotate(a, 1);
                    } else {
                        heal_layer(a);
                        schedule_timer(a);
                        update_tip(a);
                    }
                }
                return LRESULT(0);
            }
            if wparam.0 == TIMER_VERIFY {
                KillTimer(hwnd, TIMER_VERIFY).ok();
                if let Some(a) = app() {
                    let missing = !a.anims.is_empty() && !a.layer.is_active();
                    if (a.layer.is_broken() || missing) && a.verify_tries < VERIFY_MAX_TRIES {
                        let tries = a.verify_tries + 1;
                        log::line(format!(
                            "verify: layer {} -- rebuild #{}",
                            if missing { "never attached" } else { "was torn down" },
                            tries
                        ));
                        rebuild_layer(a);
                        a.verify_tries = tries;
                        unsafe {
                            SetTimer(hwnd, TIMER_VERIFY, VERIFY_DELAY_MS, None);
                        }
                    }
                }
                return LRESULT(0);
            }
            if wparam.0 == TIMER_RELAYER {
                KillTimer(hwnd, TIMER_RELAYER).ok();
                if let Some(a) = app() {
                    a.relayer_tries += 1;
                    rebuild_layer(a);
                    let unfinished = !a.anims.is_empty() && !a.layer.is_active();
                    if (unfinished || a.layer.is_broken())
                        && a.relayer_tries < RELAYER_MAX_TRIES
                    {
                        SetTimer(hwnd, TIMER_RELAYER, 2000, None);
                    }
                }
                return LRESULT(0);
            }
        }
        WM_CMD_NEXT => {
            if let Some(a) = app() {
                rotate(a, 1);
            }
            return LRESULT(0);
        }
        WM_CMD_PREV => {
            if let Some(a) = app() {
                rotate(a, -1);
            }
            return LRESULT(0);
        }
        WM_CMD_NEXT_SCREEN => {
            if let Some(a) = app() {
                rotate_one(a, wparam.0);
            }
            return LRESULT(0);
        }
        WM_CMD_RESCAN => {
            if let Some(a) = app() {
                a.lib = scan::scan(&a.cfg);
                rotate(a, 1);
            }
            return LRESULT(0);
        }
        WM_CMD_WEB_RELOAD => {
            // launcher.json was rewritten by the settings GUI.
            if let Some(a) = app() {
                a.layer.reload_web();
            }
            return LRESULT(0);
        }
        WM_CMD_OPEN_SETTINGS => {
            log::line("main: WM_CMD_OPEN_SETTINGS received");
            settings_ui::open();
            return LRESULT(0);
        }
        WM_DISPLAYCHANGE => {
            if let Some(a) = app() {
                // Remote-desktop connects fire this without changing anything.
                // A needless rebuild restarts every clip and costs a burst of
                // decoding, so only react when the monitors really moved.
                let now = a.wp.monitors();
                let same = now.len() == a.monitors.len()
                    && now.iter().zip(&a.monitors).all(|(x, y)| {
                        x.id == y.id
                            && x.rect.left == y.rect.left
                            && x.rect.top == y.rect.top
                            && x.rect.right == y.rect.right
                            && x.rect.bottom == y.rect.bottom
                    });
                if !same || a.layer.is_broken() {
                    log::line("display change: monitor layout changed, reapplying");
                    reapply(a);
                } else {
                    log::line("display change: layout unchanged, ignoring");
                }
            }
            return LRESULT(0);
        }
        WM_WTSSESSION_CHANGE => {
            if let Some(a) = app() {
                match wparam.0 as u32 {
                    0x7 => a.locked = true, // WTS_SESSION_LOCK
                    0x8 => {
                        a.locked = false; // WTS_SESSION_UNLOCK
                        heal_layer(a);
                    }
                    _ => {}
                }
                a.layer.suspend(a.should_suspend());
            }
            return LRESULT(0);
        }
        WM_POWERBROADCAST => {
            if let Some(a) = app() {
                match wparam.0 as u32 {
                    PBT_POWERSETTINGCHANGE => {
                        let setting = lparam.0 as *const POWERBROADCAST_SETTING;
                        if !setting.is_null() {
                            let guid = (*setting).PowerSetting;
                            let value = *(*setting).Data.as_ptr();
                            if guid == GUID_SESSION_DISPLAY_STATUS {
                                // Only believe "off" once we have seen "on".
                                // The value delivered at registration time is
                                // not always trustworthy, and taking it at face
                                // value suspends every animation permanently.
                                if value != 0 {
                                    a.display_seen_on = true;
                                    a.display_off = false;
                                } else if a.display_seen_on {
                                    a.display_off = true;
                                }
                                log::line(format!(
                                    "power: session display status {} -> display_off={}",
                                    value, a.display_off
                                ));
                            } else if guid == GUID_ACDC_POWER_SOURCE {
                                a.on_battery = value != 0;
                                log::line(format!("power: ac/dc source {}", value));
                            }
                            a.layer.suspend(a.should_suspend());
                        }
                    }
                    PBT_APMRESUMEAUTOMATIC | PBT_APMRESUMESUSPEND => {
                        // Wall clock may have jumped past the due time.
                        if now_unix() >= a.next_due() {
                            rotate(a, 1);
                        } else {
                            schedule_timer(a);
                            update_tip(a);
                        }
                    }
                    _ => {}
                }
            }
            return LRESULT(TRUE.0 as isize);
        }
        WM_TIMECHANGE => {
            if let Some(a) = app() {
                schedule_timer(a);
                update_tip(a);
            }
            return LRESULT(0);
        }
        WM_ENDSESSION => {
            if let Some(a) = app() {
                state::save(&a.st);
            }
            return LRESULT(0);
        }
        WM_CLOSE => {
            DestroyWindow(hwnd).ok();
            return LRESULT(0);
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            return LRESULT(0);
        }
        _ => {}
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

fn taskbar_created_message() -> u32 {
    thread_local! {
        static MSG_ID: Cell<u32> = const { Cell::new(0) };
    }
    MSG_ID.with(|c| {
        if c.get() == 0 {
            let id = unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) };
            c.set(id);
        }
        c.get()
    })
}

// ------------------------------------------------------------------ menu ---

unsafe fn show_menu(a: &mut App) {
    let menu = match CreatePopupMenu() {
        Ok(m) => m,
        Err(_) => return,
    };

    let mut header_text = format!(
        "WallRotate  --  {} images, {} GIFs, {} videos",
        a.lib.statics.len(),
        a.lib.gifs,
        a.lib.videos
    );
    if a.lib.videos_disabled > 0 {
        // Be explicit rather than silently ignoring them.
        header_text.push_str(&format!(" ({} videos off)", a.lib.videos_disabled));
    }
    if a.lib.animated_out_of_scope > 0 {
        header_text.push_str(&format!(
            " ({} outside animated_dirs)",
            a.lib.animated_out_of_scope
        ));
    }
    let header = wide(&header_text);
    let _ = AppendMenuW(
        menu,
        MF_STRING | MF_DISABLED | MF_GRAYED,
        0,
        PCWSTR(header.as_ptr()),
    );
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());

    let next_label = if a.hotkey_ok && !a.hotkey_label.is_empty() {
        format!("Next wallpapers\t{}", a.hotkey_label)
    } else {
        String::from("Next wallpapers")
    };
    let next_w = wide(&next_label);
    let _ = AppendMenuW(menu, MF_STRING, ID_NEXT, PCWSTR(next_w.as_ptr()));
    let _ = AppendMenuW(menu, MF_STRING, ID_PREV, w!("Previous wallpapers"));

    // One-shot: change a single screen and leave the others untouched.
    if a.monitors.len() > 1 {
        if let Ok(sub) = CreatePopupMenu() {
            for (i, monitor) in a.monitors.iter().enumerate() {
                let label = wide(&monitor.label(i));
                let _ = AppendMenuW(
                    sub,
                    MF_STRING,
                    ID_NEXT_SCREEN_BASE + i,
                    PCWSTR(label.as_ptr()),
                );
            }
            let _ = AppendMenuW(
                menu,
                MF_POPUP,
                sub.0 as usize,
                w!("Change just one screen"),
            );
        }
    }
    let _ = AppendMenuW(menu, MF_STRING, ID_CURRENT, w!("Show what is on screen..."));
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());

    // Animated submenu.
    if let Ok(sub) = CreatePopupMenu() {
        let mode = a.cfg.animated_mode();
        let flag = |on: bool| {
            if on {
                MF_STRING | MF_CHECKED
            } else {
                MF_STRING
            }
        };
        let _ = AppendMenuW(
            sub,
            flag(mode == AnimatedMode::Off),
            ID_ANIM_OFF,
            w!("Off -- still images only"),
        );
        let _ = AppendMenuW(
            sub,
            flag(mode == AnimatedMode::Mixed),
            ID_ANIM_MIXED,
            w!("Mixed -- some screens animate"),
        );
        let _ = AppendMenuW(
            sub,
            flag(mode == AnimatedMode::Always),
            ID_ANIM_ALWAYS,
            w!("Only animated -- every screen, GIF or video"),
        );
        let _ = AppendMenuW(sub, MF_SEPARATOR, 0, PCWSTR::null());

        // What may go in the animated pool.
        let gif_label = wide(&format!("Use GIFs ({})", a.lib.gifs));
        let _ = AppendMenuW(
            sub,
            flag(a.cfg.include_gif),
            ID_USE_GIF,
            PCWSTR(gif_label.as_ptr()),
        );
        let video_label = wide(&format!("Use videos ({})", a.lib.videos));
        let _ = AppendMenuW(
            sub,
            flag(a.cfg.include_video),
            ID_USE_VIDEO,
            PCWSTR(video_label.as_ptr()),
        );
        let _ = AppendMenuW(
            sub,
            flag(a.cfg.animated_folder_only()),
            ID_ANIM_FOLDER_ONLY,
            w!("Only from the \"animated\" folder"),
        );

        let label = wide(&format!(
            "Animated backgrounds ({})",
            match mode {
                AnimatedMode::Off => "off",
                AnimatedMode::Mixed => "mixed",
                AnimatedMode::Always => "only animated",
            }
        ));
        let _ = AppendMenuW(menu, MF_POPUP, sub.0 as usize, PCWSTR(label.as_ptr()));
    }

    // Which screens rotate.
    if let Ok(sub) = CreatePopupMenu() {
        let flag = |on: bool| {
            if on {
                MF_STRING | MF_CHECKED
            } else {
                MF_STRING
            }
        };
        let _ = AppendMenuW(
            sub,
            flag(a.cfg.rotates_all_screens()),
            ID_ROTATE_ALL,
            w!("All screens"),
        );
        let _ = AppendMenuW(sub, MF_SEPARATOR, 0, PCWSTR::null());
        for (i, monitor) in a.monitors.iter().enumerate() {
            let label = wide(&monitor.label(i));
            let _ = AppendMenuW(
                sub,
                flag(a.cfg.rotates_screen(i)),
                ID_ROTATE_SCREEN_BASE + i,
                PCWSTR(label.as_ptr()),
            );
        }
        let label = wide(&format!(
            "Rotate ({})",
            a.cfg.rotate_screens_label(a.monitors.len())
        ));
        let _ = AppendMenuW(menu, MF_POPUP, sub.0 as usize, PCWSTR(label.as_ptr()));
    }

    // The clickable web launcher wallpaper.
    if let Ok(sub) = CreatePopupMenu() {
        let flag = |on: bool| {
            if on {
                MF_STRING | MF_CHECKED
            } else {
                MF_STRING
            }
        };
        let setting = a.cfg.web_wallpaper.trim().to_string();
        let _ = AppendMenuW(sub, flag(setting.is_empty()), ID_WEB_OFF, w!("Off"));
        let _ = AppendMenuW(sub, flag(setting == "grid"), ID_WEB_GRID, w!("Grid preset"));
        let _ = AppendMenuW(sub, flag(setting == "dock"), ID_WEB_DOCK, w!("Dock preset"));
        let _ = AppendMenuW(
            sub,
            flag(setting == "minimal"),
            ID_WEB_MINIMAL,
            w!("Minimal preset"),
        );
        let _ = AppendMenuW(
            sub,
            flag(setting == "dashboard"),
            ID_WEB_DASHBOARD,
            w!("Dashboard preset (widgets)"),
        );
        if !setting.is_empty()
            && !matches!(setting.as_str(), "grid" | "dock" | "minimal" | "dashboard")
        {
            // A custom page path set in config.toml; shown, not switchable here.
            let label = wide(&format!("Custom: {}", a.cfg.web_label()));
            let _ = AppendMenuW(
                sub,
                MF_STRING | MF_CHECKED | MF_DISABLED | MF_GRAYED,
                0,
                PCWSTR(label.as_ptr()),
            );
        }
        let _ = AppendMenuW(sub, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(
            sub,
            flag(a.cfg.web_screens.is_empty()),
            ID_WEB_ALL,
            w!("On all screens"),
        );
        for (i, monitor) in a.monitors.iter().enumerate() {
            let label = wide(&monitor.label(i));
            let _ = AppendMenuW(
                sub,
                flag(a.cfg.web_screens.is_empty() || a.cfg.web_screens.contains(&(i + 1))),
                ID_WEB_SCREEN_BASE + i,
                PCWSTR(label.as_ptr()),
            );
        }
        let _ = AppendMenuW(sub, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(
            sub,
            flag(a.cfg.web_interactive),
            ID_WEB_INTERACTIVE,
            w!("Clickable (launch on click)"),
        );
        let _ = AppendMenuW(sub, MF_STRING, ID_WEB_SETTINGS, w!("Launcher settings..."));
        let _ = AppendMenuW(sub, MF_STRING, ID_WEB_EDIT, w!("Edit launcher.json..."));
        let label = wide(&format!("Web launcher ({})", a.cfg.web_label()));
        let _ = AppendMenuW(menu, MF_POPUP, sub.0 as usize, PCWSTR(label.as_ptr()));
    }

    let _ = AppendMenuW(menu, MF_STRING, ID_RESCAN, w!("Rescan wallpaper folder"));
    let _ = AppendMenuW(menu, MF_STRING, ID_OPEN_FOLDER, w!("Open wallpaper folder"));
    let _ = AppendMenuW(menu, MF_STRING, ID_OPEN_CONFIG, w!("Edit settings..."));
    let _ = AppendMenuW(menu, MF_STRING, ID_RELOAD, w!("Reload settings"));
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());

    let autostart_mode = autostart::status();
    let autostart_flags = if autostart_mode != autostart::Mode::Off {
        MF_STRING | MF_CHECKED
    } else {
        MF_STRING
    };
    let autostart_label = wide(&format!(
        "Start with Windows ({})",
        autostart_mode.label()
    ));
    let _ = AppendMenuW(
        menu,
        autostart_flags,
        ID_AUTOSTART,
        PCWSTR(autostart_label.as_ptr()),
    );
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
    let _ = AppendMenuW(menu, MF_STRING, ID_EXIT, w!("Exit"));

    let mut pos = POINT::default();
    let _ = GetCursorPos(&mut pos);
    // Required so the menu dismisses when the user clicks elsewhere.
    let _ = SetForegroundWindow(a.hwnd);
    let chosen = TrackPopupMenu(
        menu,
        TPM_RIGHTBUTTON | TPM_RETURNCMD | TPM_NONOTIFY,
        pos.x,
        pos.y,
        0,
        a.hwnd,
        None,
    );
    let _ = DestroyMenu(menu);
    let _ = PostMessageW(a.hwnd, WM_NULL, WPARAM(0), LPARAM(0));

    on_command(a, chosen.0 as usize);
}

fn on_command(a: &mut App, id: usize) {
    match id {
        ID_NEXT => rotate(a, 1),
        ID_PREV => rotate(a, -1),
        ID_RESCAN => {
            a.lib = scan::scan(&a.cfg);
            rotate(a, 1);
        }
        ID_CURRENT => show_current(a),
        ID_OPEN_FOLDER => open_path(&a.cfg.root()),
        ID_OPEN_CONFIG => {
            config::save(&a.cfg);
            open_path(&config::path());
        }
        ID_RELOAD => {
            a.cfg = config::load();
            a.lib = scan::scan(&a.cfg);
            register_hotkey(a);
            // Re-decide at the same playlist position so edits to the animated
            // settings take effect now, without skipping ahead.
            rotate(a, 0);
        }
        ID_ANIM_OFF => set_animated_mode(a, AnimatedMode::Off),
        ID_ANIM_MIXED => set_animated_mode(a, AnimatedMode::Mixed),
        ID_ANIM_ALWAYS => set_animated_mode(a, AnimatedMode::Always),
        ID_USE_GIF => {
            a.cfg.include_gif = !a.cfg.include_gif;
            apply_pool_change(a);
        }
        ID_USE_VIDEO => {
            a.cfg.include_video = !a.cfg.include_video;
            apply_pool_change(a);
        }
        ID_ANIM_FOLDER_ONLY => {
            let on = !a.cfg.animated_folder_only();
            a.cfg.set_animated_folder_only(on);
            apply_pool_change(a);
        }
        ID_AUTOSTART => {
            let want = !autostart::is_enabled();
            autostart::set(want, &a.exe);
            a.cfg.start_with_windows = want;
            config::save(&a.cfg);
        }
        ID_EXIT => unsafe {
            let _ = PostMessageW(a.hwnd, WM_CLOSE, WPARAM(0), LPARAM(0));
        },
        ID_ROTATE_ALL => {
            a.cfg.rotate_screens.clear();
            config::save(&a.cfg);
            update_tip(a);
        }
        // Changing which screens rotate does not change any wallpaper now; it
        // takes effect from the next rotation onwards.
        id if id >= ID_NEXT_SCREEN_BASE && id < ID_NEXT_SCREEN_BASE + a.monitors.len().max(1) => {
            rotate_one(a, id - ID_NEXT_SCREEN_BASE);
        }
        id if id >= ID_ROTATE_SCREEN_BASE
            && id < ID_ROTATE_SCREEN_BASE + a.monitors.len().max(1) =>
        {
            a.cfg.toggle_rotate_screen(id - ID_ROTATE_SCREEN_BASE);
            config::save(&a.cfg);
            update_tip(a);
        }
        ID_WEB_OFF => set_web_wallpaper(a, ""),
        ID_WEB_GRID => set_web_wallpaper(a, "grid"),
        ID_WEB_DOCK => set_web_wallpaper(a, "dock"),
        ID_WEB_MINIMAL => set_web_wallpaper(a, "minimal"),
        ID_WEB_DASHBOARD => set_web_wallpaper(a, "dashboard"),
        ID_WEB_SETTINGS => settings_ui::open(),
        ID_WEB_ALL => {
            a.cfg.web_screens.clear();
            config::save(&a.cfg);
            apply(a);
        }
        ID_WEB_INTERACTIVE => {
            a.cfg.web_interactive = !a.cfg.web_interactive;
            config::save(&a.cfg);
            sync_hook(a);
        }
        ID_WEB_EDIT => {
            web::write_presets();
            open_path(&web::launcher_path());
        }
        id if id >= ID_WEB_SCREEN_BASE && id < ID_WEB_SCREEN_BASE + a.monitors.len().max(1) => {
            a.cfg.toggle_web_screen(id - ID_WEB_SCREEN_BASE);
            config::save(&a.cfg);
            apply(a);
        }
        _ => {}
    }
}

/// Switch the web launcher on (a preset name) or off (""). Re-applies the
/// current assignments in place -- no playlist step, wallpapers stay put.
fn set_web_wallpaper(a: &mut App, value: &str) {
    a.cfg.web_wallpaper = String::from(value);
    if !value.is_empty() {
        // Refresh the materialised presets so a new exe's designs win.
        web::write_presets();
    }
    config::save(&a.cfg);
    apply(a);
}

/// Keep the click-forwarding hook in step with the surfaces that exist now.
fn sync_hook(a: &App) {
    hook::sync(if a.cfg.web_interactive {
        a.layer.web_targets()
    } else {
        Vec::new()
    });
}

fn set_animated_mode(a: &mut App, mode: AnimatedMode) {
    a.cfg.set_animated_mode(mode);
    config::save(&a.cfg);
    rotate(a, 0);
}

/// Which files may be animated has changed, so the library has to be rebuilt
/// before re-deciding. step 0 keeps the playlist position.
fn apply_pool_change(a: &mut App) {
    config::save(&a.cfg);
    a.lib = scan::scan(&a.cfg);
    rotate(a, 0);
}

fn show_current(a: &mut App) {
    let mut text = String::new();
    for (i, m) in a.monitors.iter().enumerate() {
        let label = m.label(i);
        match a.st.assignment_for(&m.id) {
            Some(asn) => {
                let name = Path::new(&asn.path)
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default();
                let folder = Path::new(&asn.path)
                    .parent()
                    .and_then(|p| p.file_name())
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default();
                let kind = if asn.animated { "  [animated]" } else { "" };
                let pinned = if a.cfg.rotates_screen(i) {
                    ""
                } else {
                    "  [pinned -- does not rotate]"
                };
                text.push_str(&format!(
                    "{}{}\n  {}\\{}{}\n",
                    label, pinned, folder, name, kind
                ));
                if let Some((_, note)) = a.anim_info.iter().find(|(id, _)| id == &m.id) {
                    text.push_str(&format!("  {}\n", note));
                }
                text.push('\n');
            }
            None => text.push_str(&format!("{}\n  (nothing assigned)\n\n", label)),
        }
    }
    let remaining = a.next_due().saturating_sub(now_unix());
    text.push_str(&format!("Next change in {}.\n", human_duration(remaining)));
    text.push_str(&format!(
        "Library: {} still images, {} GIFs, {} videos.\n",
        a.lib.statics.len(),
        a.lib.gifs,
        a.lib.videos
    ));
    if a.lib.videos_disabled > 0 {
        text.push_str(&format!(
            "\n{} video files are being ignored because include_video is off.",
            a.lib.videos_disabled
        ));
    }
    if a.lib.animated_out_of_scope > 0 {
        text.push_str(&format!(
            "\n{} animated files are outside the folders animated_dirs allows.",
            a.lib.animated_out_of_scope
        ));
    }
    text.push_str(&format!(
        "\nStarts with Windows: {}.",
        autostart::status().label()
    ));
    info_box("WallRotate -- current wallpapers", &text);
}

fn open_path(p: &Path) {
    let path = util::wide_path(p);
    unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            PCWSTR(path.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
    }
}

// -------------------------------------------------------------- rotation ---

/// step: 1 = forward, -1 = back, 0 = keep the current picks but re-apply.
fn rotate(a: &mut App, step: i32) {
    a.monitors = a.wp.monitors();
    if a.monitors.is_empty() {
        return;
    }
    if a.lib.is_empty() {
        a.lib = scan::scan(&a.cfg);
        if a.lib.is_empty() {
            a.tray.balloon(
                "WallRotate",
                &format!("No images found in {}", a.cfg.wallpaper_dir),
            );
            return;
        }
    }

    // step 0 re-decides the picks at the current playlist position, which is
    // what "the animated setting changed" needs: same images, fresh choice of
    // which screens animate, and the 12-hour clock left alone.
    pick(a, step, None);
    apply(a);

    if step != 0 || a.st.last_rotate == 0 {
        a.st.last_rotate = now_unix();
    }
    state::save(&a.st);
    schedule_timer(a);
    update_tip(a);
    trim_working_set();

    if a.cfg.notify_on_rotate && step != 0 {
        a.tray
            .balloon("WallRotate", "Wallpapers changed on every screen.");
    }
}

/// Advance one screen and leave the rest exactly as they are -- including any
/// video or GIF they are playing, which keeps running rather than restarting.
/// The 12-hour clock is deliberately not reset: this is a nudge, not a rotation.
fn rotate_one(a: &mut App, index: usize) {
    a.monitors = a.wp.monitors();
    if index >= a.monitors.len() {
        return;
    }
    if a.lib.is_empty() {
        a.lib = scan::scan(&a.cfg);
        if a.lib.is_empty() {
            a.tray.balloon(
                "WallRotate",
                &format!("No images found in {}", a.cfg.wallpaper_dir),
            );
            return;
        }
    }
    pick(a, 1, Some(index));
    apply_one(a, index);
    state::save(&a.st);
    update_tip(a);
    trim_working_set();
}

/// Re-apply the existing assignments without advancing the playlist.
fn reapply(a: &mut App) {
    a.monitors = a.wp.monitors();
    if a.monitors.is_empty() {
        return;
    }
    if a.st.assignments.is_empty() || a.st.assignments.len() != a.monitors.len() {
        rotate(a, 1);
    } else {
        apply(a);
    }
}

/// `only` restricts the change to a single screen, for the one-shot
/// "change this screen now" command. It overrides pins, because it is an
/// explicit instruction about that screen.
fn pick(a: &mut App, step: i32, only: Option<usize>) {
    let n = a.monitors.len();
    let mut rng = Rng::new(util::random_seed());
    let mode = a.cfg.animated_mode();
    let have_anim = !a.lib.animated.is_empty();
    let chance = a.cfg.animated_chance.clamp(0.0, 1.0);

    // Pinned screens sit out an *advance* and keep what they are showing. A
    // step of 0 means the settings changed rather than the clock, and those
    // should apply everywhere -- turning animation off should turn it off on
    // every screen, pinned or not.
    let honour_pins = step != 0;
    let kept: Vec<Option<Assignment>> = a
        .monitors
        .iter()
        .enumerate()
        .map(|(i, monitor)| {
            let changes = match only {
                Some(target) => i == target,
                None => !honour_pins || a.cfg.rotates_screen(i),
            };
            if changes {
                return None;
            }
            a.st
                .assignment_for(&monitor.id)
                .cloned()
                .filter(|asn| Path::new(&asn.path).is_file())
        })
        .collect();

    // Only the screens actually changing draw from the playlists, so pinning a
    // screen does not silently burn entries.
    let changing = kept.iter().filter(|k| k.is_none()).count();
    let want_anim: Vec<bool> = (0..changing)
        .map(|_| match mode {
            AnimatedMode::Off => false,
            AnimatedMode::Always => have_anim,
            AnimatedMode::Mixed => have_anim && rng.next_f32() < chance,
        })
        .collect();

    let statics = take_from(&mut a.st.statics, a.lib.statics.len(), changing, step);
    let anim_wanted = want_anim.iter().filter(|b| **b).count();
    let animated = take_from(&mut a.st.animated, a.lib.animated.len(), anim_wanted, step);

    let mut out = Vec::with_capacity(n);
    let mut ai = 0usize;
    let mut next = 0usize;
    for i in 0..n {
        if let Some(existing) = &kept[i] {
            out.push(existing.clone());
            continue;
        }
        let under = statics
            .get(next)
            .and_then(|&k| a.lib.statics.get(k))
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        let is_anim = want_anim.get(next).copied().unwrap_or(false) && ai < animated.len();
        let path = if is_anim {
            let k = animated[ai];
            ai += 1;
            a.lib.animated[k].to_string_lossy().to_string()
        } else {
            under.clone()
        };
        out.push(Assignment {
            monitor: a.monitors[i].id.clone(),
            path,
            under,
            animated: is_anim,
        });
        next += 1;
    }
    a.st.assignments = out;
}

fn apply(a: &mut App) {
    a.wp.set_position(&a.cfg.fit);

    // Only GIFs consume the frame budget; video streams from disk.
    let gif_count = a
        .st
        .assignments
        .iter()
        .filter(|x| x.animated && !scan::is_video(Path::new(&x.path)))
        .count();
    let mut limits = Limits::from_config(&a.cfg);
    if gif_count > 1 {
        // Share one budget across every GIF screen.
        limits.budget_bytes /= gif_count;
    }

    // The web launcher takes a screen over entirely: it draws above where the
    // GIF or video would, so decoding an animation there would be pure waste.
    let web_spec = web_spec_for(&a.cfg);

    let mut items: Vec<(MonitorInfo, Animation)> = Vec::new();
    let mut notes: Vec<(String, String)> = Vec::new();
    for (idx, monitor) in a.monitors.iter().enumerate() {
        let Some(asn) = a.st.assignments.iter().find(|x| x.monitor == monitor.id) else {
            continue;
        };

        // Always put a still image on the real wallpaper, even under a GIF, so
        // the desktop looks right whenever the animation is not running.
        let still = if asn.animated { &asn.under } else { &asn.path };
        if !still.is_empty() {
            let p = Path::new(still);
            if p.is_file() {
                let _ = a.wp.set(&monitor.id, p);
            }
        }

        if let Some(spec) = web_spec.as_ref().filter(|_| a.cfg.web_on_screen(idx)) {
            notes.push((
                monitor.id.clone(),
                format!("web launcher ({})", a.cfg.web_label()),
            ));
            items.push((monitor.clone(), Animation::Web(spec.clone())));
            continue;
        }

        if asn.animated {
            let p = PathBuf::from(&asn.path);
            if p.is_file() {
                if scan::is_video(&p) {
                    notes.push((
                        monitor.id.clone(),
                        String::from("video -- decoded on the GPU, no frames held in RAM"),
                    ));
                    items.push((monitor.clone(), Animation::Video(p)));
                } else if let Some(anim) = gifanim::load(&p, &limits) {
                    let note = describe(&anim);
                    log::line(format!("gif loaded: {} -- {}", p.display(), note));
                    notes.push((monitor.id.clone(), note));
                    items.push((monitor.clone(), Animation::Gif(Rc::new(anim))));
                }
            }
        }
    }

    a.anim_info = notes;
    a.anims = items;
    rebuild_layer(a);
}

/// Apply the assignment for a single screen, swapping just that surface.
fn apply_one(a: &mut App, index: usize) {
    let Some(monitor) = a.monitors.get(index).cloned() else {
        return;
    };
    let Some(asn) = a.st.assignment_for(&monitor.id).cloned() else {
        return;
    };
    a.wp.set_position(&a.cfg.fit);

    let still = if asn.animated { &asn.under } else { &asn.path };
    if !still.is_empty() {
        let p = Path::new(still);
        if p.is_file() {
            let _ = a.wp.set(&monitor.id, p);
        }
    }

    let gif_count = a
        .st
        .assignments
        .iter()
        .filter(|x| x.animated && !scan::is_video(Path::new(&x.path)))
        .count();
    let mut limits = Limits::from_config(&a.cfg);
    if gif_count > 1 {
        limits.budget_bytes /= gif_count;
    }

    let mut animation: Option<Animation> = None;
    let mut note: Option<String> = None;
    if a.cfg.web_on_screen(index) {
        if let Some(spec) = web_spec_for(&a.cfg) {
            note = Some(format!("web launcher ({})", a.cfg.web_label()));
            animation = Some(Animation::Web(spec));
        }
    } else if asn.animated {
        let p = PathBuf::from(&asn.path);
        if p.is_file() {
            if scan::is_video(&p) {
                note = Some(String::from("video -- decoded on the GPU, no frames held in RAM"));
                animation = Some(Animation::Video(p));
            } else if let Some(anim) = gifanim::load(&p, &limits) {
                note = Some(describe(&anim));
                animation = Some(Animation::Gif(Rc::new(anim)));
            }
        }
    }

    // Keep the cached list in step so a later recovery rebuild is faithful.
    a.anims.retain(|(m, _)| m.id != monitor.id);
    if let Some(anim) = &animation {
        a.anims.push((monitor.clone(), anim.clone()));
    }
    a.anim_info.retain(|(id, _)| id != &monitor.id);
    if let Some(note) = note {
        a.anim_info.push((monitor.id.clone(), note));
    }

    if !a.layer.replace_one(&monitor, animation.as_ref(), &a.cfg) {
        // No usable parent yet -- fall back to building the whole layer.
        rebuild_layer(a);
    } else {
        a.layer.suspend(a.should_suspend());
        sync_hook(a);
        // Same shell-churn risk as a full rebuild: check shortly afterwards.
        a.verify_tries = 0;
        unsafe {
            SetTimer(a.hwnd, TIMER_VERIFY, VERIFY_DELAY_MS, None);
        }
    }
}

/// Resolve the configured web wallpaper into the spec a surface needs.
fn web_spec_for(cfg: &Config) -> Option<web::WebSpec> {
    if !cfg.web_active() {
        return None;
    }
    web::resolve(&cfg.web_wallpaper).map(|(root, url)| web::WebSpec {
        root,
        url,
        backgrounds: cfg.root(),
    })
}

/// Put the animated surfaces back using the already-decoded frames.
fn rebuild_layer(a: &mut App) {
    let anims = std::mem::take(&mut a.anims);
    a.layer.show(&anims, &a.cfg);
    a.anims = anims;
    let suspend = a.should_suspend();
    log::line(format!(
        "suspend={} (locked={}, display_off={}, on_battery={}, pause_on_battery={})",
        suspend, a.locked, a.display_off, a.on_battery, a.cfg.pause_on_battery
    ));
    a.layer.suspend(suspend);
    sync_hook(a);
    // Verify whenever animations were wanted, not just when they appeared: at
    // sign-in the shell may not have a desktop to attach to yet.
    if !a.anims.is_empty() {
        a.verify_tries = 0;
        unsafe {
            SetTimer(a.hwnd, TIMER_VERIFY, VERIFY_DELAY_MS, None);
        }
    }
}

/// Cheap check that costs nothing unless the shell actually broke the layer.
fn heal_layer(a: &mut App) {
    if a.layer.is_broken() {
        rebuild_layer(a);
    }
}

/// A one-line summary of what the loader actually kept in memory.
fn describe(anim: &GifAnim) -> String {
    let mut note = format!(
        "{}x{}, {} frames, {} MB",
        anim.width,
        anim.height,
        anim.frames.len(),
        (anim.bytes() + 512 * 1024) / (1024 * 1024)
    );
    if anim.source_size != (anim.width, anim.height) {
        note.push_str(&format!(
            " (scaled from {}x{})",
            anim.source_size.0, anim.source_size.1
        ));
    }
    if anim.source_frames > anim.frames.len() {
        note.push_str(&format!(" (thinned from {})", anim.source_frames));
    }
    note
}

/// TOML integers are signed 64-bit, so a seed with the high bit set cannot be
/// written to the state file. Keep seeds in i64 range; splitmix64 does not care.
fn new_seed() -> u64 {
    (util::random_seed() | 1) & 0x7FFF_FFFF_FFFF_FFFF
}

/// Pull `count` distinct indices out of a shuffled playlist and move the cursor.
fn take_from(pl: &mut Playlist, len: usize, count: usize, step: i32) -> Vec<usize> {
    if len == 0 || count == 0 {
        return Vec::new();
    }
    let fresh = pl.pool_len != len || pl.seed == 0;
    if fresh {
        pl.seed = new_seed();
        pl.pool_len = len;
        pl.cursor = 0;
    } else if step > 0 {
        let next = pl.cursor + count;
        if next + count > len {
            // Not enough left for a full round: reshuffle for the next pass.
            pl.seed = new_seed();
            pl.cursor = 0;
        } else {
            pl.cursor = next;
        }
    } else if step < 0 {
        pl.cursor = if pl.cursor < count {
            len.saturating_sub(count)
        } else {
            pl.cursor - count
        };
    }

    let mut order: Vec<usize> = (0..len).collect();
    let mut rng = Rng::new(pl.seed);
    rng.shuffle(&mut order);
    (0..count).map(|i| order[(pl.cursor + i) % len]).collect()
}

// -------------------------------------------------------------- schedule ---

fn schedule_timer(a: &App) {
    let remaining = a.next_due().saturating_sub(now_unix());
    let secs = remaining.clamp(1, MAX_TIMER_SECS);
    unsafe {
        SetTimer(a.hwnd, TIMER_ROTATE, (secs * 1000) as u32, None);
    }
}

fn update_tip(a: &mut App) {
    let remaining = a.next_due().saturating_sub(now_unix());
    let hint = if a.hotkey_ok && !a.hotkey_label.is_empty() {
        format!("  ({} to change now)", a.hotkey_label)
    } else {
        String::new()
    };
    let animating = if a.layer.is_active() { " [animating]" } else { "" };
    a.tray.set_tip(&format!(
        "WallRotate -- next change in {}{}{}",
        human_duration(remaining),
        animating,
        hint
    ));
}

fn register_hotkey(a: &mut App) {
    unsafe {
        if a.hotkey_ok {
            let _ = UnregisterHotKey(a.hwnd, HOTKEY_ID);
            a.hotkey_ok = false;
        }
    }
    let Some(hk) = hotkey::parse(&a.cfg.hotkey) else {
        a.hotkey_label.clear();
        return;
    };
    a.hotkey_label = hk.label.clone();
    let ok = unsafe { RegisterHotKey(a.hwnd, HOTKEY_ID, hk.modifiers, hk.vk).is_ok() };
    a.hotkey_ok = ok;
    if !ok {
        a.tray.balloon(
            "WallRotate",
            &format!(
                "{} is already taken by another program.\n\
                 Pick a different combination in the settings file.",
                hk.label
            ),
        );
    }
}

fn startup_apply(a: &mut App) {
    a.monitors = a.wp.monitors();
    let due = now_unix() >= a.next_due();
    let usable = !a.st.assignments.is_empty()
        && a.st.assignments.len() == a.monitors.len()
        && a
            .st
            .assignments
            .iter()
            .all(|x| Path::new(&x.path).is_file());

    // Saved picks go stale if the animated mode was changed while the program
    // was not running, in either direction. "Mixed" accepts any combination.
    let have_anim = !a.lib.animated.is_empty();
    let consistent = match a.cfg.animated_mode() {
        AnimatedMode::Off => a.st.assignments.iter().all(|x| !x.animated),
        AnimatedMode::Always => !have_anim || a.st.assignments.iter().all(|x| x.animated),
        AnimatedMode::Mixed => true,
    };

    if due {
        rotate(a, 1);
    } else if !usable || !consistent {
        rotate(a, 0);
    } else {
        // Same wallpapers as last time; just put the animated layer back.
        apply(a);
    }
}

fn first_run_checks(a: &mut App) {
    if !a.cfg.root().is_dir() {
        a.tray.balloon(
            "WallRotate",
            &format!(
                "Wallpaper folder not found:\n{}\n\nUse \"Edit settings...\" to point it somewhere else.",
                a.cfg.wallpaper_dir
            ),
        );
    }
    if a.cfg.start_with_windows && !autostart::is_enabled() {
        autostart::set(true, &a.exe);
    }
}

// ----------------------------------------------------------------- misc ---

fn on_battery() -> bool {
    unsafe {
        let mut status = SYSTEM_POWER_STATUS::default();
        if GetSystemPowerStatus(&mut status).is_ok() {
            // 0 = running on battery, 1 = on AC, 255 = unknown.
            status.ACLineStatus == 0
        } else {
            false
        }
    }
}

fn trim_working_set() {
    unsafe {
        let _ = EmptyWorkingSet(GetCurrentProcess());
    }
}

fn is_control_verb(verb: &str) -> bool {
    matches!(
        verb,
        "--next"
            | "/next"
            | "--rotate"
            | "/rotate"
            | "--prev"
            | "/prev"
            | "--previous"
            | "--rescan"
            | "/rescan"
            | "--screen"
            | "/screen"
            | "--quit"
            | "/quit"
            | "--exit"
            | "--settings"
            | "/settings"
    )
}

fn forward_command(verb: &str, number: Option<usize>) {
    let (msg, wparam) = match verb {
        "--next" | "/next" | "--rotate" | "/rotate" => (WM_CMD_NEXT, 0usize),
        "--prev" | "/prev" | "--previous" => (WM_CMD_PREV, 0),
        "--rescan" | "/rescan" => (WM_CMD_RESCAN, 0),
        "--settings" | "/settings" => (WM_CMD_OPEN_SETTINGS, 0),
        // Screens are 1-based on the command line, 0-based internally.
        "--screen" | "/screen" => (
            WM_CMD_NEXT_SCREEN,
            number.unwrap_or(1).saturating_sub(1),
        ),
        "--quit" | "/quit" | "--exit" => (WM_CLOSE, 0),
        _ => return,
    };
    unsafe {
        if let Ok(hwnd) = FindWindowW(MAIN_CLASS, PCWSTR::null()) {
            if !hwnd.is_invalid() {
                let _ = PostMessageW(hwnd, msg, WPARAM(wparam), LPARAM(0));
            }
        }
    }
}

/// Block until no instance owns the main window, or the deadline passes.
fn wait_for_exit(limit: std::time::Duration) {
    let start = std::time::Instant::now();
    while start.elapsed() < limit {
        let gone = unsafe {
            match FindWindowW(MAIN_CLASS, PCWSTR::null()) {
                Ok(h) => h.is_invalid(),
                Err(_) => true,
            }
        };
        if gone {
            // Give the process a moment to release the singleton mutex.
            std::thread::sleep(std::time::Duration::from_millis(300));
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

fn install_dir() -> PathBuf {
    PathBuf::from(std::env::var("LOCALAPPDATA").unwrap_or_default())
        .join("Programs")
        .join("WallRotate")
}

fn install() {
    let Ok(src) = std::env::current_exe() else {
        error_box("WallRotate", "Could not determine the running executable.");
        return;
    };
    let dir = install_dir();
    let dst = dir.join("wallrotate.exe");

    // A previous copy holds its own file open; ask it to quit and wait for it
    // to really go, otherwise the relaunch below trips the singleton mutex.
    forward_command("--quit", None);
    wait_for_exit(std::time::Duration::from_secs(6));

    if std::fs::create_dir_all(&dir).is_err() {
        error_box("WallRotate", "Could not create the install folder.");
        return;
    }

    if src != dst {
        let mut copied = false;
        for _ in 0..20 {
            if std::fs::copy(&src, &dst).is_ok() {
                copied = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
        if !copied {
            error_box(
                "WallRotate",
                "Could not copy the program into %LOCALAPPDATA%.\n\
                 Close any running copy and try again.",
            );
            return;
        }
    }

    autostart::set(true, &dst);
    let exe = util::wide_path(&dst);
    unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            PCWSTR(exe.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
    }
}

fn uninstall() {
    forward_command("--quit", None);
    autostart::set(false, &install_dir().join("wallrotate.exe"));
    info_box(
        "WallRotate",
        &format!(
            "Removed from startup and stopped.\n\nYou can delete the folder:\n{}",
            install_dir().display()
        ),
    );
}

fn info_box(title: &str, text: &str) {
    let t = wide(title);
    let b = wide(text);
    unsafe {
        MessageBoxW(
            None,
            PCWSTR(b.as_ptr()),
            PCWSTR(t.as_ptr()),
            MB_OK | MB_ICONINFORMATION,
        );
    }
}

fn error_box(title: &str, text: &str) {
    let t = wide(title);
    let b = wide(text);
    unsafe {
        MessageBoxW(
            None,
            PCWSTR(b.as_ptr()),
            PCWSTR(t.as_ptr()),
            MB_OK | MB_ICONERROR,
        );
    }
}
