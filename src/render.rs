//! Annotation model + the one renderer that bakes annotations into pixels.
//! The editor shows the output of `flatten`, so what you see is exactly what gets copied/saved.

use ab_glyph::{Font, FontRef, PxScale, ScaleFont, point};
use image::RgbaImage;
use std::sync::OnceLock;
use tiny_skia::{FillRule, IntSize, LineCap, LineJoin, Paint, PathBuilder, Pixmap, Stroke, Transform};

pub type P = [f32; 2]; // image pixels

#[derive(Clone, Debug)]
pub enum Shape {
    Arrow { from: P, to: P },
    Line { from: P, to: P },
    Rect { a: P, b: P },
    Oval { a: P, b: P },
    Pen { points: Vec<P> },
    Highlight { points: Vec<P> },
    Pixelate { a: P, b: P },
    Spotlight { a: P, b: P },
    Text { at: P, text: String },
    Step { at: P, n: u32 },
}

#[derive(Clone, Debug)]
pub struct Mark {
    pub shape: Shape,
    pub color: [u8; 3],
    pub width: f32, // stroke width in image pixels; text/step sizes scale from it
}

pub fn text_size(width: f32) -> f32 {
    width * 7.0
}
pub fn step_radius(width: f32) -> f32 {
    width * 4.0 + 4.0
}

fn font() -> FontRef<'static> {
    static BYTES: OnceLock<Vec<u8>> = OnceLock::new();
    let bytes = BYTES.get_or_init(|| {
        std::fs::read("/System/Library/Fonts/Supplemental/Arial Bold.ttf")
            .unwrap_or_else(|_| epaint_default_fonts::UBUNTU_LIGHT.to_vec())
    });
    FontRef::try_from_slice(bytes).expect("font")
}

fn paint(color: [u8; 3], alpha: u8) -> Paint<'static> {
    let mut p = Paint::default();
    p.set_color_rgba8(color[0], color[1], color[2], alpha);
    p.anti_alias = true;
    p
}

fn round_stroke(width: f32) -> Stroke {
    Stroke { width, line_cap: LineCap::Round, line_join: LineJoin::Round, ..Default::default() }
}

fn norm(a: P, b: P) -> (P, P) {
    ([a[0].min(b[0]), a[1].min(b[1])], [a[0].max(b[0]), a[1].max(b[1])])
}

fn polyline(points: &[P]) -> Option<tiny_skia::Path> {
    let mut pb = PathBuilder::new();
    pb.move_to(points.first()?[0], points[0][1]);
    for p in &points[1..] {
        pb.line_to(p[0], p[1]);
    }
    if points.len() == 1 {
        pb.line_to(points[0][0] + 0.01, points[0][1]); // a dot
    }
    pb.finish()
}

fn pixelate(pm: &mut Pixmap, a: P, b: P, block: u32) {
    let (w, h) = (pm.width(), pm.height());
    let (min, max) = norm(a, b);
    let (x0, y0) = ((min[0].max(0.0) as u32).min(w), (min[1].max(0.0) as u32).min(h));
    let (x1, y1) = ((max[0].max(0.0) as u32).min(w), (max[1].max(0.0) as u32).min(h));
    let data = pm.data_mut();
    for by in (y0..y1).step_by(block as usize) {
        for bx in (x0..x1).step_by(block as usize) {
            let (ex, ey) = ((bx + block).min(x1), (by + block).min(y1));
            let mut sum = [0u32; 4];
            for y in by..ey {
                for x in bx..ex {
                    let i = ((y * w + x) * 4) as usize;
                    (0..4).for_each(|c| sum[c] += data[i + c] as u32);
                }
            }
            let n = (ex - bx) * (ey - by);
            for y in by..ey {
                for x in bx..ex {
                    let i = ((y * w + x) * 4) as usize;
                    (0..4).for_each(|c| data[i + c] = (sum[c] / n) as u8);
                }
            }
        }
    }
}

