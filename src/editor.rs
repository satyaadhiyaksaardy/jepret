//! The editor window that opens after a capture: annotate, crop, resize, copy and save.

use crate::render::{Backdrop, Mark, P, Shape, backdrop, flatten, text_size};
use crate::{RULER_SENSITIVITY, RULER_SENSITIVITY_MAX, copy_image, save_image, span};
use eframe::egui::{self, Color32, Key, Pos2, Rect, Sense, Stroke, StrokeKind, Vec2};
use image::RgbaImage;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

#[derive(Clone, Copy, PartialEq)]
enum Tool {
    Select,
    Arrow,
    Line,
    Rect,
    Oval,
    Pen,
    Highlight,
    Pixelate,
    Spotlight,
    Text,
    Step,
    Magnifier,
}

// (tool, name, keyboard shortcut, SF Symbol used as the toolbar icon).
const TOOLS: [(Tool, &str, Key, &str); 12] = [
    (Tool::Select, "Select / Crop", Key::V, "crop"),
    (Tool::Arrow, "Arrow", Key::A, "arrow.up.right"),
    (Tool::Line, "Line", Key::L, "line.diagonal"),
    (Tool::Rect, "Rectangle", Key::R, "rectangle"),
    (Tool::Oval, "Oval", Key::O, "circle"),
    (Tool::Pen, "Freehand Drawing", Key::D, "scribble"),
    (Tool::Highlight, "Highlighter", Key::H, "highlighter"),
    (Tool::Pixelate, "Blur/Erase", Key::B, "checkerboard.rectangle"),
    (Tool::Spotlight, "Spotlight", Key::S, "flashlight.on.fill"),
    (Tool::Text, "Text", Key::T, "textformat"),
    (Tool::Step, "Counter", Key::C, "1.circle"),
    (Tool::Magnifier, "Magnifier", Key::M, "plus.magnifyingglass"),
];
const ACTION_ICONS: [&str; 7] = [
    "arrow.uturn.backward",
    "arrow.up.left.and.arrow.down.right",
    "doc.on.doc",
    "square.and.arrow.down",
    "pin",
    "rectangle.inset.filled",
    "text.viewfinder",
];

// Backdrop gradient presets (top-left → bottom-right).
const GRADIENTS: [[[u8; 3]; 2]; 6] = [
    [[94, 114, 235], [255, 145, 175]],
    [[255, 94, 98], [255, 195, 113]],
    [[67, 206, 162], [24, 90, 157]],
    [[161, 140, 209], [251, 194, 235]],
    [[240, 242, 245], [205, 211, 220]],
    [[32, 34, 44], [74, 78, 104]],
];

// Magnifier bubble radius in points per unit of the size slider (default size 3 → 60pt, range 20–240pt).
const MAGNIFIER_RADIUS_PER_SIZE: f32 = 20.0;
const MAGNIFIER_ZOOM: f32 = 3.0;

/// A pasted image not yet placed: it can still be moved, resized and faded.
struct Floating {
    img: Arc<RgbaImage>,
    tex: egui::TextureHandle,
    a: P, // top-left, image pixels
    b: P, // bottom-right
    opacity: f32,
    drag: Option<FloatDrag>,
}

#[derive(Clone, Copy)]
enum FloatDrag {
    Move,
    Resize,
}

/// Backdrop settings in points (converted to image pixels when rendering).
struct BackdropUi {
    on: bool,
    gradient: usize,
    padding: f32,
    radius: f32,
    shadow: f32,
}

/// Height of the editor toolbar, which also hosts the (vertically centered) traffic lights.
pub const TOOLBAR_HEIGHT: f32 = 52.0;
const TRAFFIC_LIGHTS_WIDTH: f32 = 78.0;

const COLORS: [[u8; 3]; 6] = [[255, 59, 48], [255, 204, 0], [52, 199, 89], [0, 122, 255], [0, 0, 0], [255, 255, 255]];

#[derive(Clone)]
struct State {
    base: Rc<RgbaImage>,
    marks: Vec<Mark>,
}

pub enum Action {
    None,
    Close,
    /// Keep this image on screen as a floating pin (image, pixels per point).
    Pin(RgbaImage, f32),
}

pub struct Editor {
    state: State,
    undo: Vec<State>,
    redo: Vec<State>,
    flat: RgbaImage, // base + annotations
    out: RgbaImage,  // `flat` with the backdrop applied; this is what is shown, copied, saved and pinned
    tex: egui::TextureHandle,
    dirty: bool,
    out_dirty: bool,
    backdrop: BackdropUi,
    scale: f32, // image px per logical point (2 on Retina) — sizes are shown in points
    zoom: f32,  // screen points per image px
    pan: Vec2,
    fit_pending: bool,
    tool: Tool,
    color: [u8; 3],
    width: f32, // in logical points; multiplied by `scale` for image px
    drag: Option<Vec<P>>,
    sel: Option<(P, P)>,
    text: Option<(P, String)>,
    ruler: Option<bool>,
    sensitivity: f32,
    resize: Option<(String, String)>,
    toast: Option<(String, Instant)>,
    ocr_rx: Option<std::sync::mpsc::Receiver<Vec<String>>>,
    hover_px: Option<[u8; 3]>, // color under the cursor, shown in the toolbar
    floating: Option<Floating>,
    icons: HashMap<&'static str, egui::TextureHandle>,
}

