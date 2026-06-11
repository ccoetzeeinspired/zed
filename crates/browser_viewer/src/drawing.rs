//! Freehand drawing overlay for the browser tab. Stroke data is kept
//! in window-coord pixel space so SVG export and re-render are
//! straightforward.
//!
//! A `Stroke` is an open polyline; the renderer connects successive
//! points with straight line segments via `gpui::PathBuilder::stroke`.
//! Curve smoothing (catmull-rom etc.) is intentionally skipped — the
//! goal is "scribble to point at a thing", not vector illustration.

use gpui::{Hsla, Pixels, Point};

/// One continuous brush stroke from mouse-down to mouse-up.
#[derive(Debug, Clone)]
pub struct Stroke {
    pub points: Vec<Point<Pixels>>,
    pub color: Hsla,
    pub width: Pixels,
}

impl Stroke {
    pub fn new(start: Point<Pixels>, color: Hsla, width: Pixels) -> Self {
        Self {
            points: vec![start],
            color,
            width,
        }
    }

    pub fn push(&mut self, p: Point<Pixels>) {
        // Skip identical / sub-pixel-noise points so the renderer
        // doesn't waste vertices on stationary mouse jitter.
        if let Some(last) = self.points.last()
            && (p.x - last.x).abs() < gpui::px(0.5)
            && (p.y - last.y).abs() < gpui::px(0.5)
        {
            return;
        }
        self.points.push(p);
    }
}

/// Drawing-canvas state stored on `BrowserItem`. Committed strokes
/// plus the currently-being-drawn stroke (if any).
#[derive(Debug, Clone, Default)]
pub struct DrawingCanvas {
    pub strokes: Vec<Stroke>,
    pub current: Option<Stroke>,
}

impl DrawingCanvas {
    pub fn begin(&mut self, at: Point<Pixels>, color: Hsla, width: Pixels) {
        self.current = Some(Stroke::new(at, color, width));
    }

    pub fn extend(&mut self, to: Point<Pixels>) {
        if let Some(stroke) = self.current.as_mut() {
            stroke.push(to);
        }
    }

    pub fn finish(&mut self) {
        if let Some(stroke) = self.current.take() {
            if stroke.points.len() >= 2 {
                self.strokes.push(stroke);
            }
        }
    }

    pub fn clear(&mut self) {
        self.strokes.clear();
        self.current = None;
    }

    pub fn is_empty(&self) -> bool {
        self.strokes.is_empty() && self.current.is_none()
    }

    /// Render the strokes as a single SVG document. Coordinates are
    /// translated so the top-left of `origin` becomes (0, 0) — used by
    /// the submission bundle to align with the screenshot.
    pub fn to_svg(&self, origin: Point<Pixels>, size: gpui::Size<Pixels>) -> String {
        let w = f32::from(size.width).max(1.);
        let h = f32::from(size.height).max(1.);
        let mut out = String::with_capacity(256 + self.strokes.len() * 64);
        out.push_str(&format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="{w:.0}" height="{h:.0}" viewBox="0 0 {w:.0} {h:.0}">"#
        ));
        let visit = |out: &mut String, stroke: &Stroke| {
            if stroke.points.len() < 2 {
                return;
            }
            let (r, g, b, a) = hsla_to_rgba(stroke.color);
            let mut d = String::with_capacity(stroke.points.len() * 12);
            for (i, p) in stroke.points.iter().enumerate() {
                let x = f32::from(p.x) - f32::from(origin.x);
                let y = f32::from(p.y) - f32::from(origin.y);
                let cmd = if i == 0 { 'M' } else { 'L' };
                d.push_str(&format!("{cmd}{x:.1} {y:.1} "));
            }
            out.push_str(&format!(
                r#"<path d="{d}" fill="none" stroke="rgba({r}, {g}, {b}, {a:.2})" stroke-width="{w:.1}" stroke-linecap="round" stroke-linejoin="round"/>"#,
                d = d.trim_end(),
                r = r,
                g = g,
                b = b,
                a = a,
                w = f32::from(stroke.width),
            ));
        };
        for stroke in &self.strokes {
            visit(&mut out, stroke);
        }
        if let Some(c) = self.current.as_ref() {
            visit(&mut out, c);
        }
        out.push_str("</svg>");
        out
    }
}

fn hsla_to_rgba(c: Hsla) -> (u8, u8, u8, f32) {
    let (r, g, b) = hsl_to_rgb(c.h, c.s, c.l);
    (
        (r * 255.0).round() as u8,
        (g * 255.0).round() as u8,
        (b * 255.0).round() as u8,
        c.a,
    )
}

