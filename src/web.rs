//! Clickable web wallpapers: an HTML page rendered by WebView2 inside the
//! desktop layer, with tiles that launch apps, files, and URLs.
//!
//! The page lives in the same WorkerW-parented surface as GIF and video
//! wallpapers, so it sits above the still wallpaper and below the desktop
//! icons. Clicks reach it through the low-level mouse hook in hook.rs, because
//! the icon layer normally swallows all desktop input.
//!
//! Plumbing choices that matter:
//!  * Pages are served through `SetVirtualHostNameToFolderMapping` as
//!    https://wallpaper.local/ (and the wallpaper library as
//!    https://backgrounds.local/), because a plain file:// page cannot
//!    fetch() its sibling launcher.json -- Chromium treats file origins as
//!    opaque.
//!  * The page asks for actions with
//!    `window.chrome.webview.postMessage({action:"open", target, args})`;
//!    the host verifies the message came from wallpaper.local and hands the
//!    target to ShellExecute. Navigation anywhere else is cancelled (external
//!    links open in the default browser instead), so the wallpaper can never
//!    turn into an accidental web browser.
//!  * Everything WebView2 is asynchronous. Environment and controller arrive
//!    via COM callbacks on this same STA thread; desktop.rs polls with its
//!    existing slow timer and installs the finished `WebWallpaper` when it
//!    lands.

use std::cell::RefCell;
use std::path::{Path, PathBuf};

use webview2_com::Microsoft::Web::WebView2::Win32::{
    CreateCoreWebView2EnvironmentWithOptions, ICoreWebView2, ICoreWebView2Controller,
    ICoreWebView2Environment, ICoreWebView2_3, COREWEBVIEW2_HOST_RESOURCE_ACCESS_KIND_ALLOW,
};
use webview2_com::{
    CreateCoreWebView2ControllerCompletedHandler, CreateCoreWebView2EnvironmentCompletedHandler,
    NavigationStartingEventHandler, NewWindowRequestedEventHandler,
    WebMessageReceivedEventHandler,
};
use windows::core::{Interface, HSTRING, PCWSTR, PWSTR};
use windows::Win32::Foundation::{FALSE, HWND, RECT, TRUE};
use windows::Win32::System::Environment::ExpandEnvironmentStringsW;
use windows::Win32::System::WinRT::EventRegistrationToken;
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

pub const PAGES_HOST: &str = "wallpaper.local";
pub const BACKGROUNDS_HOST: &str = "backgrounds.local";

/// Everything a surface needs to bring up its page, captured at pick time.
#[derive(Clone)]
pub struct WebSpec {
    /// Folder mapped as https://wallpaper.local/
    pub root: PathBuf,
    /// Full URL to navigate to (on the wallpaper.local host).
    pub url: String,
    /// Folder mapped as https://backgrounds.local/ (the wallpaper library).
    pub backgrounds: PathBuf,
}

// ----------------------------------------------------------- environment ---

enum EnvState {
    Idle,
    Requested,
    Ready(ICoreWebView2Environment),
    Failed,
}

thread_local! {
    static ENVIRONMENT: RefCell<EnvState> = const { RefCell::new(EnvState::Idle) };
}

pub fn environment() -> Option<ICoreWebView2Environment> {
    ENVIRONMENT.with(|cell| match &*cell.borrow() {
        EnvState::Ready(env) => Some(env.clone()),
        _ => None,
    })
}

pub fn environment_failed() -> bool {
    ENVIRONMENT.with(|cell| matches!(&*cell.borrow(), EnvState::Failed))
}

