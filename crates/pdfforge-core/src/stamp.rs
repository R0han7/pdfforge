//! Visual signatures and stamps: place an image, hand-drawn ink, or text on a page.
//!
//! These are "visual" signatures — the mark becomes part of the page content (like signing a
//! printout). They are not cryptographic digital signatures.
//!
//! Positions are [`NormRect`]s on the page *as displayed* (rotation applied), so callers can use
//! the same coordinates they draw with on screen; rotated pages are handled here.

use pdfium_render::prelude::*;

use crate::{Document, Error, NormRect, Result, forms, lock};

/// A hand-drawn signature.
#[derive(Debug, Clone, PartialEq)]
pub struct Ink {
    /// Strokes as points in 0..1 within the target rect, origin top-left.
    pub strokes: Vec<Vec<(f32, f32)>>,
    pub color: [u8; 3],
    /// Pen width in points.
    pub width: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StampFont {
    Helvetica,
    HelveticaBold,
    Times,
    #[default]
    TimesItalic,
    TimesBoldItalic,
    Courier,
}

impl StampFont {
    pub const ALL: [StampFont; 6] = [
        StampFont::TimesItalic,
        StampFont::TimesBoldItalic,
        StampFont::Times,
        StampFont::Helvetica,
        StampFont::HelveticaBold,
        StampFont::Courier,
    ];

    pub fn name(self) -> &'static str {
        match self {
            StampFont::Helvetica => "Helvetica",
            StampFont::HelveticaBold => "Helvetica Bold",
            StampFont::Times => "Times",
            StampFont::TimesItalic => "Times Italic",
            StampFont::TimesBoldItalic => "Times Bold Italic",
            StampFont::Courier => "Courier",
        }
    }
}

/// Page-space geometry of a displayed rect: origin at its bottom-left corner plus the vectors
/// along its (displayed) width and height.
struct Frame {
    origin: (f32, f32),
    x: (f32, f32),
    y: (f32, f32),
}

impl Frame {
    /// Map (u, v) in 0..1 with v measured *downwards* from the top (screen convention).
    fn point(&self, u: f32, v: f32) -> (f32, f32) {
        let up = 1.0 - v;
        (self.origin.0 + self.x.0 * u + self.y.0 * up, self.origin.1 + self.x.1 * u + self.y.1 * up)
    }

    fn width(&self) -> f32 {
        self.x.0.hypot(self.x.1)
    }

    fn height(&self) -> f32 {
        self.y.0.hypot(self.y.1)
    }

    fn matrix(&self, sx: f32, sy: f32, dx: f32, dy: f32) -> PdfMatrix {
        // Unit vectors scaled by (sx, sy), translated by (dx, dy) along them.
        let (ux, uy) = (self.x.0 / self.width().max(1e-6), self.x.1 / self.width().max(1e-6));
        let (vx, vy) = (self.y.0 / self.height().max(1e-6), self.y.1 / self.height().max(1e-6));
        PdfMatrix::new(
            ux * sx,
            uy * sx,
            vx * sy,
            vy * sy,
            self.origin.0 + ux * dx + vx * dy,
            self.origin.1 + uy * dx + vy * dy,
        )
    }
}

impl Document {
    fn frame(&self, page: &PdfPage<'_>, r: NormRect) -> Result<Frame> {
        if !(r.x1 > r.x0 && r.y1 > r.y0) {
            return Err(Error::Invalid("the signature area is empty".into()));
        }
        let (w, h) = (page.width().value, page.height().value);
        // pixels_to_points takes integer pixels; render at 16x so rounding stays below 0.1 pt.
        let k = 16.0;
        let cfg = PdfRenderConfig::new().scale_page_by_factor(k);
        let to_page = |x: f32, y: f32| -> Result<(f32, f32)> {
            let (px, py) = page.pixels_to_points((x * w * k) as i32, (y * h * k) as i32, &cfg)?;
            Ok((px.value, py.value))
        };
        let bl = to_page(r.x0, r.y1)?;
        let br = to_page(r.x1, r.y1)?;
        let tl = to_page(r.x0, r.y0)?;
        Ok(Frame { origin: bl, x: (br.0 - bl.0, br.1 - bl.1), y: (tl.0 - bl.0, tl.1 - bl.1) })
    }

