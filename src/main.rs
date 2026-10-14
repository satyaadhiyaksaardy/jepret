//! snip — a lightweight screenshot and annotation tool for macOS.
//! Lives in the menu bar. ⌘⇧2 captures an area; releasing the selection opens the editor.

mod editor;
mod mac;
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

fn capture() -> Option<RgbaImage> {
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
    // Captures the primary display only. Multi-monitor support would use Monitor::from_point(cursor).
    let mon = xcap::Monitor::all().ok()?.into_iter().find(|m| m.is_primary().unwrap_or(false))?;
    mon.capture_image().map_err(|e| eprintln!("capture failed: {e}")).ok()
}

struct Shot {
    img: RgbaImage,
    tex: egui::TextureHandle,
    scale: f32, // image pixels per screen point (2.0 on Retina)
}

/// Work handed from the hotkey / menu bar to the UI.
enum Incoming {
    /// Full-screen capture to select an area from.
    Area(RgbaImage),
    /// Image to open straight in the editor; `scale` is image pixels per point (None = display scale).
    Edit(RgbaImage, Option<f32>),
}

/// Queue work for the UI; the window is reshaped and shown on the next frame.
fn deliver(ctx: &egui::Context, tx: &Sender<Incoming>, msg: Incoming) {
    tx.send(msg).ok();
    ctx.request_repaint();
}

