//! snip — a lightweight screenshot and annotation tool for macOS.
//! Lives in the menu bar. ⌘⇧2 captures an area; releasing the selection opens the editor.

mod editor;
mod mac;
mod ocr;
mod pin;
mod render;

use eframe::egui::{self, Color32, FontId, Key, Pos2, Rect, Stroke, StrokeKind, Vec2};
use global_hotkey::hotkey::{Code, HotKey, Modifiers};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use image::RgbaImage;
use std::sync::mpsc::{Receiver, Sender, channel};
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const LOUPE_PX: f32 = 15.0; // image pixels shown in the magnifier
const LOUPE_ZOOM: f32 = 8.0;
const OVERLAY_DIM: u8 = 100; // alpha of the black layer over the frozen screen
// Ruler color tolerance: Euclidean RGB distance. Default 3.54, adjustable by scrolling within 0..=20.
pub const RULER_SENSITIVITY: f32 = 3.54;
pub const RULER_SENSITIVITY_MAX: f32 = 20.0;

/// Run of pixels around (x, y) that match its color, along one axis. Returns [start, end).
/// This is the screen ruler: point between two objects and it measures the gap.
pub fn span(img: &RgbaImage, x: u32, y: u32, horizontal: bool, tol: f32) -> (u32, u32) {
    let base = img.get_pixel(x, y).0;
    let same = |x: u32, y: u32| {
        let p = img.get_pixel(x, y).0;
        (0..3).map(|i| (base[i] as f32 - p[i] as f32).powi(2)).sum::<f32>().sqrt() <= tol
    };
    let (pos, len) = if horizontal { (x, img.width()) } else { (y, img.height()) };
    let at = |i| if horizontal { same(i, y) } else { same(x, i) };
    let start = (0..pos).rev().find(|&i| !at(i)).map_or(0, |i| i + 1);
    let end = (pos + 1..len).find(|&i| !at(i)).unwrap_or(len);
    (start, end)
}

pub fn copy_image(img: &RgbaImage) {
    let data = arboard::ImageData { width: img.width() as usize, height: img.height() as usize, bytes: img.as_raw().into() };
    arboard::Clipboard::new().and_then(|mut cb| cb.set_image(data)).map_err(|e| eprintln!("copy failed: {e}")).ok();
}

pub fn save_image(img: &RgbaImage) -> Result<String, image::ImageError> {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let path = std::env::var("HOME").unwrap_or_default() + &format!("/Desktop/snip-{secs}.png");
    img.save(&path).map(|_| path)
}

/// A screenshot of one display plus where that display sits, in global screen points (top-left origin).
struct Capture {
    img: RgbaImage,
    origin: Pos2,
    size: Vec2,
}

impl Capture {
    /// Image pixels per point (2 on Retina, 1 on most external displays).
    fn scale(&self) -> f32 {
        self.img.width() as f32 / self.size.x
    }
}

/// Cursor position in global screen points, top-left origin (CoreGraphics convention). Any thread.
fn cursor_position() -> Pos2 {
    unsafe {
        let event = CGEventCreate(std::ptr::null());
        if event.is_null() {
            return Pos2::ZERO;
        }
        let p = CGEventGetLocation(event);
        CFRelease(event);
        Pos2::new(p.x as f32, p.y as f32)
    }
}

/// Capture the display the cursor is on.
fn capture() -> Option<Capture> {
    if !unsafe { CGPreflightScreenCaptureAccess() } {
        // Without permission macOS returns only the wallpaper; send the user to the setting instead.
        // A newly granted permission only applies after a relaunch, so open Settings once per launch.
        static OPENED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if !OPENED.swap(true, std::sync::atomic::Ordering::Relaxed) {
            unsafe { CGRequestScreenCaptureAccess() };
            mac::open_screen_recording_settings();
        }
        eprintln!("Screen Recording permission missing; enable Snip in System Settings, then relaunch Snip.");
        return None;
    }
    let cursor = cursor_position();
    let mon = xcap::Monitor::from_point(cursor.x as i32, cursor.y as i32)
        .or_else(|_| xcap::Monitor::all().map(|all| all.into_iter().find(|m| m.is_primary().unwrap_or(false))).and_then(|m| m.ok_or(xcap::XCapError::new("no display"))))
        .map_err(|e| eprintln!("no display: {e}"))
        .ok()?;
    let origin = Pos2::new(mon.x().ok()? as f32, mon.y().ok()? as f32);
    let size = Vec2::new(mon.width().ok()? as f32, mon.height().ok()? as f32);
    let img = mon.capture_image().map_err(|e| eprintln!("capture failed: {e}")).ok()?;
    Some(Capture { img, origin, size })
}

