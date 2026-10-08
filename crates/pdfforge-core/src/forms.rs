//! AcroForm fields: list, fill and flatten.
//!
//! Field structure and values are handled at the PDF object level (lopdf) so we control exactly
//! what is written: values, checkbox/radio appearance states, and freshly generated appearance
//! streams for text and choice fields (so every viewer — and flattening — shows the new value).
//! PDFium is used for page geometry and flattening.

use std::collections::HashMap;

use lopdf::{Dictionary, Object, ObjectId, Stream};
use pdfium_render::prelude::*;

use crate::{Document, Error, NormRect, Result, lock, lowlevel};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldKind {
    Text { multiline: bool, password: bool, comb: Option<u32> },
    Checkbox,
    Radio,
    ComboBox { editable: bool },
    ListBox { multi: bool },
    Signature,
    PushButton,
}

impl FieldKind {
    pub fn label(&self) -> &'static str {
        match self {
            FieldKind::Text { .. } => "text",
            FieldKind::Checkbox => "checkbox",
            FieldKind::Radio => "radio",
            FieldKind::ComboBox { .. } => "dropdown",
            FieldKind::ListBox { .. } => "list",
            FieldKind::Signature => "signature",
            FieldKind::PushButton => "button",
        }
    }

    pub fn is_fillable(&self) -> bool {
        !matches!(self, FieldKind::Signature | FieldKind::PushButton)
    }
}

/// One place a field is shown on a page.
#[derive(Debug, Clone, PartialEq)]
pub struct Widget {
    pub page: usize,
    pub rect: NormRect,
    /// For checkboxes and radio buttons: the name of this widget's "on" state.
    pub on_state: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FormField {
    /// Fully qualified name (`parent.child`), used to address the field.
    pub name: String,
    pub kind: FieldKind,
    /// Text / selected choice / selected radio export value / "Yes"/"Off" for checkboxes.
    pub value: String,
    pub checked: bool,
    /// Choice options (export values).
    pub options: Vec<String>,
    pub read_only: bool,
    pub required: bool,
    pub max_len: Option<u32>,
    pub widgets: Vec<Widget>,
}

/// A new value for a field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldValue {
    Text(String),
    Checked(bool),
    /// Dropdown / list option, or the export value of the radio button to select.
    Choice(String),
}

impl FieldValue {
    /// Interpret user input for a field of `kind` (e.g. from the command line).
    pub fn parse_for(kind: &FieldKind, s: &str) -> Result<FieldValue> {
        Ok(match kind {
            FieldKind::Checkbox => match s.trim().to_ascii_lowercase().as_str() {
                "yes" | "y" | "true" | "on" | "1" | "x" | "checked" => FieldValue::Checked(true),
                "no" | "n" | "false" | "off" | "0" | "" | "unchecked" => FieldValue::Checked(false),
                _ => return Err(Error::Invalid(format!("'{s}' is not yes/no for a checkbox"))),
            },
            FieldKind::Radio | FieldKind::ComboBox { .. } | FieldKind::ListBox { .. } => FieldValue::Choice(s.into()),
            _ => FieldValue::Text(s.into()),
        })
    }
}

// Field flag bits (PDF 32000-1 tables 221, 226, 228, 230), zero-based.
const FF_READ_ONLY: i64 = 1;
const FF_REQUIRED: i64 = 1 << 1;
const FF_MULTILINE: i64 = 1 << 12;
const FF_PASSWORD: i64 = 1 << 13;
const FF_RADIO: i64 = 1 << 15;
const FF_PUSHBUTTON: i64 = 1 << 16;
const FF_COMBO: i64 = 1 << 17;
const FF_EDIT: i64 = 1 << 18;
const FF_MULTISELECT: i64 = 1 << 21;
const FF_COMB: i64 = 1 << 24;

/// A terminal field found in the AcroForm tree.
struct RawField {
    id: ObjectId,
    name: String,
    ft: Vec<u8>,
    ff: i64,
    value: Option<Object>,
    opts: Vec<String>,
    max_len: Option<u32>,
    da: Option<String>,
    quadding: i64,
    /// Widget annotation ids with their /Rect.
    widgets: Vec<(ObjectId, [f32; 4])>,
}

fn deref<'a>(doc: &'a lopdf::Document, o: &'a Object) -> &'a Object {
    doc.dereference(o).map(|(_, o)| o).unwrap_or(o)
}

