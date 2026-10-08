//! The signature tool: create a signature (draw, type or image) and keep it for placing.

use std::path::PathBuf;

use eframe::egui::{self, Color32, Pos2, Rect, RichText, Sense, Stroke, TextureHandle, TextureOptions, Vec2};
use pdfforge_core::{Ink, StampFont};

/// A signature ready to place on a page.
#[derive(Clone)]
pub enum Signature {
    /// Strokes normalised to their bounding box, plus that box's width / height.
    Ink {
        ink: Ink,
        aspect: f32,
    },
    Text {
        text: String,
        font: StampFont,
        color: [u8; 3],
    },
    Image {
        image: image::DynamicImage,
        tex: TextureHandle,
    },
}

impl Signature {
    /// Width / height of the signature's natural shape.
    pub fn aspect(&self) -> f32 {
        match self {
            Signature::Ink { aspect, .. } => *aspect,
            Signature::Text { text, .. } => (text.chars().count() as f32 * 0.42 + 0.6).clamp(1.0, 12.0),
            Signature::Image { image, .. } => image.width() as f32 / image.height().max(1) as f32,
        }
    }

    /// Default placed size in PDF points: about 0.6 in tall, at most 3.5 in wide.
    pub fn default_size_pt(&self) -> (f32, f32) {
        let a = self.aspect().max(0.05);
        let h = 44.0_f32;
        let w = (h * a).min(252.0);
        (w, w / a)
    }

    /// Draw a preview filling `rect` (used for the placement ghost and the dialog).
    pub fn paint(&self, painter: &egui::Painter, rect: Rect, alpha: u8) {
        match self {
            Signature::Ink { ink, .. } => {
                let c = Color32::from_rgba_unmultiplied(ink.color[0], ink.color[1], ink.color[2], alpha);
                let width = (rect.height() / 25.0).clamp(1.0, 4.0);
                for s in &ink.strokes {
                    let pts: Vec<Pos2> = s
                        .iter()
                        .map(|&(u, v)| Pos2::new(rect.left() + u * rect.width(), rect.top() + v * rect.height()))
                        .collect();
                    painter.add(egui::Shape::line(pts, Stroke::new(width, c)));
                }
            }
            Signature::Text { text, color, font } => {
                let c = Color32::from_rgba_unmultiplied(color[0], color[1], color[2], alpha);
                let family = if *font == StampFont::Courier {
                    egui::FontFamily::Monospace
                } else {
                    egui::FontFamily::Proportional
                };
                let galley =
                    painter.layout_no_wrap(text.clone(), egui::FontId::new(rect.height() * 0.6, family.clone()), c);
                let s = (rect.width() / galley.size().x.max(1.0)).min(1.0);
                let galley = if s < 1.0 {
                    painter.layout_no_wrap(text.clone(), egui::FontId::new(rect.height() * 0.6 * s, family), c)
                } else {
                    galley
                };
                painter.galley(rect.center() - galley.size() / 2.0, galley, c);
            }
            Signature::Image { tex, .. } => {
                let tint = Color32::from_white_alpha(alpha);
                painter.image(tex.id(), rect, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), tint);
            }
        }
    }
}

const CANVAS: Vec2 = Vec2::new(420.0, 140.0);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Draw,
    Type,
    Image,
}

pub struct SignDialog {
    tab: Tab,
    strokes: Vec<Vec<(f32, f32)>>,
    text: String,
    font: StampFont,
    blue: bool,
    image: Option<(PathBuf, image::DynamicImage, TextureHandle)>,
    error: Option<String>,
}

impl Default for SignDialog {
    fn default() -> Self {
        Self {
            tab: Tab::Draw,
            strokes: Vec::new(),
            text: String::new(),
            font: StampFont::TimesItalic,
            blue: true,
            image: None,
            error: None,
        }
    }
}

pub enum SignOutcome {
    Open,
    Cancel,
    Done(Signature),
}

impl SignDialog {
    fn color(&self) -> [u8; 3] {
        if self.blue { [10, 30, 140] } else { [0, 0, 0] }
    }