struct Shot {
    img: RgbaImage,
    tex: egui::TextureHandle,
    scale: f32, // image pixels per screen point (2.0 on Retina)
}

/// Work handed from the hotkey / menu bar to the UI.
enum Incoming {
    /// Display capture to select an area from.
    Area(Capture),
    /// Display capture to select text or a QR code from; the result is copied to the clipboard.
    Ocr(Capture),
    /// Display capture plus the windows on it (in display-local points), to pick one window from.
    Window(Capture, Vec<WinInfo>),
    /// Display capture to crop to the previously selected area.
    Repeat(Capture),
    /// Image to open straight in the editor; `scale` is image pixels per point (None = display scale).
    Edit(RgbaImage, Option<f32>),
    /// Seconds left before a delayed capture (0 = done); shown next to the menu bar icon.
    Countdown(u8),
}

/// What the capture overlay does with the user's input.
#[derive(Default)]
enum Mode {
    /// Drag a selection, which opens in the editor.
    #[default]
    Edit,
    /// Drag a selection, whose text / QR codes are copied.
    Ocr,
    /// Hover highlights a window; clicking captures it.
    Window(Vec<WinInfo>),
}

/// An on-screen window, in global screen points.
struct WinInfo {
    id: u32,
    rect: Rect,
    label: String,
}

/// Normal app windows, front to back. Skips our own windows, system UI and tiny windows (menu bar extras).
fn list_windows() -> Vec<WinInfo> {
    const SYSTEM: [&str; 6] = ["Window Server", "Dock", "Control Center", "Notification Center", "SystemUIServer", "Spotlight"];
    let own = std::process::id();
    let windows = xcap::Window::all().unwrap_or_default();
    windows
        .into_iter()
        .filter_map(|w| {
            let app = w.app_name().unwrap_or_default();
            if w.pid().ok()? == own || w.is_minimized().unwrap_or(false) || SYSTEM.contains(&app.as_str()) {
                return None;
            }
            let (width, height) = (w.width().ok()? as f32, w.height().ok()? as f32);
            if width < 50.0 || height < 50.0 {
                return None;
            }
            let rect = Rect::from_min_size(Pos2::new(w.x().ok()? as f32, w.y().ok()? as f32), Vec2::new(width, height));
            let title = w.title().unwrap_or_default();
            let label = if title.is_empty() { app } else { format!("{app} — {title}") };
            Some(WinInfo { id: w.id().ok()?, rect, label })
        })
        .collect()
}

/// Capture a single window on its own, without whatever overlaps it.
fn capture_window(id: u32) -> Option<RgbaImage> {
    let window = xcap::Window::all().ok()?.into_iter().find(|w| w.id().ok() == Some(id))?;
    window.capture_image().map_err(|e| eprintln!("window capture failed: {e}")).ok()
}

/// Crop `img` (captured at `scale` pixels per point) to `rect` in points.
fn crop_points(img: &RgbaImage, rect: Rect, scale: f32) -> RgbaImage {
    let x = ((rect.min.x.max(0.0) * scale) as u32).min(img.width() - 1);
    let y = ((rect.min.y.max(0.0) * scale) as u32).min(img.height() - 1);
    let w = ((rect.width() * scale) as u32).min(img.width() - x).max(1);
    let h = ((rect.height() * scale) as u32).min(img.height() - y).max(1);
    image::imageops::crop_imm(img, x, y, w, h).to_image()
}

/// Queue work for the UI; the window is reshaped and shown on the next frame.
fn deliver(ctx: &egui::Context, tx: &Sender<Incoming>, msg: Incoming) {
    tx.send(msg).ok();
    ctx.request_repaint();
}

#[derive(Clone, Copy, PartialEq)]
enum MenuAction {
    Area,
    Window,
    Repeat,
    Delayed,
    Full,
    Ocr,
    Open,
    Login,
    Quit,
}

