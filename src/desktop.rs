//! The animated wallpaper layer.
//!
//! Windows has no notion of a moving desktop background, so we borrow the one
//! seam the shell leaves open: asking Progman to spawn a `WorkerW` splits the
//! desktop into "wallpaper painter" and "icon host". Parenting a child window
//! into that WorkerW puts our pixels above the static wallpaper but below the
//! desktop icons, which is exactly where an animated background belongs.
//!
//! Two kinds of surface live here:
//!  * GIF -- frames are decoded up front (see gifanim.rs) and drawn with
//!    Direct2D, so the GPU does the upscale and the CPU only uploads one frame
//!    per tick.
//!  * Video -- Media Foundation streams and decodes into a DXGI swap chain
//!    owned by the surface window (see video.rs); nothing is held in RAM.
//!
//! If either pipeline fails to initialise the surface is simply never created,
//! and the still wallpaper underneath shows through unchanged.

use crate::gifanim::GifAnim;
use crate::video::{VideoPlayer, WM_VIDEO_EVENT};
use crate::wallpaper::MonitorInfo;
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{BOOL, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_ALPHA_MODE_IGNORE, D2D1_COLOR_F, D2D1_PIXEL_FORMAT, D2D_RECT_F, D2D_SIZE_U,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1CreateFactory, ID2D1Bitmap1, ID2D1DeviceContext, ID2D1Factory1,
    D2D1_BITMAP_OPTIONS_CANNOT_DRAW, D2D1_BITMAP_OPTIONS_TARGET,
    D2D1_BITMAP_PROPERTIES1, D2D1_DEVICE_CONTEXT_OPTIONS_NONE, D2D1_FACTORY_TYPE_SINGLE_THREADED,
    D2D1_INTERPOLATION_MODE_LINEAR,
};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Dxgi::{IDXGIDevice, IDXGISurface, IDXGISwapChain1, DXGI_PRESENT};
use windows::core::Interface;
use windows::Win32::Media::{timeBeginPeriod, timeEndPeriod};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, EndPaint, RedrawWindow, ScreenToClient, PAINTSTRUCT, RDW_ALLCHILDREN, RDW_ERASE,
    RDW_INVALIDATE,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    SHQueryUserNotificationState, QUNS_BUSY, QUNS_PRESENTATION_MODE, QUNS_RUNNING_D3D_FULL_SCREEN,
};
use windows::Win32::UI::WindowsAndMessaging::*;

const ANIM_CLASS: PCWSTR = w!("WallRotateAnimSurface");
const TIMER_FRAME: usize = 1;
/// How often we re-check while paused. One wake a second costs nothing.
const PAUSED_RECHECK_MS: u32 = 1000;
/// Poll interval while a video is still opening.
const OPENING_RECHECK_MS: u32 = 300;
/// How often a playing video re-evaluates whether it should still be playing.
const VIDEO_STATE_CHECK_MS: u32 = 500;

/// Sent to a surface to suspend/resume it (wparam = 1 to suspend).
pub const WM_ANIM_SUSPEND: u32 = WM_APP + 20;
/// Posted by a surface's pacing thread when it is time to consider a frame.
pub const WM_ANIM_TICK: u32 = WM_APP + 22;

/// Drives frame timing from a dedicated thread.
///
/// `WM_TIMER` is unusable for this. It cannot fire faster than
/// USER_TIMER_MINIMUM (10 ms), and -- worse -- it is synthesised only when the
/// message queue is otherwise empty, so a busy surface starves the timers of
/// every other surface. With a video running at 60 fps on one screen, the GIF
/// on the other screen was being scheduled late and then catching up in bursts.
/// Posting a real message from a thread gives both surfaces equal priority and
/// millisecond-accurate pacing, while the UI thread still does all the drawing.
struct Pacer {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    active: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl Pacer {
    fn new(target: isize, interval_ms: u64) -> Pacer {
        use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
        use std::sync::Arc;
        use std::time::Duration;
        let stop = Arc::new(AtomicBool::new(false));
        let active = Arc::new(AtomicBool::new(true));
        let (stop_t, active_t) = (stop.clone(), active.clone());
        let interval = interval_ms.clamp(2, 33);
        crate::log::line(format!(
            "pacer: starting for hwnd {:#x}, every {}ms",
            target, interval
        ));
        let spawned = std::thread::Builder::new()
            .name(String::from("wallrotate-pacer"))
            .spawn(move || {
                while !stop_t.load(Relaxed) {
                    if active_t.load(Relaxed) {
                        std::thread::sleep(Duration::from_millis(interval));
                        unsafe {
                            let _ = PostMessageW(
                                HWND(target as *mut core::ffi::c_void),
                                WM_ANIM_TICK,
                                WPARAM(0),
                                LPARAM(0),
                            );
                        }
                    } else {
                        // Paused: idle cheaply until someone resumes us.
                        std::thread::sleep(Duration::from_millis(200));
                    }
                }
                crate::log::line(format!("pacer {:#x}: stopped", target));
            });
        if spawned.is_err() {
            crate::log::line("pacer: FAILED to spawn thread");
        }
        Pacer { stop, active }
    }