fn text_of(doc: &lopdf::Document, o: &Object) -> Option<String> {
    match deref(doc, o) {
        Object::String(..) => lopdf::decode_text_string(deref(doc, o)).ok(),
        Object::Name(n) => Some(String::from_utf8_lossy(n).into_owned()),
        _ => None,
    }
}

fn walk_fields(doc: &lopdf::Document) -> Vec<RawField> {
    let mut out = Vec::new();
    let Ok(af) = doc.catalog().and_then(|c| c.get(b"AcroForm")) else { return out };
    let Object::Dictionary(af) = deref(doc, af) else { return out };
    let default_da = af.get(b"DA").ok().and_then(|d| text_of(doc, d));
    let Ok(fields) = af.get(b"Fields").map(|f| deref(doc, f)).and_then(Object::as_array) else { return out };
    struct Inh {
        name: String,
        ft: Option<Vec<u8>>,
        ff: i64,
        value: Option<Object>,
        da: Option<String>,
        q: i64,
    }
    fn visit(
        doc: &lopdf::Document,
        id: ObjectId,
        inh: &Inh,
        depth: usize,
        seen: &mut std::collections::HashSet<ObjectId>,
        out: &mut Vec<RawField>,
    ) {
        if depth > 32 || !seen.insert(id) {
            return;
        }
        let Ok(d) = doc.get_dictionary(id) else { return };
        let part = d.get(b"T").ok().and_then(|t| text_of(doc, t));
        let name = match (&part, inh.name.is_empty()) {
            (Some(p), true) => p.clone(),
            (Some(p), false) => format!("{}.{p}", inh.name),
            (None, _) => inh.name.clone(),
        };
        let here = Inh {
            name,
            ft: d.get(b"FT").ok().and_then(|o| o.as_name().ok()).map(<[u8]>::to_vec).or(inh.ft.clone()),
            ff: d.get(b"Ff").ok().and_then(|o| deref(doc, o).as_i64().ok()).unwrap_or(inh.ff),
            value: d.get(b"V").ok().map(|v| deref(doc, v).clone()).or(inh.value.clone()),
            da: d.get(b"DA").ok().and_then(|o| text_of(doc, o)).or(inh.da.clone()),
            q: d.get(b"Q").ok().and_then(|o| o.as_i64().ok()).unwrap_or(inh.q),
        };
        let kids: Vec<ObjectId> = d
            .get(b"Kids")
            .map(|k| deref(doc, k))
            .and_then(Object::as_array)
            .map(|a| a.iter().filter_map(|k| k.as_reference().ok()).collect())
            .unwrap_or_default();
        // Kids that are fields (have /T) make this a non-terminal node.
        let field_kids: Vec<ObjectId> =
            kids.iter().copied().filter(|k| doc.get_dictionary(*k).is_ok_and(|kd| kd.has(b"T"))).collect();
        if !field_kids.is_empty() {
            for k in field_kids {
                visit(doc, k, &here, depth + 1, seen, out);
            }
            return;
        }
        let rect = |dict: &Dictionary| -> [f32; 4] {
            let r = dict.get(b"Rect").map(|r| deref(doc, r)).and_then(Object::as_array).ok();
            let v: Vec<f32> = r.map(|a| a.iter().map(|n| num(deref(doc, n))).collect()).unwrap_or_default();
            if v.len() == 4 { [v[0].min(v[2]), v[1].min(v[3]), v[0].max(v[2]), v[1].max(v[3])] } else { [0.0; 4] }
        };
        let widgets: Vec<(ObjectId, [f32; 4])> = if kids.is_empty() {
            vec![(id, rect(d))]
        } else {
            kids.iter().filter_map(|k| doc.get_dictionary(*k).ok().map(|kd| (*k, rect(kd)))).collect()
        };
        let opts = d
            .get(b"Opt")
            .map(|o| deref(doc, o))
            .and_then(Object::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|o| match deref(doc, o) {
                        Object::Array(pair) => pair.first().and_then(|e| text_of(doc, e)),
                        o => text_of(doc, o),
                    })
                    .collect()
            })
            .unwrap_or_default();
        out.push(RawField {
            id,
            name: here.name,
            ft: here.ft.unwrap_or_default(),
            ff: here.ff,
            value: here.value,
            opts,
            max_len: d.get(b"MaxLen").ok().and_then(|o| o.as_i64().ok()).map(|n| n.max(0) as u32),
            da: here.da,
            quadding: here.q,
            widgets,
        });
    }
    let root = Inh { name: String::new(), ft: None, ff: 0, value: None, da: default_da, q: 0 };
    let mut seen = std::collections::HashSet::new();
    for f in fields {
        if let Ok(id) = f.as_reference() {
            visit(doc, id, &root, 0, &mut seen, &mut out);
        }
    }
    out
}