fn build_tray(ctx: egui::Context, tx: Sender<Incoming>) -> Option<TrayIcon> {
    let area = MenuItem::new("Capture Area        ⌘⇧2", true, None);
    let full = MenuItem::new("Capture Full Screen", true, None);
    let open = MenuItem::new("Open Image…", true, None);
    let login = CheckMenuItem::new("Launch at Login", true, mac::launch_at_login(), None);
    let quit = MenuItem::new("Quit Snip", true, None);
    let menu = Menu::new();
    menu.append_items(&[&area, &full, &open, &PredefinedMenuItem::separator(), &login, &PredefinedMenuItem::separator(), &quit])
        .ok()?;

    let ids = [area.id().clone(), full.id().clone(), open.id().clone(), login.id().clone(), quit.id().clone()];
    MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
        let [area, full, open, login, quit] = &ids;
        if e.id == *area || e.id == *full {
            std::thread::sleep(std::time::Duration::from_millis(150)); // let the menu fade out first
            if let Some(img) = capture() {
                let msg = if e.id == *area { Incoming::Area(img) } else { Incoming::Edit(img, None) };
                deliver(&ctx, &tx, msg);
            }
        } else if e.id == *open {
            if let Some(img) = mac::pick_file().and_then(|p| image::open(p).map_err(|e| eprintln!("open failed: {e}")).ok()) {
                deliver(&ctx, &tx, Incoming::Edit(img.to_rgba8(), Some(1.0)));
            }
        } else if e.id == *login {
            // The menu toggles its own checkmark; mirror it from the file on disk.
            let enable = !mac::launch_at_login();
            mac::set_launch_at_login(enable).map_err(|e| eprintln!("launch at login: {e}")).ok();
        } else if e.id == *quit {
            std::process::exit(0);
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
    _tray: Option<TrayIcon>,
    shot: Option<Shot>,
    sel: Option<Rect>, // in screen points
    drag_from: Option<Pos2>,
    ruler: Option<bool>, // Some(horizontal)
    sensitivity: Option<f32>,
    toast: Option<(String, Instant)>,
    editor: Option<editor::Editor>,
    screen: Vec2, // primary display size in points
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
        manager
            .register(HotKey::new(Some(Modifiers::SUPER | Modifiers::SHIFT), Code::Digit2))
            .map_err(|e| eprintln!("⌘⇧2 unavailable: {e}"))
            .ok();
        let ctx = cc.egui_ctx.clone();
        GlobalHotKeyEvent::set_event_handler(Some(move |e: GlobalHotKeyEvent| {
            if e.state == HotKeyState::Pressed
                && let Some(img) = capture()
            {
                deliver(&ctx, &tx, Incoming::Area(img));
            }
        }));
        Self { rx: Some(rx), _tray: tray, _hotkeys: Some(manager), screen, ..Default::default() }
    }

    fn close(&mut self, frame: &eframe::Frame) {
        self.shot = None;
        self.editor = None;
        self.sel = None;
        self.ruler = None;
        self.reveal_in = 0;
        mac::park(frame);
    }

    fn toast(&mut self, msg: impl Into<String>) {
        self.toast = Some((msg.into(), Instant::now()));
    }

    /// Open the selection in a regular, decorated editor window.
    fn open_editor(&mut self, ctx: &egui::Context, frame: &eframe::Frame) {
        let (Some(img), Some(shot)) = (self.cropped(), &self.shot) else { return };
        let scale = shot.scale;
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
    fn show_overlay(&mut self, ctx: &egui::Context, frame: &eframe::Frame, img: RgbaImage) {
        self.editor = None;
        mac::cover_screen(frame);
        self.reveal_in = 2;
        let size = [img.width() as usize, img.height() as usize];
        let color = egui::ColorImage::from_rgba_unmultiplied(size, img.as_raw());
        let tex = ctx.load_texture("shot", color, egui::TextureOptions::NEAREST);
        let scale = ctx.pixels_per_point();
        self.shot = Some(Shot { img, tex, scale });
        self.sel = None;
        self.ruler = None;
    }

    /// Selection (or whole screen) cropped to image pixels.
    fn cropped(&self) -> Option<RgbaImage> {
        let s = self.shot.as_ref()?;
        let Some(r) = self.sel else { return Some(s.img.clone()) };
        let (x, y) = ((r.min.x * s.scale) as u32, (r.min.y * s.scale) as u32);
        let w = ((r.width() * s.scale) as u32).min(s.img.width() - x).max(1);
        let h = ((r.height() * s.scale) as u32).min(s.img.height() - y).max(1);
        Some(image::imageops::crop_imm(&s.img, x, y, w, h).to_image())
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
        match self.rx.as_ref().and_then(|rx| rx.try_iter().last()) {
            Some(Incoming::Edit(img, scale)) => {
                self.shot = None;
                let scale = scale.unwrap_or(ctx.pixels_per_point());
                return self.edit(ctx, frame, img, scale);
            }
            Some(Incoming::Area(img)) => self.show_overlay(ctx, frame, img),
            None => {}
        }
        if let Some(ed) = &mut self.editor {
            match ed.ui(ui) {
                editor::Action::None => {}
                editor::Action::Close => self.close(frame),
                editor::Action::Pin(img, scale) => {
                    self.pins.push(pin::Pin::new(ctx, img, scale, self.screen, self.pins_made));
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
            if self.ruler.is_some() || self.sel.is_some() {
                self.ruler = None;
                self.sel = None;
            } else {
                return self.close(frame);
            }
        }

        // --- mouse: drag to select, click to drop ruler ---
        let shot = self.shot.as_ref().unwrap();
        let (pressed, down_now, released) = ctx.input(|i| (i.pointer.primary_pressed(), i.pointer.primary_down(), i.pointer.primary_released()));
        if pressed {
            self.drag_from = hover;
            self.ruler = None;
        }
        if let (true, Some(a), Some(b)) = (down_now, self.drag_from, hover) {
            self.sel = Some(Rect::from_two_pos(a, b)).filter(|r| r.area() > 4.0);
        }
        if released {
            self.drag_from = None;
            if self.sel.is_some() {
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
        let hint = "drag: select & edit · Enter: edit full screen · ⌘C copy · ⌘S save · ←→↑↓ ruler · Tab color · Esc close";
        let fg = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::new("hint")));
        let galley = fg.layout_no_wrap(hint.into(), FontId::proportional(13.0), Color32::WHITE);
        let at = Pos2::new(screen.center().x - galley.size().x / 2.0, screen.max.y - 60.0);
        fg.rect_filled(Rect::from_min_size(at, galley.size()).expand(8.0), 8.0, Color32::from_black_alpha(190));
        fg.galley(at, galley, Color32::WHITE);
        if let Some((msg, t)) = &self.toast {
            if t.elapsed().as_secs_f32() < 1.5 {
                let p = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::new("toast")));
                label(&p, Pos2::new(screen.center().x - 60.0, 40.0), msg.clone());
                ctx.request_repaint();
            } else {
                self.toast = None;
            }
        }
    }
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGRequestScreenCaptureAccess() -> bool;
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