    fn set_active(&self, on: bool) {
        self.active
            .store(on, std::sync::atomic::Ordering::Relaxed);
    }
}

impl Drop for Pacer {
    fn drop(&mut self) {
        self.stop
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// D2DERR_RECREATE_TARGET -- the device was lost, drop and rebuild.
const RECREATE_TARGET: i32 = 0x8899_000Cu32 as i32;

/// What a screen should show on the animated layer.
#[derive(Clone)]
pub enum Animation {
    Gif(Rc<GifAnim>),
    /// Held as a path: the player needs a window handle, so it is built after
    /// the surface window exists and rebuilt cheaply on recovery.
    Video(PathBuf),
}

thread_local! {
    static D2D_FACTORY: RefCell<Option<ID2D1Factory1>> = const { RefCell::new(None) };
    static D2D_CONTEXT: RefCell<Option<ID2D1DeviceContext>> = const { RefCell::new(None) };
    static CLASS_REGISTERED: RefCell<bool> = const { RefCell::new(false) };
    static HIRES_REFS: RefCell<u32> = const { RefCell::new(0) };
}

/// `SetTimer` runs on the ~15.6 ms system tick by default, which is coarse
/// enough to turn a 40 ms GIF frame into a 47 ms one and to make a 30 fps video
/// poll miss every other frame -- exactly the judder you see as "choppy".
/// Windows 10 2004+ scopes this request to the calling process, so we hold the
/// 1 ms resolution only while a surface is actually alive.
fn hires_acquire() {
    HIRES_REFS.with(|cell| {
        let mut refs = cell.borrow_mut();
        if *refs == 0 {
            unsafe {
                timeBeginPeriod(1);
            }
        }
        *refs += 1;
    });
}

fn hires_release() {
    HIRES_REFS.with(|cell| {
        let mut refs = cell.borrow_mut();
        if *refs > 0 {
            *refs -= 1;
            if *refs == 0 {
                unsafe {
                    timeEndPeriod(1);
                }
            }
        }
    });
}

fn d2d_factory() -> Option<ID2D1Factory1> {
    D2D_FACTORY.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            let made: windows::core::Result<ID2D1Factory1> =
                unsafe { D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None) };
            *slot = made.ok();
        }
        slot.clone()
    })
}

/// One Direct2D context on the shared D3D device, reused by every GIF surface.
/// The render target is bound per draw, so sharing is safe and saves a device.
fn d2d_context() -> Option<ID2D1DeviceContext> {
    D2D_CONTEXT.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            let factory = d2d_factory()?;
            let d3d = crate::video::shared_d3d_device()?;
            let dxgi: IDXGIDevice = d3d.cast().ok()?;
            let device = unsafe { factory.CreateDevice(&dxgi) }.ok()?;
            let ctx =
                unsafe { device.CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE) }.ok()?;
            *slot = Some(ctx);
        }
        slot.clone()
    })
}

struct GifRender {
    anim: Rc<GifAnim>,
    frame: usize,
    dest: D2D_RECT_F,
    /// GIFs present through a DXGI swap chain, exactly like video. A Direct2D
    /// HwndRenderTarget draws without error inside the desktop layer but its
    /// output never reaches the screen.
    swapchain: Option<IDXGISwapChain1>,
    /// Source bitmap holding the current frame at its decoded size.
    src: Option<ID2D1Bitmap1>,
    /// Microsecond deadline for the next frame. Scheduling against a running
    /// deadline stops rounding error accumulating into visible drift.
    next_due: u64,
}

enum Media {
    Gif(GifRender),
    /// Set between window creation and player construction.
    VideoPending(PathBuf),
    Video(Box<VideoPlayer>),
}

/// Frame-interval statistics, so "is it smooth?" has an answer with numbers.
/// Only accumulates when WALLROTATE_DEBUG is on.
#[derive(Default)]
struct Pace {
    label: &'static str,
    last: u64,
    samples: u32,
    total: u64,
    min: u64,
    max: u64,
}

impl Pace {
    fn sample(&mut self) {
        if !crate::log::enabled() {
            return;
        }
        let now = crate::util::perf_micros();
        if self.last != 0 {
            let dt = now.saturating_sub(self.last);
            self.samples += 1;
            self.total += dt;
            if self.min == 0 || dt < self.min {
                self.min = dt;
            }
            if dt > self.max {
                self.max = dt;
            }
            if self.samples >= 120 {
                let avg = self.total / self.samples as u64;
                crate::log::line(format!(
                    "pace[{}]: {} frames, avg {:.1}ms ({:.1} fps), min {:.1}ms, max {:.1}ms",
                    self.label,
                    self.samples,
                    avg as f64 / 1000.0,
                    1_000_000.0 / avg.max(1) as f64,
                    self.min as f64 / 1000.0,
                    self.max as f64 / 1000.0
                ));
                self.samples = 0;
                self.total = 0;
                self.min = 0;
                self.max = 0;
            }
        }
        self.last = now;
    }
}

