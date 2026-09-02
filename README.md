# WallRotate

A small Windows tray app that puts a different wallpaper on every screen,
rotates them every 12 hours or on **Win+Ctrl+R**, and plays animated GIFs and
video clips as live desktop backgrounds.

Built for near-zero cost when it is not doing anything: **0% CPU and ~3 MB
while idle**, because nothing polls — the app sits blocked in `GetMessageW`
and wakes only on a timer or an OS notification. Animation stops the moment
nobody can see it.

## Features

- **Per-monitor wallpapers** — every screen gets its own image; a round never
  repeats an image across screens.
- **Animated backgrounds** — GIFs and videos (`mp4` `m4v` `mov` `wmv` `avi`
  `mkv` `webm`) drawn above the wallpaper and below the desktop icons.
  Video is hardware-decoded, muted, and looped; nothing is cached in RAM.
- **Rotation every 12 hours** (configurable), plus the **Win+Ctrl+R** hotkey.
  The clock survives reboots.
- **A tray menu for everything**: Next / Previous wallpapers, change just one
  screen, show what is on screen, an Animated backgrounds submenu
  (Off / Mixed / Only animated, with per-source toggles for GIFs, videos, and
  the `animated` folder), a Rotate submenu that pins individual screens out of
  the rotation, rescan, open wallpaper folder, edit and reload settings,
  start with Windows, exit.
- **Scriptable**: `--next` `--prev` `--screen N` `--rescan` `--quit` talk to
  the running instance; `--install` / `--uninstall` manage the login entry.
- **Pauses whenever it is invisible** — a window covering the screen,
  fullscreen apps, presentation mode, the lock screen, display off, and
  optionally battery power.
- **Cheap, and honest about it** — real measurements below, including what the
  animation actually costs in watts.

## Requirements