/// Kick off (once) the async creation of the shared WebView2 environment.
/// Requires the WebView2 Runtime, which ships with Windows 11 / Edge.
pub fn ensure_environment() {
    let already = ENVIRONMENT.with(|cell| {
        let mut state = cell.borrow_mut();
        if matches!(*state, EnvState::Idle) {
            *state = EnvState::Requested;
            false
        } else {
            true
        }
    });
    if already {
        return;
    }

    let data_dir = crate::config::dir().join("webview2");
    let _ = std::fs::create_dir_all(&data_dir);
    let data_dir = HSTRING::from(data_dir.as_os_str());

    let handler = CreateCoreWebView2EnvironmentCompletedHandler::create(Box::new(
        move |result, env: Option<ICoreWebView2Environment>| {
            ENVIRONMENT.with(|cell| {
                let mut state = cell.borrow_mut();
                match (result, env) {
                    (Ok(()), Some(env)) => {
                        crate::log::line("web: environment ready");
                        *state = EnvState::Ready(env);
                    }
                    (result, _) => {
                        crate::log::line(format!(
                            "web: environment creation failed ({:?}) -- is the WebView2 Runtime installed?",
                            result
                        ));
                        *state = EnvState::Failed;
                    }
                }
            });
            Ok(())
        },
    ));

    let hr = unsafe {
        CreateCoreWebView2EnvironmentWithOptions(
            PCWSTR::null(),
            &data_dir,
            None,
            &handler,
        )
    };
    if hr.is_err() {
        crate::log::line(format!("web: CreateCoreWebView2Environment refused: {:?}", hr));
        ENVIRONMENT.with(|cell| *cell.borrow_mut() = EnvState::Failed);
    }
}

// ------------------------------------------------------------- controller ---

/// Begin async controller creation for one surface window. Completion lands in
/// `crate::desktop::web_controller_ready`.
pub fn begin_controller(
    hwnd: HWND,
    env: &ICoreWebView2Environment,
    spec: WebSpec,
    width: i32,
    height: i32,
) {
    let target = hwnd.0 as isize;
    let handler = CreateCoreWebView2ControllerCompletedHandler::create(Box::new(
        move |result, controller: Option<ICoreWebView2Controller>| {
            let built = match (result, controller) {
                (Ok(()), Some(controller)) => configure(controller, &spec, width, height)
                    .map_err(|e| {
                        crate::log::line(format!("web: configure failed {:?}", e));
                        e
                    })
                    .ok(),
                (result, _) => {
                    crate::log::line(format!("web: controller creation failed {:?}", result));
                    None
                }
            };
            crate::desktop::web_controller_ready(target, built);
            Ok(())
        },
    ));
    if let Err(e) = unsafe { env.CreateCoreWebView2Controller(hwnd, &handler) } {
        crate::log::line(format!("web: CreateCoreWebView2Controller refused {:?}", e));
        crate::desktop::web_controller_ready(target, None);
    }
}