struct SurfaceState {
    media: Media,
    pace: Pace,
    /// Monitor bounds in screen coordinates, used for the occlusion test.
    monitor_rect: RECT,
    size: (i32, i32),
    pause_when_covered: bool,
    suspended: bool,
    /// Cached answer to should_pause(), refreshed on the slow timer so the
    /// pacing thread's ticks stay free of syscalls.
    paused_now: bool,
    gif_floor_ms: u32,
    /// One frame-time for video, in microseconds. This is the real CPU lever:
    /// the decoder runs at the clip's own rate regardless, but presenting fewer
    /// frames costs proportionally less.
    video_gap_us: u64,
    /// Deadline for the next present. Tracking a deadline rather than "time
    /// since last present" keeps the cap accurate instead of overshooting.
    next_present_us: u64,
    pacer: Option<Pacer>,
}

impl SurfaceState {
    fn should_pause(&self) -> bool {
        self.suspended || (self.pause_when_covered && unsafe { monitor_covered(&self.monitor_rect) })
    }
}

// ---------------------------------------------------------------- WorkerW ---

/// Candidate windows we can parent into, best first.
///
/// There is usually more than one WorkerW alive and only one of them actually
/// paints the desktop; the others are hidden leftovers. Parenting into a hidden
/// one produces child windows that exist but never draw, so every candidate is
/// filtered on being visible and spanning the whole virtual screen, and the
/// caller additionally verifies that the surfaces it creates come out visible.
pub fn desktop_parent_candidates() -> Vec<HWND> {
    // Only a real WorkerW is a safe parent. Progman hosts SHELLDLL_DefView,
    // which spans the whole desktop and sits above anything we parent there --
    // surfaces attached to Progman report themselves visible and still cannot
    // be seen. So the presence of Progman is NOT a reason to skip asking the
    // shell for a WorkerW.
    let mut out = collect_workers();
    if out.is_empty() {
        // 0x052C is the undocumented "split the desktop" message: it makes the
        // shell create the WorkerW that paints the wallpaper. Poking also
        // retires any existing one, so this only runs when there is none.
        unsafe {
            if let Ok(progman) = FindWindowW(w!("Progman"), PCWSTR::null()) {
                let mut result: usize = 0;
                for (wp, lp) in [(0usize, 0isize), (0x0D, 0x01), (0x0D, 0x00)] {
                    SendMessageTimeoutW(
                        progman,
                        0x052C,
                        WPARAM(wp),
                        LPARAM(lp),
                        SMTO_NORMAL,
                        1000,
                        Some(&mut result),
                    );
                }
            }
        }
        out = collect_workers();
        crate::log::line(format!(
            "desktop parent: asked Progman to split, {} WorkerW now",
            out.len()
        ));
    } else {
        crate::log::line(format!(
            "desktop parent: {} WorkerW already available",
            out.len()
        ));
    }

    // Absolute last resort. Better than nothing, but say so, because the icon
    // host will cover us here.
    if out.is_empty() {
        unsafe {
            if let Ok(progman) = FindWindowW(w!("Progman"), PCWSTR::null()) {
                if usable_parent(progman) {
                    crate::log::line("desktop parent: NO WorkerW -- falling back to Progman, the desktop icon host will hide the animation");
                    out.push(progman);
                }
            }
        }
    }
    out
}

/// Every WorkerW that could plausibly be the wallpaper painter. Progman is
/// deliberately excluded; see desktop_parent_candidates.
fn collect_workers() -> Vec<HWND> {
    let mut out: Vec<HWND> = Vec::new();
    unsafe {
        let Ok(progman) = FindWindowW(w!("Progman"), PCWSTR::null()) else {
            return out;
        };

        // Preferred: the top-level WorkerW that sits directly after the window
        // hosting SHELLDLL_DefView.
        let mut found: Option<HWND> = None;
        let _ = EnumWindows(
            Some(enum_worker),
            LPARAM(&mut found as *mut Option<HWND> as isize),
        );
        if let Some(h) = found {
            out.push(h);
        }

        // Then every other top-level WorkerW, in z-order.
        let mut after = HWND::default();
        while let Ok(h) = FindWindowExW(HWND::default(), after, w!("WorkerW"), PCWSTR::null()) {
            if h.is_invalid() {
                break;
            }
            if !out.contains(&h) {
                out.push(h);
            }
            after = h;
        }

        // Windows 11 usually keeps the real painter as a child of Progman, so
        // walk every WorkerW child rather than just the first.
        let mut after = HWND::default();
        while let Ok(child) = FindWindowExW(progman, after, w!("WorkerW"), PCWSTR::null()) {
            if child.is_invalid() {
                break;
            }
            if !out.contains(&child) {
                out.push(child);
            }
            after = child;
        }

        if crate::log::enabled() {
            for h in &out {
                let mut r = RECT::default();
                let _ = GetWindowRect(*h, &mut r);
                crate::log::line(format!(
                    "  WorkerW {:?} vis={} rect={},{}-{},{} usable={}",
                    h.0,
                    IsWindowVisible(*h).as_bool(),
                    r.left,
                    r.top,
                    r.right,
                    r.bottom,
                    usable_parent(*h)
                ));
            }
        }
        out.retain(|h| usable_parent(*h));
    }
    out
}

