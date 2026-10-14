//! The editor window that opens after a capture: annotate, crop, resize, copy and save.

use crate::render::{Backdrop, Mark, P, Shape, backdrop, flatten, text_size};
use crate::{RULER_SENSITIVITY, RULER_SENSITIVITY_MAX, copy_image, save_image, span};
use eframe::egui::{self, Color32, Key, Pos2, Rect, Sense, Stroke, StrokeKind, Vec2};
use image::RgbaImage;
use std::collections::HashMap;
use std::rc::Rc;
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
}

// (tool, name, keyboard shortcut, SF Symbol used as the toolbar icon).
const TOOLS: [(Tool, &str, Key, &str); 11] = [
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
];
const ACTION_ICONS: [&str; 6] =
    ["arrow.uturn.backward", "arrow.up.left.and.arrow.down.right", "doc.on.doc", "square.and.arrow.down", "pin", "rectangle.inset.filled"];

// Backdrop gradient presets (top-left → bottom-right).
const GRADIENTS: [[[u8; 3]; 2]; 6] = [
    [[94, 114, 235], [255, 145, 175]],
    [[255, 94, 98], [255, 195, 113]],
    [[67, 206, 162], [24, 90, 157]],
    [[161, 140, 209], [251, 194, 235]],
    [[240, 242, 245], [205, 211, 220]],
    [[32, 34, 44], [74, 78, 104]],
];

