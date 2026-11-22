//! Reads controller input in real time:
//! - Windows: Windows.Gaming.Input.RawGameController
//! - Linux:   /dev/input/js* (kernel joydev) + sysfs battery & IDs (+ WebView HTML5 Gamepad fallback)
//! - macOS:   IOHIDDevice detection + WebView HTML5 Gamepad API

use serde::Serialize;

/// One reading, as sent to the page: buttons, hat switch, axes (0..1).
#[derive(Serialize, PartialEq, Clone)]
pub struct Reading {
    pub b: Vec<bool>,
    pub h: i32,
    pub a: Vec<f64>,
}

#[cfg(windows)]
mod imp {
    use super::Reading;
    use windows::{
        Foundation::EventHandler,
        Gaming::Input::{GameControllerSwitchPosition, RawGameController},
    };

    use crate::loc::{t, tf};

    const NINTENDO_VID: u16 = 0x057E;

    pub struct Input {
        controller: Option<RawGameController>,
        buttons: Vec<bool>,
        switches: Vec<GameControllerSwitchPosition>,
        axes: Vec<f64>,
        last: Option<Reading>,
    }

    impl Input {
        /// `on_change` is called (from any thread) when a controller appears or disappears.
        pub fn new(on_change: impl Fn() + Send + Sync + Clone + 'static) -> Self {
            let added = on_change.clone();
            let _ = RawGameController::RawGameControllerAdded(&EventHandler::<RawGameController>::new(move |_, _| {
                added();
                Ok(())
            }));
            let _ = RawGameController::RawGameControllerRemoved(&EventHandler::<RawGameController>::new(move |_, _| {
                on_change();
                Ok(())
            }));
            let mut me = Input { controller: None, buttons: vec![], switches: vec![], axes: vec![], last: None };
            me.refresh();
            me
        }

        /// Picks the controller to show. Returns true when it changed.
        pub fn refresh(&mut self) -> bool {
            let all: Vec<RawGameController> = RawGameController::RawGameControllers()
                .map(|v| (0..v.Size().unwrap_or(0)).filter_map(|i| v.GetAt(i).ok()).collect())
                .unwrap_or_default();
            let c = all
                .iter()
                .find(|c| c.HardwareVendorId().ok() == Some(NINTENDO_VID))
                .or_else(|| all.iter().find(|c| c.IsWireless().unwrap_or(false)))
                .cloned();
            if c == self.controller {
                return false;
            }
            if let Some(c) = &c {
                self.buttons = vec![false; c.ButtonCount().unwrap_or(0).max(0) as usize];
                self.switches = vec![GameControllerSwitchPosition::Center; c.SwitchCount().unwrap_or(0).max(0) as usize];
                self.axes = vec![0.0; c.AxisCount().unwrap_or(0).max(0) as usize];
            }
            self.controller = c;
            self.last = None;
            true
        }

        pub fn present(&self) -> bool {
            self.controller.is_some()
        }

        /// The current reading, only when it differs from the last one returned.
        pub fn read_changed(&mut self) -> Option<Reading> {
            let c = self.controller.as_ref()?;
            c.GetCurrentReading(&mut self.buttons, &mut self.switches, &mut self.axes).ok()?;
            let r = Reading {
                b: self.buttons.clone(),
                h: self.switches.first().map(|s| s.0).unwrap_or(0),
                a: self.axes.clone(),
            };
            if self.last.as_ref() == Some(&r) {
                return None;
            }
            self.last = Some(r.clone());
            Some(r)
        }

        pub fn forget_last(&mut self) {
            self.last = None;
        }