/// Capture the screen for a capture action and hand it to the UI.
fn capture_for(action: MenuAction, ctx: &egui::Context, tx: &Sender<Incoming>) {
    let windows = if action == MenuAction::Window { list_windows() } else { vec![] };
    let Some(cap) = capture() else { return };
    let msg = match action {
        MenuAction::Window => {
            // Keep windows on this display, in display-local points.
            let display = Rect::from_min_size(cap.origin, cap.size);
            let local = windows
                .into_iter()
                .filter(|w| w.rect.intersects(display))
                .map(|w| WinInfo { rect: w.rect.translate(-cap.origin.to_vec2()), ..w })
                .collect();
            Incoming::Window(cap, local)
        }
        MenuAction::Repeat => Incoming::Repeat(cap),
        MenuAction::Full => {
            let scale = cap.scale();
            Incoming::Edit(cap.img, Some(scale))
        }
        MenuAction::Ocr => Incoming::Ocr(cap),
        _ => Incoming::Area(cap),
    };
    deliver(ctx, tx, msg);
}

fn build_tray(ctx: egui::Context, tx: Sender<Incoming>) -> Option<TrayIcon> {
    let items = [
        (MenuAction::Area, "Capture Area                 ⌘⇧2"),
        (MenuAction::Window, "Capture Window"),
        (MenuAction::Repeat, "Repeat Last Area          ⌥⇧⌘2"),
        (MenuAction::Delayed, "Capture Area in 3 Seconds"),
        (MenuAction::Full, "Capture Full Screen"),
        (MenuAction::Ocr, "Recognize Text/QR          ⌘⇧1"),
        (MenuAction::Open, "Open Image…"),
    ]
    .map(|(action, label)| (action, MenuItem::new(label, true, None)));
    let login = CheckMenuItem::new("Launch at Login", true, mac::launch_at_login(), None);
    let quit = MenuItem::new("Quit Snip", true, None);

    let menu = Menu::new();
    for (_, item) in &items {
        menu.append(item).ok()?;
    }
    let sep = PredefinedMenuItem::separator;
    menu.append_items(&[&sep(), &login, &sep(), &quit]).ok()?;

    let mut actions: Vec<_> = items.iter().map(|(a, item)| (item.id().clone(), *a)).collect();
    actions.push((login.id().clone(), MenuAction::Login));
    actions.push((quit.id().clone(), MenuAction::Quit));
    MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
        let Some(&(_, action)) = actions.iter().find(|(id, _)| *id == e.id) else { return };
        match action {
            MenuAction::Delayed => {
                let (ctx, tx) = (ctx.clone(), tx.clone());
                std::thread::spawn(move || {
                    for n in (1..=3).rev() {
                        deliver(&ctx, &tx, Incoming::Countdown(n));
                        std::thread::sleep(std::time::Duration::from_secs(1));
                    }
                    deliver(&ctx, &tx, Incoming::Countdown(0));
                    capture_for(MenuAction::Area, &ctx, &tx);
                });
            }
            MenuAction::Open => {
                if let Some(img) = mac::pick_file().and_then(|p| image::open(p).map_err(|e| eprintln!("open failed: {e}")).ok()) {
                    deliver(&ctx, &tx, Incoming::Edit(img.to_rgba8(), Some(1.0)));
                }
            }
            MenuAction::Login => {
                // The menu toggles its own checkmark; mirror it from the file on disk.
                let enable = !mac::launch_at_login();
                mac::set_launch_at_login(enable).map_err(|e| eprintln!("launch at login: {e}")).ok();
            }
            MenuAction::Quit => std::process::exit(0),
            capture_action => {
                std::thread::sleep(std::time::Duration::from_millis(150)); // let the menu fade out first
                capture_for(capture_action, &ctx, &tx);
            }
        }
    }));

    let mut builder = TrayIconBuilder::new().with_menu(Box::new(menu)).with_tooltip("Snip");
    match mac::sf_symbol("camera.viewfinder", 36.0).and_then(|i| Icon::from_rgba(i.to_vec(), i.width(), i.height()).ok()) {
        Some(icon) => builder = builder.with_icon_templated(icon),
        None => builder = builder.with_title("Snip"),
    }
    builder.build().map_err(|e| eprintln!("menu bar icon: {e}")).ok()
}

