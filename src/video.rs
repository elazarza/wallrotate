//! Video wallpapers (.mp4 &co) through Media Foundation's IMFMediaEngine.
//!
//! Unlike the GIF path, nothing is pre-decoded: the media engine streams and
//! decodes on the GPU, and we only ask it to blit the current frame into a
//! swap chain owned by the desktop-layer child window. That keeps memory flat
//! regardless of clip length, and keeps decoding off the CPU where the driver
//! supports it.
//!
//! Pausing genuinely pauses the engine, so a covered or locked screen stops the
//! decoder rather than just skipping presentation.

use std::path::Path;
use windows::core::{implement, Interface, Result as WResult, BSTR};
use windows::Win32::Foundation::{FALSE, HWND, LPARAM, RECT, TRUE, WPARAM};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11Multithread, ID3D11Texture2D,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_IGNORE, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    IDXGIDevice, IDXGIFactory2, IDXGISwapChain1, DXGI_PRESENT, DXGI_SCALING_STRETCH,
    DXGI_SWAP_CHAIN_DESC1,
    DXGI_SWAP_EFFECT_DISCARD, DXGI_SWAP_EFFECT_FLIP_DISCARD, DXGI_USAGE_RENDER_TARGET_OUTPUT,
};
use windows::Win32::Media::MediaFoundation::{
    IMFAttributes, IMFDXGIDeviceManager, IMFMediaEngine, IMFMediaEngineClassFactory,
    IMFMediaEngineNotify, IMFMediaEngineNotify_Impl, MFARGB, MFCreateAttributes,
    MFCreateDXGIDeviceManager, MFStartup, MFVideoNormalizedRect, CLSID_MFMediaEngineClassFactory,
    MF_MEDIA_ENGINE_CALLBACK, MF_MEDIA_ENGINE_DXGI_MANAGER, MF_MEDIA_ENGINE_EVENT_CANPLAY,
    MF_MEDIA_ENGINE_EVENT_ENDED, MF_MEDIA_ENGINE_EVENT_ERROR,
    MF_MEDIA_ENGINE_EVENT_FIRSTFRAMEREADY, MF_MEDIA_ENGINE_VIDEO_OUTPUT_FORMAT, MFSTARTUP_NOSOCKET,
    MF_VERSION,
};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

/// Posted to the owning surface window when the engine raises an event.
pub const WM_VIDEO_EVENT: u32 = windows::Win32::UI::WindowsAndMessaging::WM_APP + 21;

/// Extensions Media Foundation can normally open out of the box.
pub fn is_video_ext(ext: &str) -> bool {
    matches!(ext, "mp4" | "m4v" | "mov" | "wmv" | "avi" | "mkv" | "webm")
}

thread_local! {
    static MF_READY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// One D3D11 device for every surface, GIF and video alike. Building these
    /// per surface leaked worker threads and handles across rotations.
    static SHARED_D3D: std::cell::RefCell<Option<ID3D11Device>> =
        const { std::cell::RefCell::new(None) };
    static SHARED_MGR: std::cell::RefCell<Option<IMFDXGIDeviceManager>> =
        const { std::cell::RefCell::new(None) };
}

/// True once Media Foundation is up. GIF surfaces do not need it, so the D3D
/// device is available without paying for MF startup.
fn ensure_mf_started() -> bool {
    MF_READY.with(|flag| {
        if !flag.get() {
            let ok = unsafe { MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET) }.is_ok();
            flag.set(ok);
        }
        flag.get()
    })
}

/// The D3D11 device everything renders through. GIF surfaces borrow it too, so
/// both pipelines present the same way. Does not require Media Foundation.
pub fn shared_d3d_device() -> Option<ID3D11Device> {
    SHARED_D3D.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            unsafe {
                let device = create_device()?;
                // The media engine drives the device from its own threads.
                if let Ok(mt) = device.cast::<ID3D11Multithread>() {
                    let _ = mt.SetMultithreadProtected(TRUE);
                }
                *slot = Some(device);
            }
        }
        slot.clone()
    })
}