fn num(o: &Object) -> f32 {
    o.as_float().or_else(|_| o.as_i64().map(|i| i as f32)).unwrap_or(0.0)
}

fn kind_of(f: &RawField) -> FieldKind {
    match f.ft.as_slice() {
        b"Btn" if f.ff & FF_PUSHBUTTON != 0 => FieldKind::PushButton,
        b"Btn" if f.ff & FF_RADIO != 0 => FieldKind::Radio,
        b"Btn" => FieldKind::Checkbox,
        b"Ch" if f.ff & FF_COMBO != 0 => FieldKind::ComboBox { editable: f.ff & FF_EDIT != 0 },
        b"Ch" => FieldKind::ListBox { multi: f.ff & FF_MULTISELECT != 0 },
        b"Sig" => FieldKind::Signature,
        _ => FieldKind::Text {
            multiline: f.ff & FF_MULTILINE != 0,
            password: f.ff & FF_PASSWORD != 0,
            comb: (f.ff & FF_COMB != 0).then_some(f.max_len).flatten(),
        },
    }
}

/// The "on" appearance state names of a checkbox/radio widget.
fn on_state(doc: &lopdf::Document, widget: ObjectId) -> Option<String> {
    let w = doc.get_dictionary(widget).ok()?;
    let ap = deref(doc, w.get(b"AP").ok()?).as_dict().ok()?;
    let n = deref(doc, ap.get(b"N").ok()?).as_dict().ok()?;
    n.iter().map(|(k, _)| k).find(|k| k.as_slice() != b"Off").map(|k| String::from_utf8_lossy(k).into_owned())
}

/// Map annotation object id -> page index.
fn annotation_pages(doc: &lopdf::Document) -> HashMap<ObjectId, usize> {
    let mut m = HashMap::new();
    for (i, page_id) in doc.get_pages().into_values().enumerate() {
        let Ok(p) = doc.get_dictionary(page_id) else { continue };
        if let Ok(annots) = p.get(b"Annots").map(|a| deref(doc, a)).and_then(Object::as_array) {
            for a in annots {
                if let Ok(id) = a.as_reference() {
                    m.insert(id, i);
                }
            }
        }
    }
    m
}

impl Document {
    /// All form fields, in document order. Empty if the document has no form.
    pub fn form_fields(&self) -> Result<Vec<FormField>> {
        let bytes = self.to_bytes()?;
        let (doc, _) = lowlevel::load(&bytes, self.password.as_deref())?;
        let pages = annotation_pages(&doc);
        let _g = lock();
        let mut out = Vec::new();
        for f in walk_fields(&doc) {
            let kind = kind_of(&f);
            let value_name = f.value.as_ref().and_then(|v| text_of(&doc, v)).unwrap_or_default();
            let checked =
                matches!(kind, FieldKind::Checkbox | FieldKind::Radio) && !value_name.is_empty() && value_name != "Off";
            let mut widgets = Vec::new();
            for (wid, r) in &f.widgets {
                let Some(&page) = pages.get(wid) else { continue };
                let Some(rect) = self.page_rect_to_norm(page, *r) else { continue };
                let on = matches!(kind, FieldKind::Checkbox | FieldKind::Radio).then(|| on_state(&doc, *wid)).flatten();
                widgets.push(Widget { page, rect, on_state: on });
            }
            let value = match &f.value {
                Some(Object::Array(a)) => a.iter().filter_map(|v| text_of(&doc, v)).collect::<Vec<_>>().join(", "),
                _ => value_name,
            };
            out.push(FormField {
                name: f.name,
                kind,
                value,
                checked,
                options: f.opts,
                read_only: f.ff & FF_READ_ONLY != 0,
                required: f.ff & FF_REQUIRED != 0,
                max_len: f.max_len,
                widgets,
            });
        }
        Ok(out)
    }