/// A parent is only useful if it is on screen and covers every monitor.
unsafe fn usable_parent(hwnd: HWND) -> bool {
    if hwnd.is_invalid() || !IsWindowVisible(hwnd).as_bool() {
        return false;
    }
    let mut r = RECT::default();
    if GetWindowRect(hwnd, &mut r).is_err() {
        return false;
    }
    let vx = GetSystemMetrics(SM_XVIRTUALSCREEN);
    let vy = GetSystemMetrics(SM_YVIRTUALSCREEN);
    let vw = GetSystemMetrics(SM_CXVIRTUALSCREEN);
    let vh = GetSystemMetrics(SM_CYVIRTUALSCREEN);
    r.left <= vx && r.top <= vy && r.right >= vx + vw && r.bottom >= vy + vh
}

unsafe extern "system" fn enum_worker(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let out = &mut *(lparam.0 as *mut Option<HWND>);
    let defview = FindWindowExW(hwnd, HWND::default(), w!("SHELLDLL_DefView"), PCWSTR::null());
    if let Ok(dv) = defview {
        if !dv.is_invalid() {
            if let Ok(worker) = FindWindowExW(HWND::default(), hwnd, w!("WorkerW"), PCWSTR::null())
            {
                if !worker.is_invalid() {
                    *out = Some(worker);
                    return BOOL(0); // stop enumerating
                }
            }
        }
    }
    BOOL(1)
}

// ----------------------------------------------------------------- layer ---

#[derive(Default)]
pub struct DesktopLayer {
    parent: Option<HWND>,
    /// Keyed by monitor device path so a single screen can be swapped without
    /// disturbing the others -- rebuilding everything restarts the video on
    /// screens the user did not ask to change.
    surfaces: Vec<(String, HWND)>,
}

impl DesktopLayer {
    pub fn new() -> Self {
        DesktopLayer::default()
    }

    pub fn is_active(&self) -> bool {
        !self.surfaces.is_empty()
    }

    /// True once the shell has torn our surfaces down underneath us, which is
    /// what an Explorer restart does: the WorkerW dies and takes its children.
    pub fn is_broken(&self) -> bool {
        self.surfaces
            .iter()
            .any(|(_, h)| unsafe { !IsWindow(*h).as_bool() })
    }

    /// Tear down every surface and let the static wallpaper show through.
    pub fn clear(&mut self) {
        for (_, hwnd) in self.surfaces.drain(..) {
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
        }
        if let Some(parent) = self.parent {
            unsafe {
                let _ = RedrawWindow(
                    parent,
                    None,
                    None,
                    RDW_INVALIDATE | RDW_ERASE | RDW_ALLCHILDREN,
                );
            }
        }
    }

    /// Replace the current surfaces with one per supplied (monitor, animation).
    pub fn show(&mut self, items: &[(MonitorInfo, Animation)], cfg: &crate::config::Config) {
        self.clear();
        if items.is_empty() {
            return;
        }
        register_class();

        // The WorkerW is recreated whenever Explorer restarts, so re-resolve
        // it. Build against each candidate until the surfaces actually come
        // out visible -- a hidden WorkerW accepts children that never draw.
        for parent in desktop_parent_candidates() {
            let mut made: Vec<(String, HWND)> = Vec::new();
            for (monitor, animation) in items {
                match create_surface(parent, monitor, animation, cfg) {
                    Some(hwnd) => made.push((monitor.id.clone(), hwnd)),
                    None => crate::log::line(format!(
                        "  create_surface FAILED on parent {:?} for monitor {},{}",
                        parent.0, monitor.rect.left, monitor.rect.top
                    )),
                }
            }
            let all_visible = !made.is_empty()
                && made
                    .iter()
                    .all(|(_, h)| unsafe { IsWindowVisible(*h).as_bool() });
            crate::log::line(format!(
                "  parent {:?}: {} of {} surfaces made, all_visible={}",
                parent.0,
                made.len(),
                items.len(),
                all_visible
            ));
            if all_visible {
                self.parent = Some(parent);
                self.surfaces = made;
                return;
            }
            for (_, hwnd) in made {
                unsafe {
                    let _ = DestroyWindow(hwnd);
                }
            }
        }
    }

    /// Suspend or resume every surface (screen lock, display off, battery).
    pub fn suspend(&self, on: bool) {
        for (_, hwnd) in &self.surfaces {
            unsafe {
                let _ = PostMessageW(
                    *hwnd,
                    WM_ANIM_SUSPEND,
                    WPARAM(if on { 1 } else { 0 }),
                    LPARAM(0),
                );
            }
        }
    }