fn to_p(v: Pos2) -> P {
    [v.x, v.y]
}

impl Editor {
    pub fn new(ctx: &egui::Context, img: RgbaImage, scale: f32) -> Self {
        let tex = ctx.load_texture("editor", color_image(&img), egui::TextureOptions::NEAREST);
        let icons = TOOLS.iter().map(|t| t.3).chain(ACTION_ICONS)
            .filter_map(|name| Some((name, ctx.load_texture(name, color_image(&crate::mac::sf_symbol(name, 36.0)?), egui::TextureOptions::LINEAR))))
            .collect();
        Self {
            flat: img.clone(),
            out: img.clone(),
            out_dirty: false,
            backdrop: BackdropUi { on: false, gradient: 0, padding: 32.0, radius: 10.0, shadow: 20.0 },
            state: State { base: Rc::new(img), marks: vec![] },
            undo: vec![],
            redo: vec![],
            tex,
            dirty: false,
            scale,
            zoom: 1.0 / scale,
            pan: Vec2::ZERO,
            fit_pending: true,
            tool: Tool::Arrow,
            color: COLORS[0],
            width: 3.0,
            drag: None,
            sel: None,
            text: None,
            ruler: None,
            sensitivity: RULER_SENSITIVITY,
            resize: None,
            toast: None,
            ocr_rx: None,
            hover_px: None,
            floating: None,
            icons,
        }
    }

    /// Window size (points) that shows the image at 100% when it fits on screen.
    pub fn preferred_size(&self, screen: Vec2) -> Vec2 {
        let img = Vec2::new(self.out.width() as f32, self.out.height() as f32) / self.scale;
        // Wide enough for the full toolbar; the right-hand info blocks drop out when narrower.
        (img + Vec2::new(40.0, TOOLBAR_HEIGHT + 40.0)).max(Vec2::new(1040.0, 420.0)).min(screen * 0.85)
    }

    fn commit(&mut self, f: impl FnOnce(&mut State)) {
        self.undo.push(self.state.clone());
        self.redo.clear();
        f(&mut self.state);
        self.dirty = true;
    }

    fn push(&mut self, shape: Shape) {
        let mark = Mark { shape, color: self.color, width: self.width * self.scale };
        self.commit(|s| s.marks.push(mark));
    }

    fn crop(&mut self) {
        let Some((a, b)) = self.sel.take() else { return };
        let (w, h) = self.flat.dimensions();
        let x0 = (a[0].min(b[0]).max(0.0) as u32).min(w - 1);
        let y0 = (a[1].min(b[1]).max(0.0) as u32).min(h - 1);
        let x1 = (a[0].max(b[0]) as u32).clamp(x0 + 1, w);
        let y1 = (a[1].max(b[1]) as u32).clamp(y0 + 1, h);
        let img = image::imageops::crop_imm(&self.flat, x0, y0, x1 - x0, y1 - y0).to_image();
        self.commit(|s| *s = State { base: Rc::new(img), marks: vec![] }); // crop bakes marks in
        self.fit_pending = true;
    }

    fn apply_resize(&mut self, w: u32, h: u32) {
        let img = image::imageops::resize(&self.flat, w.max(1), h.max(1), image::imageops::FilterType::Lanczos3);
        self.commit(|s| *s = State { base: Rc::new(img), marks: vec![] });
        self.fit_pending = true;
    }

    /// Cmd+V: paste clipboard text into the focused text field, or float a clipboard image for placing.
    pub fn paste(&mut self, ctx: &egui::Context) {
        let Ok(mut clipboard) = arboard::Clipboard::new() else { return };
        if self.text.is_some() || self.resize.is_some() {
            // Hand egui the paste it never saw, so the focused field inserts at its cursor.
            if let Ok(t) = clipboard.get_text() {
                ctx.input_mut(|i| i.events.push(egui::Event::Paste(t)));
            }
            return;
        }
        let Ok(data) = clipboard.get_image() else {
            self.toast("No image on the clipboard");
            return;
        };
        let Some(img) = RgbaImage::from_raw(data.width as u32, data.height as u32, data.bytes.into_owned()) else { return };
        // Center it, shrunk to at most 80% of the screenshot.
        let (bw, bh) = (self.flat.width() as f32, self.flat.height() as f32);
        let fit = (bw * 0.8 / img.width() as f32).min(bh * 0.8 / img.height() as f32).min(1.0);
        let (w, h) = (img.width() as f32 * fit, img.height() as f32 * fit);
        let a = [(bw - w) / 2.0, (bh - h) / 2.0];
        let tex = ctx.load_texture("pasted", color_image(&img), egui::TextureOptions::LINEAR);
        self.floating = Some(Floating { img: Arc::new(img), tex, a, b: [a[0] + w, a[1] + h], opacity: 1.0, drag: None });
        self.tool = Tool::Select;
    }

