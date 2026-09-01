//! Small helpers: wide strings, time formatting, and a dependency-free PRNG.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// UTF-16, NUL-terminated. Keep the returned Vec alive while the pointer is in use.
pub fn wide(s: &str) -> Vec<u16> {
    OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
}

pub fn wide_path(p: &Path) -> Vec<u16> {
    p.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
}

/// # Safety
/// `p` must be null or point at a NUL-terminated UTF-16 string.
pub unsafe fn from_wide_ptr(p: *const u16) -> String {
    if p.is_null() {
        return String::new();
    }
    let mut len = 0usize;
    while *p.add(len) != 0 {
        len += 1;
    }
    String::from_utf16_lossy(std::slice::from_raw_parts(p, len))
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// "11h 32m" / "48m" / "under a minute"
pub fn human_duration(secs: u64) -> String {
    if secs < 60 {
        return String::from("under a minute");
    }
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    if h == 0 {
        format!("{}m", m)
    } else {
        format!("{}h {}m", h, m)
    }
}

/// splitmix64 -- tiny, fast, and plenty good for shuffling a playlist.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    pub fn next_f32(&mut self) -> f32 {
        (self.next_u64() >> 40) as f32 / (1u32 << 24) as f32
    }

    pub fn shuffle<T>(&mut self, v: &mut [T]) {
        if v.len() < 2 {
            return;
        }
        for i in (1..v.len()).rev() {
            let j = (self.next_u64() % (i as u64 + 1)) as usize;
            v.swap(i, j);
        }
    }
}

/// Monotonic microseconds from the performance counter. Unlike GetTickCount64
/// this does not quantise to the system tick, so it can measure frame jitter.
pub fn perf_micros() -> u64 {
    use std::sync::atomic::{AtomicI64, Ordering};
    use windows::Win32::System::Performance::{
        QueryPerformanceCounter, QueryPerformanceFrequency,
    };
    // The frequency is fixed for the life of the system, and this runs on the
    // animation hot path, so look it up once.
    static FREQ: AtomicI64 = AtomicI64::new(0);
    let mut freq = FREQ.load(Ordering::Relaxed);
    if freq == 0 {
        unsafe {
            let _ = QueryPerformanceFrequency(&mut freq);
        }
        FREQ.store(freq, Ordering::Relaxed);
    }
    if freq <= 0 {
        return 0;
    }
    let mut count: i64 = 0;
    unsafe {
        let _ = QueryPerformanceCounter(&mut count);
    }
    ((count as u128 * 1_000_000) / freq as u128) as u64
}

/// Seed from the high-resolution counter, the pid, and the wall clock.
pub fn random_seed() -> u64 {
    use windows::Win32::System::Performance::QueryPerformanceCounter;
    let mut qpc: i64 = 0;
    unsafe {
        let _ = QueryPerformanceCounter(&mut qpc);
    }
    let pid = std::process::id() as u64;
    (qpc as u64)
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add(pid.wrapping_mul(0x0000_0001_0000_01B3))
        .wrapping_add(now_unix())
}