    /// PDF user-space rect [x0 y0 x1 y1] on `page` → normalised displayed rect.
    fn page_rect_to_norm(&self, page: usize, r: [f32; 4]) -> Option<NormRect> {
        let p = self.doc.pages().get(page as i32).ok()?;
        let (w, h) = (p.width().value.max(1.0), p.height().value.max(1.0));
        let cfg = PdfRenderConfig::new().scale_page_by_factor(1.0);
        let a = p.points_to_pixels(PdfPoints::new(r[0]), PdfPoints::new(r[1]), &cfg).ok()?;
        let b = p.points_to_pixels(PdfPoints::new(r[2]), PdfPoints::new(r[3]), &cfg).ok()?;
        let (ax, ay, bx, by) = (a.0 as f32 / w, a.1 as f32 / h, b.0 as f32 / w, b.1 as f32 / h);
        Some(NormRect { x0: ax.min(bx), y0: ay.min(by), x1: ax.max(bx), y1: ay.max(by) })
    }

    /// Set several fields at once (one save/reload cycle). Field names are fully qualified.
    pub fn fill_fields(&mut self, values: &[(String, FieldValue)]) -> Result<()> {
        if values.is_empty() {
            return Ok(());
        }
        let bytes = self.to_bytes()?;
        let (mut doc, state) = lowlevel::load(&bytes, self.password.as_deref())?;
        doc.trailer.remove(b"Encrypt");
        doc.encryption_state = None;
        let fields = walk_fields(&doc);
        let by_name: HashMap<&str, &RawField> = fields.iter().map(|f| (f.name.as_str(), f)).collect();
        let mut needs_fonts = false;
        for (name, value) in values {
            let f =
                by_name.get(name.as_str()).ok_or_else(|| Error::Invalid(format!("no form field named '{name}'")))?;
            if f.ff & FF_READ_ONLY != 0 {
                return Err(Error::Invalid(format!("field '{name}' is read-only")));
            }
            needs_fonts |= set_value(&mut doc, f, value)?;
        }
        if needs_fonts {
            ensure_acroform_font(&mut doc)?;
        }
        let out = lowlevel::save(doc, state.as_ref(), false)?;
        let pw = self.password.clone();
        self.replace_bytes(out, pw)
    }

    /// Bake form fields (and other annotations with appearances) into the page content so they
    /// can no longer be edited. `pages = None` flattens every page.
    pub fn flatten(&mut self, pages: Option<&[usize]>) -> Result<()> {
        let _g = lock();
        let all: Vec<usize> = (0..self.page_count()).collect();
        let pages = pages.unwrap_or(&all);
        self.check_all(pages)?;
        self.touch();
        for &i in pages {
            let mut p = self.doc.pages().get(i as i32)?;
            p.flatten()?;
        }
        // Remove the now-dangling AcroForm field list when the whole document was flattened.
        if pages.len() == self.page_count() {
            let bytes = self.doc.save_to_bytes()?;
            let (mut doc, state) = lowlevel::load(&bytes, self.password.as_deref())?;
            doc.trailer.remove(b"Encrypt");
            doc.encryption_state = None;
            if let Ok(cat) = doc.catalog_mut() {
                cat.remove(b"AcroForm");
            }
            let out = lowlevel::save(doc, state.as_ref(), false)?;
            let pw = self.password.clone();
            self.replace_bytes(out, pw)?;
            self.touch(); // let PDFium write from here on
        }
        Ok(())
    }
}

