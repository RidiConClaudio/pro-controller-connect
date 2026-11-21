//! Localization. "auto" follows the Windows display language; English is the fallback.
//! The texts live in loc.json, shared with the window's page.
//! To add a language: add a table to loc.json and an entry in `LANGUAGES`.

use std::{collections::HashMap, fmt::Display, sync::OnceLock};

pub const LANGUAGES: [(&str, &str); 7] =
    [("auto", "Auto"), ("en", "EN"), ("it", "IT"), ("es", "ES"), ("fr", "FR"), ("de", "DE"), ("pt", "PT")];

type Table = HashMap<String, String>;

fn tables() -> &'static HashMap<String, Table> {
    static T: OnceLock<HashMap<String, Table>> = OnceLock::new();
    T.get_or_init(|| serde_json::from_str(include_str!("loc.json")).expect("loc.json"))
}

static CODE: OnceLock<String> = OnceLock::new();

/// Picks the language once at startup (a language change restarts the app).
pub fn init(setting: &str) {
    let mut code = setting.to_ascii_lowercase();
    if code == "auto" || !tables().contains_key(&code) {
        code = windows_ui_language();
    }
    if !tables().contains_key(&code) {
        code = "en".into();
    }
    let _ = CODE.set(code);
}

fn code() -> &'static str {
    CODE.get().map(String::as_str).unwrap_or("en")
}

pub fn t(key: &str) -> String {
    tables()
        .get(code())
        .and_then(|t| t.get(key))
        .or_else(|| tables()["en"].get(key))
        .cloned()
        .unwrap_or_else(|| key.to_string())
}

/// Like `string.Format`: replaces {0}, {1}… with the arguments.
pub fn tf(key: &str, args: &[&dyn Display]) -> String {
    let mut s = t(key);
    for (i, a) in args.iter().enumerate() {
        s = s.replace(&format!("{{{i}}}"), &a.to_string());
    }
    s
}

/// The current table merged over English, for the page.
pub fn page_table() -> serde_json::Value {
    let mut merged = tables()["en"].clone();
    if let Some(cur) = tables().get(code()) {
        merged.extend(cur.iter().map(|(k, v)| (k.clone(), v.clone())));
    }
    serde_json::to_value(merged).unwrap_or_default()
}

/// Two-letter code of the Windows display language (like CultureInfo.CurrentUICulture).
fn windows_ui_language() -> String {
    use windows::Win32::Globalization::{GetUserDefaultUILanguage, LCIDToLocaleName};
    let mut buf = [0u16; 85];
    let n = unsafe { LCIDToLocaleName(GetUserDefaultUILanguage() as u32, Some(&mut buf), 0) };
    if n <= 1 {
        return String::new();
    }
    let name = String::from_utf16_lossy(&buf[..(n - 1) as usize]);
    name.split('-').next().unwrap_or("").to_ascii_lowercase()
}