    /// Swap the surface for one screen, leaving every other screen running.
    /// Returns false when there is no usable parent yet, so the caller can fall
    /// back to a full rebuild.
    pub fn replace_one(
        &mut self,
        monitor: &MonitorInfo,
        animation: Option<&Animation>,
        cfg: &crate::config::Config,
    ) -> bool {
        if let Some(pos) = self.surfaces.iter().position(|(id, _)| id == &monitor.id) {
            let (_, hwnd) = self.surfaces.remove(pos);
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
        }
        let Some(animation) = animation else {
            // Nothing animated here any more; the still wallpaper shows through.
            if let Some(parent) = self.parent {
                unsafe {
                    let _ = RedrawWindow(parent, None, None, RDW_INVALIDATE | RDW_ERASE);
                }
            }
            return true;
        };
        let Some(parent) = self.parent.filter(|p| unsafe { usable_parent(*p) }) else {
            crate::log::line("replace_one: stored parent is gone, full rebuild needed");
            return false;
        };
        register_class();
        let Some(hwnd) = create_surface(parent, monitor, animation, cfg) else {
            crate::log::line("replace_one: create_surface failed");
            return false;
        };
        if unsafe { !IsWindowVisible(hwnd).as_bool() } {
            crate::log::line("replace_one: new surface came out invisible");
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            return false;
        }
        crate::log::line(format!(
            "replace_one: swapped screen at {},{} in place",
            monitor.rect.left, monitor.rect.top
        ));
        self.surfaces.push((monitor.id.clone(), hwnd));
        true
    }
}

impl Drop for DesktopLayer {
    fn drop(&mut self) {
        self.clear();
    }
}

fn register_class() {
    CLASS_REGISTERED.with(|flag| {
        let mut done = flag.borrow_mut();
        if *done {
            return;
        }
        unsafe {
            let hinstance = GetModuleHandleW(PCWSTR::null()).unwrap_or_default();
            let wc = WNDCLASSW {
                style: CS_HREDRAW | CS_VREDRAW,
                lpfnWndProc: Some(anim_proc),
                hInstance: hinstance.into(),
                lpszClassName: ANIM_CLASS,
                hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
                ..Default::default()
            };
            RegisterClassW(&wc);
        }
        *done = true;
    });
}

fn create_surface(
    parent: HWND,
    monitor: &MonitorInfo,
    animation: &Animation,
    cfg: &crate::config::Config,
) -> Option<HWND> {
    let w = monitor.width();
    let h = monitor.height();
    if w <= 0 || h <= 0 {
        return None;
    }

    let media = match animation {
        Animation::Gif(anim) => {
            // Direct2D is the only way a GIF surface can draw anything.
            d2d_factory()?;
            Media::Gif(GifRender {
                dest: cover_rect(anim.width as f32, anim.height as f32, w as f32, h as f32),
                anim: anim.clone(),
                frame: 0,
                swapchain: None,
                src: None,
                next_due: 0,
            })
        }
        Animation::Video(path) => Media::VideoPending(path.clone()),
    };

    // Child coordinates are relative to the parent's *client* origin, which is
    // not always its window origin -- some WorkerW instances carry a non-client
    // border. Let the OS do the conversion rather than assuming.
    let mut origin = POINT {
        x: monitor.rect.left,
        y: monitor.rect.top,
    };
    unsafe {
        let _ = ScreenToClient(parent, &mut origin);
    }
    let (x, y) = (origin.x, origin.y);

    let pace_label = match animation {
        Animation::Gif(_) => "gif",
        Animation::Video(_) => "video",
    };
    let state = Box::new(SurfaceState {
        media,
        pace: Pace {
            label: pace_label,
            ..Default::default()
        },
        monitor_rect: monitor.rect,
        size: (w, h),
        pause_when_covered: cfg.pause_when_covered,
        suspended: false,
        paused_now: false,
        gif_floor_ms: cfg.frame_floor_ms(),
        video_gap_us: cfg.video_frame_gap_us(),
        next_present_us: 0,
        pacer: None,
    });
    let state_ptr = Box::into_raw(state);

    unsafe {
        let hinstance = GetModuleHandleW(PCWSTR::null()).unwrap_or_default();
        let hwnd = CreateWindowExW(
            WS_EX_NOACTIVATE | WS_EX_NOPARENTNOTIFY,
            ANIM_CLASS,
            PCWSTR::null(),
            WS_CHILD | WS_VISIBLE | WS_CLIPSIBLINGS,
            x,
            y,
            w,
            h,
            parent,
            None,
            hinstance,
            Some(state_ptr as *const core::ffi::c_void),
        );
        let hwnd = match hwnd {
            Ok(hwnd) if !hwnd.is_invalid() => hwnd,
            _ => {
                drop(Box::from_raw(state_ptr));
                return None;
            }
        };

        let Some(st) = surface_state(hwnd) else {
            let _ = DestroyWindow(hwnd);
            return None;
        };

        // The video player needs the window, so it is built here rather than
        // above. A failure means no surface at all: the still image shows.
        if let Media::VideoPending(path) = &st.media {
            match VideoPlayer::new(hwnd, path, w as u32, h as u32) {
                Some(player) => st.media = Media::Video(Box::new(player)),
                None => {
                    crate::log::line(format!("  VideoPlayer::new failed for {}", path.display()));
                    let _ = DestroyWindow(hwnd);
                    return None;
                }
            }
        }

        // A single-frame GIF is painted once and needs no pacing at all.
        let animates = !matches!(&st.media, Media::Gif(g) if g.anim.is_static());
        if animates {
            let pace_ms = match &st.media {
                // Poll a fraction of the shortest frame this clip actually has.
                // A fixed fast interval wastes hundreds of wakeups a second on
                // a GIF whose frames are held for a tenth of a second.
                Media::Gif(g) => {
                    let shortest = g.anim.delays_ms.iter().copied().min().unwrap_or(100) as u64;
                    (shortest / 6).clamp(4, 16)
                }
                _ => cfg.video_pace_interval_ms(),
            };
            st.pacer = Some(Pacer::new(hwnd.0 as isize, pace_ms));
        }
        // Slow timer: only re-evaluates whether we should be running at all.
        SetTimer(hwnd, TIMER_FRAME, VIDEO_STATE_CHECK_MS, None);
        // Accurate frame pacing matters for as long as this surface lives.
        hires_acquire();
        let _ = RedrawWindow(hwnd, None, None, RDW_INVALIDATE);
        Some(hwnd)
    }
}