/// Apply one value. Returns true when an appearance using the shared /Helv font was written.
fn set_value(doc: &mut lopdf::Document, f: &RawField, v: &FieldValue) -> Result<bool> {
    let kind = kind_of(f);
    let set_field = |doc: &mut lopdf::Document, key: &str, val: Object| -> Result<()> {
        let d = doc.get_dictionary_mut(f.id).map_err(|e| Error::Lopdf(e.to_string()))?;
        d.set(key, val);
        Ok(())
    };
    match (&kind, v) {
        (FieldKind::Checkbox, FieldValue::Checked(on)) => {
            for (wid, _) in &f.widgets {
                let state = if *on { on_state(doc, *wid).unwrap_or_else(|| "Yes".into()) } else { "Off".into() };
                set_field(doc, "V", Object::Name(state.clone().into_bytes()))?;
                if let Ok(w) = doc.get_dictionary_mut(*wid) {
                    w.set("AS", Object::Name(state.into_bytes()));
                }
            }
            Ok(false)
        }
        (FieldKind::Radio, FieldValue::Choice(export)) | (FieldKind::Checkbox, FieldValue::Choice(export)) => {
            let states: Vec<Option<String>> = f.widgets.iter().map(|(w, _)| on_state(doc, *w)).collect();
            if !states.iter().any(|s| s.as_deref() == Some(export.as_str())) {
                let opts: Vec<String> = states.into_iter().flatten().collect();
                return Err(Error::Invalid(format!(
                    "'{}' has no option '{export}' (options: {})",
                    f.name,
                    opts.join(", ")
                )));
            }
            set_field(doc, "V", Object::Name(export.clone().into_bytes()))?;
            for ((wid, _), s) in f.widgets.iter().zip(states) {
                let on = s.as_deref() == Some(export.as_str());
                if let Ok(w) = doc.get_dictionary_mut(*wid) {
                    w.set("AS", Object::Name(if on { export.clone().into_bytes() } else { b"Off".to_vec() }));
                }
            }
            Ok(false)
        }
        (FieldKind::Text { multiline, password, comb }, FieldValue::Text(t)) => {
            let t = match f.max_len {
                Some(n) if n > 0 => t.chars().take(n as usize).collect(),
                _ => t.clone(),
            };
            set_field(doc, "V", lopdf::text_string(&t))?;
            let shown = if *password { "*".repeat(t.chars().count()) } else { t };
            let style = AppearanceStyle { multiline: *multiline, comb: *comb, quadding: f.quadding };
            write_appearances(doc, f, &shown, style)?;
            Ok(true)
        }
        (FieldKind::ComboBox { .. } | FieldKind::ListBox { .. }, FieldValue::Choice(c) | FieldValue::Text(c)) => {
            // Only editable dropdowns accept values that are not in the option list.
            let free = matches!(kind, FieldKind::ComboBox { editable: true });
            if !free && !f.opts.is_empty() && !f.opts.iter().any(|o| o == c) {
                return Err(Error::Invalid(format!(
                    "'{}' has no option '{c}' (options: {})",
                    f.name,
                    f.opts.join(", ")
                )));
            }
            set_field(doc, "V", lopdf::text_string(c))?;
            if let Some(i) = f.opts.iter().position(|o| o == c) {
                set_field(doc, "I", Object::Array(vec![Object::Integer(i as i64)]))?;
            }
            let style = AppearanceStyle { multiline: false, comb: None, quadding: f.quadding };
            write_appearances(doc, f, c, style)?;
            Ok(true)
        }
        (k, v) => Err(Error::Invalid(format!("cannot set {} field '{}' to {v:?}", k.label(), f.name))),
    }
}

#[derive(Clone, Copy)]
struct AppearanceStyle {
    multiline: bool,
    comb: Option<u32>,
    quadding: i64,
}

/// Font size and colour operators from a /DA string like "/Helv 0 Tf 0 g".
fn parse_da(da: Option<&str>) -> (f32, String) {
    let toks: Vec<&str> = da.unwrap_or("").split_whitespace().collect();
    let mut size = 0.0;
    let mut color = String::from("0 g");
    for (i, t) in toks.iter().enumerate() {
        match *t {
            "Tf" if i >= 1 => size = toks[i - 1].parse().unwrap_or(0.0),
            "g" if i >= 1 => color = format!("{} g", toks[i - 1]),
            "rg" if i >= 3 => color = format!("{} {} {} rg", toks[i - 3], toks[i - 2], toks[i - 1]),
            "k" if i >= 4 => color = format!("{} {} {} {} k", toks[i - 4], toks[i - 3], toks[i - 2], toks[i - 1]),
            _ => {}
        }
    }
    (size, color)
}

