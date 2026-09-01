//! GIF decoding into a bounded set of ready-to-upload BGRA frames.
//!
//! Frames live in RAM for as long as a GIF is on screen, so the loader works to
//! a hard memory budget. The important design choice is *what gives way* when
//! the budget is tight: resolution, not frame rate. Dropping frames is what
//! makes an animation look broken, while a slightly softer image on a wallpaper
//! is barely noticeable -- and the GPU is doing the upscale anyway.
//!
//! So the loader probes the clip first, picks the largest size at which every
//! frame fits the budget, and only falls back to thinning frames if even a very
//! small render would not fit.

use image::imageops::FilterType;
use image::AnimationDecoder;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

pub struct GifAnim {
    pub width: u32,
    pub height: u32,
    /// Fully composited frames: BGRA8, top-down, tightly packed.
    pub frames: Vec<Vec<u8>>,
    pub delays_ms: Vec<u32>,
    /// Native canvas size before any downscale, for diagnostics.
    pub source_size: (u32, u32),
    pub source_frames: usize,
}

impl GifAnim {
    pub fn stride(&self) -> u32 {
        self.width * 4
    }
    pub fn bytes(&self) -> usize {
        self.frames.len() * self.width as usize * self.height as usize * 4
    }
    pub fn is_static(&self) -> bool {
        self.frames.len() < 2
    }
}

pub struct Limits {
    pub max_width: u32,
    pub max_height: u32,
    pub max_frames: usize,
    pub budget_bytes: usize,
    pub min_delay_ms: u32,
}

impl Limits {
    pub fn from_config(cfg: &crate::config::Config) -> Self {
        Limits {
            max_width: cfg.max_gif_width.max(64),
            max_height: cfg.max_gif_height.max(64),
            max_frames: cfg.max_gif_frames.max(2),
            budget_bytes: cfg.gif_memory_budget_mb.max(8) * 1024 * 1024,
            min_delay_ms: cfg.frame_floor_ms(),
        }
    }
}

fn open(path: &Path) -> Option<image::codecs::gif::GifDecoder<BufReader<File>>> {
    let file = File::open(path).ok()?;
    image::codecs::gif::GifDecoder::new(BufReader::new(file)).ok()
}

fn normalise_delay(frame: &image::Frame, floor: u32) -> u32 {
    let (num, den) = frame.delay().numer_denom_ms();
    let mut delay = if den == 0 { 100 } else { num / den.max(1) };
    // 0ms and 10ms are historically rendered as 100ms.
    if delay < 20 {
        delay = 100;
    }
    delay.max(floor)
}

/// How many frames does this clip have, and how big is its canvas?
fn probe(path: &Path, limits: &Limits) -> Option<(usize, u32, u32)> {
    let decoder = open(path)?;
    let mut count = 0usize;
    let mut size = (0u32, 0u32);
    let mut carry = 0u32;
    for frame in decoder.into_frames() {
        let Ok(frame) = frame else { break };
        // Count what the decode pass will actually keep, so the size estimate
        // matches: sub-floor frames get merged into their predecessor.
        let delay = normalise_delay(&frame, 1);
        carry = carry.saturating_add(delay);
        if size.0 == 0 {
            let buf = frame.into_buffer();
            size = (buf.width(), buf.height());
        }
        if carry >= limits.min_delay_ms || count == 0 {
            count += 1;
            carry = 0;
        }
        if count >= limits.max_frames {
            break;
        }
    }
    if count == 0 || size.0 == 0 {
        return None;
    }
    Some((count, size.0, size.1))
}

/// Largest size that respects both the configured cap and the memory budget.
fn target_size(w: u32, h: u32, frames: usize, limits: &Limits) -> (u32, u32) {
    let (mut tw, mut th) = fit_within(w, h, limits.max_width, limits.max_height);
    let per_frame = limits.budget_bytes / frames.max(1);
    let max_pixels = per_frame / 4;
    let pixels = tw as usize * th as usize;
    if max_pixels > 0 && pixels > max_pixels {
        // Round *down*, with a little headroom. Rounding up here overshoots the
        // budget by a few kilobytes, and the safety net below reacts to that by
        // halving the frame count -- a catastrophic answer to a rounding error.
        let scale = (max_pixels as f64 / pixels as f64).sqrt() * 0.99;
        tw = ((tw as f64 * scale).floor() as u32).max(16);
        th = ((th as f64 * scale).floor() as u32).max(16);
    }
    (tw, th)
}