- Windows 10 2004+ or Windows 11.
- A [Rust toolchain](https://rustup.rs) to build.
- Optional: [ffmpeg](https://ffmpeg.org) on `PATH`, if you want
  `scripts/get-backgrounds.ps1` to normalize downloaded videos to 1080p30.

## Install

Grab `wallrotate.exe` from the [latest release](https://github.com/elazarza/wallrotate/releases/latest) and run:

```
wallrotate.exe --install
```

Package managers (submissions in review):

```
winget install wallrotate
scoop bucket add extras && scoop install wallrotate
```

Or build from source:

```
cargo build --release
./target/release/wallrotate.exe --install
```

`--install` copies the binary to `%LOCALAPPDATA%\Programs\WallRotate\`,
registers a logon scheduled task, and starts it. Re-running `--install`
upgrades an existing copy in place; `--uninstall` reverses all of it.

## Getting wallpapers

```powershell
./scripts/get-backgrounds.ps1
```

fetches a curated gallery — a sparse clone of
[dharmx/walls](https://github.com/dharmx/walls) — into your wallpaper folder,
or point `wallpaper_dir` in the settings at any folder of your own images.

Settings live at `%APPDATA%\WallRotate\config.toml`; `state.toml` beside it
holds the rotation clock, and `WALLROTATE_DEBUG=1` turns on a debug log.
**Everything else — the tray menu, every setting, the CLI, troubleshooting —
is in [docs/MANUAL.md](docs/MANUAL.md).**

## How it works

**Per-monitor wallpapers.** `IDesktopWallpaper` assigns each monitor its own
image by device path, so every screen genuinely differs. Picks come from a
shuffled playlist stored as a *seed plus cursor* rather than a list of paths,
so the state file stays tiny no matter how large the library is. The playlist
reshuffles when it runs out.

**The animated layer.** Windows has no moving-wallpaper concept, so the app
uses the one seam the shell leaves open: asking `Progman` to spawn a `WorkerW`
splits the desktop into a wallpaper painter and an icon host. A child window
parented into that `WorkerW` draws above the wallpaper and below the desktop
icons.

Picking the right parent is most of the work, and getting it wrong fails
*silently* — the surfaces report themselves visible and render perfectly into
a place nobody can see.

- A live system typically has a dozen or more `WorkerW` windows and only one
  actually paints; the rest are hidden 136×39 leftovers that will happily
  accept child windows which then never draw. Candidates are filtered on being
  visible and spanning the whole virtual screen.
- **Progman is not an acceptable parent.** It hosts `SHELLDLL_DefView`, the
  icon host, which spans the entire desktop and sits above anything parented
  to Progman. So the existence of Progman is never a reason to skip asking the
  shell for a real `WorkerW`; it is used only as a logged last resort.
- Progman *is* poked with the undocumented split message when no suitable
  `WorkerW` exists, because that message is what makes the shell create one.
  It is not poked otherwise, since poking also retires the `WorkerW` we may
  already be attached to.
- Child position is resolved with `ScreenToClient` — some `WorkerW` instances
  carry a non-client border, so assuming the window origin puts the surface
  tens of pixels off.
- After creation the surfaces are re-checked, and the next candidate is tried
  if they did not come out visible.

The end state to look for is `Progman > WorkerW > WallRotateAnimSurface`, with
`SHELLDLL_DefView` a sibling of the `WorkerW`.

**Both** kinds of surface present through a DXGI swap chain on a shared D3D11
device. That detail is load-bearing: a Direct2D `HwndRenderTarget` inside the
desktop layer draws without any error and reports success, but its output
never reaches the screen. GIFs are decoded up front and blitted with a
Direct2D device context bound to the swap chain's back buffer; video goes
through Media Foundation's `IMFMediaEngine`, hardware-decoded, muted and
looped. Pausing pauses the *engine*, so a covered or locked screen stops
decoding rather than just skipping presentation.

### Knowing when nobody is looking

The traps found so far, each of which broke the pause logic in a different way:

- **The desktop is a window.** When you click the desktop, `Progman` becomes
  the foreground window and its rect spans every monitor. A naive "does the
  foreground window cover this monitor?" test therefore pauses at exactly the
  moment the wallpaper is most visible, and *resumes* when you open something.
  Shell classes (`Progman`, `WorkerW`, `SHELLDLL_DefView`, the taskbars) are
  excluded from the coverage test.
- **The foreground window is not the only window.** With one app maximised per
  screen, the foreground window covers only its own monitor — the other is
  hidden by a *background* window. The test enumerates every top-level window,
  so each monitor pauses as soon as *any* window fully covers it.
- **A maximised window does not reach the bottom of the screen.** It stops at
  the taskbar (1040 on a 1080 monitor), so testing containment against the full
  monitor rect meant ordinary maximised apps *never* counted as covering — video
  decoded for hours behind them, while true fullscreen surfaces (RDP, screenshot
  overlays) did pause. Coverage is tested against the monitor's **work area**;
  the strip behind the taskbar is not visible wallpaper anyway.
- **Never let the media engine auto-play.** With `SetAutoPlay`, the engine
  starts itself when the source loads, the app's own playing-flag stays false,
  and a flag-guarded `pause()` becomes a no-op: an unstoppable decoder. Playback
  is started only by the surface's reconcile logic, `pause()` asks the *engine*
  whether it is running, and the desired state is re-asserted on every check
  rather than only on transitions — edge-triggered pause lost a race with the
  engine's "can play" event and got stuck in the playing state.
- **"Visible" windows can be invisible.** UWP keeps suspended apps around as
  visible but DWM-cloaked full-screen windows; counting those would pause the
  wallpaper forever. Cloaked windows (`DWMWA_CLOAKED`) are skipped.
- **`GUID_CONSOLE_DISPLAY_STATE` lies.** On a machine that has had a Remote
  Desktop session it reports "display off" while the monitors are plainly on,
  and registering for a power setting delivers the current value immediately —
  so one bogus reading suspended every animation permanently. The app uses the
  per-session `GUID_SESSION_DISPLAY_STATUS` instead, and only believes "off"
  after it has first seen "on", so a wrong initial value can no longer wedge
  it.

### Frame pacing

Smooth playback needed three things that are easy to get wrong:

1. **Don't present on a timer.** `IMFMediaEngine::OnVideoStreamTick` returns
   S_OK for a fresh frame and S_FALSE for "nothing new", but the safe wrapper
   maps both to `Ok`. Presenting on every tick regardless duplicates and drops
   frames against the clip's own clock. The vtable is called directly so the
   answer can be honoured.
2. **Don't use `WM_TIMER`.** It cannot fire faster than 10 ms, and it is
   synthesised only when the message queue is empty — so a 60 fps video on one
   screen starved the GIF timer on the other, which then caught up in bursts.
   Each surface gets a small pacing thread that posts a real message instead,
   giving both equal priority.
3. **Ask for millisecond timer resolution while animating.** The default
   ~15.6 ms tick turns a 40 ms GIF frame into a 47 ms one. Windows 10 2004+
   scopes the request to the calling process, and it is released as soon as
   the last surface goes away.

GIF frames are scheduled against a running deadline so rounding error cannot
accumulate into drift, and video presents are gated on a deadline too, which
is what makes `video_max_fps` an accurate cap rather than an approximate one.

**Bounded GIF memory — resolution first, frame rate last.** Decoded GIF frames
live in RAM, so the loader works to a hard budget (`gif_memory_budget_mb`).
What gives way matters: dropping frames is what makes an animation look
broken, while a slightly softer wallpaper is barely noticeable, and the GPU is
doing the upscale anyway. So the loader probes the clip, picks the largest
size at which *every* frame fits the budget, and only thins frames if even a
tiny render would not fit (and says so in the log when it does). Video needs
none of this: it streams.

**Staying cheap.** Nothing polls while idle. One timer for the rotation
schedule (capped at one wake per hour so a resumed machine catches up), a slow
housekeeping timer per animated surface, and OS notifications for lock/unlock,
display on/off, AC/battery, display changes, and clock changes.

**Autostart via logon task.** The registry Run key is the obvious mechanism,
but the shell deliberately defers those entries and staggers them behind
everything else in the startup queue, which can push a launch minutes past
sign-in. A logon scheduled task fires as soon as the session exists. The Run
key is kept as an automatic fallback if task creation is refused; only one of
the two is ever armed, so nothing starts twice, and the tray menu shows which
one is in use.

**Surviving the shell.** An Explorer restart destroys the `WorkerW` and every
child of it. The app listens for `TaskbarCreated`, re-adds its tray icon, and
rebuilds the desktop layer on a short retry schedule using the
already-decoded frames. A shorter verify timer fires after every attach,
because the shell occasionally retires a `WorkerW` seconds after we parent
into it — and because at sign-in there may not be a desktop to attach to yet.

## Measured

Windows 11 build 26200, two 1920×1080 screens. GIFs are 1920×1080 sources; the
video clip is 1080p **60 fps** H.264. Rendering rows were measured with
`pause_when_covered` off so the work actually happens.

| State | CPU (of one core) | Private memory | GPU 3D | GPU video decode |
| --- | --- | --- | --- | --- |
| Idle, still wallpapers | **0.00%** | **3 MB** | 0% | 0% |
| 2 GIFs | 7.7% | 210 MB | 3–4% | **0%** |
| 1 GIF + 1 video (1080p30) | ~8% | 228 MB | 1–2% | ~13% |
| 2 videos (1080p30) | 7.3% | 195 MB | 1–3% | **25–29%** |
| Anything, screens covered | **0.1%** | unchanged | 0% | 0% |

Measured frame pacing, from the debug log:

| | Before | After |
| --- | --- | --- |
| 1080p60 video | 33.9 fps, intervals 13–63 ms | **60.0 fps, 12–20 ms** |
| city.gif (native 25 fps) | 12.5 fps, intervals 62–105 ms | **25.0 fps, 35–44 ms** |

### Where the GPU time actually goes

Our own drawing is only **1–4% of the 3D engine**. The large number is the
**video decode engine**, and no presentation setting can influence it: the
decoder runs at the clip's native resolution and frame rate whether we present
every frame or none of them. `video_max_fps` is an accurate cap on
*presentation* (30 gives exactly 30.0 fps) and saves CPU, but it will not move
the decode figure.

The levers that genuinely reduce GPU load, in order of effect:

1. **Turn videos off** — tray ▸ Animated backgrounds ▸ *Use videos*. GIFs cost
   **0%** on the decode engine; that is the 25% → 0% difference above.
2. **Use smaller clips.** Decode cost scales with pixels × frame rate, so a
   1080p30 clip costs roughly an eighth of a 4K60 one. This is why
   `scripts/get-backgrounds.ps1` can normalize clips to 1080p30 — measured
   effect on the same clip pair: decode 54% → 26%.
3. **Use `mixed` rather than `only animated`**, so fewer screens animate at
   once.

All of it stops when a window covers the screen, so the cost is only paid
while you are actually looking at the wallpaper.

In real power terms (nvidia-smi, same machine): idle/paused 16.6 W, two
1080p30 videos playing 21 W — the animation costs **about 4–5 watts**.
Utilization percentages overstate it, because the GPU downclocks to ~210 MHz
for this workload and "busy %" is relative to that idle clock.

Memory during GIF playback is governed by `gif_memory_budget_mb`; video memory
is decoder buffers and is roughly flat regardless of clip length.

## Limits

- Video playback depends on the codecs Windows has. H.264/H.265 MP4 works out
  of the box; VP9/WebM needs the free *Web Media Extensions* from the Store. A
  clip that will not open is skipped and the still image underneath shows
  instead.
- WebP stills only work if the WebP codec is installed; off by default.
- The `WorkerW` technique is undocumented. The app degrades to still
  wallpapers rather than breaking if a future Windows build closes that seam.

## Credits

The wallpaper gallery fetched by `scripts/get-backgrounds.ps1` is
[dharmx/walls](https://github.com/dharmx/walls); some of the animated clips
came from [moewalls](https://moewalls.com).

## License

MIT