/// A swap chain for a desktop-layer child window. This is the presentation path
/// that actually composites there -- a Direct2D HwndRenderTarget renders without
/// error but never reaches the screen.
pub fn swapchain_for(hwnd: HWND, width: u32, height: u32) -> Option<IDXGISwapChain1> {
    let device = shared_d3d_device()?;
    unsafe { create_swapchain(&device, hwnd, width.max(1), height.max(1)) }
}

fn shared_mf_manager() -> Option<IMFDXGIDeviceManager> {
    let device = shared_d3d_device()?;
    SHARED_MGR.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            unsafe {
                let mut token: u32 = 0;
                let mut manager: Option<IMFDXGIDeviceManager> = None;
                if let Err(e) = MFCreateDXGIDeviceManager(&mut token, &mut manager) {
                    crate::log::line(format!("  video: MFCreateDXGIDeviceManager {:?}", e));
                    return None;
                }
                let manager = manager?;
                if let Err(e) = manager.ResetDevice(&device, token) {
                    crate::log::line(format!("  video: ResetDevice {:?}", e));
                    return None;
                }
                *slot = Some(manager);
            }
        }
        slot.clone()
    })
}

// ------------------------------------------------------------- callback ---

/// The engine calls back on its own worker threads, so the only thing we do
/// there is post to the surface window and handle it on the UI thread.
#[implement(IMFMediaEngineNotify)]
struct EngineNotify {
    target: isize,
}

impl IMFMediaEngineNotify_Impl for EngineNotify_Impl {
    fn EventNotify(&self, event: u32, param1: usize, _param2: u32) -> WResult<()> {
        unsafe {
            let _ = PostMessageW(
                HWND(self.target as *mut core::ffi::c_void),
                WM_VIDEO_EVENT,
                WPARAM(event as usize),
                LPARAM(param1 as isize),
            );
        }
        Ok(())
    }
}

// --------------------------------------------------------------- player ---

pub struct VideoPlayer {
    engine: IMFMediaEngine,
    swapchain: IDXGISwapChain1,
    _device: ID3D11Device,
    _manager: IMFDXGIDeviceManager,
    _notify: IMFMediaEngineNotify,
    /// Target surface size in pixels.
    size: (u32, u32),
    native: (u32, u32),
    ready: bool,
    failed: bool,
    playing: bool,
}

impl VideoPlayer {
    pub fn new(hwnd: HWND, path: &Path, width: u32, height: u32) -> Option<VideoPlayer> {
        if !ensure_mf_started() {
            return None;
        }
        unsafe {
            let (Some(device), Some(manager)) = (shared_d3d_device(), shared_mf_manager()) else {
                crate::log::line("  video: no D3D11 device / MF manager");
                return None;
            };

            let Some(swapchain) = create_swapchain(&device, hwnd, width.max(1), height.max(1))
            else {
                crate::log::line("  video: CreateSwapChainForHwnd failed");
                return None;
            };

            let notify: IMFMediaEngineNotify = EngineNotify {
                target: hwnd.0 as isize,
            }
            .into();

            let mut attrs: Option<IMFAttributes> = None;
            if let Err(e) = MFCreateAttributes(&mut attrs, 4) {
                crate::log::line(format!("  video: MFCreateAttributes {:?}", e));
                return None;
            }
            let attrs = attrs?;
            attrs.SetUnknown(&MF_MEDIA_ENGINE_CALLBACK, &notify).ok()?;
            attrs.SetUnknown(&MF_MEDIA_ENGINE_DXGI_MANAGER, &manager).ok()?;
            attrs
                .SetUINT32(
                    &MF_MEDIA_ENGINE_VIDEO_OUTPUT_FORMAT,
                    DXGI_FORMAT_B8G8R8A8_UNORM.0 as u32,
                )
                .ok()?;

            let factory: IMFMediaEngineClassFactory =
                match CoCreateInstance(&CLSID_MFMediaEngineClassFactory, None, CLSCTX_INPROC_SERVER)
                {
                    Ok(f) => f,
                    Err(e) => {
                        crate::log::line(format!("  video: class factory {:?}", e));
                        return None;
                    }
                };
            let engine = match factory.CreateInstance(0, &attrs) {
                Ok(e) => e,
                Err(e) => {
                    crate::log::line(format!("  video: CreateInstance {:?}", e));
                    return None;
                }
            };

            // A wallpaper must never make noise.
            let _ = engine.SetMuted(TRUE);
            let _ = engine.SetVolume(0.0);
            let _ = engine.SetLoop(TRUE);
            // Never AutoPlay. With it on, the engine starts itself when the
            // source loads, our `playing` flag stays false, and pause() -- which
            // trusted that flag -- became a no-op: an unstoppable decoder that
            // burned GPU behind covered screens. Playback starts only when the
            // surface's reconcile logic asks for it.
            let _ = engine.SetAutoPlay(FALSE);

            let url = BSTR::from(path.as_os_str().to_string_lossy().as_ref());
            if let Err(e) = engine.SetSource(&url) {
                crate::log::line(format!("  video: SetSource {:?}", e));
                return None;
            }
            crate::log::line(format!("  video: opened {}", path.display()));

            Some(VideoPlayer {
                engine,
                swapchain,
                _device: device,
                _manager: manager,
                _notify: notify,
                size: (width.max(1), height.max(1)),
                native: (0, 0),
                ready: false,
                failed: false,
                playing: false,
            })
        }
    }