#[derive(Default)]
struct App {
    rx: Option<Receiver<Incoming>>,
    tray: Option<TrayIcon>,
    shot: Option<Shot>,
    sel: Option<Rect>, // in screen points
    drag_from: Option<Pos2>,
    ruler: Option<bool>, // Some(horizontal)
    sensitivity: Option<f32>,
    toast: Option<(String, Instant)>,
    mode: Mode,
    last_area: Option<Rect>, // last selection opened in the editor, for Repeat Last Area
    window_label: Option<String>, // app/title of the hovered window in window mode
    ocr_rx: Option<Receiver<Vec<String>>>,   // pending recognition
    close_at: Option<Instant>,               // close the overlay after showing a result
    editor: Option<editor::Editor>,
    screen: Vec2,   // size of the display being captured, in points
    origin: Pos2,   // its top-left corner in global screen points
    parked_once: bool,
    pins: Vec<pin::Pin>,
    pins_made: u64,
    reveal_in: u8, // frames to wait before making a reshaped window visible
    _hotkeys: Option<GlobalHotKeyManager>,
}

impl App {
    fn new(cc: &eframe::CreationContext, screen: Vec2) -> Self {
        mac::menu_bar_only();
        let (tx, rx) = channel();
        let tray = build_tray(cc.egui_ctx.clone(), tx.clone());
        let manager = GlobalHotKeyManager::new().expect("hotkey manager");
        let area_key = HotKey::new(Some(Modifiers::SUPER | Modifiers::SHIFT), Code::Digit2);
        let ocr_key = HotKey::new(Some(Modifiers::SUPER | Modifiers::SHIFT), Code::Digit1);
        let repeat_key = HotKey::new(Some(Modifiers::SUPER | Modifiers::SHIFT | Modifiers::ALT), Code::Digit2);
        for (key, name) in [(area_key, "⌘⇧2"), (ocr_key, "⌘⇧1"), (repeat_key, "⌥⇧⌘2")] {
            manager.register(key).map_err(|e| eprintln!("{name} unavailable: {e}")).ok();
        }
        let ctx = cc.egui_ctx.clone();
        GlobalHotKeyEvent::set_event_handler(Some(move |e: GlobalHotKeyEvent| {
            if e.state == HotKeyState::Pressed {
                let action = match e.id {
                    id if id == ocr_key.id() => MenuAction::Ocr,
                    id if id == repeat_key.id() => MenuAction::Repeat,
                    _ => MenuAction::Area,
                };
                capture_for(action, &ctx, &tx);
            }
        }));
        Self { rx: Some(rx), tray, _hotkeys: Some(manager), screen, ..Default::default() }
    }

    fn close(&mut self, frame: &eframe::Frame) {
        self.shot = None;
        self.editor = None;
        self.sel = None;
        self.ruler = None;
        self.reveal_in = 0;
        self.mode = Mode::Edit;
        self.ocr_rx = None;
        self.close_at = None;
        mac::park(frame);
    }

    fn toast(&mut self, msg: impl Into<String>) {
        self.toast = Some((msg.into(), Instant::now()));
    }

    /// Open the selection in a regular, decorated editor window.
    fn open_editor(&mut self, ctx: &egui::Context, frame: &eframe::Frame) {
        let (Some(img), Some(shot)) = (self.cropped(), &self.shot) else { return };
        let scale = shot.scale;
        if matches!(self.mode, Mode::Edit) && self.sel.is_some() {
            self.last_area = self.sel;
        }
        self.shot = None;
        self.edit(ctx, frame, img, scale);
    }

    fn edit(&mut self, ctx: &egui::Context, frame: &eframe::Frame, img: RgbaImage, scale: f32) {
        let ed = editor::Editor::new(ctx, img, scale);
        let size = ed.preferred_size(self.screen);
        mac::editor_window(frame, size.x as f64, size.y as f64);
        self.reveal_in = 2;
        self.editor = Some(ed);
        self.drag_from = None;
        ctx.request_repaint();
    }

    /// Switch the window to a borderless, always-on-top, full-screen overlay showing `img`.
    fn show_overlay(&mut self, ctx: &egui::Context, frame: &eframe::Frame, cap: Capture) {
        self.editor = None;
        self.screen = cap.size;
        self.origin = cap.origin;
        mac::cover_screen(frame, Rect::from_min_size(cap.origin, cap.size));
        self.reveal_in = 2;
        let scale = cap.scale();
        let img = cap.img;
        let size = [img.width() as usize, img.height() as usize];
        let color = egui::ColorImage::from_rgba_unmultiplied(size, img.as_raw());
        let tex = ctx.load_texture("shot", color, egui::TextureOptions::NEAREST);
        self.shot = Some(Shot { img, tex, scale });
        self.sel = None;
        self.ruler = None;
    }