        pub fn describe(&self) -> Option<String> {
            let c = self.controller.as_ref()?;
            let mut battery = String::new();
            if let Ok(report) = c.TryGetBatteryReport() {
                let rem = report.RemainingCapacityInMilliwattHours().and_then(|v| v.Value());
                let full = report.FullChargeCapacityInMilliwattHours().and_then(|v| v.Value());
                if let (Ok(rem), Ok(full)) = (rem, full) {
                    if full > 0 {
                        battery = format!(" · {}", tf("battery", &[&(rem * 100 / full)]));
                    }
                }
            }
            let wireless = t(if c.IsWireless().unwrap_or(false) { "wireless" } else { "wired" });
            Some(format!(
                "ID {:04X}:{:04X} · {wireless}{battery}",
                c.HardwareVendorId().unwrap_or(0),
                c.HardwareProductId().unwrap_or(0)
            ))
        }
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use super::Reading;
    use crate::loc::{t, tf};
    use std::fs;
    use std::os::unix::io::RawFd;
    use std::path::PathBuf;

    pub struct Input {
        dev_name: Option<String>,
        is_evdev: bool,
        fd: Option<RawFd>,
        vid: u16,
        pid: u16,
        buttons: Vec<bool>,
        axes: Vec<f64>,
        dpad_x: i32,
        dpad_y: i32,
        last: Option<Reading>,
    }

    impl Input {
        pub fn new(on_change: impl Fn() + Send + Sync + Clone + 'static) -> Self {
            std::thread::spawn(move || {
                let mut prev = find_best_device().map(|(n, _, _, _)| n);
                loop {
                    std::thread::sleep(std::time::Duration::from_secs(2));
                    let cur = find_best_device().map(|(n, _, _, _)| n);
                    if cur != prev {
                        prev = cur;
                        on_change();
                    }
                }
            });
            let mut me = Input {
                dev_name: None,
                is_evdev: false,
                fd: None,
                vid: 0x057E,
                pid: 0x2009,
                buttons: vec![false; 16],
                axes: vec![0.5; 4],
                dpad_x: 0,
                dpad_y: 0,
                last: None,
            };
            me.refresh();
            me
        }

        pub fn refresh(&mut self) -> bool {
            let found = find_best_device();
            let new_name = found.as_ref().map(|(n, _, _, _)| n.clone());
            if new_name == self.dev_name {
                return false;
            }
            if let Some(fd) = self.fd.take() {
                unsafe {
                    libc::close(fd);
                }
            }
            self.dev_name = new_name;
            self.buttons = vec![false; 16];
            self.axes = vec![0.5; 4];
            self.dpad_x = 0;
            self.dpad_y = 0;
            self.last = None;
            if let Some((name, is_evdev, vid, pid)) = found {
                self.is_evdev = is_evdev;
                self.vid = vid;
                self.pid = pid;
                let path = format!("/dev/input/{name}\0");
                let fd = unsafe { libc::open(path.as_ptr() as *const libc::c_char, libc::O_RDONLY | libc::O_NONBLOCK) };
                if fd >= 0 {
                    self.fd = Some(fd);
                }
            }
            true
        }

        pub fn present(&self) -> bool {
            self.dev_name.is_some()
        }