    fn place_floating(&mut self) {
        if let Some(f) = self.floating.take() {
            self.push(Shape::Image { a: f.a, b: f.b, img: f.img, opacity: f.opacity });
        }
    }

    fn toast(&mut self, msg: impl Into<String>) {
        self.toast = Some((msg.into(), Instant::now()));
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) -> Action {
        let ctx = ui.ctx().clone();
        if self.dirty {
            self.flat = flatten(&self.state.base, &self.state.marks);
            self.dirty = false;
            self.out_dirty = true;
        }
        if self.out_dirty {
            let b = &self.backdrop;
            self.out = if b.on {
                let px = |v: f32| v * self.scale;
                let settings = Backdrop { colors: GRADIENTS[b.gradient], padding: px(b.padding), radius: px(b.radius), shadow: px(b.shadow) };
                backdrop(&self.flat, &settings)
            } else {
                self.flat.clone()
            };
            self.tex.set(color_image(&self.out), egui::TextureOptions::NEAREST);
            self.out_dirty = false;
        }
        let typing = self.text.is_some() || self.resize.is_some();

        // --- keyboard ---
        let (cmd, shift) = ctx.input(|i| (i.modifiers.command, i.modifiers.shift));
        let pressed = |k| ctx.input(|i| i.key_pressed(k));
        if !typing {
            if !cmd && pressed(Key::K) {
                self.backdrop.on = !self.backdrop.on;
                self.out_dirty = true;
                self.fit_pending = true;
            }
            if !cmd {
                for (tool, _, key, _) in TOOLS {
                    if pressed(key) {
                        self.tool = tool;
                    }
                }
            }
            // Ruler: 1 or ←/→ for horizontal, 2 or ↑/↓ for vertical.
            if (!cmd && pressed(Key::Num1)) || pressed(Key::ArrowLeft) || pressed(Key::ArrowRight) {
                self.ruler = Some(true);
            }
            if (!cmd && pressed(Key::Num2)) || pressed(Key::ArrowUp) || pressed(Key::ArrowDown) {
                self.ruler = Some(false);
            }
            if cmd && pressed(Key::Z) {
                let (from, to) = if shift { (&mut self.redo, &mut self.undo) } else { (&mut self.undo, &mut self.redo) };
                if let Some(s) = from.pop() {
                    to.push(std::mem::replace(&mut self.state, s));
                    self.dirty = true;
                }
            }
            if ctx.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Copy))) {
                copy_image(&self.out);
                return Action::Close;
            }
            if cmd && pressed(Key::P) {
                return Action::Pin(self.out.clone(), self.scale);
            }
            if cmd && pressed(Key::S) {
                match save_image(&self.out) {
                    Ok(path) => self.toast(format!("Saved {path}")),
                    Err(e) => self.toast(format!("Save failed: {e}")),
                }
            }
            if cmd && pressed(Key::Num0) {
                self.zoom = 1.0 / self.scale;
                self.pan = Vec2::ZERO;
            }
            if cmd && pressed(Key::Num1) {
                self.fit_pending = true;
            }
            if pressed(Key::Enter) {
                if self.floating.is_some() {
                    self.place_floating();
                } else {
                    self.crop();
                }
            }
            if pressed(Key::Escape) {
                if self.floating.is_some() {
                    self.floating = None;
                } else if self.sel.is_some() || self.ruler.is_some() {
                    self.sel = None;
                    self.ruler = None;
                } else {
                    return Action::Close;
                }
            }
        }

        // --- toolbar: one bar under the hidden title bar, beside the traffic lights ---
        let mut close = false;
        let mut pin = false;
        let bar_frame = egui::Frame::NONE.fill(ui.visuals().panel_fill);
        egui::Panel::top("toolbar").exact_size(TOOLBAR_HEIGHT).frame(bar_frame).show(ui, |ui| {
            // Dragging empty toolbar space moves the window, like a native title bar.
            let drag = ui.interact(ui.max_rect(), ui.id().with("titlebar"), Sense::click_and_drag());
            if drag.drag_started() {
                ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
            }
            ui.horizontal_centered(|ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                ui.add_space(TRAFFIC_LIGHTS_WIDTH);

                if self.icon_button(ui, ACTION_ICONS[2], "Copy", false, true, "Copy (⌘C)").clicked() {
                    copy_image(&self.out);
                    close = true;
                }
                if self.icon_button(ui, ACTION_ICONS[3], "Save", false, true, "Save to Desktop (⌘S)").clicked() {
                    match save_image(&self.out) {
                        Ok(path) => self.toast(format!("Saved {path}")),
                        Err(e) => self.toast(format!("Save failed: {e}")),
                    }
                }
                if self.icon_button(ui, ACTION_ICONS[4], "Pin", false, true, "Floating Pin (⌘P)").clicked() {
                    pin = true;
                }
                let idle = self.ocr_rx.is_none();
                if self.icon_button(ui, ACTION_ICONS[6], "Text", false, idle, "Recognize Text/QR and copy it").clicked() {
                    self.ocr_rx = Some(crate::ocr::spawn(self.flat.clone()));
                    self.toast("Recognizing…");
                }
                if self.icon_button(ui, ACTION_ICONS[5], "Backdrop", self.backdrop.on, true, "Backdrop (K)").clicked() {
                    self.backdrop.on = !self.backdrop.on;
                    self.out_dirty = true;
                    self.fit_pending = true;
                }
                ui.add_space(6.0);
                ui.separator();
                ui.add_space(6.0);

                for (tool, name, key, sym) in TOOLS {
                    let tip = format!("{name} ({})", key.name());
                    if self.icon_button(ui, sym, name, self.tool == tool, true, &tip).clicked() {
                        self.tool = tool;
                    }
                }
                self.style_button(ui);

                // Right side: color under the cursor, image size, zoom — added right to left, dropped when narrow.
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add_space(10.0);
                    let zoom = format!("{:.0}%", self.zoom * self.scale * 100.0);
                    if info_block(ui, &zoom, "Zoom", None).on_hover_text("Fit to window (⌘1)").clicked() {
                        self.fit_pending = true;
                    }
                    let (w, h) = self.flat.dimensions();
                    let size = format!("{:.0}×{:.0}pt", w as f32 / self.scale, h as f32 / self.scale);
                    if ui.available_width() > 130.0 {
                        ui.separator();
                        if info_block(ui, &size, "Image size", None).on_hover_text("Resize").clicked() {
                            self.resize = Some((w.to_string(), h.to_string()));
                        }
                    }
                    if let Some([r, g, b]) = self.hover_px
                        && ui.available_width() > 140.0
                    {
                        ui.separator();
                        info_block(ui, &format!("#{r:02X}{g:02X}{b:02X}"), "Tab to copy", Some(Color32::from_rgb(r, g, b)));
                    }
                });
            });
        });
        if self.backdrop.on {
            egui::Panel::top("backdrop").show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label("Backdrop");
                    for (i, [a, b]) in GRADIENTS.iter().enumerate() {
                        let (rect, resp) = ui.allocate_exact_size(Vec2::new(28.0, 18.0), Sense::click());
                        let (l, r) = rect.split_left_right_at_fraction(0.5);
                        ui.painter().rect_filled(l, 0.0, Color32::from_rgb(a[0], a[1], a[2]));
                        ui.painter().rect_filled(r, 0.0, Color32::from_rgb(b[0], b[1], b[2]));
                        if self.backdrop.gradient == i {
                            ui.painter().rect_stroke(rect, 2.0, Stroke::new(2.0, ui.visuals().strong_text_color()), StrokeKind::Outside);
                        }
                        if resp.clicked() {
                            self.backdrop.gradient = i;
                            self.out_dirty = true;
                        }
                    }
                    ui.separator();
                    let b = &mut self.backdrop;
                    let changed = ui.add(egui::Slider::new(&mut b.padding, 0.0..=160.0).text("padding")).changed()
                        | ui.add(egui::Slider::new(&mut b.radius, 0.0..=40.0).text("corners")).changed()
                        | ui.add(egui::Slider::new(&mut b.shadow, 0.0..=60.0).text("shadow")).changed();
                    if changed {
                        self.out_dirty = true;
                    }
                });
            });
        }
        if let Some(f) = &mut self.floating {
            let mut place = false;
            let mut cancel = false;
            egui::Panel::top("pasted").show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.add_space(TRAFFIC_LIGHTS_WIDTH);
                    ui.label("Pasted image");
                    ui.add(egui::Slider::new(&mut f.opacity, 0.1..=1.0).text("opacity"));
                    place = ui.button("Place (Enter)").clicked();
                    cancel = ui.button("Cancel (Esc)").clicked();
                });
            });
            if place {
                self.place_floating();
            } else if cancel {
                self.floating = None;
            }
        }
        if close {
            return Action::Close;
        }
        if pin {
            return Action::Pin(self.out.clone(), self.scale);
        }

        self.hover_px = None;
        let canvas_bg = if ui.visuals().dark_mode { Color32::from_gray(40) } else { Color32::from_gray(222) };
        egui::CentralPanel::default().frame(egui::Frame::NONE.fill(canvas_bg)).show(ui, |ui| {
            let (rect, resp) = ui.allocate_exact_size(ui.available_size(), Sense::click_and_drag());
            let img_size = Vec2::new(self.flat.width() as f32, self.flat.height() as f32);
            let out_size = Vec2::new(self.out.width() as f32, self.out.height() as f32);
            if self.fit_pending {
                self.zoom = ((rect.width() - 20.0) / out_size.x).min((rect.height() - 40.0) / out_size.y).min(1.0 / self.scale);
                self.pan = Vec2::ZERO;
                self.fit_pending = false;
            }

            // Zoom (pinch / ⌘+scroll) around the cursor, scroll to pan, right-drag or space-drag to pan.
            let hover = resp.hover_pos();
            let origin = |zoom: f32, pan: Vec2| rect.center() + pan - img_size * zoom / 2.0;
            let (zd, scroll, right, space, delta) = ctx.input(|i| {
                (i.zoom_delta(), i.smooth_scroll_delta, i.pointer.secondary_down(), i.key_down(Key::Space), i.pointer.delta())
            });
            if zd != 1.0 && let Some(h) = hover {
                let before = (h - origin(self.zoom, self.pan)) / self.zoom;
                self.zoom = (self.zoom * zd).clamp(0.02, 40.0);
                self.pan += h - (origin(self.zoom, self.pan) + before * self.zoom);
            } else if resp.hovered() && scroll != Vec2::ZERO {
                if let Some(_) = self.ruler {
                    self.sensitivity = (self.sensitivity + scroll.y / 10.0).clamp(0.0, RULER_SENSITIVITY_MAX);
                } else {
                    self.pan += scroll;
                }
            }
            if resp.hovered() && (right || (space && resp.dragged())) {
                self.pan += delta;
            }
            let o = origin(self.zoom, self.pan);
            let zoom = self.zoom;
            let to_screen = |p: P| o + Vec2::new(p[0], p[1]) * zoom;
            let to_img = |s: Pos2| to_p(((s - o) / zoom).to_pos2());

            let p = ui.painter_at(rect);
            // The backdrop pads all sides equally, so the image stays centered within `out`.
            let img_rect = Rect::from_center_size(o + img_size * zoom / 2.0, out_size * zoom);
            p.rect_filled(img_rect.expand(1.0), 0.0, Color32::from_black_alpha(120));
            let uv = Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));
            p.image(self.tex.id(), img_rect, uv, Color32::WHITE);

            // --- tool input ---
            let panning = space || right;
            let col = Color32::from_rgb(self.color[0], self.color[1], self.color[2]);
            let stroke_pts = (self.width * self.scale * zoom).max(1.0);
            if let Some(f) = &mut self.floating
                && !panning
            {
                let r = Rect::from_two_pos(to_screen(f.a), to_screen(f.b));
                let handle = Rect::from_center_size(r.max, Vec2::splat(14.0));
                if resp.drag_started_by(egui::PointerButton::Primary) && let Some(h) = hover {
                    f.drag = if handle.contains(h) {
                        Some(FloatDrag::Resize)
                    } else if r.contains(h) {
                        Some(FloatDrag::Move)
                    } else {
                        None
                    };
                }
                if resp.dragged_by(egui::PointerButton::Primary) && let Some(h) = hover {
                    let d = resp.drag_delta() / zoom;
                    match f.drag {
                        Some(FloatDrag::Move) => {
                            f.a = [f.a[0] + d.x, f.a[1] + d.y];
                            f.b = [f.b[0] + d.x, f.b[1] + d.y];
                        }
                        Some(FloatDrag::Resize) => {
                            // Keep the aspect ratio; width follows the cursor.
                            let aspect = f.img.height() as f32 / f.img.width() as f32;
                            let w = (to_img(h)[0] - f.a[0]).max(8.0);
                            f.b = [f.a[0] + w, f.a[1] + w * aspect];
                        }
                        None => {}
                    }
                }
                if resp.drag_stopped() {
                    f.drag = None;
                }
                let place = resp.clicked() && hover.is_some_and(|h| !r.contains(h));
                let uv = Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));
                p.image(f.tex.id(), r, uv, Color32::WHITE.gamma_multiply(f.opacity));
                p.rect_stroke(r, 0.0, Stroke::new(1.0, Color32::from_rgb(0, 122, 255)), StrokeKind::Outside);
                p.rect_filled(handle.shrink(3.0), 2.0, Color32::from_rgb(0, 122, 255));
                if place {
                    self.place_floating();
                }
            } else if !panning && self.text.is_none() {
                if resp.drag_started_by(egui::PointerButton::Primary) && let Some(h) = hover {
                    self.drag = Some(vec![to_img(h)]);
                    self.ruler = None;
                }
                if let (Some(d), Some(h)) = (self.drag.as_mut(), hover)
                    && resp.dragged_by(egui::PointerButton::Primary)
                {
                    let cur = to_img(h);
                    if matches!(self.tool, Tool::Pen | Tool::Highlight) {
                        d.push(cur);
                    } else {
                        d.truncate(1);
                        d.push(cur);
                    }
                    if self.tool == Tool::Select {
                        self.sel = Some((d[0], cur));
                    }
                }
                if resp.drag_stopped()
                    && let Some(d) = self.drag.take()
                    && d.len() > 1
                {
                    let (a, b) = (d[0], *d.last().unwrap());
                    let tiny = (a[0] - b[0]).abs() + (a[1] - b[1]).abs() < 3.0;
                    match self.tool {
                        Tool::Select | Tool::Text | Tool::Step => {}
                        Tool::Pen => self.push(Shape::Pen { points: d }),
                        Tool::Highlight => self.push(Shape::Highlight { points: d }),
                        _ if tiny => {}
                        Tool::Arrow => self.push(Shape::Arrow { from: a, to: b }),
                        Tool::Line => self.push(Shape::Line { from: a, to: b }),
                        Tool::Rect => self.push(Shape::Rect { a, b }),
                        Tool::Oval => self.push(Shape::Oval { a, b }),
                        Tool::Pixelate => self.push(Shape::Pixelate { a, b }),
                        Tool::Spotlight => self.push(Shape::Spotlight { a, b }),
                        Tool::Magnifier => self.push(Shape::Magnifier {
                            target: a,
                            center: b,
                            radius: self.width * MAGNIFIER_RADIUS_PER_SIZE * self.scale,
                            zoom: MAGNIFIER_ZOOM,
                        }),
                    }
                }
                if resp.clicked() && let Some(h) = hover {
                    let at = to_img(h);
                    match self.tool {
                        Tool::Text => self.text = Some((at, String::new())),
                        Tool::Step => {
                            let n = 1 + self.state.marks.iter().filter(|m| matches!(m.shape, Shape::Step { .. })).count() as u32;
                            self.push(Shape::Step { at, n });
                        }
                        Tool::Select => self.sel = None,
                        _ => {}
                    }
                }
            }

            // --- live preview of the shape being drawn (final pixels come from `flatten`) ---
            if let Some(d) = &self.drag {
                let pts: Vec<Pos2> = d.iter().map(|q| to_screen(*q)).collect();
                let (a, b) = (pts[0], *pts.last().unwrap());
                let r = Rect::from_two_pos(a, b);
                let s = Stroke::new(stroke_pts, col);
                match self.tool {
                    Tool::Arrow => {
                        p.arrow(a, b - a, s);
                    }
                    Tool::Line => {
                        p.line_segment([a, b], s);
                    }
                    Tool::Rect => {
                        p.rect_stroke(r, 0.0, s, StrokeKind::Middle);
                    }
                    Tool::Oval => {
                        p.add(egui::Shape::ellipse_stroke(r.center(), r.size() / 2.0, s));
                    }
                    Tool::Pen => {
                        p.line(pts, s);
                    }
                    Tool::Highlight => {
                        p.line(pts, Stroke::new(stroke_pts * 6.0, col.gamma_multiply(0.35)));
                    }
                    Tool::Pixelate | Tool::Spotlight => {
                        p.rect_stroke(r, 0.0, Stroke::new(1.0, Color32::WHITE), StrokeKind::Outside);
                    }
                    _ => {}
                }
            }
            if let Some((a, b)) = self.sel {
                let r = Rect::from_two_pos(to_screen(a), to_screen(b));
                p.rect_stroke(r, 0.0, Stroke::new(1.0, Color32::WHITE), StrokeKind::Outside);
                p.rect_stroke(r.expand(1.0), 0.0, Stroke::new(1.0, Color32::BLACK), StrokeKind::Outside);
                let size = format!("{:.0} × {:.0}  (Enter to crop)", (a[0] - b[0]).abs() / self.scale, (a[1] - b[1]).abs() / self.scale);
                p.text(r.left_top() - Vec2::new(0.0, 4.0), egui::Align2::LEFT_BOTTOM, size, egui::FontId::monospace(12.0), Color32::WHITE);
            }

            // --- live magnifier preview: real enlarged pixels, before and while dragging ---
            if self.tool == Tool::Magnifier
                && self.floating.is_none()
                && let Some(h) = hover
            {
                let radius = self.width * MAGNIFIER_RADIUS_PER_SIZE * self.scale * zoom; // screen points
                let (target, bubble) = match &self.drag {
                    Some(d) => (to_screen(d[0]), h),
                    None => (h, h),
                };
                // Texture coordinates of the target: `tex` shows `out`, which may include backdrop padding.
                let pad = (self.out.width() - self.flat.width()) as f32 / 2.0;
                let out_size = Vec2::new(self.out.width() as f32, self.out.height() as f32);
                let t = to_img(target);
                let uv_center = Pos2::new((t[0] + pad) / out_size.x, (t[1] + pad) / out_size.y);
                let src_radius = radius / zoom / MAGNIFIER_ZOOM; // image pixels shown around the target
                let uv_radius = Vec2::new(src_radius / out_size.x, src_radius / out_size.y);
                p.add(textured_circle(self.tex.id(), bubble, radius, uv_center, uv_radius));
                let s = Stroke::new(stroke_pts.max(2.0), col);
                p.circle_stroke(bubble, radius, s);
                if self.drag.is_some() {
                    let ring = radius / MAGNIFIER_ZOOM;
                    p.circle_stroke(target, ring, s);
                    let d = bubble - target;
                    if d.length() > radius + ring {
                        let u = d.normalized();
                        p.line_segment([bubble - u * radius, target + u * ring], s);
                    }
                }
            }

            // --- ruler + pixel info ---
            if let Some(h) = hover {
                let q = to_img(h);
                let (w, hh) = self.flat.dimensions();
                if q[0] >= 0.0 && q[1] >= 0.0 && (q[0] as u32) < w && (q[1] as u32) < hh {
                    let (x, y) = (q[0] as u32, q[1] as u32);
                    let [r, g, b, _] = self.flat.get_pixel(x, y).0;
                    let hex = format!("#{r:02X}{g:02X}{b:02X}");
                    self.hover_px = Some([r, g, b]);
                    if ctx.input(|i| i.key_pressed(Key::Tab)) && !typing {
                        arboard::Clipboard::new().and_then(|mut cb| cb.set_text(&hex)).ok();
                        self.toast(format!("Copied {hex}"));
                    }
                    if let Some(horizontal) = self.ruler {
                        let (s0, s1) = span(&self.flat, x, y, horizontal, self.sensitivity);
                        let (s0, s1) = (s0 as f32, s1 as f32);
                        let (p0, p1) = if horizontal { ([s0, q[1]], [s1, q[1]]) } else { ([q[0], s0], [q[0], s1]) };
                        p.line_segment([to_screen(p0), to_screen(p1)], Stroke::new(1.5, Color32::from_rgb(255, 60, 120)));
                        let label = format!("{:.0} px  ±{:.1} (scroll)", (s1 - s0) / self.scale, self.sensitivity);
                        p.text(h + Vec2::new(10.0, 10.0), egui::Align2::LEFT_TOP, label, egui::FontId::monospace(12.0), Color32::WHITE);
                    }
                }
            }

            // --- text entry ---
            if let Some((at, text)) = &mut self.text {
                let pos = to_screen(*at);
                let size = text_size(self.width * self.scale) * zoom;
                let mut done = false;
                egui::Area::new(egui::Id::new("text-entry")).fixed_pos(pos).order(egui::Order::Foreground).show(&ctx, |ui| {
                    let edit = egui::TextEdit::multiline(text)
                        .font(egui::FontId::proportional(size.max(10.0)))
                        .text_color(col)
                        .desired_rows(1)
                        .desired_width(400.0)
                        .hint_text("Type, Enter to place, Shift+Enter newline");
                    let r = ui.add(edit);
                    r.request_focus();
                    done = ctx.input(|i| i.key_pressed(Key::Enter) && !i.modifiers.shift) || ctx.input(|i| i.key_pressed(Key::Escape));
                });
                if done {
                    let (at, text) = self.text.take().unwrap();
                    let text = text.trim_end().to_string();
                    if !text.is_empty() && !ctx.input(|i| i.key_pressed(Key::Escape)) {
                        self.push(Shape::Text { at, text });
                    }
                }
            }
        });

        // --- resize dialog ---
        if let Some((ws, hs)) = &mut self.resize {
            let (w0, h0) = self.flat.dimensions();
            let mut apply = None;
            let mut cancel = false;
            egui::Window::new("Resize (pixels)").collapsible(false).resizable(false).anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO).show(&ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label("W");
                    if ui.text_edit_singleline(ws).changed() && let Ok(w) = ws.parse::<f32>() {
                        *hs = ((w * h0 as f32 / w0 as f32).round() as u32).to_string();
                    }
                    ui.label("H");
                    if ui.text_edit_singleline(hs).changed() && let Ok(h) = hs.parse::<f32>() {
                        *ws = ((h * w0 as f32 / h0 as f32).round() as u32).to_string();
                    }
                });
                ui.horizontal(|ui| {
                    for pct in [25, 50, 75, 200] {
                        if ui.button(format!("{pct}%")).clicked() {
                            *ws = (w0 * pct / 100).to_string();
                            *hs = (h0 * pct / 100).to_string();
                        }
                    }
                });
                ui.horizontal(|ui| {
                    if ui.button("Apply").clicked() {
                        apply = ws.parse::<u32>().ok().zip(hs.parse::<u32>().ok());
                    }
                    cancel = ui.button("Cancel").clicked();
                });
            });
            if let Some((w, h)) = apply {
                self.resize = None;
                self.apply_resize(w.min(16384), h.min(16384));
            } else if cancel {
                self.resize = None;
            }
        }

        if let Some(rx) = &self.ocr_rx {
            match rx.try_recv() {
                Ok(lines) => {
                    let msg = crate::ocr::copy_result(&lines);
                    self.ocr_rx = None;
                    self.toast(msg);
                }
                Err(_) => ctx.request_repaint_after(std::time::Duration::from_millis(50)),
            }
        }

        // --- toast ---
        let fg = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::new("toast")));
        let screen = ctx.content_rect();
        if let Some((msg, t)) = &self.toast {
            if t.elapsed().as_secs_f32() < 2.0 {
                let galley = fg.layout_no_wrap(msg.clone(), egui::FontId::proportional(14.0), Color32::WHITE);
                let at = Pos2::new(screen.center().x - galley.size().x / 2.0, screen.min.y + TOOLBAR_HEIGHT + 16.0);
                fg.rect_filled(Rect::from_min_size(at, galley.size()).expand2(Vec2::new(14.0, 8.0)), 10.0, Color32::from_black_alpha(200));
                fg.galley(at, galley, Color32::WHITE);
                ctx.request_repaint();
            } else {
                self.toast = None;
            }
        }
        Action::None
    }
}

