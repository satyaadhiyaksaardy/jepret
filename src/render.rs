//! Annotation model + the one renderer that bakes annotations into pixels.
//! The editor shows the output of `flatten`, so what you see is exactly what gets copied/saved.

use ab_glyph::{Font, FontRef, PxScale, ScaleFont, point};
use image::RgbaImage;
use std::sync::OnceLock;
use std::sync::Arc;
use tiny_skia::{
    FillRule, FilterQuality, IntSize, LineCap, LineJoin, Paint, PathBuilder, Pattern, Pixmap, PixmapPaint, SpreadMode, Stroke,
    Transform,
};

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
    /// A pasted image stretched to the rectangle `a`–`b`.
    Image { a: P, b: P, img: Arc<RgbaImage>, opacity: f32 },
    /// Round bubble at `center` showing the area around `target` enlarged `zoom` times.
    Magnifier { target: P, center: P, radius: f32, zoom: f32 },
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
            Shape::Image { a, b, img, opacity } => {
                let (min, max) = norm(*a, *b);
                if let Some(src) = premultiplied_pixmap(img) {
                    let (sx, sy) = ((max[0] - min[0]) / img.width() as f32, (max[1] - min[1]) / img.height() as f32);
                    let paint = PixmapPaint { opacity: *opacity, quality: FilterQuality::Bilinear, ..Default::default() };
                    pm.draw_pixmap(0, 0, src.as_ref(), &paint, Transform::from_row(sx, 0.0, 0.0, sy, min[0], min[1]), None);
                }
            }
            Shape::Magnifier { target, center, radius, zoom } => {
                // Enlarged copy of what is under `target` so far, with crisp pixels.
                let snapshot = pm.clone();
                let to_bubble = Transform::from_row(*zoom, 0.0, 0.0, *zoom, center[0] - target[0] * zoom, center[1] - target[1] * zoom);
                let pattern = Pattern::new(snapshot.as_ref(), SpreadMode::Pad, FilterQuality::Nearest, 1.0, to_bubble);
                let bubble = PathBuilder::from_circle(center[0], center[1], *radius);
                if let Some(path) = &bubble {
                    let paint = Paint { shader: pattern, anti_alias: true, ..Default::default() };
                    pm.fill_path(path, &paint, FillRule::Winding, id, None);
                }
                // Ring around the source area, a connector, and the bubble border.
                let ring = radius / zoom;
                let (dx, dy) = (target[0] - center[0], target[1] - center[1]);
                let dist = (dx * dx + dy * dy).sqrt();
                let border = round_stroke(m.width.max(2.0));
                if dist > radius + ring {
                    let (ux, uy) = (dx / dist, dy / dist);
                    let from = [center[0] + ux * radius, center[1] + uy * radius];
                    let to = [target[0] - ux * ring, target[1] - uy * ring];
                    if let Some(path) = polyline(&[from, to]) {
                        pm.stroke_path(&path, &paint(c, 255), &border, id, None);
                    }
                }
                for path in [bubble, PathBuilder::from_circle(target[0], target[1], ring)].iter().flatten() {
                    pm.stroke_path(path, &paint(c, 255), &border, id, None);
                }
            }
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

/// Pixmap from straight-alpha RGBA, premultiplying as tiny-skia expects.
fn premultiplied_pixmap(img: &RgbaImage) -> Option<Pixmap> {
    let mut data = img.as_raw().clone();
    for px in data.chunks_exact_mut(4) {
        let a = px[3] as u32;
        for c in &mut px[..3] {
            *c = (*c as u32 * a / 255) as u8;
        }
    }
    Pixmap::from_vec(data, IntSize::from_wh(img.width(), img.height())?)
}

/// Gradient background with padding, rounded corners and a drop shadow. Sizes are in image pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Backdrop {
    pub colors: [[u8; 3]; 2], // top-left → bottom-right gradient
    pub padding: f32,
    pub radius: f32,
    pub shadow: f32, // blur radius; 0 disables the shadow
}