    /// Selection (or whole screen) cropped to image pixels.
    fn cropped(&self) -> Option<RgbaImage> {
        let s = self.shot.as_ref()?;
        Some(match self.sel {
            Some(r) => crop_points(&s.img, r, s.scale),
            None => s.img.clone(),
        })
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let ctx = &ui.ctx().clone();
        if !self.parked_once {
            self.parked_once = true;
            mac::park(frame);
        }
        for p in &mut self.pins {
            p.show(ctx);
        }
        self.pins.retain(|p| !p.is_closed());
        // Closing the window parks it; the app keeps running in the menu bar.
        if ctx.input(|i| i.viewport().close_requested()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            return self.close(frame);
        }
        // Reveal only after egui has painted at the new window size, so no black frame shows.
        if self.reveal_in > 0 {
            self.reveal_in -= 1;
            if self.reveal_in == 0 {
                mac::reveal(frame);
            }
            ctx.request_repaint();
        }
        let incoming: Vec<Incoming> = self.rx.as_ref().map(|rx| rx.try_iter().collect()).unwrap_or_default();
        for msg in incoming {
            let display_scale = ctx.pixels_per_point();
            match msg {
                Incoming::Edit(img, scale) => {
                    self.shot = None;
                    self.edit(ctx, frame, img, scale.unwrap_or(display_scale));
                }
                Incoming::Area(cap) => self.show_overlay(ctx, frame, cap),
                Incoming::Ocr(cap) => {
                    self.show_overlay(ctx, frame, cap);
                    self.mode = Mode::Ocr;
                }
                Incoming::Window(cap, windows) => {
                    self.show_overlay(ctx, frame, cap);
                    self.mode = Mode::Window(windows);
                }
                Incoming::Repeat(cap) => match self.last_area {
                    Some(area) => {
                        self.shot = None;
                        self.screen = cap.size;
                        self.origin = cap.origin;
                        let scale = cap.scale();
                        self.edit(ctx, frame, crop_points(&cap.img, area, scale), scale);
                    }
                    None => self.show_overlay(ctx, frame, cap), // nothing to repeat yet
                },
                Incoming::Countdown(n) => {
                    if let Some(tray) = &self.tray {
                        tray.set_title((n > 0).then(|| n.to_string()));
                    }
                }
            }
        }
        if let Some(rx) = &self.ocr_rx {
            match rx.try_recv() {
                Ok(lines) => {
                    let msg = ocr::copy_result(&lines);
                    self.ocr_rx = None;
                    self.toast(msg);
                    self.close_at = Some(Instant::now() + std::time::Duration::from_millis(1200));
                }
                Err(_) => ctx.request_repaint_after(std::time::Duration::from_millis(50)),
            }
        }
        if let Some(t) = self.close_at {
            if Instant::now() >= t {
                return self.close(frame);
            }
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        }
        if let Some(ed) = &mut self.editor {
            mac::place_traffic_lights(frame, editor::TOOLBAR_HEIGHT as f64);
            match ed.ui(ui) {
                editor::Action::None => {}
                editor::Action::Close => self.close(frame),
                editor::Action::Pin(img, scale) => {
                    let display = Rect::from_min_size(self.origin, self.screen);
                    self.pins.push(pin::Pin::new(ctx, img, scale, display, self.pins_made));
                    self.pins_made += 1;
                    self.close(frame);
                }
            }
            return;
        }
        let Some(shot) = &self.shot else { return };

        let screen = Rect::from_min_size(Pos2::ZERO, shot.tex.size_vec2() / shot.scale);
        let hover = ctx.input(|i| i.pointer.hover_pos()).map(|p| screen.clamp(p));
        let px = |p: Pos2| {
            let (w, h) = shot.img.dimensions();
            (((p.x * shot.scale) as u32).min(w - 1), ((p.y * shot.scale) as u32).min(h - 1))
        };

        // --- keyboard ---
        let mut msg = None;
        let (cmd, keys) = ctx.input(|i| {
            let k = [Key::Escape, Key::Enter, Key::Tab, Key::ArrowLeft, Key::ArrowRight, Key::ArrowUp, Key::ArrowDown, Key::C, Key::S];
            (i.modifiers.command, k.map(|k| i.key_pressed(k)))
        });
        let [esc, enter, tab, left, right, up, down, c, s] = keys;
        if left || right {
            self.ruler = Some(true);
        }
        if up || down {
            self.ruler = Some(false);
        }
        // Scrolling while the ruler is active adjusts its color tolerance.
        let scroll = ctx.input(|i| i.smooth_scroll_delta.y);
        if self.ruler.is_some() && scroll != 0.0 {
            let v = self.sensitivity.unwrap_or(RULER_SENSITIVITY) + scroll / 10.0;
            self.sensitivity = Some(v.clamp(0.0, RULER_SENSITIVITY_MAX));
        }
        if tab && let Some(p) = hover {
            let (x, y) = px(p);
            let [r, g, b, _] = shot.img.get_pixel(x, y).0;
            let hex = format!("#{r:02X}{g:02X}{b:02X}");
            arboard::Clipboard::new().and_then(|mut cb| cb.set_text(&hex)).ok();
            msg = Some(format!("Copied {hex}"));
        }
        if enter {
            return self.open_editor(ctx, frame); // whole screen (or current selection)
        }
        if cmd && c {
            if let Some(img) = self.cropped() {
                copy_image(&img);
            }
            return self.close(frame);
        }
        if cmd && s {
            match self.cropped().map(|img| save_image(&img)) {
                Some(Ok(_)) => return self.close(frame),
                Some(Err(e)) => msg = Some(format!("Save failed: {e}")),
                None => {}
            }
        }
        if esc {
            // In window mode the highlight is not a user selection, so Esc always closes.
            if (self.ruler.is_some() || self.sel.is_some()) && !matches!(self.mode, Mode::Window(_)) {
                self.ruler = None;
                self.sel = None;
            } else {
                return self.close(frame);
            }
        }

        // --- mouse: drag to select, click to drop ruler ---
        let shot = self.shot.as_ref().unwrap();
        let (pressed, down_now, released) = ctx.input(|i| (i.pointer.primary_pressed(), i.pointer.primary_down(), i.pointer.primary_released()));
        let busy = self.ocr_rx.is_some() || self.close_at.is_some();
        if pressed && !busy {
            self.drag_from = hover;
            self.ruler = None;
        }
        if let (true, Some(a), Some(b)) = (down_now, self.drag_from, hover) {
            self.sel = Some(Rect::from_two_pos(a, b)).filter(|r| r.area() > 4.0);
        }
        if let Mode::Window(windows) = &self.mode {
            // Highlight the frontmost window under the cursor (drawn like a selection); click captures it.
            self.drag_from = None;
            let hovered = hover.and_then(|h| windows.iter().find(|w| w.rect.contains(h)));
            self.sel = hovered.map(|w| w.rect.intersect(screen));
            self.window_label = hovered.map(|w| w.label.clone());
            if released && let Some(w) = hovered {
                let (id, rect, scale) = (w.id, w.rect, shot.scale);
                let img = capture_window(id).unwrap_or_else(|| crop_points(&shot.img, rect.intersect(screen), scale));
                let scale = img.width() as f32 / rect.width();
                self.mode = Mode::Edit;
                self.shot = None;
                return self.edit(ctx, frame, img, scale);
            }
        } else if released {
            self.drag_from = None;
            if self.sel.is_some() && matches!(self.mode, Mode::Ocr) {
                if self.ocr_rx.is_none() && self.close_at.is_none() && let Some(img) = self.cropped() {
                    self.ocr_rx = Some(ocr::spawn(img));
                    msg = Some("Recognizing…".to_string());
                }
            } else if self.sel.is_some() {
                return self.open_editor(ctx, frame); // releasing the selection opens the editor
            }
        }

        // --- paint ---
        let label = |p: &egui::Painter, pos: Pos2, text: String| {
            let galley = p.layout_no_wrap(text, FontId::monospace(12.0), Color32::WHITE);
            let r = Rect::from_min_size(pos, galley.size()).expand(3.0);
            p.rect_filled(r, 3.0, Color32::from_black_alpha(200));
            p.galley(pos, galley, Color32::WHITE);
        };
        egui::CentralPanel::default().frame(egui::Frame::NONE).show(ui, |ui| {
            let p = ui.painter();
            let uv = Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));
            p.image(shot.tex.id(), screen, uv, Color32::WHITE);

            // Dim the frozen screen so the capture mode is obvious; the selection stays at full brightness.
            let dim = Color32::from_black_alpha(OVERLAY_DIM);
            ctx.set_cursor_icon(egui::CursorIcon::Crosshair);
            if self.sel.is_none() {
                p.rect_filled(screen, 0.0, dim);
            }
            if let Some(sel) = self.sel {
                for r in [
                    Rect::from_min_max(screen.min, Pos2::new(screen.max.x, sel.min.y)),
                    Rect::from_min_max(Pos2::new(screen.min.x, sel.max.y), screen.max),
                    Rect::from_min_max(Pos2::new(screen.min.x, sel.min.y), Pos2::new(sel.min.x, sel.max.y)),
                    Rect::from_min_max(Pos2::new(sel.max.x, sel.min.y), Pos2::new(screen.max.x, sel.max.y)),
                ] {
                    p.rect_filled(r, 0.0, dim);
                }
                p.rect_stroke(sel, 0.0, Stroke::new(1.0, Color32::WHITE), StrokeKind::Outside);
                label(p, sel.min - Vec2::new(0.0, 20.0), format!("{:.0} × {:.0}", sel.width(), sel.height()));
                if let (Mode::Window(_), Some(name)) = (&self.mode, &self.window_label) {
                    label(p, sel.min + Vec2::new(8.0, 8.0), name.clone());
                }
            }

            let Some(h) = hover else { return };
            let (x, y) = px(h);

            if let Some(horizontal) = self.ruler {
                let tol = self.sensitivity.unwrap_or(RULER_SENSITIVITY);
                let (a, b) = span(&shot.img, x, y, horizontal, tol);
                let (a, b) = (a as f32 / shot.scale, b as f32 / shot.scale);
                let (p0, p1) = if horizontal { (Pos2::new(a, h.y), Pos2::new(b, h.y)) } else { (Pos2::new(h.x, a), Pos2::new(h.x, b)) };
                p.line_segment([p0, p1], Stroke::new(1.0, Color32::from_rgb(255, 60, 120)));
                label(p, h + Vec2::new(10.0, 10.0), format!("{:.0} px  ±{tol:.1}", b - a));
            }

            // Loupe: magnified pixels + crosshair + color under cursor.
            let half = LOUPE_PX / 2.0;
            let (iw, ih) = (shot.img.width() as f32, shot.img.height() as f32);
            let src = Rect::from_min_max(
                Pos2::new((x as f32 - half) / iw, (y as f32 - half) / ih),
                Pos2::new((x as f32 + half + 1.0) / iw, (y as f32 + half + 1.0) / ih),
            );
            let size = (LOUPE_PX + 1.0) * LOUPE_ZOOM;
            let mut at = h + Vec2::splat(24.0);
            if at.x + size > screen.max.x { at.x = h.x - 24.0 - size; }
            if at.y + size + 24.0 > screen.max.y { at.y = h.y - 24.0 - size - 24.0; }
            let loupe = Rect::from_min_size(at, Vec2::splat(size));
            p.image(shot.tex.id(), loupe, src, Color32::WHITE);
            let cell = Rect::from_min_size(loupe.min + Vec2::splat(half * LOUPE_ZOOM), Vec2::splat(LOUPE_ZOOM));
            p.rect_stroke(cell, 0.0, Stroke::new(1.0, Color32::RED), StrokeKind::Outside);
            p.rect_stroke(loupe, 0.0, Stroke::new(1.0, Color32::WHITE), StrokeKind::Outside);
            let [r, g, b, _] = shot.img.get_pixel(x, y).0;
            label(p, loupe.left_bottom() + Vec2::new(0.0, 6.0), format!("#{r:02X}{g:02X}{b:02X}  {:.0},{:.0}", h.x, h.y));
        });

        if let Some(m) = msg {
            self.toast(m);
        }
        let hint = if matches!(self.mode, Mode::Ocr) {
            "drag over text or a QR code to copy it · Esc cancel"
        } else if matches!(self.mode, Mode::Window(_)) {
            "click a window to capture it · Esc cancel"
        } else {
            "drag: select & edit · Enter: edit full screen · ⌘C copy · ⌘S save · ←→↑↓ ruler · Tab color · Esc close"
        };
        let fg = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::new("hint")));
        let galley = fg.layout_no_wrap(hint.into(), FontId::proportional(13.0), Color32::WHITE);
        let at = Pos2::new(screen.center().x - galley.size().x / 2.0, screen.max.y - 60.0);
        fg.rect_filled(Rect::from_min_size(at, galley.size()).expand(8.0), 8.0, Color32::from_black_alpha(190));
        fg.galley(at, galley, Color32::WHITE);
        if let Some((msg, t)) = &self.toast {
            if t.elapsed().as_secs_f32() < 1.5 || self.ocr_rx.is_some() {
                let p = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::new("toast")));
                let galley = p.layout_no_wrap(msg.clone(), FontId::proportional(18.0), Color32::WHITE);
                let at = Pos2::new(screen.center().x - galley.size().x / 2.0, screen.center().y - galley.size().y / 2.0);
                p.rect_filled(Rect::from_min_size(at, galley.size()).expand2(Vec2::new(18.0, 12.0)), 12.0, Color32::from_black_alpha(210));
                p.galley(at, galley, Color32::WHITE);
                ctx.request_repaint();
            } else {
                self.toast = None;
            }
        }
    }
}