/// Backdrop settings in points (converted to image pixels when rendering).
struct BackdropUi {
    on: bool,
    gradient: usize,
    padding: f32,
    radius: f32,
    shadow: f32,
}

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
            icons,
        }
    }

    /// Window size (points) that shows the image at 100% when it fits on screen.
    pub fn preferred_size(&self, screen: Vec2) -> Vec2 {
        let img = Vec2::new(self.out.width() as f32, self.out.height() as f32) / self.scale;
        (img + Vec2::new(0.0, 80.0)).max(Vec2::new(760.0, 420.0)).min(screen * 0.85)
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
            if cmd && pressed(Key::C) {
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
                self.crop();
            }
            if pressed(Key::Escape) {
                if self.sel.is_some() || self.ruler.is_some() {
                    self.sel = None;
                    self.ruler = None;
                } else {
                    return Action::Close;
                }
            }
        }

        // --- toolbar ---
        let mut close = false;
        let mut pin = false;
        egui::Panel::top("toolbar").show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                for (tool, name, key, sym) in TOOLS {
                    let tip = format!("{name} ({})", key.name());
                    let resp = match self.icons.get(sym) {
                        Some(t) => ui.add(egui::Button::image(icon(t, ui)).selected(self.tool == tool)),
                        None => ui.selectable_label(self.tool == tool, name),
                    };
                    if resp.on_hover_text(tip).clicked() {
                        self.tool = tool;
                    }
                }
                ui.separator();
                for (i, c) in COLORS.iter().enumerate() {
                    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(18.0), Sense::click());
                    ui.painter().circle_filled(rect.center(), 8.0, Color32::from_rgb(c[0], c[1], c[2]));
                    if self.color == *c {
                        ui.painter().circle_stroke(rect.center(), 9.5, Stroke::new(2.0, ui.visuals().strong_text_color()));
                    }
                    let _ = i;
                    if resp.clicked() {
                        self.color = *c;
                    }
                }
                ui.color_edit_button_srgb(&mut self.color);
                ui.add(egui::Slider::new(&mut self.width, 1.0..=12.0).text("size"));
                ui.separator();
                if ui.add_enabled(!self.undo.is_empty(), self.action_button(ui, ACTION_ICONS[0], "↶")).on_hover_text("Undo (⌘Z)").clicked() {
                    let s = self.undo.pop().unwrap();
                    self.redo.push(std::mem::replace(&mut self.state, s));
                    self.dirty = true;
                }
                if ui.add_enabled(self.sel.is_some(), egui::Button::new("Crop")).on_hover_text("Enter").clicked() {
                    self.crop();
                }
                let (w, h) = self.flat.dimensions();
                let size = format!("{:.0}×{:.0}", w as f32 / self.scale, h as f32 / self.scale);
                let resize_btn = match self.icons.get(ACTION_ICONS[1]) {
                    Some(t) => egui::Button::image_and_text(icon(t, ui), size),
                    None => egui::Button::new(size),
                };
                if ui.add(resize_btn).on_hover_text("Resize").clicked() {
                    self.resize = Some((w.to_string(), h.to_string()));
                }
                if ui.add(self.action_button(ui, ACTION_ICONS[2], "Copy")).on_hover_text("Copy (⌘C)").clicked() {
                    copy_image(&self.out);
                    close = true;
                }
                let backdrop_btn = self.action_button(ui, ACTION_ICONS[5], "Backdrop").selected(self.backdrop.on);
                if ui.add(backdrop_btn).on_hover_text("Backdrop (K)").clicked() {
                    self.backdrop.on = !self.backdrop.on;
                    self.out_dirty = true;
                    self.fit_pending = true;
                }
                if ui.add(self.action_button(ui, ACTION_ICONS[4], "Pin")).on_hover_text("Floating Pin (⌘P)").clicked() {
                    pin = true;
                }
                if ui.add(self.action_button(ui, ACTION_ICONS[3], "Save")).on_hover_text("Save to Desktop (⌘S)").clicked() {
                    match save_image(&self.out) {
                        Ok(path) => self.toast(format!("Saved {path}")),
                        Err(e) => self.toast(format!("Save failed: {e}")),
                    }
                }
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
        if close {
            return Action::Close;
        }
        if pin {
            return Action::Pin(self.out.clone(), self.scale);
        }

        // --- status bar (filled in after we know the hover pixel) ---
        let mut status = String::new();

        egui::CentralPanel::default().frame(egui::Frame::NONE.fill(Color32::from_gray(40))).show(ui, |ui| {
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
            if !panning && self.text.is_none() {
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

            // --- ruler + pixel info ---
            if let Some(h) = hover {
                let q = to_img(h);
                let (w, hh) = self.flat.dimensions();
                if q[0] >= 0.0 && q[1] >= 0.0 && (q[0] as u32) < w && (q[1] as u32) < hh {
                    let (x, y) = (q[0] as u32, q[1] as u32);
                    let [r, g, b, _] = self.flat.get_pixel(x, y).0;
                    let hex = format!("#{r:02X}{g:02X}{b:02X}");
                    status = format!("{:.0}, {:.0}   {hex}   Tab copies color", x as f32 / self.scale, y as f32 / self.scale);
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

        // --- status bar + toast ---
        let zoom_pct = self.zoom * self.scale * 100.0;
        let fg = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::new("status")));
        let screen = ctx.content_rect();
        let line = format!("{status}   {zoom_pct:.0}%   ⌘1 fit · ⌘0 100% · pinch/⌘scroll zoom · ←→↑↓ ruler");
        fg.text(screen.left_bottom() + Vec2::new(8.0, -6.0), egui::Align2::LEFT_BOTTOM, line, egui::FontId::monospace(11.0), Color32::from_gray(200));
        if let Some((msg, t)) = &self.toast {
            if t.elapsed().as_secs_f32() < 2.0 {
                fg.text(screen.center_top() + Vec2::new(0.0, 60.0), egui::Align2::CENTER_TOP, msg, egui::FontId::proportional(14.0), Color32::WHITE);
                ctx.request_repaint();
            } else {
                self.toast = None;
            }
        }
        Action::None
    }
}

fn icon<'a>(t: &egui::TextureHandle, ui: &egui::Ui) -> egui::Image<'a> {
    egui::Image::new((t.id(), Vec2::splat(18.0))).tint(ui.visuals().text_color())
}

impl Editor {
    fn action_button<'a>(&self, ui: &egui::Ui, sym: &str, fallback: &'a str) -> egui::Button<'a> {
        match self.icons.get(sym) {
            Some(t) => egui::Button::image(icon(t, ui)),
            None => egui::Button::new(fallback),
        }
    }
}

fn color_image(img: &RgbaImage) -> egui::ColorImage {
    egui::ColorImage::from_rgba_unmultiplied([img.width() as usize, img.height() as usize], img.as_raw())
}