    pub fn is_ready(&self) -> bool {
        self.ready && !self.failed
    }

    pub fn has_failed(&self) -> bool {
        self.failed
    }

    /// Handle one engine event, posted from the callback thread.
    pub fn on_event(&mut self, event: u32) {
        let e = event as i32;
        if e == MF_MEDIA_ENGINE_EVENT_ERROR.0 {
            self.failed = true;
            return;
        }
        if e == MF_MEDIA_ENGINE_EVENT_CANPLAY.0 || e == MF_MEDIA_ENGINE_EVENT_FIRSTFRAMEREADY.0 {
            unsafe {
                let mut cx: u32 = 0;
                let mut cy: u32 = 0;
                if self
                    .engine
                    .GetNativeVideoSize(Some(&mut cx as *mut u32), Some(&mut cy as *mut u32))
                    .is_ok()
                {
                    self.native = (cx, cy);
                }
            }
            if !self.ready {
                crate::log::line(format!(
                    "  video: ready, native {}x{} -> surface {}x{}",
                    self.native.0, self.native.1, self.size.0, self.size.1
                ));
            }
            // Do NOT start playback here. Whether this surface should be
            // running is the owner's decision (it knows if the screen is
            // covered); it reconciles right after handling this event.
            self.ready = true;
        }
        if e == MF_MEDIA_ENGINE_EVENT_ENDED.0 && self.playing {
            // SetLoop normally handles this; rewind explicitly if it did not.
            unsafe {
                let _ = self.engine.SetCurrentTime(0.0);
            }
            self.playing = false;
            self.play();
        }
    }

    pub fn play(&mut self) {
        if self.failed || !self.ready || self.playing {
            return;
        }
        unsafe {
            if self.engine.Play().is_ok() {
                self.playing = true;
            }
        }
    }

    /// Genuinely pauses decoding, not just presentation. Asks the *engine*
    /// whether it is running rather than trusting our flag -- a flag that
    /// disagrees with the engine is precisely the failure mode that once left
    /// a decoder running forever behind covered screens.
    pub fn pause(&mut self) {
        unsafe {
            if !self.engine.IsPaused().as_bool() {
                let _ = self.engine.Pause();
            }
        }
        self.playing = false;
    }