fn draw_text(pm: &mut Pixmap, at: P, text: &str, size: f32, color: [u8; 3], center: bool) {
    let font = font();
    let sf = font.as_scaled(PxScale::from(size));
    let (w, h) = (pm.width() as i32, pm.height() as i32);
    let data = pm.data_mut();
    for (line_no, line) in text.lines().enumerate() {
        let line_w: f32 = line.chars().map(|c| sf.h_advance(font.glyph_id(c))).sum();
        let mut x = if center { at[0] - line_w / 2.0 } else { at[0] };
        let baseline = if center { at[1] + (sf.ascent() + sf.descent()) / 2.0 } else { at[1] + sf.ascent() };
        let y = baseline + line_no as f32 * (sf.height() + sf.line_gap());
        for c in line.chars() {
            let glyph = sf.scaled_glyph(c);
            let adv = sf.h_advance(glyph.id);
            let glyph = ab_glyph::Glyph { position: point(x, y), ..glyph };
            x += adv;
            let Some(outline) = font.outline_glyph(glyph) else { continue };
            let bounds = outline.px_bounds();
            outline.draw(|gx, gy, cov| {
                let (px, py) = (bounds.min.x as i32 + gx as i32, bounds.min.y as i32 + gy as i32);
                if px < 0 || py < 0 || px >= w || py >= h {
                    return;
                }
                let i = ((py * w + px) * 4) as usize;
                for c in 0..3 {
                    data[i + c] = (data[i + c] as f32 * (1.0 - cov) + color[c] as f32 * cov) as u8;
                }
                data[i + 3] = data[i + 3].max((cov * 255.0) as u8);
            });
        }
    }
}

