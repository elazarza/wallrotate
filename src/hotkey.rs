//! Parsing of hotkey specs like "win+ctrl+r" into RegisterHotKey arguments.

use windows::Win32::UI::Input::KeyboardAndMouse::{
    HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN,
};

pub struct Hotkey {
    pub modifiers: HOT_KEY_MODIFIERS,
    pub vk: u32,
    /// Pretty form for menus, e.g. "Win+Ctrl+R".
    pub label: String,
}

pub fn parse(spec: &str) -> Option<Hotkey> {
    let mut modifiers = MOD_NOREPEAT;
    let mut vk: Option<u32> = None;
    let mut parts: Vec<String> = Vec::new();

    for raw in spec.split('+') {
        let token = raw.trim().to_ascii_lowercase();
        if token.is_empty() {
            continue;
        }
        match token.as_str() {
            "win" | "super" | "meta" | "cmd" => {
                modifiers |= MOD_WIN;
                parts.push(String::from("Win"));
            }
            "ctrl" | "control" => {
                modifiers |= MOD_CONTROL;
                parts.push(String::from("Ctrl"));
            }
            "alt" => {
                modifiers |= MOD_ALT;
                parts.push(String::from("Alt"));
            }
            "shift" => {
                modifiers |= MOD_SHIFT;
                parts.push(String::from("Shift"));
            }
            other => {
                let (code, pretty) = key_code(other)?;
                vk = Some(code);
                parts.push(pretty);
            }
        }
    }

    Some(Hotkey {
        modifiers,
        vk: vk?,
        label: parts.join("+"),
    })
}

fn key_code(token: &str) -> Option<(u32, String)> {
    // Single letter or digit.
    if token.len() == 1 {
        let c = token.chars().next()?;
        if c.is_ascii_alphanumeric() {
            let up = c.to_ascii_uppercase();
            return Some((up as u32, up.to_string()));
        }
    }
    // Function keys.
    if let Some(rest) = token.strip_prefix('f') {
        if let Ok(n) = rest.parse::<u32>() {
            if (1..=24).contains(&n) {
                return Some((0x70 + n - 1, format!("F{}", n)));
            }
        }
    }
    let named = match token {
        "space" => (0x20, "Space"),
        "enter" | "return" => (0x0D, "Enter"),
        "tab" => (0x09, "Tab"),
        "esc" | "escape" => (0x1B, "Esc"),
        "left" => (0x25, "Left"),
        "up" => (0x26, "Up"),
        "right" => (0x27, "Right"),
        "down" => (0x28, "Down"),
        "home" => (0x24, "Home"),
        "end" => (0x23, "End"),
        "pageup" | "pgup" => (0x21, "PageUp"),
        "pagedown" | "pgdn" => (0x22, "PageDown"),
        "insert" | "ins" => (0x2D, "Insert"),
        "delete" | "del" => (0x2E, "Delete"),
        _ => return None,
    };
    Some((named.0, String::from(named.1)))
}