fn hsl_to_rgb(h: f32, s: f32, l: f32) -> (f32, f32, f32) {
    if s == 0.0 {
        return (l, l, l);
    }
    let q = if l < 0.5 {
        l * (1.0 + s)
    } else {
        l + s - l * s
    };
    let p = 2.0 * l - q;
    let hue_to_rgb = |t: f32| -> f32 {
        let mut t = t;
        if t < 0.0 {
            t += 1.0;
        }
        if t > 1.0 {
            t -= 1.0;
        }
        if t < 1.0 / 6.0 {
            return p + (q - p) * 6.0 * t;
        }
        if t < 0.5 {
            return q;
        }
        if t < 2.0 / 3.0 {
            return p + (q - p) * (2.0 / 3.0 - t) * 6.0;
        }
        p
    };
    (
        hue_to_rgb(h + 1.0 / 3.0),
        hue_to_rgb(h),
        hue_to_rgb(h - 1.0 / 3.0),
    )
}

/// Windows-only: composite the design annotations (selected-element
/// outline + freehand strokes) onto a page screenshot so the agent
/// receives a single self-documenting image.
///
/// The design overlay's coordinates are in window-space *logical*
/// pixels; the screenshot from WebView2 `CapturePreview` is in *device*
/// pixels. We derive the scale from the decoded image dimensions vs.
/// the logical viewport size, so no DPI query is needed.
#[cfg(target_os = "windows")]
mod annotate {
    use super::{DrawingCanvas, Stroke, hsla_to_rgba};
    use anyhow::{Context as _, Result};
    use gpui::{Pixels, Point, Size};
    use image::{ImageEncoder, Rgba, RgbaImage};

    /// Byte budget for the dispatched PNG (AC-P4-4: screenshot <= 2 MB).
    const MAX_PNG_BYTES: usize = 2 * 1024 * 1024;
    /// Cap the longest edge so a 4K hi-DPI capture doesn't blow the budget.
    const MAX_EDGE: u32 = 2000;
    /// Outline color for the selected element — orange, pops on most pages.
    const OUTLINE: Rgba<u8> = Rgba([255, 106, 0, 255]);

    /// Decode `png`, draw the element outline (viewport-local CSS px) and
    /// the freehand strokes (window-space logical px) onto it, and return
    /// a re-encoded PNG no larger than [`MAX_PNG_BYTES`].
    pub fn annotate_screenshot(
        png: &[u8],
        element_rect: Option<(f32, f32, f32, f32)>,
        drawing: &DrawingCanvas,
        viewport_origin: Point<Pixels>,
        viewport_size: Size<Pixels>,
    ) -> Result<Vec<u8>> {
        let decoded = image::load_from_memory(png).context("decode screenshot png")?;
        let mut img = decoded.to_rgba8();

        // Pre-shrink very large captures before drawing so the
        // annotations stay crisp relative to the final image.
        if img.width().max(img.height()) > MAX_EDGE {
            let (w, h) = scaled_dims(img.width(), img.height(), MAX_EDGE);
            img = image::imageops::resize(&img, w, h, image::imageops::FilterType::Triangle);
        }

        let vp_w = f32::from(viewport_size.width).max(1.0);
        let vp_h = f32::from(viewport_size.height).max(1.0);
        let scale_x = img.width() as f32 / vp_w;
        let scale_y = img.height() as f32 / vp_h;

        // rx/ry/rw/rh come from the page's getBoundingClientRect and are
        // untrusted — a hostile or buggy page can post Inf/NaN/huge values
        // that would otherwise blow up the rasterizer. Only draw a finite
        // outline (the line rasterizer also caps its step count).
        if let Some((rx, ry, rw, rh)) = element_rect {
            if [rx, ry, rw, rh].iter().all(|v| v.is_finite()) {
                let thickness = (2.5 * scale_y).round().max(2.0) as i32;
                draw_rect_outline(
                    &mut img,
                    rx * scale_x,
                    ry * scale_y,
                    rw * scale_x,
                    rh * scale_y,
                    thickness,
                    OUTLINE,
                );
            }
        }

        let ox = f32::from(viewport_origin.x);
        let oy = f32::from(viewport_origin.y);
        for stroke in drawing.strokes.iter().chain(drawing.current.iter()) {
            draw_stroke(&mut img, stroke, ox, oy, scale_x, scale_y);
        }

        encode_capped(img)
    }