/// Bake marks into a copy of `base`, in order (so pixelate only hides what's under it at that point).
pub fn flatten(base: &RgbaImage, marks: &[Mark]) -> RgbaImage {
    let (w, h) = base.dimensions();
    let mut pm = Pixmap::from_vec(base.as_raw().clone(), IntSize::from_wh(w, h).unwrap()).unwrap();
    let id = Transform::identity();
    for m in marks {
        let c = m.color;
        match &m.shape {
            Shape::Arrow { from, to } => {
                let (dx, dy) = (to[0] - from[0], to[1] - from[1]);
                let len = (dx * dx + dy * dy).sqrt().max(0.001);
                let (ux, uy) = (dx / len, dy / len);
                let head = (m.width * 4.5).min(len);
                let base_pt = [to[0] - ux * head, to[1] - uy * head];
                if let Some(path) = polyline(&[*from, base_pt]) {
                    pm.stroke_path(&path, &paint(c, 255), &round_stroke(m.width), id, None);
                }
                let mut pb = PathBuilder::new();
                pb.move_to(to[0], to[1]);
                pb.line_to(base_pt[0] - uy * head * 0.5, base_pt[1] + ux * head * 0.5);
                pb.line_to(base_pt[0] + uy * head * 0.5, base_pt[1] - ux * head * 0.5);
                pb.close();
                if let Some(path) = pb.finish() {
                    pm.fill_path(&path, &paint(c, 255), FillRule::Winding, id, None);
                }
            }
            Shape::Line { from, to } => {
                if let Some(path) = polyline(&[*from, *to]) {
                    pm.stroke_path(&path, &paint(c, 255), &round_stroke(m.width), id, None);
                }
            }
            Shape::Rect { a, b } | Shape::Oval { a, b } => {
                let (min, max) = norm(*a, *b);
                let Some(r) = tiny_skia::Rect::from_ltrb(min[0], min[1], max[0].max(min[0] + 1.0), max[1].max(min[1] + 1.0)) else { continue };
                let path = if matches!(m.shape, Shape::Oval { .. }) { PathBuilder::from_oval(r) } else { Some(PathBuilder::from_rect(r)) };
                if let Some(path) = path {
                    pm.stroke_path(&path, &paint(c, 255), &round_stroke(m.width), id, None);
                }
            }
            Shape::Pen { points } => {
                if let Some(path) = polyline(points) {
                    pm.stroke_path(&path, &paint(c, 255), &round_stroke(m.width), id, None);
                }
            }
            Shape::Highlight { points } => {
                if let Some(path) = polyline(points) {
                    // Plain alpha blend rather than a multiply blend; works well on light backgrounds.
                    let s = Stroke { width: m.width * 6.0, line_cap: LineCap::Butt, line_join: LineJoin::Round, ..Default::default() };
                    pm.stroke_path(&path, &paint(c, 90), &s, id, None);
                }
            }
            Shape::Pixelate { a, b } => pixelate(&mut pm, *a, *b, (m.width * 3.0).max(6.0) as u32),
            Shape::Spotlight { a, b } => {
                // Dim everything except the rectangle.
                let (min, max) = norm(*a, *b);
                let mut pb = PathBuilder::new();
                pb.push_rect(tiny_skia::Rect::from_xywh(0.0, 0.0, w as f32, h as f32).unwrap());
                if let Some(r) = tiny_skia::Rect::from_ltrb(min[0], min[1], max[0].max(min[0] + 1.0), max[1].max(min[1] + 1.0)) {
                    pb.push_rect(r);
                }
                if let Some(path) = pb.finish() {
                    pm.fill_path(&path, &paint([0, 0, 0], 140), FillRule::EvenOdd, id, None);
                }
            }
            Shape::Text { at, text } => draw_text(&mut pm, *at, text, text_size(m.width), c, false),
            Shape::Step { at, n } => {
                let r = step_radius(m.width);
                if let Some(path) = PathBuilder::from_circle(at[0], at[1], r) {
                    pm.fill_path(&path, &paint(c, 255), FillRule::Winding, id, None);
                }
                draw_text(&mut pm, *at, &n.to_string(), r * 1.2, [255, 255, 255], true);
            }
        }
    }
    // Base is opaque, so premultiplied == straight alpha here.
    RgbaImage::from_raw(w, h, pm.take()).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mark(shape: Shape) -> Mark {
        Mark { shape, color: [255, 0, 0], width: 4.0 }
    }

    #[test]
    fn flatten_draws_marks() {
        let mut base = RgbaImage::from_pixel(100, 100, image::Rgba([255, 255, 255, 255]));
        for x in 0..100 {
            base.put_pixel(x, 50, image::Rgba([0, 0, 0, 255])); // a line for pixelate to smear
        }
        let white = image::Rgba([255, 255, 255, 255]);

        let out = flatten(&base, &[mark(Shape::Rect { a: [10.0, 10.0], b: [40.0, 40.0] })]);
        assert_eq!(out.get_pixel(10, 25).0, [255, 0, 0, 255], "rect edge is red");
        assert_eq!(*out.get_pixel(25, 25), white, "rect inside untouched");

        let out = flatten(&base, &[mark(Shape::Pixelate { a: [0.0, 48.0], b: [12.0, 60.0] })]);
        assert_eq!(out.get_pixel(0, 48), out.get_pixel(11, 59), "one uniform block");
        assert_ne!(*out.get_pixel(0, 48), white, "the black line got averaged in");

        let out = flatten(&base, &[mark(Shape::Text { at: [5.0, 5.0], text: "Hi".into() })]);
        assert!(out.pixels().any(|p| p.0[1] < 100 && p.0[0] > 200), "text painted red pixels");

        let out = flatten(&base, &[mark(Shape::Spotlight { a: [20.0, 20.0], b: [30.0, 30.0] })]);
        assert_eq!(*out.get_pixel(25, 25), white, "spotlight keeps the hole bright");
        assert!(out.get_pixel(5, 5).0[0] < 200, "and dims the rest");

        assert_eq!(flatten(&base, &[]), base);
    }
}