    /// Blit the current frame into the swap chain and show it, but only when
    /// the engine actually has a new frame.
    ///
    /// This is the whole smoothness story: `OnVideoStreamTick` answers S_OK for
    /// a fresh frame and S_FALSE for "nothing new". Presenting on every timer
    /// tick regardless -- which is what the safe wrapper forces, since it maps
    /// both to Ok -- duplicates and drops frames against the clip's own clock
    /// and reads as judder. So call the vtable directly and honour the answer.
    pub fn present(&mut self) -> bool {
        if !self.is_ready() {
            return false;
        }
        unsafe {
            let mut pts: i64 = 0;
            let vtable = Interface::vtable(&self.engine);
            let hr = (vtable.OnVideoStreamTick)(Interface::as_raw(&self.engine), &mut pts);
            if hr != windows::core::HRESULT(0) {
                return false; // S_FALSE, or an error: nothing new to show
            }

            let back: ID3D11Texture2D = match self.swapchain.GetBuffer(0) {
                Ok(b) => b,
                Err(_) => return false,
            };
            let dst = RECT {
                left: 0,
                top: 0,
                right: self.size.0 as i32,
                bottom: self.size.1 as i32,
            };
            let border = MFARGB {
                rgbBlue: 0,
                rgbGreen: 0,
                rgbRed: 0,
                rgbAlpha: 255,
            };
            let src = self.cover_source_rect();
            let transferred = self
                .engine
                .TransferVideoFrame(&back, Some(&src), &dst, Some(&border))
                .is_ok();
            if !transferred {
                return false;
            }
            self.swapchain.Present(0, DXGI_PRESENT(0)).ok().is_ok()
        }
    }

    /// Normalized crop of the source that fills the target without distortion.
    fn cover_source_rect(&self) -> MFVideoNormalizedRect {
        let (vw, vh) = self.native;
        let (dw, dh) = self.size;
        if vw == 0 || vh == 0 || dw == 0 || dh == 0 {
            return MFVideoNormalizedRect {
                left: 0.0,
                top: 0.0,
                right: 1.0,
                bottom: 1.0,
            };
        }
        let scale = (dw as f32 / vw as f32).max(dh as f32 / vh as f32);
        // How much of the source is visible once scaled to cover the target.
        let sw = ((dw as f32 / scale) / vw as f32).clamp(0.0, 1.0);
        let sh = ((dh as f32 / scale) / vh as f32).clamp(0.0, 1.0);
        let left = (1.0 - sw) / 2.0;
        let top = (1.0 - sh) / 2.0;
        MFVideoNormalizedRect {
            left,
            top,
            right: left + sw,
            bottom: top + sh,
        }
    }
}

impl Drop for VideoPlayer {
    fn drop(&mut self) {
        unsafe {
            let _ = self.engine.Shutdown();
        }
    }
}

// ---------------------------------------------------------------- setup ---

unsafe fn create_device() -> Option<ID3D11Device> {
    for driver in [D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP] {
        let mut device: Option<ID3D11Device> = None;
        let ok = D3D11CreateDevice(
            None,
            driver,
            None,
            D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            None,
        )
        .is_ok();
        if ok {
            if let Some(d) = device {
                return Some(d);
            }
        }
    }
    None
}

unsafe fn create_swapchain(
    device: &ID3D11Device,
    hwnd: HWND,
    width: u32,
    height: u32,
) -> Option<IDXGISwapChain1> {
    let dxgi_device: IDXGIDevice = device.cast().ok()?;
    let adapter = dxgi_device.GetAdapter().ok()?;
    let factory: IDXGIFactory2 = adapter.GetParent().ok()?;

    // Flip model first; fall back to the legacy blt model on older drivers.
    for (effect, buffers) in [
        (DXGI_SWAP_EFFECT_FLIP_DISCARD, 2u32),
        (DXGI_SWAP_EFFECT_DISCARD, 1u32),
    ] {
        let desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: width,
            Height: height,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            Stereo: FALSE,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: buffers,
            Scaling: DXGI_SCALING_STRETCH,
            SwapEffect: effect,
            AlphaMode: DXGI_ALPHA_MODE_IGNORE,
            Flags: 0,
        };
        if let Ok(sc) = factory.CreateSwapChainForHwnd(device, hwnd, &desc, None, None) {
            return Some(sc);
        }
    }
    None
}
