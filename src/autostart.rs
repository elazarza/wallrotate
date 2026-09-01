//! Run-at-login.
//!
//! The registry Run key is the obvious mechanism, but the shell deliberately
//! defers those entries and staggers them behind everything else in the startup
//! queue, which can push a launch minutes past sign-in. A logon scheduled task
//! fires as soon as the session exists, so that is the preferred path; the Run
//! key stays as a fallback for when task creation is not permitted.
//!
//! Only one of the two is ever active, so nothing starts twice.

use crate::util::wide;
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::{Command, Output};
use windows::core::PCWSTR;
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
    HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ,
};

const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const VALUE_NAME: &str = "WallRotate";
const TASK_NAME: &str = "WallRotate";
/// Keep schtasks from flashing a console window.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Off,
    LogonTask,
    RunKey,
}

impl Mode {
    pub fn label(self) -> &'static str {
        match self {
            Mode::Off => "off",
            Mode::LogonTask => "logon task",
            Mode::RunKey => "Run key",
        }
    }
}

pub fn status() -> Mode {
    if task_exists() {
        Mode::LogonTask
    } else if run_key_set() {
        Mode::RunKey
    } else {
        Mode::Off
    }
}

pub fn is_enabled() -> bool {
    status() != Mode::Off
}

pub fn set(enabled: bool, exe: &Path) -> bool {
    if !enabled {
        let removed_task = delete_task();
        let removed_key = set_run_key(false, exe);
        return removed_task || removed_key;
    }
    if create_task(exe) {
        // Never leave both armed.
        let _ = set_run_key(false, exe);
        crate::log::line("autostart: logon task created");
        return true;
    }
    crate::log::line("autostart: task creation refused, falling back to Run key");
    set_run_key(true, exe)
}

// ------------------------------------------------------------ schtasks ---

fn schtasks(args: &[&str]) -> Option<Output> {
    Command::new("schtasks.exe")
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()
}

fn task_exists() -> bool {
    schtasks(&["/Query", "/TN", TASK_NAME])
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn delete_task() -> bool {
    schtasks(&["/Delete", "/TN", TASK_NAME, "/F"])
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn create_task(exe: &Path) -> bool {
    let xml_path = std::env::temp_dir().join("wallrotate-logon-task.xml");
    if write_utf16(&xml_path, &task_xml(exe)).is_err() {
        return false;
    }
    let xml_arg = xml_path.to_string_lossy().to_string();
    let ok = schtasks(&["/Create", "/TN", TASK_NAME, "/XML", &xml_arg, "/F"])
        .map(|o| o.status.success())
        .unwrap_or(false);
    let _ = std::fs::remove_file(&xml_path);
    ok
}

/// schtasks only accepts UTF-16 task definitions.
fn write_utf16(path: &Path, text: &str) -> std::io::Result<()> {
    let mut bytes = vec![0xFF, 0xFE];
    for unit in text.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    std::fs::write(path, bytes)
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn current_user() -> String {
    let name = std::env::var("USERNAME").unwrap_or_default();
    let domain = std::env::var("USERDOMAIN")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_default();
    if domain.is_empty() {
        name
    } else {
        format!("{}\\{}", domain, name)
    }
}

fn task_xml(exe: &Path) -> String {
    let user = xml_escape(&current_user());
    let command = xml_escape(&exe.display().to_string());
    // PT0S execution limit means "no limit" -- the command-line form of
    // schtasks defaults to 72 hours, which would kill a tray app after 3 days.
    // The short delay just lets the shell finish coming up first.
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>Starts WallRotate when you sign in.</Description>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <UserId>{user}</UserId>
      <Delay>PT5S</Delay>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{user}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>false</AllowHardTerminate>
    <StartWhenAvailable>true</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <IdleSettings>
      <StopOnIdleEnd>false</StopOnIdleEnd>
      <RestartOnIdle>false</RestartOnIdle>
    </IdleSettings>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <WakeToRun>false</WakeToRun>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>7</Priority>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{command}</Command>
    </Exec>
  </Actions>
</Task>"#
    )
}

// ------------------------------------------------------------ Run key ---

fn run_key_set() -> bool {
    unsafe {
        let sub = wide(RUN_KEY);
        let mut key = HKEY::default();
        if RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(sub.as_ptr()),
            0,
            KEY_QUERY_VALUE,
            &mut key,
        ) != ERROR_SUCCESS
        {
            return false;
        }
        let name = wide(VALUE_NAME);
        let mut size: u32 = 0;
        let status = RegQueryValueExW(
            key,
            PCWSTR(name.as_ptr()),
            None,
            None,
            None,
            Some(&mut size),
        );
        let _ = RegCloseKey(key);
        status == ERROR_SUCCESS
    }
}

fn set_run_key(enabled: bool, exe: &Path) -> bool {
    unsafe {
        let sub = wide(RUN_KEY);
        let mut key = HKEY::default();
        let status = RegCreateKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(sub.as_ptr()),
            0,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE | KEY_QUERY_VALUE,
            None,
            &mut key,
            None,
        );
        if status != ERROR_SUCCESS {
            return false;
        }
        let name = wide(VALUE_NAME);
        let ok = if enabled {
            // Quote the path so a space in it does not split the command line.
            let command = format!("\"{}\"", exe.display());
            let data = wide(&command);
            let bytes = std::slice::from_raw_parts(
                data.as_ptr() as *const u8,
                data.len() * std::mem::size_of::<u16>(),
            );
            RegSetValueExW(key, PCWSTR(name.as_ptr()), 0, REG_SZ, Some(bytes)) == ERROR_SUCCESS
        } else {
            let status = RegDeleteValueW(key, PCWSTR(name.as_ptr()));
            // Already absent counts as success.
            status == ERROR_SUCCESS || !run_key_set()
        };
        let _ = RegCloseKey(key);
        ok
    }
}