        pub fn read_changed(&mut self) -> Option<Reading> {
            let fd = self.fd?;
            if self.is_evdev {
                #[repr(C)]
                struct InputEvent {
                    tv_sec: libc::time_t,
                    tv_usec: libc::suseconds_t,
                    type_: u16,
                    code: u16,
                    value: i32,
                }
                loop {
                    let mut ev = InputEvent { tv_sec: 0, tv_usec: 0, type_: 0, code: 0, value: 0 };
                    let n = unsafe { libc::read(fd, &mut ev as *mut _ as *mut libc::c_void, std::mem::size_of::<InputEvent>()) };
                    if n != std::mem::size_of::<InputEvent>() as isize {
                        break;
                    }
                    if ev.type_ == 0x01 {
                        // EV_KEY -> map standard Linux gamepad / hid-nintendo button codes to Pro Controller indices
                        let idx = match ev.code {
                            0x130 => Some(0),  // BTN_SOUTH (B)
                            0x131 => Some(1),  // BTN_EAST  (A)
                            0x134 => Some(2),  // BTN_WEST  (Y)
                            0x133 => Some(3),  // BTN_NORTH (X)
                            0x136 => Some(4),  // BTN_TL    (L)
                            0x137 => Some(5),  // BTN_TR    (R)
                            0x138 => Some(6),  // BTN_TL2   (ZL)
                            0x139 => Some(7),  // BTN_TR2   (ZR)
                            0x13a => Some(8),  // BTN_SELECT (-)
                            0x13b => Some(9),  // BTN_START  (+)
                            0x13d => Some(10), // BTN_THUMBL (LS)
                            0x13e => Some(11), // BTN_THUMBR (RS)
                            0x13c => Some(12), // BTN_MODE   (Home)
                            0x135 => Some(13), // BTN_Z      (Capture)
                            _ => None,
                        };
                        if let Some(i) = idx {
                            self.buttons[i] = ev.value != 0;
                        }
                    } else if ev.type_ == 0x03 {
                        // EV_ABS
                        match ev.code {
                            0x00 => self.axes[0] = ((ev.value as f64) / 32767.0 + 1.0).clamp(0.0, 2.0) * 0.5,
                            0x01 => self.axes[1] = ((ev.value as f64) / 32767.0 + 1.0).clamp(0.0, 2.0) * 0.5,
                            0x03 => self.axes[2] = ((ev.value as f64) / 32767.0 + 1.0).clamp(0.0, 2.0) * 0.5,
                            0x04 => self.axes[3] = ((ev.value as f64) / 32767.0 + 1.0).clamp(0.0, 2.0) * 0.5,
                            0x10 => self.dpad_x = ev.value,
                            0x11 => self.dpad_y = ev.value,
                            _ => {}
                        }
                    }
                }
            } else {
                #[repr(C)]
                struct JsEvent {
                    time: u32,
                    value: i16,
                    type_: u8,
                    number: u8,
                }
                loop {
                    let mut ev = JsEvent { time: 0, value: 0, type_: 0, number: 0 };
                    let n = unsafe { libc::read(fd, &mut ev as *mut _ as *mut libc::c_void, std::mem::size_of::<JsEvent>()) };
                    if n != std::mem::size_of::<JsEvent>() as isize {
                        break;
                    }
                    let kind = ev.type_ & !0x80;
                    if kind == 1 {
                        let idx = ev.number as usize;
                        if idx < self.buttons.len() {
                            self.buttons[idx] = ev.value != 0;
                        }
                    } else if kind == 2 {
                        let idx = ev.number as usize;
                        if idx < 4 {
                            self.axes[idx] = ((ev.value as f64) / 32767.0 + 1.0) * 0.5;
                        } else if idx == 4 {
                            self.dpad_x = ev.value as i32;
                        } else if idx == 5 {
                            self.dpad_y = ev.value as i32;
                        }
                    }
                }
            }
            let h = match (self.dpad_x.signum(), self.dpad_y.signum()) {
                (0, -1) => 1,
                (1, -1) => 2,
                (1, 0) => 3,
                (1, 1) => 4,
                (0, 1) => 5,
                (-1, 1) => 6,
                (-1, 0) => 7,
                (-1, -1) => 8,
                _ => 0,
            };
            let r = Reading { b: self.buttons.clone(), h, a: self.axes.clone() };
            if self.last.as_ref() == Some(&r) {
                return None;
            }
            self.last = Some(r.clone());
            Some(r)
        }

        pub fn forget_last(&mut self) {
            self.last = None;
        }

        pub fn describe(&self) -> Option<String> {
            self.dev_name.as_ref()?;
            let mut battery = String::new();
            if let Ok(entries) = fs::read_dir("/sys/class/power_supply") {
                for e in entries.flatten() {
                    let n = e.file_name().to_string_lossy().to_lowercase();
                    if n.contains("nintendo") || n.contains("hid") {
                        if let Ok(cap) = fs::read_to_string(e.path().join("capacity")) {
                            if let Ok(pct) = cap.trim().parse::<i32>() {
                                battery = format!(" · {}", tf("battery", &[&pct]));
                                break;
                            }
                        }
                    }
                }
            }
            let wireless = t("wireless");
            Some(format!("ID {:04X}:{:04X} · {wireless}{battery}", self.vid, self.pid))
        }
    }