fn configure(
    controller: ICoreWebView2Controller,
    spec: &WebSpec,
    width: i32,
    height: i32,
) -> windows::core::Result<WebWallpaper> {
    unsafe {
        controller.SetBounds(RECT {
            left: 0,
            top: 0,
            right: width.max(1),
            bottom: height.max(1),
        })?;
        let webview = controller.CoreWebView2()?;

        // A wallpaper, not a browser.
        if let Ok(settings) = webview.Settings() {
            let _ = settings.SetAreDefaultContextMenusEnabled(FALSE);
            let _ = settings.SetAreDevToolsEnabled(FALSE);
            let _ = settings.SetIsStatusBarEnabled(FALSE);
            let _ = settings.SetIsZoomControlEnabled(FALSE);
        }

        let wv3: ICoreWebView2_3 = webview.cast()?;
        wv3.SetVirtualHostNameToFolderMapping(
            &HSTRING::from(PAGES_HOST),
            &HSTRING::from(spec.root.as_os_str()),
            COREWEBVIEW2_HOST_RESOURCE_ACCESS_KIND_ALLOW,
        )?;
        if spec.backgrounds.is_dir() {
            let _ = wv3.SetVirtualHostNameToFolderMapping(
                &HSTRING::from(BACKGROUNDS_HOST),
                &HSTRING::from(spec.backgrounds.as_os_str()),
                COREWEBVIEW2_HOST_RESOURCE_ACCESS_KIND_ALLOW,
            );
        }

        let mut tokens = [EventRegistrationToken::default(); 3];

        // Tile clicks arrive here.
        webview.add_WebMessageReceived(
            &WebMessageReceivedEventHandler::create(Box::new(|_, args| {
                if let Some(args) = args {
                    let mut source = PWSTR::null();
                    let _ = args.Source(&mut source);
                    let source = crate::util::from_wide_ptr(source.0);
                    if !source.starts_with(&format!("https://{}", PAGES_HOST)) {
                        return Ok(());
                    }
                    let mut json = PWSTR::null();
                    if args.WebMessageAsJson(&mut json).is_ok() {
                        handle_message(&crate::util::from_wide_ptr(json.0));
                    }
                }
                Ok(())
            })),
            &mut tokens[0],
        )?;

        // The page may only ever be our local hosts; anything else opens in
        // the default browser instead of inside the wallpaper.
        webview.add_NavigationStarting(
            &NavigationStartingEventHandler::create(Box::new(|_, args| {
                if let Some(args) = args {
                    let mut uri = PWSTR::null();
                    let _ = args.Uri(&mut uri);
                    let uri = crate::util::from_wide_ptr(uri.0);
                    let local = uri.starts_with(&format!("https://{}", PAGES_HOST))
                        || uri.starts_with(&format!("https://{}", BACKGROUNDS_HOST))
                        || uri.starts_with("about:")
                        || uri.starts_with("data:");
                    if !local {
                        let _ = args.SetCancel(TRUE);
                        if uri.starts_with("http://") || uri.starts_with("https://") {
                            open_target(&uri, "");
                        }
                    }
                }
                Ok(())
            })),
            &mut tokens[1],
        )?;

        webview.add_NewWindowRequested(
            &NewWindowRequestedEventHandler::create(Box::new(|_, args| {
                if let Some(args) = args {
                    let _ = args.SetHandled(TRUE);
                    let mut uri = PWSTR::null();
                    let _ = args.Uri(&mut uri);
                    let uri = crate::util::from_wide_ptr(uri.0);
                    if uri.starts_with("http://") || uri.starts_with("https://") {
                        open_target(&uri, "");
                    }
                }
                Ok(())
            })),
            &mut tokens[2],
        )?;

        webview.Navigate(&HSTRING::from(spec.url.as_str()))?;
        crate::log::line(format!("web: navigating to {}", spec.url));

        Ok(WebWallpaper {
            controller,
            webview,
            visible: true,
        })
    }
}

/// `{"action":"open","target":"...","args":"..."}` from the page.
fn handle_message(json: &str) {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(json) else {
        return;
    };
    if value.get("action").and_then(|v| v.as_str()) != Some("open") {
        return;
    }
    let Some(target) = value.get("target").and_then(|v| v.as_str()) else {
        return;
    };
    let args = value.get("args").and_then(|v| v.as_str()).unwrap_or("");
    open_target(target, args);
}