pub fn load(path: &Path, limits: &Limits) -> Option<GifAnim> {
    let (frame_count, src_w, src_h) = probe(path, limits)?;
    let (width, height) = target_size(src_w, src_h, frame_count, limits);

    let decoder = open(path)?;
    let mut frames: Vec<Vec<u8>> = Vec::with_capacity(frame_count);
    let mut delays: Vec<u32> = Vec::with_capacity(frame_count);
    let mut source_frames = 0usize;

    for frame in decoder.into_frames() {
        let Ok(frame) = frame else { break };
        source_frames += 1;
        // Raw delay, matching what probe() counted. The playback floor is
        // applied when drawing, not here, so the two passes agree on frame
        // count -- if they disagreed the size estimate would be wrong.
        let delay = normalise_delay(&frame, 1);

        // Hold the previous frame longer rather than storing a frame that would
        // be on screen for less than one frame-time.
        let rate_ok = delays.last().map_or(true, |d| *d >= limits.min_delay_ms);
        if !frames.is_empty() && !rate_ok {
            if let Some(last) = delays.last_mut() {
                *last = last.saturating_add(delay);
            }
            continue;
        }

        let buf = frame.into_buffer();
        let rgba = if buf.width() != width || buf.height() != height {
            image::imageops::resize(&buf, width, height, FilterType::Triangle)
        } else {
            buf
        };

        let mut data = rgba.into_raw();
        // RGBA -> BGRA, which is what Direct2D wants.
        for px in data.chunks_exact_mut(4) {
            px.swap(0, 2);
        }

        // A frame identical to the one already on screen costs nothing to skip.
        if frames.last().is_some_and(|last| *last == data) {
            if let Some(last) = delays.last_mut() {
                *last = last.saturating_add(delay);
            }
            continue;
        }

        frames.push(data);
        delays.push(delay);
        if frames.len() >= limits.max_frames {
            break;
        }
    }

    if frames.is_empty() {
        return None;
    }

    // Safety net: the probe should have sized this to fit, so reaching here
    // means the estimate was wrong. Say so, because thinning frames is exactly
    // what makes an animation look broken.
    let frame_bytes = width as usize * height as usize * 4;
    while frames.len() * frame_bytes > limits.budget_bytes && frames.len() > 2 {
        crate::log::line(format!(
            "gif: over budget at {}x{} with {} frames -- thinning (raise gif_memory_budget_mb)",
            width,
            height,
            frames.len()
        ));
        halve(&mut frames, &mut delays);
    }

    Some(GifAnim {
        width,
        height,
        frames,
        delays_ms: delays,
        source_size: (src_w, src_h),
        source_frames,
    })
}

fn fit_within(w: u32, h: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    if w <= max_w && h <= max_h {
        return (w.max(1), h.max(1));
    }
    let s = (max_w as f64 / w as f64).min(max_h as f64 / h as f64);
    (
        ((w as f64 * s).round() as u32).max(1),
        ((h as f64 * s).round() as u32).max(1),
    )
}

/// Keep the even-indexed frames, folding each dropped delay into its predecessor.
fn halve(frames: &mut Vec<Vec<u8>>, delays: &mut Vec<u32>) {
    let old_delays = std::mem::take(delays);
    let old_frames = std::mem::take(frames);
    for (i, f) in old_frames.into_iter().enumerate() {
        let d = old_delays.get(i).copied().unwrap_or(100);
        if i % 2 == 0 {
            frames.push(f);
            delays.push(d);
        } else if let Some(last) = delays.last_mut() {
            *last = last.saturating_add(d);
        }
    }
}
