//! Pro Controller Connect: automatic Bluetooth reconnection for the Pro Controller.
//!
//! The service (Bluetooth, WinRT) runs on its own thread; the notification area icon is plain Win32.
//! The window is an HTML page in WebView2 that exists only while it is open: in the notification
//! area the app keeps no browser engine in memory.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod input;
mod loc;
mod service;
mod settings;
mod tray;

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use serde_json::{json, Value};
#[cfg(target_os = "linux")]
use tao::platform::unix::WindowExtUnix;
#[cfg(windows)]
use tao::platform::windows::{IconExtWindows, WindowBuilderExtWindows, WindowExtWindows};
#[cfg(windows)]
use tao::window::Icon;
use tao::{
    dpi::{LogicalSize, PhysicalPosition},
    event::{Event, WindowEvent},
    event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy},
    window::{Theme, Window, WindowBuilder},
};
#[cfg(windows)]
use windows::{
    core::{w, HSTRING},
    Win32::{
        Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE, HWND, WAIT_OBJECT_0},
        Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_BORDER_COLOR, DWMWA_USE_IMMERSIVE_DARK_MODE, DWMWA_WINDOW_CORNER_PREFERENCE},
        System::Threading::{CreateEventW, CreateMutexW, OpenEventW, SetEvent, WaitForSingleObject, EVENT_MODIFY_STATE, INFINITE},
        UI::{
            Shell::ShellExecuteW,
            WindowsAndMessaging::{AllowSetForegroundWindow, ASFW_ANY, SW_SHOWNORMAL},
        },
    },
};
#[cfg(target_os = "linux")]
use wry::WebViewBuilderExtUnix;
#[cfg(windows)]
use wry::WebViewBuilderExtWindows;
use wry::{WebContext, WebView, WebViewBuilder};

use crate::{
    input::Input,
    loc::t,
    service::{LinkState, Service, ServiceEvent, Status},
    settings::Settings,
    tray::{MenuItem, Tray, TrayEvent},
};

const PAGE: &str = include_str!("../ui/index.html");
const WIDTH: f64 = 440.0;
const BG: (u8, u8, u8, u8) = (0x0E, 0x10, 0x14, 0xFF);
const GRAY: u32 = 0x8B93A3;

enum UserEvent {
    Service(ServiceEvent),
    Ipc(String),
    Tray(TrayEvent),
    /// Another copy was started: bring this one to the front
    Show,
    /// A game controller appeared or disappeared
    Devices,
}

fn main() {
    #[cfg(target_os = "linux")]
    if std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_none() {
        std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
    }

    std::panic::set_hook(Box::new(|info| settings::write_log_file(&format!("panic: {info}"))));

    // Single instance: if already running, bring it to the front.
    let Some(mutex) = single_instance() else { return };

    let settings = Settings::load();
    loc::init(&settings.language);
    settings.apply_startup(); // refresh the registry path in case the exe was moved
    let minimized = std::env::args().skip(1).any(|a| a.eq_ignore_ascii_case("--minimized"));
    let settings = Arc::new(Mutex::new(settings));

    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();
    listen_for_show(&mutex, proxy.clone());

    let p = proxy.clone();
    let service = Service::new(settings.clone(), move |e| {
        let _ = p.send_event(UserEvent::Service(e));
    });

    let wb = WindowBuilder::new()
        .with_title("Pro Controller Connect")
        .with_decorations(false)
        .with_resizable(false)
        .with_inner_size(LogicalSize::new(WIDTH, 640.0))
        .with_theme(Some(Theme::Dark))
        .with_background_color(BG)
        .with_visible(false);
    #[cfg(windows)]
    let wb = wb
        .with_window_icon(Icon::from_resource(1, None).ok())
        .with_undecorated_shadow(true);
    let window = wb.build(&event_loop).expect("window");
    #[cfg(windows)]
    style_window(HWND(window.hwnd() as *mut _));

    let p = proxy.clone();
    let tray = Tray::new(GRAY, "Pro Controller Connect", move |e| {
        let _ = p.send_event(UserEvent::Tray(e));
    });
    let p = proxy.clone();
    let input = Input::new(move || {
        let _ = p.send_event(UserEvent::Devices);
    });

    let mut app = App {
        window,
        webview: None,
        context: WebContext::new(Some(settings::folder().join("WebView2"))),
        proxy,
        service: service.clone(),
        settings,
        tray,
        input,
        mutex: Some(mutex),
        status: service.current(),
        log: VecDeque::new(),
        pending_show: false,
        positioned: false,
        want_input: false,
    };
    app.on_status(app.status.clone());
    if !minimized {
        app.show();
    }
    service.start();

    event_loop.run(move |event, _, control_flow| {
        match event {
            Event::UserEvent(e) => app.on_user_event(e),
            Event::WindowEvent { event: WindowEvent::CloseRequested, .. } => app.hide(), // Alt+F4
            _ => {}
        }
        if app.mutex.is_none() {
            *control_flow = ControlFlow::Exit;
        } else if app.polling() {
            app.poll_input();
            *control_flow = ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(16));
        } else {
            *control_flow = ControlFlow::Wait;
        }
    });
}