/// Hand a user-configured target to the shell: an exe, a document, a folder,
/// or a URL. %ENV% variables are expanded first.
fn open_target(target: &str, args: &str) {
    let target = expand_env(target.trim());
    if target.is_empty() {
        return;
    }
    crate::log::line(format!("web: open '{}' args '{}'", target, args));
    let target_w = crate::util::wide(&target);
    let args_w = crate::util::wide(args);
    unsafe {
        ShellExecuteW(
            None,
            windows::core::w!("open"),
            PCWSTR(target_w.as_ptr()),
            if args.is_empty() {
                PCWSTR::null()
            } else {
                PCWSTR(args_w.as_ptr())
            },
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
    }
}

fn expand_env(s: &str) -> String {
    if !s.contains('%') {
        return s.to_string();
    }
    let src = crate::util::wide(s);
    let mut buf = vec![0u16; 1024];
    let n = unsafe { ExpandEnvironmentStringsW(PCWSTR(src.as_ptr()), Some(&mut buf)) };
    if n == 0 || n as usize > buf.len() {
        return s.to_string();
    }
    String::from_utf16_lossy(&buf[..n as usize - 1])
}

// ---------------------------------------------------------------- surface ---

pub struct WebWallpaper {
    controller: ICoreWebView2Controller,
    webview: ICoreWebView2,
    visible: bool,
}

impl WebWallpaper {
    /// Hiding the controller stops WebView2 compositing; TrySuspend also lets
    /// Chromium park the renderer process. Together a covered launcher costs
    /// nothing, same as paused video.
    pub fn set_visible(&mut self, on: bool) {
        if self.visible == on {
            return;
        }
        self.visible = on;
        unsafe {
            let _ = self.controller.SetIsVisible(on);
            if let Ok(wv3) = self.webview.cast::<ICoreWebView2_3>() {
                if on {
                    let _ = wv3.Resume();
                } else {
                    let _ = wv3.TrySuspend(None);
                }
            }
        }
    }
}

impl Drop for WebWallpaper {
    fn drop(&mut self) {
        unsafe {
            let _ = self.controller.Close();
        }
    }
}

// ----------------------------------------------------------------- presets ---

const PRESET_GRID: &str = include_str!("../presets/web/grid/index.html");
const PRESET_DOCK: &str = include_str!("../presets/web/dock/index.html");
const PRESET_MINIMAL: &str = include_str!("../presets/web/minimal/index.html");

const DEFAULT_LAUNCHER: &str = r#"{
    "_help": "Targets for the web launcher wallpaper. 'target' can be an exe, a document, a folder, a URL, or anything else the shell can open; %ENV% variables are expanded. 'background' may be empty, or an image from your library via https://backgrounds.local/<relative path>. Edit and pick 'Reload settings' (or just wait: pages re-read this on each rotation).",
    "background": "",
    "clock": true,
    "tiles": [
        { "icon": "🗒️", "label": "Notepad",   "target": "notepad.exe" },
        { "icon": "🧮", "label": "Calculator", "target": "calc.exe" },
        { "icon": "📁", "label": "Downloads",  "target": "%USERPROFILE%\\Downloads" },
        { "icon": "🖼️", "label": "Wallpapers", "target": "%USERPROFILE%\\Pictures\\backgrounds" },
        { "icon": "⚙️", "label": "Settings",   "target": "ms-settings:" },
        { "icon": "🐙", "label": "WallRotate", "target": "https://github.com/elazarza/wallrotate" }
    ]
}
"#;

pub fn web_root() -> PathBuf {
    crate::config::dir().join("web")
}

/// Materialise the embedded presets (always refreshed -- they are ours) and a
/// starter launcher.json (only if missing -- that one belongs to the user).
pub fn write_presets() {
    let root = web_root();
    for (name, body) in [
        ("grid", PRESET_GRID),
        ("dock", PRESET_DOCK),
        ("minimal", PRESET_MINIMAL),
    ] {
        let dir = root.join("presets").join(name);
        if std::fs::create_dir_all(&dir).is_ok() {
            let _ = std::fs::write(dir.join("index.html"), body);
        }
    }
    let launcher = root.join("launcher.json");
    if !launcher.exists() {
        let _ = std::fs::write(&launcher, DEFAULT_LAUNCHER);
    }
}

pub fn launcher_path() -> PathBuf {
    web_root().join("launcher.json")
}

/// Resolve the configured web wallpaper into (folder to serve, URL to load).
pub fn resolve(setting: &str) -> Option<(PathBuf, String)> {
    let setting = setting.trim();
    if setting.is_empty() {
        return None;
    }
    if matches!(setting, "grid" | "dock" | "minimal") {
        return Some((
            web_root(),
            format!("https://{}/presets/{}/index.html", PAGES_HOST, setting),
        ));
    }
    // A custom page: serve its own folder, expect launcher.json beside it.
    let path = Path::new(setting);
    if path.is_file() {
        let parent = path.parent()?.to_path_buf();
        let file = path.file_name()?.to_string_lossy().to_string();
        return Some((parent, format!("https://{}/{}", PAGES_HOST, file)));
    }
    crate::log::line(format!("web: '{}' is not a preset name or an existing file", setting));
    None
}