    pub fn show(&mut self, ctx: &egui::Context) -> SignOutcome {
        let mut outcome = SignOutcome::Open;
        egui::Modal::new(egui::Id::new("sign")).show(ctx, |ui| {
            ui.heading("Create signature");
            ui.label(
                RichText::new(
                    "A visual signature, like signing a printout. It is not a certificate-based digital signature.",
                )
                .small()
                .weak(),
            );
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.tab, Tab::Draw, "Draw");
                ui.selectable_value(&mut self.tab, Tab::Type, "Type");
                ui.selectable_value(&mut self.tab, Tab::Image, "Image");
            });
            ui.separator();
            match self.tab {
                Tab::Draw => self.draw_tab(ui),
                Tab::Type => self.type_tab(ui),
                Tab::Image => self.image_tab(ui),
            }
            if self.tab != Tab::Image {
                ui.horizontal(|ui| {
                    ui.label("Ink:");
                    ui.radio_value(&mut self.blue, true, "Blue");
                    ui.radio_value(&mut self.blue, false, "Black");
                });
            }
            if let Some(e) = &self.error {
                ui.colored_label(ui.visuals().error_fg_color, e);
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.button("Place on page").clicked() {
                    match self.build() {
                        Ok(s) => outcome = SignOutcome::Done(s),
                        Err(e) => self.error = Some(e),
                    }
                }
                if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    outcome = SignOutcome::Cancel;
                }
            });
        });
        outcome
    }

    fn draw_tab(&mut self, ui: &mut egui::Ui) {
        ui.label("Draw your signature with the mouse, pen or touchpad:");
        let (rect, resp) = ui.allocate_exact_size(CANVAS, Sense::drag());
        let p = ui.painter_at(rect);
        p.rect_filled(rect, 4.0, Color32::WHITE);
        p.line_segment(
            [Pos2::new(rect.left() + 20.0, rect.bottom() - 30.0), Pos2::new(rect.right() - 20.0, rect.bottom() - 30.0)],
            Stroke::new(1.0, Color32::from_gray(200)),
        );
        if resp.drag_started() {
            self.strokes.push(Vec::new());
            self.error = None;
        }
        if (resp.dragged() || resp.drag_started())
            && let Some(pos) = resp.interact_pointer_pos()
            && let Some(s) = self.strokes.last_mut()
        {
            let u = ((pos.x - rect.left()) / rect.width()).clamp(0.0, 1.0);
            let v = ((pos.y - rect.top()) / rect.height()).clamp(0.0, 1.0);
            if s.last().is_none_or(|&(lu, lv)| (lu - u).abs() + (lv - v).abs() > 0.002) {
                s.push((u, v));
            }
        }
        let c = self.color();
        let ink = Color32::from_rgb(c[0], c[1], c[2]);
        for s in &self.strokes {
            let pts: Vec<Pos2> = s
                .iter()
                .map(|&(u, v)| Pos2::new(rect.left() + u * rect.width(), rect.top() + v * rect.height()))
                .collect();
            if pts.len() == 1 {
                p.circle_filled(pts[0], 1.5, ink);
            } else {
                p.add(egui::Shape::line(pts, Stroke::new(2.5, ink)));
            }
        }
        if ui.button("Clear").clicked() {
            self.strokes.clear();
        }
    }

    fn type_tab(&mut self, ui: &mut egui::Ui) {
        ui.label("Type your name:");
        let r = ui.add(egui::TextEdit::singleline(&mut self.text).hint_text("Your Name").desired_width(CANVAS.x));
        if r.changed() {
            self.error = None;
        }
        egui::ComboBox::from_label("Font").selected_text(self.font.name()).show_ui(ui, |ui| {
            for f in StampFont::ALL {
                ui.selectable_value(&mut self.font, f, f.name());
            }
        });
        let (rect, _) = ui.allocate_exact_size(Vec2::new(CANVAS.x, 70.0), Sense::hover());
        ui.painter().rect_filled(rect, 4.0, Color32::WHITE);
        if !self.text.trim().is_empty() {
            let sig = Signature::Text { text: self.text.trim().into(), font: self.font, color: self.color() };
            sig.paint(ui.painter(), rect.shrink(8.0), 255);
        }
        ui.label(RichText::new("The preview uses the app font; the PDF uses the chosen font.").small().weak());
    }

    fn image_tab(&mut self, ui: &mut egui::Ui) {
        ui.label("Use a picture of your signature (PNG with a transparent background works best):");
        if ui.button("Choose image…").clicked()
            && let Some(path) = rfd::FileDialog::new().add_filter("Images", &["png", "jpg", "jpeg"]).pick_file()
        {
            match image::open(&path) {
                Ok(img) => {
                    let rgba = img.to_rgba8();
                    let ci = egui::ColorImage::from_rgba_unmultiplied(
                        [rgba.width() as usize, rgba.height() as usize],
                        &rgba,
                    );
                    let tex = ui.ctx().load_texture("signature-image", ci, TextureOptions::LINEAR);
                    self.image = Some((path, img, tex));
                    self.error = None;
                }
                Err(e) => self.error = Some(format!("Could not read {}: {e}", path.display())),
            }
        }
        if let Some((path, img, tex)) = &self.image {
            ui.label(path.file_name().unwrap_or_default().to_string_lossy().into_owned());
            let a = img.width() as f32 / img.height().max(1) as f32;
            let size = if a > CANVAS.x / CANVAS.y {
                Vec2::new(CANVAS.x, CANVAS.x / a)
            } else {
                Vec2::new(CANVAS.y * a, CANVAS.y)
            };
            let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
            ui.painter().rect_filled(rect, 0.0, Color32::WHITE);
            ui.painter().image(tex.id(), rect, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
        }
    }

    fn build(&self) -> Result<Signature, String> {
        match self.tab {
            Tab::Draw => {
                let ink = normalise(&self.strokes, self.color()).ok_or("Draw your signature first")?;
                Ok(Signature::Ink { ink, aspect: ink_box_aspect(&self.strokes) })
            }
            Tab::Type => {
                let t = self.text.trim();
                if t.is_empty() {
                    return Err("Type your name first".into());
                }
                Ok(Signature::Text { text: t.into(), font: self.font, color: self.color() })
            }
            Tab::Image => {
                let (_, img, tex) = self.image.as_ref().ok_or("Choose an image first")?;
                Ok(Signature::Image { image: img.clone(), tex: tex.clone() })
            }
        }
    }
}

