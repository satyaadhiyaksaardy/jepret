//! Floating pins: screenshots kept on screen as borderless, always-on-top windows.

use crate::{copy_image, save_image};
use eframe::egui::{self, Color32, Key, Pos2, Rect, Sense, Stroke, StrokeKind, Vec2, ViewportCommand};
use image::RgbaImage;

pub struct Pin {
    id: egui::ViewportId,
    img: RgbaImage,
    tex: egui::TextureHandle,
    base: Vec2, // size in points at 100%
    zoom: f32,
    pos: Pos2, // initial position; afterwards the window is moved natively by dragging
    closed: bool,
}

impl Pin {
    /// `scale` is image pixels per point; `display` is the display (global points) to center it on;
    /// `index` staggers new pins so they don't stack exactly.
    pub fn new(ctx: &egui::Context, img: RgbaImage, scale: f32, display: Rect, index: u64) -> Self {
        let screen = display.size();
        let color = egui::ColorImage::from_rgba_unmultiplied([img.width() as usize, img.height() as usize], img.as_raw());
        let tex = ctx.load_texture(format!("pin-{index}"), color, egui::TextureOptions::LINEAR);
        let base = Vec2::new(img.width() as f32, img.height() as f32) / scale;
        // Start at 100%, shrunk to fit if the image is larger than 60% of the screen.
        let zoom = (screen.x * 0.6 / base.x).min(screen.y * 0.6 / base.y).min(1.0);
        let offset = (index % 8) as f32 * 24.0;
        let pos = display.min + (screen - base * zoom) / 2.0 + Vec2::splat(offset);
        Self { id: egui::ViewportId::from_hash_of(("pin", index)), img, tex, base, zoom, pos, closed: false }
    }

    pub fn is_closed(&self) -> bool {
        self.closed
    }

    pub fn show(&mut self, ctx: &egui::Context) {
        let size = self.base * self.zoom;
        let builder = egui::ViewportBuilder::default()
            .with_title("Snip Pin")
            .with_decorations(false)
            .with_resizable(false)
            .with_always_on_top()
            .with_position(self.pos)
            .with_inner_size(size);
        ctx.show_viewport_immediate(self.id, builder, |ui, _| {
            let ctx = ui.ctx().clone();
            let rect = ctx.content_rect();
            let resp = ui.allocate_rect(rect, Sense::click_and_drag());
            let uv = Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));
            ui.painter().image(self.tex.id(), rect, uv, Color32::WHITE);
            ui.painter().rect_stroke(rect, 0.0, Stroke::new(1.0, Color32::from_white_alpha(90)), StrokeKind::Inside);

            if resp.drag_started() {
                ctx.send_viewport_cmd(ViewportCommand::StartDrag);
            }
            // Scroll (or pinch) to resize around the window's top-left corner.
            let (scroll, pinch) = ctx.input(|i| (i.smooth_scroll_delta.y, i.zoom_delta()));
            let factor = pinch * (1.0 + scroll / 400.0);
            if factor != 1.0 && resp.hovered() {
                self.zoom = (self.zoom * factor).clamp(0.1, 4.0);
                ctx.send_viewport_cmd(ViewportCommand::InnerSize(self.base * self.zoom));
            }

            let mut close = resp.double_clicked() || ctx.input(|i| i.key_pressed(Key::Escape) || i.viewport().close_requested());
            if ctx.input(|i| i.modifiers.command && i.key_pressed(Key::C)) {
                copy_image(&self.img);
            }
            resp.context_menu(|ui| {
                if ui.button("Copy").clicked() {
                    copy_image(&self.img);
                    ui.close();
                }
                if ui.button("Save to Desktop").clicked() {
                    save_image(&self.img).map_err(|e| eprintln!("save failed: {e}")).ok();
                    ui.close();
                }
                if ui.button("Actual Size").clicked() {
                    self.zoom = 1.0;
                    ctx.send_viewport_cmd(ViewportCommand::InnerSize(self.base));
                    ui.close();
                }
                ui.separator();
                if ui.button("Close").clicked() {
                    close = true;
                    ui.close();
                }
            });
            self.closed |= close;
        });
    }
}