/// Value over caption, e.g. "840×582pt" / "Image size"; optional color swatch on the left.
fn info_block(ui: &mut egui::Ui, value: &str, caption: &str, swatch: Option<Color32>) -> egui::Response {
    let (strong, weak) = (ui.visuals().strong_text_color(), ui.visuals().weak_text_color());
    let v = ui.painter().layout_no_wrap(value.to_owned(), egui::FontId::proportional(14.0), strong);
    let c = ui.painter().layout_no_wrap(caption.to_owned(), egui::FontId::proportional(11.0), weak);
    let swatch_w = if swatch.is_some() { 30.0 } else { 0.0 };
    let size = Vec2::new(swatch_w + v.size().x.max(c.size().x) + 16.0, 40.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let p = ui.painter();
    if resp.hovered() {
        p.rect_filled(rect, 6.0, ui.visuals().widgets.hovered.weak_bg_fill);
    }
    let mut x = rect.min.x + 8.0;
    if let Some(col) = swatch {
        let center = Pos2::new(x + 11.0, rect.center().y);
        p.circle_filled(center, 11.0, col);
        p.circle_stroke(center, 11.0, Stroke::new(1.0, weak.gamma_multiply(0.5)));
        x += swatch_w;
    }
    p.galley(Pos2::new(x, rect.center().y - v.size().y), v, strong);
    p.galley(Pos2::new(x, rect.center().y + 1.0), c, weak);
    resp
}

impl Editor {
    /// Frameless toolbar icon; `selected` draws a rounded highlight behind it.
    fn icon_button(&self, ui: &mut egui::Ui, sym: &str, fallback: &str, selected: bool, enabled: bool, tip: &str) -> egui::Response {
        let sense = if enabled { Sense::click() } else { Sense::hover() };
        let (rect, resp) = ui.allocate_exact_size(Vec2::new(34.0, 34.0), sense);
        let visuals = ui.visuals();
        let bg = if selected {
            Some(visuals.widgets.active.weak_bg_fill)
        } else if resp.hovered() && enabled {
            Some(visuals.widgets.hovered.weak_bg_fill)
        } else {
            None
        };
        if let Some(bg) = bg {
            ui.painter().rect_filled(rect, 8.0, bg);
        }
        let tint = if enabled { visuals.strong_text_color() } else { visuals.weak_text_color() };
        match self.icons.get(sym) {
            Some(t) => {
                let img = egui::Image::new((t.id(), Vec2::splat(20.0))).tint(tint);
                img.paint_at(ui, Rect::from_center_size(rect.center(), Vec2::splat(20.0)));
            }
            None => {
                ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, fallback, egui::FontId::proportional(11.0), tint);
            }
        }
        resp.on_hover_text(tip)
    }

    /// Current annotation color and stroke size; opens a popover with presets, a picker and a size slider.
    fn style_button(&mut self, ui: &mut egui::Ui) {
        let (rect, resp) = ui.allocate_exact_size(Vec2::new(34.0, 34.0), Sense::click());
        if resp.hovered() {
            ui.painter().rect_filled(rect, 8.0, ui.visuals().widgets.hovered.weak_bg_fill);
        }
        let col = Color32::from_rgb(self.color[0], self.color[1], self.color[2]);
        ui.painter().circle_filled(rect.center(), 9.0, col);
        ui.painter().circle_stroke(rect.center(), 11.0, Stroke::new(1.5, ui.visuals().weak_text_color()));
        let resp = resp.on_hover_text("Color & size");
        egui::Popup::from_toggle_button_response(&resp)
            .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
            .show(|ui| {
                ui.horizontal(|ui| {
                    for c in COLORS {
                        let (r, resp) = ui.allocate_exact_size(Vec2::splat(22.0), Sense::click());
                        ui.painter().circle_filled(r.center(), 9.0, Color32::from_rgb(c[0], c[1], c[2]));
                        if self.color == c {
                            ui.painter().circle_stroke(r.center(), 10.5, Stroke::new(2.0, ui.visuals().strong_text_color()));
                        }
                        if resp.clicked() {
                            self.color = c;
                        }
                    }
                    ui.color_edit_button_srgb(&mut self.color);
                });
                ui.add(egui::Slider::new(&mut self.width, 1.0..=12.0).text("size"));
            });
    }
}

/// A disc showing the texture area around `uv_center` (radius `uv_radius` in UV units), drawn at `center`.
fn textured_circle(tex: egui::TextureId, center: Pos2, radius: f32, uv_center: Pos2, uv_radius: Vec2) -> egui::Shape {
    const SEGMENTS: u32 = 72;
    let mut mesh = egui::Mesh::with_texture(tex);
    mesh.vertices.push(egui::epaint::Vertex { pos: center, uv: uv_center, color: Color32::WHITE });
    for i in 0..=SEGMENTS {
        let angle = i as f32 / SEGMENTS as f32 * std::f32::consts::TAU;
        let dir = Vec2::angled(angle);
        mesh.vertices.push(egui::epaint::Vertex {
            pos: center + dir * radius,
            uv: uv_center + Vec2::new(dir.x * uv_radius.x, dir.y * uv_radius.y),
            color: Color32::WHITE,
        });
        if i > 0 {
            mesh.add_triangle(0, i, i + 1);
        }
    }
    egui::Shape::mesh(mesh)
}

fn color_image(img: &RgbaImage) -> egui::ColorImage {
    egui::ColorImage::from_rgba_unmultiplied([img.width() as usize, img.height() as usize], img.as_raw())
}
