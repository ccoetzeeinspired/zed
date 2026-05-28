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
    let q = if l < 0.5 { l * (1.0 + s) } else { l + s - l * s };
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
