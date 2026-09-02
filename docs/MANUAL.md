# WallRotate — User Manual

WallRotate is a small Windows tray app that puts a different wallpaper on every
screen, rotates them every 12 hours (and on **Win+Ctrl+R**), and can play
animated GIFs and video clips as live desktop backgrounds. While it is not
doing anything it costs 0% CPU and about 3 MB of RAM.

This manual covers everything: installing, organising your wallpaper folder,
every tray menu item, the hotkey, the command line, every setting, what the
animation actually costs, and what to do when something does not work.

- [1. Installing](#1-installing)
- [2. Setting up your wallpaper folder](#2-setting-up-your-wallpaper-folder)
- [3. The tray menu](#3-the-tray-menu)
- [4. The hotkey](#4-the-hotkey)
- [5. The clickable web launcher](#5-the-clickable-web-launcher)
- [6. Command line reference](#6-command-line-reference)
- [7. Settings reference](#7-settings-reference)
- [8. Performance and GPU cost](#8-performance-and-gpu-cost)
- [9. Troubleshooting](#9-troubleshooting)

---

## 1. Installing

### Build

You need a Rust toolchain (https://rustup.rs). Then:

```
cargo build --release
```

The result is a single executable, `target\release\wallrotate.exe`. It has no
installer dependencies and no runtime besides Windows itself.

### Install

```
target\release\wallrotate.exe --install
```

`--install` does three things:

1. **Copies the binary** to `%LOCALAPPDATA%\Programs\WallRotate\wallrotate.exe`.
2. **Registers a logon scheduled task** named `WallRotate`, so the app starts
   as soon as you sign in. If task creation is refused (some managed machines
   forbid it), it falls back to the classic registry Run key instead. Only one
   of the two is ever armed, so nothing starts twice; the tray menu's
   **Start with Windows** item shows which mechanism is in use.
3. **Starts the installed copy.**

A scheduled task is used instead of the Run key because Windows deliberately
defers Run-key entries behind everything else in the startup queue, which can
push a launch minutes past sign-in. A logon task fires as soon as the session
exists.

Re-running `--install` upgrades an existing installation in place: it asks the
running copy to quit, waits for it to exit, copies the new binary over the old
one, and relaunches.

### Uninstall

```
wallrotate.exe --uninstall
```

This stops the running instance and removes the login entry (task or Run key).
It then tells you the install folder is safe to delete:

```
%LOCALAPPDATA%\Programs\WallRotate
```

Your settings live separately in `%APPDATA%\WallRotate` (see
[Settings reference](#7-settings-reference)); delete that folder too if you
want nothing left behind.

---

## 2. Setting up your wallpaper folder

This is the one piece of setup that matters. WallRotate draws everything from
a single root folder, and one naming convention decides which files are
allowed to animate.

### The root folder: `wallpaper_dir`

By default WallRotate looks in:

```
%USERPROFILE%\Pictures\backgrounds
```

The folder is **scanned recursively**, so organise it however you like —
category subfolders are welcome and change nothing about how images are
picked. Still images are `.jpg`, `.jpeg`, `.png`, and `.bmp` (plus `.webp` if
you enable `include_webp` and have the WebP codec installed).

To point it somewhere else, open the tray menu, pick **Edit settings...**,
change `wallpaper_dir`, save, and pick **Reload settings**. TOML treats
backslashes in double-quoted strings as escapes, so write the path in one of
these two ways:

```toml
wallpaper_dir = 'D:\Wallpapers'        # single quotes: backslashes are literal
wallpaper_dir = "D:\\Wallpapers"       # double quotes: backslashes doubled
```

### Animated backgrounds: `animated_dirs`

GIFs and videos are **not** used as animated backgrounds just because they are
in the library. The rule is:

> Only GIFs and video clips that sit inside a folder whose name matches one of
> the entries in `animated_dirs` become animated backgrounds.

- The default is `animated_dirs = ["animated"]` — so a folder named
  `animated`, anywhere in the tree, marks its contents (including
  subfolders) as animated-eligible.
- Folder-name matching is case-insensitive, and everything *below* a matching
  folder inherits the eligibility, so `animated\nature\rain.mp4` qualifies.
- **Multiple names are allowed**: `animated_dirs = ["animated", "live",
  "loops"]` makes all three folder names work.
- An **empty list** (`animated_dirs = []`) means animated files are eligible
  from **anywhere** in the library. The tray menu item *Only from the
  "animated" folder* toggles between `["animated"]` and `[]`.
- **Dot-folders are always ignored** by the scan, wherever they are. This is
  deliberate: `animated\.originals` is the conventional place to keep
  full-resolution source clips that you have transcoded down for wallpaper use
  (see [Performance](#8-performance-and-gpu-cost)) — the scanner will never
  pick them up. The `.git` folder of a cloned wallpaper repo is skipped for
  the same reason.

Animated files found *outside* the allowed folders are not silently dropped:
the tray menu header and **Show what is on screen...** report them as
"outside animated_dirs" so you can tell why something is not playing.

### A worked example

```
Pictures\backgrounds\
├── nature\
│   ├── forest.jpg          → still image pool
│   └── coast.png           → still image pool
├── cities\
│   └── tokyo-night.jpg     → still image pool
├── animated\
│   ├── rain-loop.mp4       → animated pool (video)
│   ├── campfire.gif        → animated pool (GIF)
│   ├── space\
│   │   └── nebula.mp4      → animated pool (inherits from animated\)
│   └── .originals\
│       └── rain-loop-4k.mp4  → ignored (dot-folder)
├── clip.mp4                → found, but OUT OF SCOPE (not under an
│                             animated_dirs folder) — reported, not played
└── .git\                   → ignored (dot-folder)
```

With the defaults, that library is: 3 still images, 1 GIF, 2 videos, and one
video reported as outside `animated_dirs`.

Two more knobs interact with the scan:

- `exclude_dirs` — a list of folder names skipped entirely (neither stills nor
  animated files under them are seen).
- `include_gif` / `include_video` — turn a whole source type off. The tray
  submenu shows live counts, e.g. `Use videos (11)`, so you can see what the
  scan actually found.

After adding or removing files, pick **Rescan wallpaper folder** from the tray
menu (or run `wallrotate.exe --rescan`).

### The quick route: `get-backgrounds.ps1`

If you do not have a wallpaper library yet, the repo ships a script that
builds one:

```
powershell -ExecutionPolicy Bypass -File scripts\get-backgrounds.ps1
```

It fetches the wallpaper gallery from the
[dharmx/walls](https://github.com/dharmx/walls) GitHub repo via a sparse
clone into your wallpaper folder, excluding the `girl`, `boccha`, `decay`,
`devicons`, and `weirdcore` folders, and can optionally normalise video clips
to 1080p / 30 fps with ffmpeg — the format that keeps GPU decode cost low.
See the comments at the top of the script for its options.

---

## 3. The tray menu

Right-click the WallRotate tray icon. (On Windows 11, new tray icons start in
the overflow flyout — click `^` near the clock, then drag the icon onto the
taskbar to pin it.)

The greyed first line is a live summary of the library — for example
`WallRotate -- 214 images, 5 GIFs, 11 videos` — and appends warnings such as
`(3 videos off)` when `include_video` is disabled or `(2 outside
animated_dirs)` when animated files sit in the wrong place.

### Next wallpapers / Previous wallpapers

Advance every rotating screen to fresh picks, or step back to the previous
ones. **Next wallpapers** shows the current hotkey next to it (Win+Ctrl+R by
default). Picks come from a shuffled playlist; a round never repeats an image
across screens, and the playlist reshuffles when it runs out.

### Change just one screen ▸

Only shown when you have more than one monitor. Pick a screen and only that
one advances, right now. This is a **one-shot nudge**: the other screens keep
their wallpaper *and* keep playing whatever video or GIF they are on, and the
12-hour clock is not reset. Contrast with the **Rotate** submenu below, which
is the persistent version. The same nudge from a script:
`wallrotate.exe --screen 2`.

### Show what is on screen...

Opens a dialog listing, for each screen:

- the wallpaper's file name and its parent folder,
- an `[animated]` marker when that screen is playing a GIF or video,
- a `[pinned -- does not rotate]` marker for screens excluded from rotation,
- a playback note for animated screens,

followed by the time until the next automatic change, the library counts
(still images / GIFs / videos), any warnings (videos ignored because
`include_video` is off, animated files outside `animated_dirs`), and the
current Start-with-Windows status.

### Animated backgrounds (mode) ▸

The live-wallpaper control centre. The submenu label shows the current mode,
and every item takes effect immediately without advancing the playlist or
resetting the 12-hour clock.

| Item | Meaning |
| --- | --- |
| **Off — still images only** | No animation anywhere. |
| **Mixed — some screens animate** | Each screen has a chance (`animated_chance`, default 0.34 — about one in three) of drawing from the animated pool at each rotation; the rest get still images. |
| **Only animated — every screen, GIF or video** | Every screen draws from the animated pool. |
| **Use GIFs (n)** | Include `.gif` files in the animated pool. The count is live from the last scan — `(0)` means the scan found none. |
| **Use videos (n)** | Include video files (`.mp4` `.m4v` `.mov` `.wmv` `.avi` `.mkv` `.webm`). |
| **Only from the "animated" folder** | Ticked: only files under an `animated_dirs` folder are eligible (`animated_dirs = ["animated"]`). Unticked: animated files from anywhere (`animated_dirs = []`). |

Even in *Only animated* mode, still images are assigned underneath each
screen, so the desktop looks right whenever the app is not running.

### Rotate (which screens) ▸

The **persistent** counterpart to *Change just one screen*: it chooses which
screens take part in the automatic rotation and the hotkey.

- **All screens** — everything rotates (the default, `rotate_screens = []`).
- **Screen N (resolution)** — tick/untick individual screens. Unticking a
  screen **pins** it: it keeps whatever it is showing — including a running
  video or GIF, which carries on playing — while the others rotate on
  schedule and on the hotkey.
- Clicking a single screen while "All screens" is active is a shortcut for
  "only this one rotates".

Pins apply to **rotation only**. Changing the animated mode or the source
types still applies to every screen, so turning animation off turns it off
everywhere, pinned or not.

### Web launcher ▸

Everything about the clickable web wallpaper: pick a preset (Grid / Dock /
Minimal) or turn it **Off**, choose which screens show it, toggle
**Clickable (launch on click)**, **Launcher settings...** for the GUI editor,
and **Edit launcher.json...** to open
`launcher.json`. Covered in full in
[section 5](#5-the-clickable-web-launcher).

### The rest

| Item | Effect |
| --- | --- |
| **Rescan wallpaper folder** | Re-read the wallpaper folder after adding or removing files. |
| **Open wallpaper folder** | Open `wallpaper_dir` in Explorer. |
| **Edit settings...** | Open `config.toml` in your default editor. |
| **Reload settings** | Re-read `config.toml` and apply it. Required after hand-editing. |
| **Start with Windows (logon task)** | Toggle run-at-login. The label shows which mechanism is armed: `logon task`, `Run key`, or `off`. |
| **Exit** | Quit the app. Wallpapers stay as they are; animation stops. |

---

## 4. The hotkey

**Win+Ctrl+R** advances every rotating screen — the same as **Next
wallpapers**. It works system-wide.

To change it, edit `hotkey` in `config.toml` and pick **Reload settings**:

```toml
hotkey = "win+ctrl+r"
```

A hotkey is one or more modifiers plus exactly one key, joined with `+`
(case-insensitive, spaces around `+` are fine):

- **Modifiers:** `win` (also accepted: `super`, `meta`, `cmd`), `ctrl` (or
  `control`), `alt`, `shift`.
- **Key:** a letter `a`–`z`, a digit `0`–`9`, a function key `f1`–`f24`, or
  one of the named keys: `space`, `enter`/`return`, `tab`, `esc`/`escape`,
  `left`, `up`, `right`, `down`, `home`, `end`, `pageup`/`pgup`,
  `pagedown`/`pgdn`, `insert`/`ins`, `delete`/`del`.

Examples:

```toml
hotkey = "ctrl+alt+w"
hotkey = "win+shift+f9"
hotkey = "win+pageup"
```

If another application already owns the combination, registration fails; see
[Troubleshooting](#9-troubleshooting).

---

## 5. The clickable web launcher

The web launcher turns a screen's background into an HTML page with tiles that
open apps, folders, and URLs when you click them — rendered below the desktop
icons like any other WallRotate background. It uses the Edge WebView2 runtime
that ships with Windows 11, so there is nothing extra to install.

### Turning it on

Tray ▸ **Web launcher ▸** and pick a design:

| Preset | Look |
| --- | --- |
| **Grid** | A centred clock with a row of labelled tiles beneath it. |
| **Dock** | A macOS-style dock of tiles along the bottom edge, clock above. |
| **Minimal** | Small text links in a corner; the quietest of the four. |
| **Dashboard** | A widget board: clock + web search header, weather, live PC stats, a calendar, and your tiles as quick links. See below. |

The same submenu chooses **which screens** show the launcher (all, or ticked
individually — the other screens keep their normal rotating wallpapers), and
whether it is **Clickable**. With *Clickable* off the page is purely
decorative and WallRotate does not touch mouse input at all.

### The settings GUI

Pick **Launcher settings...** from the submenu. A window opens where you can:

- **Edit tiles** — icon (any emoji), label, target, and arguments per tile,
  with a **Browse** button that opens a native file picker, up/down reordering,
  add and remove. Targets can be an exe, a document, a folder, a URL, or a
  `ms-settings:` link; `%ENV%` variables are expanded.
- **Page options** — the background image and the clock toggle.
- **Dashboard widgets** — turn each widget on or off, and set the weather
  city and °C/°F unit.

**Save** writes `launcher.json` and reloads the wallpaper immediately — you
see the change behind the window as soon as you click it.

### The dashboard preset

The dashboard turns a screen into a widget board in the style of a homelab
dashboard: translucent cards on your chosen background.

| Widget | What it shows | Where the data comes from |
| --- | --- | --- |
| **Clock + search** | Big clock, date, and a web search bar (Enter opens your default browser). | Local. |
| **Weather** | Current temperature and conditions, humidity, wind, 5-day forecast. | [Open-Meteo](https://open-meteo.com), no API key. Set your city in the settings GUI. This is the only widget that touches the network. |
| **System** | Live CPU %, RAM, per-disk free space, network up/down, uptime, battery. | The WallRotate process itself feeds the page a snapshot every 2 seconds — nothing is installed and nothing else runs. |
| **Calendar** | The current month, today highlighted. | Local. |
| **Quick links** | Your launcher tiles in compact form. | `launcher.json`. |

When the dashboard is covered by a window it is suspended like every other
animated background, and the stats polling stops with it — a covered
dashboard costs nothing.

### Configuring by hand: launcher.json

Prefer the GUI above; the file behind it is
`%APPDATA%\WallRotate\web\launcher.json` (**Edit launcher.json...** in the
submenu opens it). All presets read this one file:

```json
{
  "background": "",
  "clock": true,
  "widgets": {
    "search": true,
    "stats": true,
    "calendar": true,
    "links": true,
    "weather": { "enabled": true, "city": "Tel Aviv", "unit": "c" }
  },
  "tiles": [
    { "icon": "🗒️", "label": "Notepad",   "target": "notepad.exe" },
    { "icon": "📁", "label": "Downloads",  "target": "%USERPROFILE%\\Downloads" },
    { "icon": "🌐", "label": "GitHub",     "target": "https://github.com" },
    { "icon": "🎮", "label": "Steam",      "target": "C:\\Program Files (x86)\\Steam\\steam.exe", "args": "-silent" }
  ]
}
```

- `target` is anything the Windows shell can open: an exe on the PATH, a full
  path to a program or document, a folder, a URL, or a `ms-settings:` link.
  `%ENV%` variables are expanded.
- `args` (optional) is passed to the program as its command line.
- `icon` is an emoji (or any short text); `label` is the caption.
- `background` may be empty (each preset has its own look), or an image from
  your wallpaper library via the `https://backgrounds.local/` mapping — e.g.
  `"https://backgrounds.local/nature/forest.jpg"` for
  `<wallpaper_dir>\nature\forest.jpg`.
- `clock` shows or hides the clock on presets that have one.
- `widgets` only affects the Dashboard preset; the other presets ignore it.

Changes take effect the next time the page loads — pick the preset again in
the tray menu, or **Reload settings**.

### Using your own page

Set `web_wallpaper` in `config.toml` to a full path to your own `.html` file.
Its folder is served as `https://wallpaper.local/`, and a `launcher.json`
beside it is what `fetch('/launcher.json')` returns. Your page launches things
by posting a message:

```js
window.chrome.webview.postMessage({ action: "open", target: "notepad.exe", args: "" });
```

Keep the left ~150 px and the bottom ~90 px free if you use desktop icons or
a taskbar — the presets do.

Two safety properties worth knowing: the host only accepts messages from the
page it loaded (`wallpaper.local`), and any navigation away from your local
page is cancelled — external links open in your default browser instead of
inside the wallpaper.

### What it costs

WebView2 is Chromium, so the launcher brings the standard set of helper
processes (~7 processes, ~400 MB working set for two screens, mostly shared
memory). The presets animate with cheap CSS only; measured GPU load is a few
percent of the idle-clocked GPU, i.e. fractions of a watt. When a window
covers the screen the page is hidden and its renderer suspended, exactly like
the video decoder — a covered launcher costs nothing.

---

## 6. Command line reference

| Verb | Effect |
| --- | --- |
| *(none)* | Run in the tray |
| `--install` | Copy to `%LOCALAPPDATA%\Programs\WallRotate`, register run-at-login, start |
| `--uninstall` | Stop the running instance and remove the login entry |
| `--next` | Advance every rotating screen (same as the hotkey) |
| `--prev` | Step back to the previous wallpapers |
| `--screen N` | Change only screen N (1 = leftmost), leaving the others alone |
| `--rescan` | Re-read the wallpaper folder |
| `--settings` | Open the launcher settings window (see [section 5](#5-the-clickable-web-launcher)) |
| `--quit` | Stop the running instance |

Only one instance runs at a time. The control verbs (`--next`, `--prev`,
`--screen`, `--rescan`, `--quit`) only ever talk to an already-running
instance — they never start one.

---

## 7. Settings reference

Settings live in `%APPDATA%\WallRotate\config.toml`. Edit the file (tray ▸
**Edit settings...**), then pick **Reload settings**. The file self-upgrades:
options added by a newer build appear in it automatically, and
`config_version` lets a new build migrate old values forward. `state.toml`
next to it holds the rotation clock and the current picks — you never need to
edit that one.

| Key | Default | What it does |
| --- | --- | --- |
| `config_version` | `2` | Schema version, managed by the app so old files can be migrated forward. Leave it alone. |
| `wallpaper_dir` | `%USERPROFILE%\Pictures\backgrounds` | Root folder, scanned recursively, for all wallpapers. See [section 2](#2-setting-up-your-wallpaper-folder). |
| `interval_hours` | `12` | Hours between automatic rotations. Fractions work (`0.5` = every 30 minutes). The clock survives restarts and reboots — restarting the app does not reset it. |
| `hotkey` | `"win+ctrl+r"` | Global hotkey for the next rotation. See [section 4](#4-the-hotkey). |
| `fit` | `"fill"` | How still images fit the screen: `fill`, `fit`, `stretch`, `center`, `tile`, or `span`. |
| `animated_mode` | `"mixed"` | `off` (stills only), `mixed` (some screens animate), or `always` (shown as *Only animated* in the menu: every screen animates). |
| `animated_chance` | `0.34` | In `mixed` mode, the chance that a given screen draws from the animated pool. `0.34` is roughly one screen in three. Ignored when the mode is `off` or `always`. |
| `animated_dirs` | `["animated"]` | Folder names whose contents may animate. Only GIFs/videos under a folder with one of these names are eligible; an empty list allows them from anywhere. See [section 2](#2-setting-up-your-wallpaper-folder). |
| `rotate_screens` | `[]` | Screen numbers (1-based, left to right) that take part in rotation. Empty = all screens. `[1]` rotates only screen 1 and pins the rest. |
| `exclude_dirs` | `[]` | Folder names skipped entirely during the scan. |
| `include_webp` | `false` | Use `.webp` still images. Windows only shows these when the WebP codec is installed. |
| `max_fps` | `50` | Ceiling on GIF frame rate. A *safety valve*, not a throttle — set it below a GIF's native rate and frames get merged. Lowering it also thins stored frames, cutting memory as well as CPU. |
| `video_max_fps` | `60` | Real cap on video *presentation* rate, and the main CPU lever for video. It does not reduce GPU decode cost (see [section 8](#8-performance-and-gpu-cost)). |
| `include_video` | `true` | Play `.mp4` `.m4v` `.mov` `.wmv` `.avi` `.mkv` `.webm` as animated backgrounds. |
| `include_gif` | `true` | Use `.gif` files as animated backgrounds. |
| `max_gif_width` | `1280` | Upper bound on decoded GIF size. Bigger GIFs are downscaled at load; the GPU upscales for display. |
| `max_gif_height` | `720` | See above. |
| `max_gif_frames` | `400` | Upper bound on decoded frames per GIF. |
| `gif_memory_budget_mb` | `128` | Hard ceiling on RAM for decoded GIF frames. The loader picks the largest size at which every frame fits the budget — resolution gives way first, frame rate last. |
| `pause_when_covered` | `true` | Stop rendering (and decoding) while a window fully covers the screen. Strongly recommended; this is what makes animation nearly free in practice. |
| `pause_on_battery` | `false` | Stop rendering while on battery power. |
| `start_with_windows` | `true` | Run at login. Mirrors the tray toggle. |
| `notify_on_rotate` | `false` | Show a tray balloon after each rotation. |
| `web_wallpaper` | `""` | The clickable web launcher: `""` (off), a preset name (`"grid"`, `"dock"`, `"minimal"`), or a full path to your own `.html` page. See [section 5](#5-the-clickable-web-launcher). |
| `web_screens` | `[]` | Screen numbers (1-based) that show the web launcher. Empty = all screens. Screens not listed keep their normal rotating wallpapers. |
| `web_interactive` | `true` | Forward desktop clicks into the page so tiles launch things. Off = purely decorative, and no mouse hook is installed. |

Notes on the file itself:

- A UTF-8 byte-order mark (which some Windows editors prepend) is tolerated.
- If the file cannot be parsed, it is renamed to `config.toml.bad` so you can
  see what broke, and a fresh default file is written. See
  [Troubleshooting](#9-troubleshooting).

---

## 8. Performance and GPU cost

What each state costs (measured on Windows 11, two 1920×1080 screens — see
the README for the full table):

- **Idle, still wallpapers: 0.00% CPU, ~3 MB.** Nothing polls; the app sleeps
  until a timer or an OS notification wakes it.
- **GIFs** are decoded up front into a RAM budget (`gif_memory_budget_mb`) and
  blitted by the GPU's 3D engine. They use **no video-decode hardware at all**
  — if GPU decode load is what you care about, GIFs are free.
- **Video** is hardware-decoded and streams (nothing cached in RAM). In real
  power terms, two 1080p30 clips cost **about 4–5 watts** on the measured
  machine. Decode-engine utilisation percentages overstate the cost, because
  the GPU downclocks for this workload.
- **Covered, locked, fullscreen app, or display off: everything stops**
  (~0.1% CPU, GPU at zero) when `pause_when_covered` is on. Pausing pauses the
  decoder itself, not just the drawing — so the cost is only paid while the
  wallpaper is actually visible.

The levers that genuinely reduce GPU load, in order of effect:

1. **Turn videos off** (tray ▸ Animated backgrounds ▸ *Use videos*). GIFs cost
   0% on the decode engine.
2. **Use smaller clips.** Decode cost scales with pixels × frame rate — a
   1080p30 clip costs roughly an eighth of a 4K60 one. Normalising a library
   to 1080p30 halved the measured decode load (54% → 26%) on the same clips.
3. **Use `mixed` rather than `only animated`**, so fewer screens animate at
   once.

Note that `video_max_fps` caps *presentation* (and saves CPU) but cannot
reduce decode cost: the decoder runs at the clip's native resolution and frame
rate regardless.

### Keep clips at 1080p / 30 fps

`scripts\get-backgrounds.ps1` can normalise fetched videos to 1080p30 for you.
To do the same to a single file with ffmpeg:

```
ffmpeg -i clip.mp4 -vf "scale=-2:1080" -r 30 -c:v libx264 -crf 20 -an clip-1080p30.mp4
```

Keep the original somewhere the scanner ignores — a dot-folder such as
`animated\.originals` is the convention — and put the normalised copy in the
`animated` folder.

---

## 9. Troubleshooting

### Nothing animates

Work down this list:

1. Tray ▸ **Animated backgrounds** — is the mode *Off*? In *Mixed* mode only
   some screens animate (default chance 0.34 per screen), so a rotation with
   no animated screen is normal; *Only animated* forces every screen.
2. Look at the counts in the same submenu: `Use GIFs (0)` / `Use videos (0)`
   means the scan found nothing of that kind. Remember the rule: animated
   files must sit under a folder named in `animated_dirs` (default:
   `animated`). The menu header and **Show what is on screen...** call out
   files that are "outside animated_dirs".
3. Still stuck? Enable the debug log. Set `WALLROTATE_DEBUG=1` in the
   environment and restart the app:

   ```powershell
   wallrotate.exe --quit
   $env:WALLROTATE_DEBUG = "1"
   & "$env:LOCALAPPDATA\Programs\WallRotate\wallrotate.exe"
   ```

   The log is written to `%APPDATA%\WallRotate\debug.log` and contains
   desktop-layer diagnostics, media-open failures, and live frame-pacing
   statistics. Look for lines about attaching the animation surface (the
   healthy end state is `Progman > WorkerW > WallRotateAnimSurface`) and for
   media-open failures naming your clips — those usually mean a codec problem
   (next item). The log is silent and free when the variable is not set.

### A video will not play

Video playback depends on the codecs Windows has. H.264/H.265 MP4 works out
of the box; **VP9/WebM needs the free *Web Media Extensions*** from the
Microsoft Store. A clip that will not open is skipped — the still image
underneath shows instead — and is named in the debug log.

### WebP images do not show up

`.webp` needs the WebP codec installed in Windows, and is off by default —
set `include_webp = true` and reload.

### The hotkey does not work

Some other application already registered the same combination — Windows
gives a global hotkey to whichever app asks first. When registration fails,
the **Next wallpapers** menu item shows no shortcut label next to it. Pick a
different combination in `config.toml` (see [section 4](#4-the-hotkey)) and
choose **Reload settings**.

### I cannot find the tray icon

On Windows 11 new tray icons start in the overflow flyout: click `^` near the
clock. Drag the icon onto the taskbar to pin it permanently.

### The desktop flickered / Explorer restarted

Nothing to do — WallRotate self-heals. When Explorer restarts it destroys the
desktop layer the animations live in; the app notices, re-adds its tray icon,
and rebuilds the layer within a few seconds from the already-decoded frames.

### I edited config.toml and nothing changed

Settings are applied when the app starts and when you pick **Reload
settings** from the tray menu — hand-editing the file alone does nothing
until you reload.

### My config file disappeared / was reset

If `config.toml` cannot be parsed (a typo, an unclosed quote, an unescaped
backslash in a double-quoted path), it is renamed to `config.toml.bad` in the
same folder and a fresh default file is written. Your old file is still
there: fix the syntax error in `config.toml.bad`, copy the contents back into
`config.toml`, and pick **Reload settings**.