/// Crop strokes (canvas-normalised) to their bounding box, keeping the canvas aspect so the
/// signature is not distorted. Returns `None` when nothing was drawn.
pub fn normalise(strokes: &[Vec<(f32, f32)>], color: [u8; 3]) -> Option<Ink> {
    let pts = strokes.iter().flatten();
    let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    for &(u, v) in pts {
        x0 = x0.min(u);
        y0 = y0.min(v);
        x1 = x1.max(u);
        y1 = y1.max(v);
    }
    if x0 > x1 {
        return None;
    }
    // Work in canvas pixels so aspect is preserved, then pad 4 %.
    let (cw, ch) = (CANVAS.x, CANVAS.y);
    let (bw, bh) = (((x1 - x0) * cw).max(1.0), ((y1 - y0) * ch).max(1.0));
    let pad = 0.04 * bw.max(bh);
    let (bw, bh) = (bw + 2.0 * pad, bh + 2.0 * pad);
    let (ox, oy) = (x0 * cw - pad, y0 * ch - pad);
    let strokes = strokes
        .iter()
        .filter(|s| !s.is_empty())
        .map(|s| s.iter().map(|&(u, v)| ((u * cw - ox) / bw, (v * ch - oy) / bh)).collect())
        .collect();
    Some(Ink { strokes, color, width: 1.8 })
}

/// Aspect of a normalised ink signature, recovered from the canvas-space bounding box.
pub fn ink_box_aspect(strokes: &[Vec<(f32, f32)>]) -> f32 {
    let pts = strokes.iter().flatten();
    let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    for &(u, v) in pts {
        x0 = x0.min(u);
        y0 = y0.min(v);
        x1 = x1.max(u);
        y1 = y1.max(v);
    }
    if x0 > x1 {
        return CANVAS.x / CANVAS.y;
    }
    let pad = 0.04 * ((x1 - x0) * CANVAS.x).max((y1 - y0) * CANVAS.y);
    (((x1 - x0) * CANVAS.x).max(1.0) + 2.0 * pad) / (((y1 - y0) * CANVAS.y).max(1.0) + 2.0 * pad)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalises_strokes_to_their_box() {
        assert!(normalise(&[], [0; 3]).is_none());
        assert!(normalise(&[vec![]], [0; 3]).is_none());
        let ink = normalise(&[vec![(0.25, 0.25), (0.75, 0.75)]], [1, 2, 3]).unwrap();
        let s = &ink.strokes[0];
        assert!(s[0].0 > 0.0 && s[0].0 < 0.1 && s[1].0 > 0.9 && s[1].0 < 1.0, "{s:?}");
        assert!(s[0].1 > 0.0 && s[1].1 < 1.0);
        assert_eq!(ink.color, [1, 2, 3]);
        // The box is 210x70 canvas px (+pad), so about 3:1.
        let a = ink_box_aspect(&[vec![(0.25, 0.25), (0.75, 0.75)]]);
        assert!((a - 2.6).abs() < 0.3, "{a}");
    }
}