struct App {
    window: Window,
    webview: Option<WebView>,
    context: WebContext,
    proxy: EventLoopProxy<UserEvent>,
    service: Service,
    settings: Arc<Mutex<Settings>>,
    tray: Tray,
    input: Input,
    /// None once the app is exiting
    mutex: Option<SingleGuard>,
    status: Status,
    /// Activity shown in the window, kept while the window is closed
    log: VecDeque<String>,
    pending_show: bool,
    positioned: bool,
    want_input: bool,
}

impl App {
    fn on_user_event(&mut self, e: UserEvent) {
        match e {
            UserEvent::Service(ServiceEvent::Status(s)) => self.on_status(s),
            UserEvent::Service(ServiceEvent::Log(msg)) => self.add_log(&msg, true),
            UserEvent::Service(ServiceEvent::Notify(title, text)) => {
                if self.settings.lock().unwrap().notifications {
                    self.tray.balloon(&title, &text);
                }
            }
            UserEvent::Ipc(body) => self.on_ipc(&body),
            UserEvent::Tray(TrayEvent::Open) | UserEvent::Show => self.show(),
            UserEvent::Tray(TrayEvent::Menu) => self.tray_menu(),
            UserEvent::Devices => {
                if self.input.refresh() {
                    self.send_device();
                }
            }
        }
    }

    // ───────────────────────────── Window ─────────────────────────────

    fn show(&mut self) {
        if self.webview.is_some() {
            self.window.set_visible(true);
            self.window.set_minimized(false);
            self.window.set_focus();
            return;
        }
        // The window appears when the page has measured its height
        self.pending_show = true;
        match self.create_webview() {
            Ok(wv) => self.webview = Some(wv),
            Err(e) => {
                settings::write_log_file(&format!("WebView2: {e}"));
                self.pending_show = false;
            }
        }
    }

    /// Back to the notification area (Windows) or minimized to Dock/Taskbar (Linux/macOS).
    fn hide(&mut self) {
        #[cfg(windows)]
        {
            self.window.set_visible(false);
            self.webview = None;
            self.want_input = false;
            self.pending_show = false;
            let mut s = self.settings.lock().unwrap();
            if !s.hide_tip_shown {
                self.tray.balloon("Pro Controller Connect", &t("hide_tip"));
                s.hide_tip_shown = true;
                s.save();
            }
        }
        #[cfg(not(windows))]
        {
            self.window.set_minimized(true);
        }
    }

    fn exit(&mut self) {
        self.service.stop();
        self.tray.remove();
        self.webview = None;
        self.mutex = None;
    }

    fn create_webview(&mut self) -> wry::Result<WebView> {
        let s = self.settings.lock().unwrap().clone();
        let boot = json!({
            "os": std::env::consts::OS,
            "loc": loc::page_table(),
            "langs": loc::LANGUAGES,
            "settings": settings_json(&s),
        });
        let boot = boot.to_string().replace("</", "<\\/");
        let html = PAGE.replace("/*BOOT*/", &format!("window.BOOT = {boot};"));
        let p = self.proxy.clone();
        let builder = WebViewBuilder::new_with_web_context(&mut self.context)
            .with_html(html)
            .with_background_color(BG)
            .with_devtools(cfg!(debug_assertions))
            .with_default_context_menus(false)
            .with_ipc_handler(move |req| {
                let _ = p.send_event(UserEvent::Ipc(req.body().clone()));
            })
            .with_navigation_handler(|uri| uri.starts_with("about:") || uri.starts_with("data:"));
        #[cfg(windows)]
        let builder = builder.with_browser_accelerator_keys(false);
        #[cfg(target_os = "linux")]
        if let Some(vbox) = self.window.default_vbox() {
            return builder.build_gtk(vbox);
        }
        builder.build(&self.window)
    }

    fn send(&self, msg: Value) {
        if let Some(wv) = &self.webview {
            let _ = wv.evaluate_script(&format!("window.onHost && onHost({msg})"));
        }
    }

    // ───────────────────────────── Page → app ─────────────────────────────