    impl Drop for Input {
        fn drop(&mut self) {
            if let Some(fd) = self.fd.take() {
                unsafe {
                    libc::close(fd);
                }
            }
        }
    }

    /// Finds the best input device: prefers joydev (`jsN`) with Nintendo VID, then evdev (`eventN`)
    /// with Nintendo VID ( excluding IMU/Motion Sensors sub-devices), then any `jsN`.
    fn find_best_device() -> Option<(String, bool, u16, u16)> {
        let mut js_nintendo = None;
        let mut ev_nintendo = None;
        let mut js_fallback = None;
        for e in fs::read_dir("/sys/class/input").ok()?.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            let is_js = name.starts_with("js");
            let is_ev = name.starts_with("event");
            if !is_js && !is_ev {
                continue;
            }
            let dev_dir: PathBuf = e.path().join("device");
            let dev_title = fs::read_to_string(dev_dir.join("name")).unwrap_or_default().to_lowercase();
            if dev_title.contains("imu") || dev_title.contains("motion") {
                continue;
            }
            let vid = fs::read_to_string(dev_dir.join("id/vendor"))
                .ok()
                .and_then(|s| u16::from_str_radix(s.trim(), 16).ok())
                .unwrap_or(0);
            let pid = fs::read_to_string(dev_dir.join("id/product"))
                .ok()
                .and_then(|s| u16::from_str_radix(s.trim(), 16).ok())
                .unwrap_or(0);
            if is_js && vid == 0x057E {
                js_nintendo = Some((name, false, vid, pid));
                break;
            } else if is_ev && (vid == 0x057E || dev_title.contains("pro controller")) && ev_nintendo.is_none() {
                ev_nintendo = Some((name, true, if vid != 0 { vid } else { 0x057E }, if pid != 0 { pid } else { 0x2009 }));
            } else if is_js && js_fallback.is_none() {
                js_fallback = Some((name, false, vid, pid));
            }
        }
        js_nintendo.or(ev_nintendo).or(js_fallback)
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use super::Reading;
    use crate::loc::t;

    pub struct Input {
        present: bool,
    }

    impl Input {
        pub fn new(on_change: impl Fn() + Send + Sync + Clone + 'static) -> Self {
            std::thread::spawn(move || {
                let mut prev = detect_nintendo_hid();
                loop {
                    std::thread::sleep(std::time::Duration::from_secs(2));
                    let cur = detect_nintendo_hid();
                    if cur != prev {
                        prev = cur;
                        on_change();
                    }
                }
            });
            Input { present: detect_nintendo_hid() }
        }

        pub fn refresh(&mut self) -> bool {
            let cur = detect_nintendo_hid();
            let changed = cur != self.present;
            self.present = cur;
            changed
        }

        pub fn present(&self) -> bool {
            self.present
        }

        /// On macOS, live button/axis readings are streamed via the WebView's HTML5 Gamepad API.
        pub fn read_changed(&mut self) -> Option<Reading> {
            None
        }

        pub fn forget_last(&mut self) {}

        pub fn describe(&self) -> Option<String> {
            self.present.then(|| format!("ID 057E:2009 · {}", t("wireless")))
        }
    }

    fn detect_nintendo_hid() -> bool {
        std::process::Command::new("ioreg")
            .args(["-r", "-c", "IOHIDDevice", "-d", "1"])
            .output()
            .ok()
            .map(|o| {
                let s = String::from_utf8_lossy(&o.stdout);
                s.contains("Pro Controller") || s.contains("\"VendorID\" = 1406")
            })
            .unwrap_or(false)
    }
}

pub use imp::Input;