unsafe fn surface_state<'a>(hwnd: HWND) -> Option<&'a mut SurfaceState> {
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut SurfaceState;
    if ptr.is_null() {
        None
    } else {
        Some(&mut *ptr)
    }
}

unsafe extern "system" fn anim_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_NCCREATE => {
            let cs = lparam.0 as *const CREATESTRUCTW;
            if !cs.is_null() {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, (*cs).lpCreateParams as isize);
            }
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        WM_ERASEBKGND => return LRESULT(1),
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            BeginPaint(hwnd, &mut ps);
            if let Some(st) = surface_state(hwnd) {
                draw(hwnd, st);
            }
            let _ = EndPaint(hwnd, &ps);
            return LRESULT(0);
        }
        WM_TIMER => {
            if wparam.0 == TIMER_FRAME {
                if let Some(st) = surface_state(hwnd) {
                    tick(hwnd, st);
                }
                return LRESULT(0);
            }
        }
        WM_VIDEO_EVENT => {
            if let Some(st) = surface_state(hwnd) {
                let paused = st.should_pause();
                st.paused_now = paused;
                if let Media::Video(player) = &mut st.media {
                    player.on_event(wparam.0 as u32);
                }
                if let Some(pacer) = &st.pacer {
                    pacer.set_active(!paused);
                }
            }
            return LRESULT(0);
        }
        WM_ANIM_TICK => {
            // Hot path: the pacing thread fires this a few hundred times a
            // second, so it does no syscalls beyond the drawing itself.
            if let Some(st) = surface_state(hwnd) {
                on_pace_tick(hwnd, st);
            }
            return LRESULT(0);
        }
        WM_ANIM_SUSPEND => {
            if let Some(st) = surface_state(hwnd) {
                let on = wparam.0 != 0;
                if st.suspended != on {
                    st.suspended = on;
                    st.paused_now = st.should_pause();
                    if let Media::Video(player) = &mut st.media {
                        if st.paused_now {
                            player.pause();
                        } else {
                            player.play();
                        }
                    }
                    if let Some(pacer) = &st.pacer {
                        pacer.set_active(!st.paused_now);
                    }
                }
            }
            return LRESULT(0);
        }
        WM_DESTROY => {
            KillTimer(hwnd, TIMER_FRAME).ok();
            let ptr = SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) as *mut SurfaceState;
            if !ptr.is_null() {
                drop(Box::from_raw(ptr));
                hires_release();
            }
            return LRESULT(0);
        }
        _ => {}
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

/// Slow housekeeping: decides whether this surface should be running.
/// Drawing happens on WM_ANIM_TICK, not here.
unsafe fn tick(hwnd: HWND, st: &mut SurfaceState) {
    let paused = st.should_pause();
    if paused != st.paused_now {
        crate::log::line(format!(
            "pause[{}]: {} -> {}",
            st.pace.label, st.paused_now, paused
        ));
        st.paused_now = paused;
        if let Media::Video(player) = &mut st.media {
            // Stop the decoder outright rather than just skipping presentation.
            if paused {
                player.pause();
            } else {
                player.play();
            }
        }
        if let Some(pacer) = &st.pacer {
            pacer.set_active(!paused);
        }
    }

    let mut next = if paused {
        PAUSED_RECHECK_MS
    } else {
        VIDEO_STATE_CHECK_MS
    };
    if let Media::Video(player) = &mut st.media {
        if player.has_failed() {
            KillTimer(hwnd, TIMER_FRAME).ok();
            if let Some(pacer) = &st.pacer {
                pacer.set_active(false);
            }
            return;
        }
        if !player.is_ready() {
            next = OPENING_RECHECK_MS;
        } else if !paused {
            player.play();
        }
    }
    if matches!(st.media, Media::VideoPending(_)) {
        next = OPENING_RECHECK_MS;
    }
    SetTimer(hwnd, TIMER_FRAME, next, None);
}