/// Helvetica advance widths (1/1000 em) for ASCII 32–126.
const HELV: [u16; 95] = [
    278, 278, 355, 556, 556, 889, 667, 191, 333, 333, 389, 584, 278, 333, 278, 278, 556, 556, 556, 556, 556, 556, 556,
    556, 556, 556, 278, 278, 584, 584, 584, 556, 1015, 667, 667, 722, 722, 667, 611, 778, 722, 278, 500, 667, 556, 833,
    722, 778, 667, 778, 722, 667, 611, 722, 667, 944, 667, 667, 611, 278, 278, 278, 469, 556, 333, 556, 556, 500, 556,
    556, 278, 556, 556, 222, 222, 500, 222, 833, 556, 556, 556, 556, 333, 500, 278, 556, 500, 722, 500, 500, 500, 334,
    260, 334, 584,
];

pub(crate) fn text_width(s: &str, size: f32) -> f32 {
    s.chars().map(|c| HELV.get((c as usize).wrapping_sub(32)).copied().unwrap_or(556) as f32).sum::<f32>() * size
        / 1000.0
}

/// Greedy word wrap to `width` points.
fn wrap(text: &str, size: f32, width: f32) -> Vec<String> {
    let mut lines = Vec::new();
    for para in text.split('\n') {
        let mut line = String::new();
        for word in para.split(' ') {
            let candidate = if line.is_empty() { word.to_string() } else { format!("{line} {word}") };
            if text_width(&candidate, size) <= width || line.is_empty() {
                line = candidate;
            } else {
                lines.push(std::mem::take(&mut line));
                line = word.to_string();
            }
        }
        lines.push(line);
    }
    lines
}

/// Escape for a PDF literal string, encoding as WinAnsi/Latin-1 (`?` for other characters).
fn pdf_literal(s: &str) -> Vec<u8> {
    let mut out = vec![b'('];
    for c in s.chars() {
        let b = match c {
            '\u{20}'..='\u{7e}' | '\u{a0}'..='\u{ff}' => c as u32 as u8,
            '€' => 0x80,
            '–' => 0x96,
            '—' => 0x97,
            '‘' => 0x91,
            '’' => 0x92,
            '“' => 0x93,
            '”' => 0x94,
            '•' => 0x95,
            _ => b'?',
        };
        if matches!(b, b'(' | b')' | b'\\') {
            out.push(b'\\');
        }
        out.push(b);
    }
    out.push(b')');
    out
}