    fn on_ipc(&mut self, body: &str) {
        let Ok(v) = serde_json::from_str::<Value>(body) else { return };
        match v["t"].as_str().unwrap_or("") {
            "ready" => {
                self.send(json!({ "type": "status", "status": self.status }));
                self.send_device();
                self.send(json!({ "type": "logAll", "lines": self.log }));
            }
            "size" => {
                let h = v["h"].as_f64().unwrap_or(600.0).clamp(200.0, 2000.0);
                self.window.set_inner_size(LogicalSize::new(WIDTH, h));
                if self.pending_show {
                    self.pending_show = false;
                    if !self.positioned {
                        self.center();
                        self.positioned = true;
                    }
                    self.window.set_visible(true);
                    self.window.set_focus();
                }
            }
            "drag" => {
                let _ = self.window.drag_window();
            }
            "min" => self.window.set_minimized(true),
            "close" => self.hide(),
            "quit" => self.exit(),
            "main" => {
                if self.service.flow_running() {
                    self.service.cancel_flow();
                } else {
                    self.service.connect_now();
                }
            }
            "setting" => {
                let on = v["value"].as_bool().unwrap_or(false);
                let mut s = self.settings.lock().unwrap();
                let auto_before = s.auto_mode;
                match v["key"].as_str().unwrap_or("") {
                    "AutoMode" => s.auto_mode = on,
                    "StartWithWindows" => s.start_with_windows = on,
                    "StartMinimized" => s.start_minimized = on,
                    "Notifications" => s.notifications = on,
                    _ => return,
                }
                s.save();
                s.apply_startup();
                let changed = auto_before != s.auto_mode;
                drop(s);
                if changed {
                    self.service.on_auto_mode_changed();
                }
            }
            "lang" => {
                let code = v["code"].as_str().unwrap_or("auto").to_string();
                let mut s = self.settings.lock().unwrap();
                if s.language.eq_ignore_ascii_case(&code) {
                    return;
                }
                s.language = code;
                s.save();
                drop(s);
                self.restart();
            }
            "restartBt" => self.service.restart_bluetooth(),
            "btSettings" => open_bt_settings(),
            "input" => {
                self.want_input = v["on"].as_bool().unwrap_or(false);
                self.input.forget_last();
            }
            "log" => {
                if let Some(m) = v["msg"].as_str() {
                    self.add_log(m, false);
                }
            }
            _ => {}
        }
    }

    // ───────────────────────────── App → page / tray ─────────────────────────────

    fn on_status(&mut self, s: Status) {
        let color = match s.state {
            LinkState::Connected => 0x34C76F,
            LinkState::Searching | LinkState::Pairing | LinkState::Removing => 0x5B8CFF,
            LinkState::Disconnected => 0xFFB020,
            LinkState::BluetoothOff | LinkState::NoAdapter | LinkState::Error => 0xFF5A5F,
            _ => GRAY,
        };
        self.tray.set(color, &format!("Pro Controller Connect · {}", s.title));
        self.send(json!({ "type": "status", "status": s }));
        self.status = s;
    }

    fn add_log(&mut self, msg: &str, send: bool) {
        let line = format!("{}  {msg}", settings::now_text(false));
        if send {
            self.send(json!({ "type": "log", "line": line }));
        }
        self.log.push_back(line);
        while self.log.len() > 400 {
            self.log.pop_front();
        }
    }

    fn send_device(&self) {
        self.send(json!({ "type": "device", "text": self.input.describe(), "present": self.input.present() }));
    }

    fn polling(&self) -> bool {
        self.want_input && self.webview.is_some() && self.window.is_visible()
    }

    fn poll_input(&mut self) {
        if let Some(r) = self.input.read_changed() {
            self.send(json!({ "type": "input", "r": r }));
        }
    }

    fn tray_menu(&mut self) {
        let auto = self.settings.lock().unwrap().auto_mode;
        let items = [
            Some(MenuItem { id: 1, text: t("tray_open"), checked: false, default: true }),
            Some(MenuItem {
                id: 2,
                text: t(if self.status.busy { "btn_cancel" } else { "btn_connect" }),
                checked: false,
                default: false,
            }),
            Some(MenuItem { id: 3, text: t("set_auto_title"), checked: auto, default: false }),
            None,
            Some(MenuItem { id: 4, text: t("tray_exit"), checked: false, default: false }),
        ];
        match self.tray.menu(&items) {
            Some(1) => self.show(),
            Some(2) => {
                if self.service.flow_running() {
                    self.service.cancel_flow();
                } else {
                    self.service.connect_now();
                }
            }
            Some(3) => {
                let s = {
                    let mut s = self.settings.lock().unwrap();
                    s.auto_mode = !s.auto_mode;
                    s.save();
                    s.clone()
                };
                self.service.on_auto_mode_changed();
                self.send(json!({ "type": "settings", "settings": settings_json(&s) }));
            }
            Some(4) => self.exit(),
            _ => {}
        }
    }

    fn center(&self) {
        let Some(m) = self.window.current_monitor().or_else(|| self.window.primary_monitor()) else { return };
        let (pos, size, win) = (m.position(), m.size(), self.window.outer_size());
        let x = pos.x + (size.width as i32 - win.width as i32) / 2;
        let y = pos.y + (size.height as i32 - win.height as i32) / 2;
        self.window.set_outer_position(PhysicalPosition::new(x, y.max(pos.y)));
    }