/// Fired by the pacing thread. Draws only when a frame is genuinely due.
unsafe fn on_pace_tick(hwnd: HWND, st: &mut SurfaceState) {
    if st.paused_now {
        return;
    }
    let floor_ms = st.gif_floor_ms;
    let size = st.size;
    let gap = st.video_gap_us;
    let deadline = st.next_present_us;
    let mut drew = false;
    let mut presented_at = 0u64;
    match &mut st.media {
        Media::Gif(gif) => {
            if gif.anim.frames.len() < 2 {
                return;
            }
            let now = crate::util::perf_micros();
            if gif.next_due == 0 {
                gif.next_due = now;
            }
            // Half a millisecond of tolerance so we do not miss by a hair.
            if now + 500 < gif.next_due {
                return;
            }
            gif.frame = (gif.frame + 1) % gif.anim.frames.len();
            let delay_us = gif
                .anim
                .delays_ms
                .get(gif.frame)
                .copied()
                .unwrap_or(100)
                .max(floor_ms) as u64
                * 1000;
            draw_gif(hwnd, gif, size);
            // Resync rather than sprint to catch up after a long stall.
            gif.next_due = if now > gif.next_due.saturating_add(500_000) {
                now + delay_us
            } else {
                gif.next_due + delay_us
            };
            drew = true;
        }
        Media::Video(player) => {
            let now = crate::util::perf_micros();
            // A quarter frame-time of slack: without it the poll grid never
            // lines up with the deadline and the effective rate undershoots.
            if deadline != 0 && now + gap / 4 < deadline {
                return; // capped by video_max_fps
            }
            drew = player.present();
            if drew {
                presented_at = now.max(1);
            }
        }
        Media::VideoPending(_) => {}
    }
    if drew {
        if presented_at != 0 {
            st.next_present_us = if presented_at > deadline.saturating_add(gap) {
                presented_at + gap // fell behind: resync rather than sprint
            } else {
                deadline + gap
            };
        }
        st.pace.sample();
    }
}

unsafe fn draw(hwnd: HWND, st: &mut SurfaceState) {
    let size = st.size;
    match &mut st.media {
        Media::Gif(gif) => draw_gif(hwnd, gif, size),
        Media::Video(player) => {
            player.present();
        }
        Media::VideoPending(_) => {}
    }
}

unsafe fn draw_gif(hwnd: HWND, gif: &mut GifRender, size: (i32, i32)) {
    if gif.swapchain.is_none() && !create_target(hwnd, gif, size) {
        return;
    }
    let (Some(swapchain), Some(src), Some(ctx)) =
        (gif.swapchain.clone(), gif.src.clone(), d2d_context())
    else {
        return;
    };
    let Some(frame) = gif.anim.frames.get(gif.frame) else {
        return;
    };

    let _ = src.CopyFromMemory(
        None,
        frame.as_ptr() as *const core::ffi::c_void,
        gif.anim.stride(),
    );

    // The flip model rotates buffers, so the target is bound per frame.
    let back: IDXGISurface = match swapchain.GetBuffer(0) {
        Ok(b) => b,
        Err(e) => {
            crate::log::line(format!("  gif: GetBuffer failed {:?}", e));
            return;
        }
    };
    let target_props = D2D1_BITMAP_PROPERTIES1 {
        pixelFormat: D2D1_PIXEL_FORMAT {
            format: DXGI_FORMAT_B8G8R8A8_UNORM,
            alphaMode: D2D1_ALPHA_MODE_IGNORE,
        },
        dpiX: 96.0,
        dpiY: 96.0,
        bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET | D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
        ..Default::default()
    };
    let target = match ctx.CreateBitmapFromDxgiSurface(&back, Some(&target_props)) {
        Ok(t) => t,
        Err(e) => {
            crate::log::line(format!("  gif: CreateBitmapFromDxgiSurface failed {:?}", e));
            return;
        }
    };

    ctx.SetTarget(&target);
    ctx.BeginDraw();
    ctx.Clear(Some(&D2D1_COLOR_F {
        r: 0.0,
        g: 0.0,
        b: 0.0,
        a: 1.0,
    }));
    ctx.DrawBitmap(
        &src,
        Some(&gif.dest),
        1.0,
        D2D1_INTERPOLATION_MODE_LINEAR,
        None,
        None,
    );
    let finished = ctx.EndDraw(None, None);
    ctx.SetTarget(None);

    if let Err(e) = finished {
        crate::log::line(format!("  gif: EndDraw failed {:?}", e));
        if e.code().0 == RECREATE_TARGET {
            gif.swapchain = None;
            gif.src = None;
        }
        return;
    }
    let _ = swapchain.Present(0, DXGI_PRESENT(0)).ok();
}

