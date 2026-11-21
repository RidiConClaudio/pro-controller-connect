//! Settings (same settings.json as the .NET version), start with Windows and the activity log file.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct Settings {
    /// Automatically repairs and re-pairs the controller.
    pub auto_mode: bool,
    pub start_with_windows: bool,
    pub start_minimized: bool,
    pub notifications: bool,
    /// Device names to manage, separated by ';'.
    pub name_filter: String,
    /// Seconds to wait before removing a "stuck" pairing.
    pub grace_seconds: i32,
    /// "auto" (Windows language) or a code such as "en", "it".
    pub language: String,
    pub hide_tip_shown: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            auto_mode: true,
            start_with_windows: false,
            start_minimized: true,
            notifications: true,
            name_filter: "Pro Controller".into(),
            grace_seconds: 15,
            language: "auto".into(),
            hide_tip_shown: false,
        }
    }
}

/// PCC_DATA lets you use another data folder, for example for tests:
/// then the registry ("Start with Windows") is never touched.
fn test_folder() -> Option<PathBuf> {
    std::env::var_os("PCC_DATA").filter(|d| !d.is_empty()).map(PathBuf::from)
}

pub fn folder() -> PathBuf {
    test_folder()
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("APPDATA").unwrap_or_default()).join("ProControllerConnect"))
}

fn file_path() -> PathBuf {
    folder().join("settings.json")
}

impl Settings {
    pub fn load() -> Self {
        std::fs::read_to_string(file_path())
            .ok()
            .and_then(|s| serde_json::from_str(s.trim_start_matches('\u{feff}')).ok())
            .unwrap_or_default() // missing or corrupted file: defaults
    }

    pub fn save(&self) {
        let _ = std::fs::create_dir_all(folder());
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(file_path(), json);
        }
    }

    pub fn matches(&self, device_name: &str) -> bool {
        name_matches(&self.name_filter, device_name)
    }

    pub fn apply_startup(&self) {
        const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
        const VALUE: &str = "ProControllerConnect";
        if test_folder().is_some() {
            return;
        }
        let Ok(key) = windows_registry::CURRENT_USER.create(RUN_KEY) else { return };
        match std::env::current_exe() {
            Ok(exe) if self.start_with_windows => {
                let args = if self.start_minimized { " --minimized" } else { "" };
                let _ = key.set_string(VALUE, format!("\"{}\"{args}", exe.display()));
            }
            _ => {
                let _ = key.remove_value(VALUE);
            }
        }
    }
}

/// Case-insensitive "contains" against each ';'-separated filter.
pub fn name_matches(filter: &str, device_name: &str) -> bool {
    let name = device_name.trim().to_lowercase();
    if name.is_empty() {
        return false;
    }
    filter.split(';').map(str::trim).filter(|f| !f.is_empty()).any(|f| name.contains(&f.to_lowercase()))
}

/// Local time as "yyyy-MM-dd HH:mm:ss" (full) or "HH:mm:ss".
pub fn now_text(full: bool) -> String {
    let t = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    if full {
        format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond)
    } else {
        format!("{:02}:{:02}:{:02}", t.wHour, t.wMinute, t.wSecond)
    }
}

pub fn write_log_file(message: &str) {
    let _ = std::fs::create_dir_all(folder());
    let file = folder().join("log.txt");
    if std::fs::metadata(&file).map(|m| m.len() > 512 * 1024).unwrap_or(false) {
        let _ = std::fs::remove_file(&file);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&file) {
        use std::io::Write;
        let _ = writeln!(f, "{}  {message}", now_text(true));
    }
}
