//! Notification area icon: coloured by state, dark menu, balloon notifications.

pub enum TrayEvent {
    Open,
    Menu,
}

pub struct MenuItem {
    pub id: u32,
    pub text: String,
    pub checked: bool,
    pub default: bool,
}

#[cfg(windows)]
mod imp {
    use super::{MenuItem, TrayEvent};
    use std::{
        cell::RefCell,
        collections::HashMap,
        sync::atomic::{AtomicU32, Ordering},
    };

    use windows::{
        core::{w, PCSTR, PCWSTR},
        Win32::{
            Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
            Graphics::Gdi::*,
            System::LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryW},
            UI::{Shell::*, WindowsAndMessaging::*},
        },
    };

    const WM_TRAY: u32 = WM_APP + 1;
    static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);

    thread_local! {
        static SINK: RefCell<Option<Box<dyn Fn(TrayEvent)>>> = const { RefCell::new(None) };
        /// What the icon shows, kept so it can be added again if Explorer restarts.
        static SHOWN: RefCell<Option<(HWND, HICON, String)>> = const { RefCell::new(None) };
    }

    pub struct Tray {
        hwnd: HWND,
        icons: HashMap<u32, HICON>,
    }

    impl Tray {
        pub fn new(color: u32, tip: &str, sink: impl Fn(TrayEvent) + 'static) -> Self {
            SINK.with(|s| *s.borrow_mut() = Some(Box::new(sink)));
            enable_dark_menus();
            unsafe {
                TASKBAR_CREATED.store(RegisterWindowMessageW(w!("TaskbarCreated")), Ordering::SeqCst);
                let hinstance = GetModuleHandleW(None).unwrap_or_default();
                let class = WNDCLASSW {
                    lpfnWndProc: Some(wndproc),
                    hInstance: hinstance.into(),
                    lpszClassName: w!("ProControllerConnect.Tray"),
                    ..Default::default()
                };
                RegisterClassW(&class);
                let hwnd = CreateWindowExW(
                    WS_EX_TOOLWINDOW,
                    w!("ProControllerConnect.Tray"),
                    w!("Pro Controller Connect"),
                    WS_POPUP,
                    0,
                    0,
                    0,
                    0,
                    None,
                    None,
                    Some(hinstance.into()),
                    None,
                )
                .unwrap_or_default();
                let mut tray = Tray { hwnd, icons: HashMap::new() };
                let icon = tray.icon(color);
                SHOWN.with(|s| *s.borrow_mut() = Some((hwnd, icon, tip.to_string())));
                add_icon();
                tray
            }
        }

        pub fn set(&mut self, color: u32, tip: &str) {
            let icon = self.icon(color);
            SHOWN.with(|s| *s.borrow_mut() = Some((self.hwnd, icon, tip.to_string())));
            let mut nid = base(self.hwnd);
            nid.uFlags = NIF_ICON | NIF_TIP;
            nid.hIcon = icon;
            copy(&mut nid.szTip, tip);
            unsafe {
                let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
            }
        }

        pub fn balloon(&self, title: &str, text: &str) {
            let mut nid = base(self.hwnd);
            nid.uFlags = NIF_INFO;
            nid.dwInfoFlags = NIIF_INFO;
            copy(&mut nid.szInfoTitle, title);
            copy(&mut nid.szInfo, text);
            unsafe {
                let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
            }
        }

        /// Shows the menu at the mouse and returns the chosen item.
        pub fn menu(&self, items: &[Option<MenuItem>]) -> Option<u32> {
            unsafe {
                let menu = CreatePopupMenu().ok()?;
                for item in items {
                    match item {
                        None => {
                            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
                        }
                        Some(it) => {
                            let flags = MF_STRING | if it.checked { MF_CHECKED } else { MF_UNCHECKED };
                            let text: Vec<u16> = it.text.encode_utf16().chain([0]).collect();
                            let _ = AppendMenuW(menu, flags, it.id as usize, PCWSTR(text.as_ptr()));
                            if it.default {
                                let _ = SetMenuDefaultItem(menu, it.id, 0);
                            }
                        }
                    }
                }
                let mut pt = POINT::default();
                let _ = GetCursorPos(&mut pt);
                let _ = SetForegroundWindow(self.hwnd);
                let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_NONOTIFY, pt.x, pt.y, Some(0), self.hwnd, None);
                let _ = PostMessageW(Some(self.hwnd), WM_NULL, WPARAM(0), LPARAM(0));
                let _ = DestroyMenu(menu);
                (cmd.0 != 0).then_some(cmd.0 as u32)
            }
        }

        pub fn remove(&self) {
            let nid = base(self.hwnd);
            unsafe {
                let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
            }
        }

        fn icon(&mut self, color: u32) -> HICON {
            *self.icons.entry(color).or_insert_with(|| make_icon(color))
        }
    }

    fn base(hwnd: HWND) -> NOTIFYICONDATAW {
        NOTIFYICONDATAW { cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32, hWnd: hwnd, uID: 1, ..Default::default() }
    }

    fn add_icon() {
        SHOWN.with(|s| {
            if let Some((hwnd, icon, tip)) = &*s.borrow() {
                let mut nid = base(*hwnd);
                nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
                nid.uCallbackMessage = WM_TRAY;
                nid.hIcon = *icon;
                copy(&mut nid.szTip, tip);
                unsafe {
                    let _ = Shell_NotifyIconW(NIM_ADD, &nid);
                }
            }
        });
    }

    fn copy(dst: &mut [u16], s: &str) {
        let max = dst.len() - 1;
        for (i, c) in s.encode_utf16().take(max).enumerate() {
            dst[i] = c;
        }
        dst[s.encode_utf16().count().min(max)] = 0;
    }

    unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if msg == WM_TRAY {
            let event = match lparam.0 as u32 {
                WM_LBUTTONUP => Some(TrayEvent::Open),
                WM_RBUTTONUP | WM_CONTEXTMENU => Some(TrayEvent::Menu),
                _ => None,
            };
            if let Some(e) = event {
                SINK.with(|s| {
                    if let Some(f) = &*s.borrow() {
                        f(e)
                    }
                });
            }
            return LRESULT(0);
        }
        if msg != 0 && msg == TASKBAR_CREATED.load(Ordering::SeqCst) {
            add_icon(); // Explorer restarted
            return LRESULT(0);
        }
        unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
    }

    /// Dark context menus, like the rest of the app (uxtheme's SetPreferredAppMode, Windows 10 1903+).
    fn enable_dark_menus() {
        unsafe {
            let Ok(ux) = LoadLibraryW(w!("uxtheme.dll")) else { return };
            if let Some(set_mode) = GetProcAddress(ux, PCSTR(135 as *const u8)) {
                let set_mode: unsafe extern "system" fn(i32) -> i32 = std::mem::transmute(set_mode);
                set_mode(2); // ForceDark
            }
            if let Some(flush) = GetProcAddress(ux, PCSTR(136 as *const u8)) {
                let flush: unsafe extern "system" fn() = std::mem::transmute(flush);
                flush();
            }
        }
    }

    /// A coloured disc with the white controller glyph (Segoe Fluent Icons), drawn at the small-icon size.
    fn make_icon(rgb: u32) -> HICON {
        unsafe {
            let size = GetSystemMetrics(SM_CXSMICON).max(16);
            let (r, g, b) = ((rgb >> 16) & 0xFF, (rgb >> 8) & 0xFF, rgb & 0xFF);

            // 1. The glyph as a grey-level mask, drawn by GDI with antialiasing
            let glyph = render_glyph(size);

            // 2. Antialiased disc + glyph, straight alpha
            let mut pixels = vec![0u32; (size * size) as usize];
            let (c, radius) = (size as f32 / 2.0, size as f32 * 31.0 / 64.0);
            for y in 0..size {
                for x in 0..size {
                    let mut cover = 0.0f32;
                    for sy in 0..4 {
                        for sx in 0..4 {
                            let (px, py) = (x as f32 + (sx as f32 + 0.5) / 4.0, y as f32 + (sy as f32 + 0.5) / 4.0);
                            if (px - c).powi(2) + (py - c).powi(2) <= radius * radius {
                                cover += 1.0 / 16.0;
                            }
                        }
                    }
                    let i = (y * size + x) as usize;
                    let gl = glyph[i] as f32 / 255.0;
                    let mix = |ch: u32| (ch as f32 * (1.0 - gl) + 255.0 * gl).round() as u32;
                    let a = (cover * 255.0).round() as u32;
                    pixels[i] = a << 24 | mix(r) << 16 | mix(g) << 8 | mix(b);
                }
            }

            let bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: size,
                    biHeight: -size,
                    biPlanes: 1,
                    biBitCount: 32,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits = std::ptr::null_mut();
            let color = CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits, None, 0).unwrap_or_default();
            if !bits.is_null() {
                std::ptr::copy_nonoverlapping(pixels.as_ptr(), bits as *mut u32, pixels.len());
            }
            let mask = CreateBitmap(size, size, 1, 1, None);
            let info = ICONINFO { fIcon: true.into(), hbmMask: mask, hbmColor: color, ..Default::default() };
            let icon = CreateIconIndirect(&info).unwrap_or_default();
            let _ = DeleteObject(color.into());
            let _ = DeleteObject(mask.into());
            icon
        }
    }

    unsafe fn render_glyph(size: i32) -> Vec<u8> {
        unsafe {
            let dc = CreateCompatibleDC(None);
            let bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: size,
                    biHeight: -size,
                    biPlanes: 1,
                    biBitCount: 32,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits = std::ptr::null_mut();
            let bmp = CreateDIBSection(Some(dc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0).unwrap_or_default();
            let old_bmp = SelectObject(dc, bmp.into());

            let font_for = |face: PCWSTR| {
                CreateFontW(-(size / 2), 0, 0, 0, 400, 0, 0, 0, DEFAULT_CHARSET, OUT_DEFAULT_PRECIS, CLIP_DEFAULT_PRECIS, ANTIALIASED_QUALITY, 0, face)
            };
            let mut font = font_for(w!("Segoe Fluent Icons"));
            let mut old_font = SelectObject(dc, font.into());
            let mut face = [0u16; 64];
            let n = GetTextFaceW(dc, Some(&mut face)) as usize;
            if String::from_utf16_lossy(&face[..n.saturating_sub(1).min(64)]).trim_end_matches('\0') != "Segoe Fluent Icons" {
                SelectObject(dc, old_font);
                let _ = DeleteObject(font.into());
                font = font_for(w!("Segoe MDL2 Assets"));
                old_font = SelectObject(dc, font.into());
            }
            SetTextColor(dc, windows::Win32::Foundation::COLORREF(0x00FFFFFF));
            SetBkMode(dc, TRANSPARENT);
            let mut text: Vec<u16> = "\u{E7FC}".encode_utf16().collect();
            let mut rc = RECT { left: 0, top: size / 32, right: size, bottom: size + size / 32 };
            DrawTextW(dc, &mut text, &mut rc, DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
            let _ = GdiFlush();

            let mut out = vec![0u8; (size * size) as usize];
            if !bits.is_null() {
                let px = std::slice::from_raw_parts(bits as *const u32, out.len());
                for (o, p) in out.iter_mut().zip(px) {
                    *o = ((p >> 8) & 0xFF) as u8; // green channel = coverage
                }
            }
            SelectObject(dc, old_font);
            SelectObject(dc, old_bmp);
            let _ = DeleteObject(font.into());
            let _ = DeleteObject(bmp.into());
            let _ = DeleteDC(dc);
            out
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use super::{MenuItem, TrayEvent};

    pub struct Tray;

    impl Tray {
        pub fn new(_color: u32, _tip: &str, _sink: impl Fn(TrayEvent) + 'static) -> Self {
            Tray
        }

        pub fn set(&mut self, _color: u32, _tip: &str) {}

        pub fn balloon(&self, title: &str, text: &str) {
            #[cfg(target_os = "linux")]
            {
                let _ = std::process::Command::new("notify-send")
                    .args(["-a", "Pro Controller Connect", title, text])
                    .spawn();
            }
            #[cfg(target_os = "macos")]
            {
                let esc = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
                let script = format!("display notification \"{}\" with title \"{}\"", esc(text), esc(title));
                let _ = std::process::Command::new("osascript").args(["-e", &script]).spawn();
            }
        }

        pub fn menu(&self, _items: &[Option<MenuItem>]) -> Option<u32> {
            None
        }

        pub fn remove(&self) {}
    }
}

pub use imp::Tray;