    fn scaled_dims(w: u32, h: u32, max_edge: u32) -> (u32, u32) {
        if w >= h {
            let nh = ((h as f32) * (max_edge as f32) / (w as f32))
                .round()
                .max(1.0) as u32;
            (max_edge, nh)
        } else {
            let nw = ((w as f32) * (max_edge as f32) / (h as f32))
                .round()
                .max(1.0) as u32;
            (nw, max_edge)
        }
    }

    fn encode_png(img: &RgbaImage) -> Result<Vec<u8>> {
        let mut buf = Vec::new();
        image::codecs::png::PngEncoder::new(&mut buf)
            .write_image(
                img.as_raw(),
                img.width(),
                img.height(),
                image::ExtendedColorType::Rgba8,
            )
            .context("encode annotated png")?;
        Ok(buf)
    }

    fn encode_capped(mut img: RgbaImage) -> Result<Vec<u8>> {
        for _ in 0..4 {
            let buf = encode_png(&img)?;
            if buf.len() <= MAX_PNG_BYTES || img.width() <= 320 || img.height() <= 320 {
                return Ok(buf);
            }
            let w = (img.width() as f32 * 0.8).round().max(1.0) as u32;
            let h = (img.height() as f32 * 0.8).round().max(1.0) as u32;
            img = image::imageops::resize(&img, w, h, image::imageops::FilterType::Triangle);
        }
        encode_png(&img)
    }

    fn draw_stroke(img: &mut RgbaImage, stroke: &Stroke, ox: f32, oy: f32, sx: f32, sy: f32) {
        if stroke.points.len() < 2 {
            return;
        }
        let (r, g, b, a) = hsla_to_rgba(stroke.color);
        let alpha = (a.clamp(0.0, 1.0) * 255.0).round() as u8;
        let color = Rgba([r, g, b, alpha]);
        let half = ((f32::from(stroke.width) * sy) / 2.0).round().max(1.0) as i32;
        let mut prev: Option<(f32, f32)> = None;
        for p in &stroke.points {
            let x = (f32::from(p.x) - ox) * sx;
            let y = (f32::from(p.y) - oy) * sy;
            if let Some((px, py)) = prev {
                draw_thick_line(img, px, py, x, y, half, color);
            }
            prev = Some((x, y));
        }
    }

    fn draw_rect_outline(
        img: &mut RgbaImage,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        thickness: i32,
        color: Rgba<u8>,
    ) {
        let half = (thickness / 2).max(1);
        draw_thick_line(img, x, y, x + w, y, half, color);
        draw_thick_line(img, x, y + h, x + w, y + h, half, color);
        draw_thick_line(img, x, y, x, y + h, half, color);
        draw_thick_line(img, x + w, y, x + w, y + h, half, color);
    }

    fn draw_thick_line(
        img: &mut RgbaImage,
        x0: f32,
        y0: f32,
        x1: f32,
        y1: f32,
        half: i32,
        color: Rgba<u8>,
    ) {
        let dx = x1 - x0;
        let dy = y1 - y0;
        // Clamp the step count defensively so an out-of-range coordinate can
        // never spin the loop for billions of iterations.
        let steps = dx.abs().max(dy.abs()).ceil().clamp(1.0, 16384.0) as i32;
        for i in 0..=steps {
            let t = i as f32 / steps as f32;
            let cx = (x0 + dx * t).round() as i32;
            let cy = (y0 + dy * t).round() as i32;
            stamp(img, cx, cy, half, color);
        }
    }

    fn stamp(img: &mut RgbaImage, cx: i32, cy: i32, half: i32, color: Rgba<u8>) {
        let (w, h) = (img.width() as i32, img.height() as i32);
        for yy in cy.saturating_sub(half)..=cy.saturating_add(half) {
            for xx in cx.saturating_sub(half)..=cx.saturating_add(half) {
                if xx >= 0 && yy >= 0 && xx < w && yy < h {
                    blend_pixel(img.get_pixel_mut(xx as u32, yy as u32), color);
                }
            }
        }
    }

    fn blend_pixel(dst: &mut Rgba<u8>, src: Rgba<u8>) {
        let sa = src.0[3] as f32 / 255.0;
        if sa >= 1.0 {
            *dst = src;
            return;
        }
        for i in 0..3 {
            dst.0[i] = (src.0[i] as f32 * sa + dst.0[i] as f32 * (1.0 - sa)).round() as u8;
        }
        dst.0[3] = 255;
    }
}

#[cfg(target_os = "windows")]
pub use annotate::annotate_screenshot;