#[repr(C)]
struct CGPoint {
    x: f64,
    y: f64,
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGRequestScreenCaptureAccess() -> bool;
    fn CGEventCreate(source: *const std::ffi::c_void) -> *mut std::ffi::c_void;
    fn CGEventGetLocation(event: *mut std::ffi::c_void) -> CGPoint;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(cf: *const std::ffi::c_void);
}

fn main() -> eframe::Result {
    // Ask once at launch so the user sees the system prompt before their first capture.
    if !unsafe { CGPreflightScreenCaptureAccess() } {
        unsafe { CGRequestScreenCaptureAccess() };
    }
    let mon = xcap::Monitor::all().ok().and_then(|m| m.into_iter().find(|m| m.is_primary().unwrap_or(false)));
    let size = mon.map_or([1440.0, 900.0], |m| [m.width().unwrap_or(1440) as f32, m.height().unwrap_or(900) as f32]);
    let screen = Vec2::from(size);
    let options = eframe::NativeOptions {
        // The window is parked (transparent, click-through) on the first frame; see mac.rs.
        viewport: egui::ViewportBuilder::default().with_decorations(false).with_inner_size([400.0, 300.0]),
        ..Default::default()
    };
    eframe::run_native("snip", options, Box::new(|cc| Ok(Box::new(App::new(cc, screen)))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crop_points_converts_to_pixels_and_clamps() {
        let img = RgbaImage::from_fn(200, 100, |x, y| image::Rgba([x as u8, y as u8, 0, 255]));
        let out = crop_points(&img, Rect::from_min_size(Pos2::new(10.0, 5.0), Vec2::new(20.0, 10.0)), 2.0);
        assert_eq!(out.dimensions(), (40, 20));
        assert_eq!(out.get_pixel(0, 0).0, [20, 10, 0, 255], "origin scaled to pixels");
        let edge = crop_points(&img, Rect::from_min_size(Pos2::new(90.0, 40.0), Vec2::new(50.0, 50.0)), 2.0);
        assert_eq!(edge.dimensions(), (20, 20), "clamped to the image");
    }

    #[test]
    fn ruler_measures_gap_between_objects() {
        // 20px white row with black "objects" at x=3 and x=12 → gap is x 4..12 (8px).
        let mut img = RgbaImage::from_pixel(20, 3, image::Rgba([255, 255, 255, 255]));
        for y in 0..3 {
            img.put_pixel(3, y, image::Rgba([0, 0, 0, 255]));
            img.put_pixel(12, y, image::Rgba([0, 0, 0, 255]));
        }
        assert_eq!(span(&img, 7, 1, true, 3.54), (4, 12));
        assert_eq!(span(&img, 15, 1, true, 3.54), (13, 20)); // runs to the edge
        assert_eq!(span(&img, 7, 1, false, 3.54), (0, 3));
        assert_eq!(span(&img, 3, 1, true, 3.54), (3, 4)); // on the object itself
        // Faint antialiasing pixel (250) stops the default ruler but not a loose one.
        img.put_pixel(9, 1, image::Rgba([250, 250, 250, 255]));
        assert_eq!(span(&img, 7, 1, true, 3.54), (4, 9));
        assert_eq!(span(&img, 7, 1, true, 20.0), (4, 12));
    }
}