/// Signed distance from `(x, y)` to a rounded rectangle centered at `c` with half-size `half`.
fn rounded_rect_distance(x: f32, y: f32, c: (f32, f32), half: (f32, f32), r: f32) -> f32 {
    let qx = (x - c.0).abs() - (half.0 - r);
    let qy = (y - c.1).abs() - (half.1 - r);
    let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
    outside + qx.max(qy).min(0.0) - r
}

/// Composites `img` onto a gradient with rounded corners and a soft drop shadow.
///
/// Rendered analytically rather than with a rasterizer and blur: the shadow is a Gaussian falloff of
/// the distance to the (offset) rounded rectangle, and only the padding band and the four corners are
/// computed per pixel. The image interior is copied row by row. This keeps slider changes interactive
/// on full-screen Retina captures.
pub fn backdrop(img: &RgbaImage, b: &Backdrop) -> RgbaImage {
    let (w, h) = img.dimensions();
    let pad = b.padding.max(0.0).round() as u32;
    let (cw, ch) = (w + 2 * pad, h + 2 * pad);
    let r = b.radius.max(0.0).min(w.min(h) as f32 / 2.0);
    let opaque = img.pixels().all(|p| p.0[3] == 255);

    let [c0, c1] = b.colors.map(|c| c.map(|v| v as f32));
    let grad_len2 = (cw as f32).powi(2) + (ch as f32).powi(2);
    let center = (cw as f32 / 2.0, ch as f32 / 2.0);
    let half = (w as f32 / 2.0, h as f32 / 2.0);
    let sigma = (b.shadow / 2.0).max(0.001);
    let shadow_center = (center.0, center.1 + b.shadow * 0.3);
    const SHADOW_OPACITY: f32 = 0.5;

    // Gradient along the top-left → bottom-right diagonal, darkened by the shadow.
    let background = |x: u32, y: u32| -> [f32; 3] {
        let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
        let t = ((fx * cw as f32 + fy * ch as f32) / grad_len2).clamp(0.0, 1.0);
        let shade = if b.shadow > 0.0 {
            let d = rounded_rect_distance(fx, fy, shadow_center, half, r);
            SHADOW_OPACITY / (1.0 + (1.702 * d / sigma).exp()) // logistic approximation of the Gaussian CDF
        } else {
            0.0
        };
        std::array::from_fn(|i| (c0[i] + (c1[i] - c0[i]) * t) * (1.0 - shade))
    };

    let mut out = RgbaImage::new(cw, ch);
    let in_image = |x: u32, y: u32| x >= pad && y >= pad && x < pad + w && y < pad + h;
    let ri = r.ceil() as u32;
    let in_corner = |x: u32, y: u32| {
        let (ix, iy) = (x - pad, y - pad);
        (ix < ri || ix >= w - ri.min(w)) && (iy < ri || iy >= h - ri.min(h))
    };
    for y in 0..ch {
        let row_in_image = y >= pad && y < pad + h;
        let interior_row = row_in_image && opaque && !((y - pad) < ri || (y - pad) >= h.saturating_sub(ri));
        if interior_row {
            // Copy the image span and only compute the padding on either side.
            let src = &img.as_raw()[((y - pad) * w * 4) as usize..((y - pad + 1) * w * 4) as usize];
            let dst = &mut out.as_mut()[((y * cw + pad) * 4) as usize..((y * cw + pad + w) * 4) as usize];
            dst.copy_from_slice(src);
        }
        for x in 0..cw {
            if interior_row && x >= pad && x < pad + w {
                continue;
            }
            let bg = background(x, y);
            let mut px = [bg[0], bg[1], bg[2]];
            if in_image(x, y) {
                let src = img.get_pixel(x - pad, y - pad).0;
                let edge = if in_corner(x, y) {
                    let d = rounded_rect_distance(x as f32 + 0.5, y as f32 + 0.5, center, half, r);
                    (0.5 - d).clamp(0.0, 1.0) // anti-aliased rounded edge
                } else {
                    1.0
                };
                let a = edge * src[3] as f32 / 255.0;
                for i in 0..3 {
                    px[i] = px[i] * (1.0 - a) + src[i] as f32 * a;
                }
            }
            out.put_pixel(x, y, image::Rgba([px[0] as u8, px[1] as u8, px[2] as u8, 255]));
        }
    }
    out
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

    #[test]
    fn pasted_image_and_magnifier_render() {
        let mut base = RgbaImage::from_pixel(200, 120, image::Rgba([255, 255, 255, 255]));
        base.put_pixel(20, 20, image::Rgba([0, 0, 255, 255])); // a single blue pixel to magnify

        let pasted = Arc::new(RgbaImage::from_pixel(10, 10, image::Rgba([255, 0, 0, 255])));
        let solid = flatten(&base, &[mark(Shape::Image { a: [100.0, 10.0], b: [140.0, 50.0], img: pasted.clone(), opacity: 1.0 })]);
        assert_eq!(solid.get_pixel(120, 30).0, [255, 0, 0, 255], "pasted image stretched into its rectangle");
        assert_eq!(solid.get_pixel(150, 30).0, [255, 255, 255, 255], "nothing outside it");
        let faded = flatten(&base, &[mark(Shape::Image { a: [100.0, 10.0], b: [140.0, 50.0], img: pasted, opacity: 0.5 })]);
        let px = faded.get_pixel(120, 30).0;
        assert!(px[0] == 255 && px[1] > 100 && px[1] < 160, "half opacity blends with the screenshot: {px:?}");

        let zoomed = flatten(&base, &[mark(Shape::Magnifier { target: [20.5, 20.5], center: [100.0, 80.0], radius: 30.0, zoom: 4.0 })]);
        let blue = zoomed.get_pixel(100, 80).0;
        assert_eq!(blue, [0, 0, 255, 255], "bubble center shows the target pixel enlarged");
        // Source pixel 20 spans [20, 21) → bubble x in 100 + ([20, 21) - 20.5) * 4 = [98, 102): a 4×4 block.
        for (x, y) in [(98, 78), (101, 81)] {
            assert_eq!(zoomed.get_pixel(x, y).0, [0, 0, 255, 255], "inside the enlarged pixel at {x},{y}");
        }
        assert_eq!(zoomed.get_pixel(97, 80).0, [255, 255, 255, 255], "just outside it");
    }

    #[test]
    fn backdrop_pads_rounds_and_keeps_image() {
        let img = RgbaImage::from_pixel(100, 60, image::Rgba([10, 200, 10, 255]));
        let b = Backdrop { colors: [[255, 0, 0], [0, 0, 255]], padding: 20.0, radius: 12.0, shadow: 0.0 };
        let out = backdrop(&img, &b);
        assert_eq!(out.dimensions(), (140, 100));
        assert_eq!(out.get_pixel(70, 50).0, [10, 200, 10, 255], "image in the middle");
        assert!(out.get_pixel(1, 1).0[0] > 200, "gradient starts red at the top-left");
        assert!(out.get_pixel(138, 98).0[2] > 200, "and ends blue at the bottom-right");
        assert_ne!(out.get_pixel(20, 20).0, [10, 200, 10, 255], "image corner is rounded off");
        assert_eq!(out.get_pixel(70, 20).0, [10, 200, 10, 255], "top edge between the corners is image");
        assert_eq!(out.get_pixel(20, 50).0, [10, 200, 10, 255], "left edge between the corners is image");

        let shadowed = backdrop(&img, &Backdrop { shadow: 12.0, ..b });
        assert!(shadowed.get_pixel(70, 83).0[1] < out.get_pixel(70, 83).0[1] || shadowed.get_pixel(70, 83).0[2] < out.get_pixel(70, 83).0[2],
            "shadow darkens the background just below the image");
    }
}
