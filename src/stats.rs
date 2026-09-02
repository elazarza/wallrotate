//! System statistics for the dashboard preset.
//!
//! The page cannot see the machine, so it asks the host: every couple of
//! seconds it posts `{action:"get_stats"}` and gets one JSON snapshot back.
//! Pull, not push, on purpose: a hidden (suspended) page stops asking, so a
//! covered dashboard costs nothing -- the same economics as paused video.
//!
//! Rates (CPU %, network B/s) need two samples; the first call primes the
//! deltas and reports zeros, which the page renders as dashes for a beat.

use std::cell::RefCell;

use windows::Win32::Foundation::FILETIME;
use windows::Win32::NetworkManagement::IpHelper::{FreeMibTable, GetIfTable2, MIB_IF_TABLE2};
use windows::Win32::Storage::FileSystem::{
    GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives,
};
use windows::Win32::System::Power::GetSystemPowerStatus;
use windows::Win32::System::SystemInformation::{
    GetTickCount64, GlobalMemoryStatusEx, MEMORYSTATUSEX,
};
use windows::Win32::System::Threading::GetSystemTimes;
use windows::Win32::System::Power::SYSTEM_POWER_STATUS;

struct Prev {
    idle: u64,
    kernel: u64,
    user: u64,
    net_rx: u64,
    net_tx: u64,
    at_us: u64,
}

thread_local! {
    static PREV: RefCell<Option<Prev>> = const { RefCell::new(None) };
}

fn ft(f: FILETIME) -> u64 {
    ((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64
}

/// Total bytes in/out across operational non-loopback interfaces.
fn net_totals() -> (u64, u64) {
    let mut rx = 0u64;
    let mut tx = 0u64;
    unsafe {
        let mut table: *mut MIB_IF_TABLE2 = std::ptr::null_mut();
        if GetIfTable2(&mut table).is_ok() && !table.is_null() {
            let n = (*table).NumEntries as usize;
            let rows = (*table).Table.as_ptr();
            for i in 0..n {
                let row = &*rows.add(i);
                // 24 = IF_TYPE_SOFTWARE_LOOPBACK
                if row.Type != 24 && row.OperStatus.0 == 1 {
                    rx = rx.wrapping_add(row.InOctets);
                    tx = tx.wrapping_add(row.OutOctets);
                }
            }
            FreeMibTable(table as *const _);
        }
    }
    (rx, tx)
}

/// One snapshot, as the JSON object the dashboard page expects.
pub fn sample() -> serde_json::Value {
    let mut cpu_pct = 0.0f64;
    let mut net_rx_kbps = 0.0f64;
    let mut net_tx_kbps = 0.0f64;

    let (mut idle_ft, mut kernel_ft, mut user_ft) =
        (FILETIME::default(), FILETIME::default(), FILETIME::default());
    let _ = unsafe { GetSystemTimes(Some(&mut idle_ft), Some(&mut kernel_ft), Some(&mut user_ft)) };
    let (idle, kernel, user) = (ft(idle_ft), ft(kernel_ft), ft(user_ft));
    let (rx, tx) = net_totals();
    let now_us = crate::util::perf_micros();

    PREV.with(|cell| {
        let mut slot = cell.borrow_mut();
        if let Some(p) = &*slot {
            // Kernel time includes idle time, so busy = (k + u) - idle.
            let total = (kernel.wrapping_sub(p.kernel)) + (user.wrapping_sub(p.user));
            let idled = idle.wrapping_sub(p.idle);
            if total > 0 {
                cpu_pct = 100.0 * (total.saturating_sub(idled)) as f64 / total as f64;
            }
            let dt_s = now_us.saturating_sub(p.at_us) as f64 / 1_000_000.0;
            if dt_s > 0.2 {
                net_rx_kbps = rx.wrapping_sub(p.net_rx) as f64 / dt_s / 1024.0;
                net_tx_kbps = tx.wrapping_sub(p.net_tx) as f64 / dt_s / 1024.0;
            }
        }
        *slot = Some(Prev {
            idle,
            kernel,
            user,
            net_rx: rx,
            net_tx: tx,
            at_us: now_us,
        });
    });

    let mut mem = MEMORYSTATUSEX {
        dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };
    let _ = unsafe { GlobalMemoryStatusEx(&mut mem) };
    let mem_total_mb = mem.ullTotalPhys / (1024 * 1024);
    let mem_used_mb = mem.ullTotalPhys.saturating_sub(mem.ullAvailPhys) / (1024 * 1024);

    let mut disks = Vec::new();
    let mask = unsafe { GetLogicalDrives() };
    for i in 0..26u32 {
        if mask & (1 << i) == 0 {
            continue;
        }
        let letter = (b'A' + i as u8) as char;
        let root: Vec<u16> = format!("{}:\\", letter).encode_utf16().chain([0]).collect();
        let root_p = windows::core::PCWSTR(root.as_ptr());
        // 3 = DRIVE_FIXED; skip removable/network/cd drives.
        if unsafe { GetDriveTypeW(root_p) } != 3 {
            continue;
        }
        let (mut free, mut total) = (0u64, 0u64);
        if unsafe { GetDiskFreeSpaceExW(root_p, None, Some(&mut total), Some(&mut free)) }.is_ok()
            && total > 0
        {
            disks.push(serde_json::json!({
                "letter": letter.to_string(),
                "free_gb": free as f64 / (1024.0 * 1024.0 * 1024.0),
                "total_gb": total as f64 / (1024.0 * 1024.0 * 1024.0),
            }));
        }
    }

    let mut power = SYSTEM_POWER_STATUS::default();
    let _ = unsafe { GetSystemPowerStatus(&mut power) };
    // 255 = unknown, and desktops report 128 (no system battery).
    let battery_pct = if power.BatteryLifePercent <= 100 && power.BatteryFlag != 128 {
        serde_json::json!(power.BatteryLifePercent)
    } else {
        serde_json::Value::Null
    };

    serde_json::json!({
        "type": "stats",
        "cpu_pct": (cpu_pct * 10.0).round() / 10.0,
        "mem_used_mb": mem_used_mb,
        "mem_total_mb": mem_total_mb,
        "disk": disks,
        "net_rx_kbps": (net_rx_kbps * 10.0).round() / 10.0,
        "net_tx_kbps": (net_tx_kbps * 10.0).round() / 10.0,
        "uptime_secs": unsafe { GetTickCount64() } / 1000,
        "battery_pct": battery_pct,
        "on_ac": power.ACLineStatus == 1,
    })
}