/// Replace each widget's normal appearance with one that shows `text`.
fn write_appearances(doc: &mut lopdf::Document, f: &RawField, text: &str, st: AppearanceStyle) -> Result<()> {
    let (da_size, color) = parse_da(f.da.as_deref());
    for (wid, r) in &f.widgets {
        let (w, h) = ((r[2] - r[0]).max(1.0), (r[3] - r[1]).max(1.0));
        // Widgets rotated with /MK /R swap their box.
        let rot = doc
            .get_dictionary(*wid)
            .ok()
            .and_then(|d| d.get(b"MK").ok())
            .and_then(|mk| deref(doc, mk).as_dict().ok())
            .and_then(|mk| mk.get(b"R").ok())
            .and_then(|r| r.as_i64().ok())
            .unwrap_or(0)
            .rem_euclid(360);
        let (bw, bh) = if rot == 90 || rot == 270 { (h, w) } else { (w, h) };
        let pad = 2.0;
        let avail_w = (bw - 2.0 * pad).max(1.0);
        let mut size = if da_size > 0.0 { da_size } else { 12.0 };
        let mut lines = if st.multiline { wrap(text, size, avail_w) } else { vec![text.replace('\n', " ")] };
        if da_size <= 0.0 {
            // Auto size: shrink until it fits.
            loop {
                let lh = size * 1.15;
                let fits = if st.multiline {
                    lines.len() as f32 * lh <= bh - 2.0 * pad
                } else {
                    text_width(&lines[0], size) <= avail_w && size <= bh - 2.0 * pad
                };
                if fits || size <= 4.0 {
                    break;
                }
                size -= 0.5;
                if st.multiline {
                    lines = wrap(text, size, avail_w);
                }
            }
            if !st.multiline {
                size = size.min((bh - 2.0 * pad) * 0.8).max(4.0);
            }
        }
        let (bg, border, border_w) = widget_look(doc, *wid);
        let mut c = String::new();
        if let Some(bg) = bg {
            c.push_str(&format!("q {bg} 0 0 {bw} {bh} re f Q\n"));
        }
        if let Some(bc) = border
            && border_w > 0.0
        {
            let s = border_w / 2.0;
            c.push_str(&format!(
                "q {} {border_w} w {s} {s} {} {} re S Q\n",
                bc.replace(" rg", " RG").replace(" g", " G").replace(" k", " K"),
                bw - border_w,
                bh - border_w
            ));
        }
        c.push_str("/Tx BMC\nq\n");
        c.push_str(&format!("{pad} {pad} {} {} re W n\n", bw - 2.0 * pad, bh - 2.0 * pad));
        c.push_str(&format!("BT\n/Helv {size:.2} Tf\n{color}\n"));
        let mut content = c.into_bytes();
        let lh = size * 1.15;
        if let (Some(cells), false) = (st.comb, st.multiline) {
            let cell = bw / cells.max(1) as f32;
            let y = (bh - size) / 2.0 + size * 0.22;
            for (i, ch) in lines[0].chars().take(cells as usize).enumerate() {
                let s = ch.to_string();
                let x = i as f32 * cell + (cell - text_width(&s, size)) / 2.0;
                content.extend(format!("1 0 0 1 {x:.2} {y:.2} Tm ").bytes());
                content.extend(pdf_literal(&s));
                content.extend(b" Tj\n");
            }
        } else {
            let top = if st.multiline { bh - pad - size } else { (bh - size) / 2.0 + size * 0.22 };
            for (i, line) in lines.iter().enumerate() {
                let tw = text_width(line, size);
                let x = match st.quadding {
                    1 => (bw - tw) / 2.0,
                    2 => bw - pad - tw,
                    _ => pad,
                };
                let y = top - i as f32 * lh;
                content.extend(format!("1 0 0 1 {x:.2} {y:.2} Tm ").bytes());
                content.extend(pdf_literal(line));
                content.extend(b" Tj\n");
            }
        }
        content.extend(b"ET\nQ\nEMC\n");
        let matrix = match rot {
            90 => vec![0.into(), 1.into(), Object::Integer(-1), 0.into(), w.into(), 0.into()],
            180 => vec![Object::Integer(-1), 0.into(), 0.into(), Object::Integer(-1), w.into(), h.into()],
            270 => vec![0.into(), Object::Integer(-1), 1.into(), 0.into(), 0.into(), h.into()],
            _ => vec![1.into(), 0.into(), 0.into(), 1.into(), 0.into(), 0.into()],
        };
        let font = helv_font(doc);
        let mut sd = Dictionary::new();
        sd.set("Type", Object::Name(b"XObject".to_vec()));
        sd.set("Subtype", Object::Name(b"Form".to_vec()));
        sd.set("BBox", Object::Array(vec![0.into(), 0.into(), bw.into(), bh.into()]));
        sd.set("Matrix", Object::Array(matrix));
        let mut fonts = Dictionary::new();
        fonts.set("Helv", Object::Reference(font));
        let mut res = Dictionary::new();
        res.set("Font", Object::Dictionary(fonts));
        sd.set("Resources", Object::Dictionary(res));
        let mut stream = Stream::new(sd, content);
        let _ = stream.compress();
        let ap_id = doc.add_object(stream);
        if let Ok(wd) = doc.get_dictionary_mut(*wid) {
            let mut ap = Dictionary::new();
            ap.set("N", Object::Reference(ap_id));
            wd.set("AP", Object::Dictionary(ap));
        }
    }
    Ok(())
}

/// Fill colour, border colour (as PDF fill operators) and border width from a widget's /MK and /BS.
fn widget_look(doc: &lopdf::Document, wid: ObjectId) -> (Option<String>, Option<String>, f32) {
    let Ok(w) = doc.get_dictionary(wid) else { return (None, None, 0.0) };
    let mk = w.get(b"MK").ok().and_then(|m| deref(doc, m).as_dict().ok());
    let color = |key: &[u8]| -> Option<String> {
        let a = deref(doc, mk?.get(key).ok()?).as_array().ok()?;
        let v: Vec<String> = a.iter().map(|n| format!("{:.3}", num(deref(doc, n)))).collect();
        match v.len() {
            1 => Some(format!("{} g", v[0])),
            3 => Some(format!("{} rg", v.join(" "))),
            4 => Some(format!("{} k", v.join(" "))),
            _ => None,
        }
    };
    let width = w
        .get(b"BS")
        .ok()
        .and_then(|b| deref(doc, b).as_dict().ok())
        .and_then(|b| b.get(b"W").ok())
        .map(|n| num(deref(doc, n)))
        .unwrap_or(1.0);
    (color(b"BG"), color(b"BC"), width)
}