    /// Lets a freshly started copy run (used to apply a language change).
    fn restart(&mut self) {
        let exe = std::env::current_exe();
        self.exit();
        if let Ok(exe) = exe {
            let _ = std::process::Command::new(exe).spawn();
        }
    }
}

fn settings_json(s: &Settings) -> Value {
    json!({
        "AutoMode": s.auto_mode,
        "StartWithWindows": s.start_with_windows,
        "StartMinimized": s.start_minimized,
        "Notifications": s.notifications,
        "Language": s.language,
    })
}


#[cfg(windows)]
struct SingleGuard(HANDLE);

#[cfg(windows)]
impl Drop for SingleGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

#[cfg(windows)]
fn single_instance() -> Option<SingleGuard> {
    unsafe {
        let mutex = CreateMutexW(None, true, w!(r"Local\ProControllerConnect.Single")).ok()?;
        if GetLastError() == ERROR_ALREADY_EXISTS {
            if let Ok(ev) = OpenEventW(EVENT_MODIFY_STATE, false, w!(r"Local\ProControllerConnect.Show")) {
                let _ = AllowSetForegroundWindow(ASFW_ANY);
                let _ = SetEvent(ev);
                let _ = CloseHandle(ev);
            }
            let _ = CloseHandle(mutex);
            return None;
        }
        Some(SingleGuard(mutex))
    }
}

#[cfg(windows)]
fn listen_for_show(_guard: &SingleGuard, proxy: EventLoopProxy<UserEvent>) {
    let Ok(ev) = (unsafe { CreateEventW(None, false, false, w!(r"Local\ProControllerConnect.Show")) }) else { return };
    let raw = ev.0 as isize;
    std::thread::spawn(move || loop {
        if unsafe { WaitForSingleObject(HANDLE(raw as _), INFINITE) } != WAIT_OBJECT_0 {
            break;
        }
        let _ = proxy.send_event(UserEvent::Show);
    });
}

#[cfg(not(windows))]
struct SingleGuard {
    path: std::path::PathBuf,
    listener: Option<std::os::unix::net::UnixListener>,
}

#[cfg(not(windows))]
impl Drop for SingleGuard {
    fn drop(&mut self) {
        self.listener = None;
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(not(windows))]
fn single_instance() -> Option<SingleGuard> {
    use std::io::Write;
    use std::os::unix::net::{UnixListener, UnixStream};
    let dir = settings::folder();
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("single.sock");
    if let Ok(mut stream) = UnixStream::connect(&path) {
        let _ = stream.write_all(b"show\n");
        return None;
    }
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).ok();
    Some(SingleGuard { path, listener })
}

#[cfg(not(windows))]
fn listen_for_show(guard: &SingleGuard, proxy: EventLoopProxy<UserEvent>) {
    let Some(listener) = guard.listener.as_ref().and_then(|l| l.try_clone().ok()) else { return };
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            if stream.is_ok() {
                let _ = proxy.send_event(UserEvent::Show);
            }
        }
    });
}

/// Windows 11 look: dark, rounded corners, no coloured border.
#[cfg(windows)]
fn style_window(hwnd: HWND) {
    let (dark, round, border): (i32, i32, u32) = (1, 2, 0xFFFF_FFFE);
    unsafe {
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, &dark as *const _ as _, 4);
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE, &round as *const _ as _, 4);
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_BORDER_COLOR, &border as *const _ as _, 4);
    }
}

#[cfg(windows)]
fn open_bt_settings() {
    unsafe {
        ShellExecuteW(None, &HSTRING::from("open"), &HSTRING::from("ms-settings:bluetooth"), None, None, SW_SHOWNORMAL);
    }
}

#[cfg(target_os = "macos")]
fn open_bt_settings() {
    let _ = std::process::Command::new("open")
        .arg("x-apple.systempreferences:com.apple.BluetoothSettings")
        .spawn();
}

#[cfg(target_os = "linux")]
fn open_bt_settings() {
    use std::process::Command;
    for (cmd, args) in [
        ("plasma-open-settings", &["kcm_bluetooth"][..]),
        ("systemsettings", &["kcm_bluetooth"][..]),
        ("kcmshell6", &["kcm_bluetooth"][..]),
        ("blueman-manager", &[][..]),
        ("overskride", &[][..]),
        ("gnome-control-center", &["bluetooth"][..]),
        ("kcmshell5", &["kcm_bluetooth"][..]),
        ("xdg-open", &["settings://bluetooth"][..]),
    ] {
        if Command::new(cmd).args(args).spawn().is_ok() {
            break;
        }
    }
}