unsafe fn create_target(hwnd: HWND, gif: &mut GifRender, size: (i32, i32)) -> bool {
    let Some(ctx) = d2d_context() else {
        crate::log::line("  gif: no Direct2D device context");
        return false;
    };
    let Some(swapchain) =
        crate::video::swapchain_for(hwnd, size.0.max(1) as u32, size.1.max(1) as u32)
    else {
        crate::log::line("  gif: swap chain creation failed");
        return false;
    };
    let bprops = D2D1_BITMAP_PROPERTIES1 {
        pixelFormat: D2D1_PIXEL_FORMAT {
            format: DXGI_FORMAT_B8G8R8A8_UNORM,
            alphaMode: D2D1_ALPHA_MODE_IGNORE,
        },
        dpiX: 96.0,
        dpiY: 96.0,
        ..Default::default()
    };
    let src = match ctx.CreateBitmap(
        D2D_SIZE_U {
            width: gif.anim.width,
            height: gif.anim.height,
        },
        None,
        0,
        &bprops,
    ) {
        Ok(b) => b,
        Err(e) => {
            crate::log::line(format!("  gif: CreateBitmap failed {:?}", e));
            return false;
        }
    };
    crate::log::line(format!(
        "  gif: swap chain {}x{} for source {}x{}",
        size.0, size.1, gif.anim.width, gif.anim.height
    ));
    gif.swapchain = Some(swapchain);
    gif.src = Some(src);
    true
}

/// Scale to cover the target, cropping the overflow. Direct2D clips for us.
fn cover_rect(src_w: f32, src_h: f32, dst_w: f32, dst_h: f32) -> D2D_RECT_F {
    if src_w <= 0.0 || src_h <= 0.0 {
        return D2D_RECT_F {
            left: 0.0,
            top: 0.0,
            right: dst_w,
            bottom: dst_h,
        };
    }
    let scale = (dst_w / src_w).max(dst_h / src_h);
    let w = src_w * scale;
    let h = src_h * scale;
    let x = (dst_w - w) / 2.0;
    let y = (dst_h - h) / 2.0;
    D2D_RECT_F {
        left: x,
        top: y,
        right: x + w,
        bottom: y + h,
    }
}

/// The shell's own windows span the whole desktop, so treating them as
/// "covering" gets the logic exactly backwards: clicking the desktop -- the one
/// moment the wallpaper is definitely visible -- would pause every screen.
unsafe fn is_desktop_window(hwnd: HWND) -> bool {
    let mut buf = [0u16; 64];
    let len = GetClassNameW(hwnd, &mut buf);
    if len <= 0 {
        return false;
    }
    let class = String::from_utf16_lossy(&buf[..len as usize]);
    matches!(
        class.as_str(),
        "Progman"
            | "WorkerW"
            | "SHELLDLL_DefView"
            | "SysListView32"
            | "Shell_TrayWnd"
            | "Shell_SecondaryTrayWnd"
            | "WallRotateAnimSurface"
    )
}

/// State threaded through the EnumWindows callback below.
struct CoverScan {
    monitor: RECT,
    covered: bool,
}

/// True when nobody can see this monitor's wallpaper right now.
///
/// This must look at *every* top-level window, not just the foreground one.
/// With one app maximised per screen, the foreground window covers only its own
/// monitor -- the other monitor is hidden by a background window, and checking
/// only the foreground one left that screen decoding video nobody could see.
unsafe fn monitor_covered(monitor: &RECT) -> bool {
    // Catches fullscreen games and presentation mode across every screen.
    if let Ok(state) = SHQueryUserNotificationState() {
        if state == QUNS_RUNNING_D3D_FULL_SCREEN
            || state == QUNS_PRESENTATION_MODE
            || state == QUNS_BUSY
        {
            return true;
        }
    }
    let mut scan = CoverScan {
        monitor: *monitor,
        covered: false,
    };
    let _ = EnumWindows(
        Some(enum_cover),
        LPARAM(&mut scan as *mut CoverScan as isize),
    );
    scan.covered
}

unsafe extern "system" fn enum_cover(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let scan = &mut *(lparam.0 as *mut CoverScan);
    if !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() || is_desktop_window(hwnd) {
        return BOOL(1);
    }
    // UWP keeps suspended apps around as "visible" but DWM-cloaked full-screen
    // windows; counting those would pause the wallpaper forever.
    let mut cloaked: u32 = 0;
    let _ = DwmGetWindowAttribute(
        hwnd,
        DWMWA_CLOAKED,
        &mut cloaked as *mut u32 as *mut core::ffi::c_void,
        std::mem::size_of::<u32>() as u32,
    );
    if cloaked != 0 {
        return BOOL(1);
    }
    let mut r = RECT::default();
    if GetWindowRect(hwnd, &mut r).is_err() {
        return BOOL(1);
    }
    let m = &scan.monitor;
    if r.left <= m.left && r.top <= m.top && r.right >= m.right && r.bottom >= m.bottom {
        scan.covered = true;
        return BOOL(0); // stop enumerating
    }
    BOOL(1)
}