    /// Place an image (e.g. a scanned signature, PNG with transparency) filling `rect`.
    pub fn stamp_image(&mut self, page: usize, rect: NormRect, image: &image::DynamicImage) -> Result<()> {
        let _g = lock();
        let i = self.check(page)?;
        self.touch();
        let mut p = self.doc.pages().get(i)?;
        let f = self.frame(&p, rect)?;
        let mut obj = p.objects_mut().create_image_object(PdfPoints::ZERO, PdfPoints::ZERO, image, None, None)?;
        obj.reset_matrix(f.matrix(f.width(), f.height(), 0.0, 0.0))?;
        Ok(())
    }

    /// Draw ink strokes as vector paths inside `rect`.
    pub fn stamp_ink(&mut self, page: usize, rect: NormRect, ink: &Ink) -> Result<()> {
        let _g = lock();
        let i = self.check(page)?;
        if ink.strokes.iter().all(|s| s.is_empty()) {
            return Err(Error::Invalid("the signature has no strokes".into()));
        }
        self.touch();
        let mut p = self.doc.pages().get(i)?;
        let f = self.frame(&p, rect)?;
        let color = PdfColor::new(ink.color[0], ink.color[1], ink.color[2], 255);
        for stroke in ink.strokes.iter().filter(|s| !s.is_empty()) {
            let (x0, y0) = f.point(stroke[0].0, stroke[0].1);
            let mut path = PdfPagePathObject::new(
                &self.doc,
                PdfPoints::new(x0),
                PdfPoints::new(y0),
                Some(color),
                Some(PdfPoints::new(ink.width.max(0.1))),
                None,
            )?;
            if stroke.len() == 1 {
                // A dot: a zero-length segment with round caps.
                path.line_to(PdfPoints::new(x0 + 0.01), PdfPoints::new(y0))?;
            }
            for &(u, v) in &stroke[1..] {
                let (x, y) = f.point(u, v);
                path.line_to(PdfPoints::new(x), PdfPoints::new(y))?;
            }
            path.set_line_cap(PdfPageObjectLineCap::Round)?;
            path.set_line_join(PdfPageObjectLineJoin::Round)?;
            p.objects_mut().add_path_object(path)?;
        }
        Ok(())
    }

    /// Write `text` sized to fit `rect` (a typed signature, a date, initials...).
    pub fn stamp_text(
        &mut self,
        page: usize,
        rect: NormRect,
        text: &str,
        font: StampFont,
        color: [u8; 3],
    ) -> Result<()> {
        let _g = lock();
        let i = self.check(page)?;
        let text = text.trim();
        if text.is_empty() {
            return Err(Error::Invalid("nothing to write".into()));
        }
        self.touch();
        let mut p = self.doc.pages().get(i)?;
        let f = self.frame(&p, rect)?;
        let token = {
            let fonts = self.doc.fonts_mut();
            match font {
                StampFont::Helvetica => fonts.helvetica(),
                StampFont::HelveticaBold => fonts.helvetica_bold(),
                StampFont::Times => fonts.times_roman(),
                StampFont::TimesItalic => fonts.times_italic(),
                StampFont::TimesBoldItalic => fonts.times_bold_italic(),
                StampFont::Courier => fonts.courier(),
            }
        };
        // Size from Helvetica metrics (Times is narrower, so it always fits); Courier is fixed.
        let em = match font {
            StampFont::Courier => 0.6 * text.chars().count() as f32,
            _ => forms::text_width(text, 1.0),
        };
        let size = (f.height() * 0.72).min(f.width() / em.max(0.01) * 0.95).max(1.0);
        let mut obj =
            p.objects_mut().create_text_object(PdfPoints::ZERO, PdfPoints::ZERO, text, token, PdfPoints::new(size))?;
        obj.set_fill_color(PdfColor::new(color[0], color[1], color[2], 255))?;
        // Baseline a little above the bottom so descenders stay inside the box.
        let baseline = (f.height() - size) / 2.0 + size * 0.22;
        obj.reset_matrix(f.matrix(1.0, 1.0, 0.0, baseline))?;
        Ok(())
    }
}