/// Object id of a Helvetica (WinAnsi) font dictionary, created once per document.
fn helv_font(doc: &mut lopdf::Document) -> ObjectId {
    let existing = doc.objects.iter().find_map(|(id, o)| {
        let d = o.as_dict().ok()?;
        (d.get(b"Type").ok()?.as_name().ok()? == b"Font"
            && d.get(b"BaseFont").ok()?.as_name().ok()? == b"Helvetica"
            && d.get(b"Encoding").ok()?.as_name().ok()? == b"WinAnsiEncoding")
            .then_some(*id)
    });
    existing.unwrap_or_else(|| {
        let mut f = Dictionary::new();
        f.set("Type", Object::Name(b"Font".to_vec()));
        f.set("Subtype", Object::Name(b"Type1".to_vec()));
        f.set("BaseFont", Object::Name(b"Helvetica".to_vec()));
        f.set("Encoding", Object::Name(b"WinAnsiEncoding".to_vec()));
        doc.add_object(f)
    })
}

/// Make sure /AcroForm /DR has /Helv so viewers that regenerate appearances find the font.
fn ensure_acroform_font(doc: &mut lopdf::Document) -> Result<()> {
    let font = helv_font(doc);
    let af_ref = doc.catalog().ok().and_then(|c| c.get(b"AcroForm").ok()).and_then(|a| a.as_reference().ok());
    let af = match af_ref {
        Some(id) => doc.get_dictionary_mut(id).map_err(|e| Error::Lopdf(e.to_string()))?,
        None => match doc.catalog_mut().map_err(|e| Error::Lopdf(e.to_string()))?.get_mut(b"AcroForm") {
            Ok(Object::Dictionary(d)) => d,
            _ => return Ok(()),
        },
    };
    let mut dr = match af.get(b"DR") {
        Ok(Object::Dictionary(d)) => d.clone(),
        _ => Dictionary::new(),
    };
    let mut fonts = match dr.get(b"Font") {
        Ok(Object::Dictionary(d)) => d.clone(),
        _ => Dictionary::new(),
    };
    if !fonts.has(b"Helv") {
        fonts.set("Helv", Object::Reference(font));
    }
    dr.set("Font", Object::Dictionary(fonts));
    af.set("DR", Object::Dictionary(dr));
    // We wrote real appearances, so viewers need not regenerate (and possibly re-style) them.
    af.remove(b"NeedAppearances");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_default_appearance() {
        assert_eq!(parse_da(Some("/Helv 0 Tf 0 g")), (0.0, "0 g".into()));
        assert_eq!(parse_da(Some("0.2 0.3 0.4 rg /Helv 11 Tf")), (11.0, "0.2 0.3 0.4 rg".into()));
        assert_eq!(parse_da(None), (0.0, "0 g".into()));
    }

    #[test]
    fn wraps_and_measures() {
        assert!((text_width("Hi", 10.0) - 7.22 - 2.22).abs() < 0.01);
        let lines = wrap("the quick brown fox jumps", 10.0, 60.0);
        assert!(lines.len() > 1 && lines.iter().all(|l| text_width(l, 10.0) <= 60.0 || !l.contains(' ')));
        assert_eq!(wrap("a\nb", 10.0, 100.0), ["a", "b"]);
    }

    #[test]
    fn escapes_literals() {
        assert_eq!(pdf_literal("a(b)\\c"), b"(a\\(b\\)\\\\c)".to_vec());
        assert_eq!(pdf_literal("café €"), b"(caf\xe9 \x80)".to_vec());
        assert_eq!(pdf_literal("日"), b"(?)".to_vec());
    }

    #[test]
    fn parses_values() {
        assert_eq!(FieldValue::parse_for(&FieldKind::Checkbox, "Yes").unwrap(), FieldValue::Checked(true));
        assert_eq!(FieldValue::parse_for(&FieldKind::Checkbox, "off").unwrap(), FieldValue::Checked(false));
        assert!(FieldValue::parse_for(&FieldKind::Checkbox, "maybe").is_err());
        assert_eq!(FieldValue::parse_for(&FieldKind::Radio, "pro").unwrap(), FieldValue::Choice("pro".into()));
    }
}
