//! Operations done at the PDF object level with [`lopdf`]: password protection, compression
//! and setting form values. PDFium does not expose these (or not fully), so the document is
//! round-tripped: PDFium bytes → lopdf → modified bytes → PDFium.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use lopdf::content::Content;
use lopdf::encryption::crypt_filters::{Aes256CryptFilter, CryptFilter};
use lopdf::{Dictionary, EncryptionState, EncryptionVersion, Object, ObjectId, Permissions};
use rand::RngExt as _;

use crate::{Document, Error, Result};

fn lo_err(e: lopdf::Error) -> Error {
    match e {
        lopdf::Error::InvalidPassword => Error::PasswordRequired,
        e => Error::Lopdf(e.to_string()),
    }
}

/// Load bytes into lopdf, decrypting with `password` (lopdf tries the empty password itself).
/// Returns the document and the encryption state it was decrypted with, if any.
pub(crate) fn load(bytes: &[u8], password: Option<&str>) -> Result<(lopdf::Document, Option<EncryptionState>)> {
    let opts = match password {
        Some(p) => lopdf::LoadOptions::with_password(p),
        None => lopdf::LoadOptions::default(),
    };
    let doc = lopdf::Document::load_mem_with_options(bytes, opts).map_err(lo_err)?;
    if doc.is_encrypted() && doc.encryption_state.is_none() {
        return Err(Error::PasswordRequired);
    }
    let state = doc.encryption_state.clone();
    Ok((doc, state))
}

/// Serialise a lopdf document, encrypting it with `state` if given.
pub(crate) fn save(mut doc: lopdf::Document, state: Option<&EncryptionState>, object_streams: bool) -> Result<Vec<u8>> {
    if let Some(s) = state {
        doc.encrypt(s).map_err(lo_err)?;
    }
    let mut out = Vec::new();
    // Object streams and encryption are not combined: not every reader handles that pairing.
    if object_streams && state.is_none() {
        if version_less_than(&doc.version, "1.5") {
            doc.version = "1.5".into();
        }
        doc.save_modern(&mut out)?;
    } else {
        doc.save_to(&mut out)?;
    }
    Ok(out)
}

fn version_less_than(v: &str, than: &str) -> bool {
    let parse = |s: &str| -> (u32, u32) {
        let mut it = s.split('.').map(|p| p.trim().parse().unwrap_or(0));
        (it.next().unwrap_or(1), it.next().unwrap_or(0))
    };
    parse(v) < parse(than)
}

// ------------------------------------------------------------------ security

/// What a password-protected document allows when opened with the user password.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Allow {
    pub print: bool,
    pub copy: bool,
    pub modify: bool,
    pub annotate: bool,
    pub fill_forms: bool,
    pub assemble: bool,
}

impl Allow {
    pub const ALL: Allow =
        Allow { print: true, copy: true, modify: true, annotate: true, fill_forms: true, assemble: true };
    pub const NONE: Allow =
        Allow { print: false, copy: false, modify: false, annotate: false, fill_forms: false, assemble: false };

    fn to_lopdf(self) -> Permissions {
        let mut p = Permissions::empty();
        let mut set = |on: bool, f: Permissions| {
            if on {
                p |= f;
            }
        };
        set(self.print, Permissions::PRINTABLE | Permissions::PRINTABLE_IN_HIGH_QUALITY);
        set(self.copy, Permissions::COPYABLE);
        // Accessibility extraction is always allowed (screen readers).
        set(true, Permissions::COPYABLE_FOR_ACCESSIBILITY);
        set(self.modify, Permissions::MODIFIABLE);
        set(self.annotate, Permissions::ANNOTABLE);
        set(self.fill_forms || self.annotate, Permissions::FILLABLE);
        set(self.assemble, Permissions::ASSEMBLABLE);
        p
    }

    fn from_lopdf(p: Permissions) -> Self {
        Allow {
            print: p.contains(Permissions::PRINTABLE),
            copy: p.contains(Permissions::COPYABLE),
            modify: p.contains(Permissions::MODIFIABLE),
            annotate: p.contains(Permissions::ANNOTABLE),
            fill_forms: p.contains(Permissions::FILLABLE),
            assemble: p.contains(Permissions::ASSEMBLABLE),
        }
    }
}

impl Default for Allow {
    fn default() -> Self {
        Allow::ALL
    }
}

/// Settings for [`Document::protect`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Protection {
    /// Needed to open the document. Empty = anyone can open it (only permissions apply).
    pub user_password: String,
    /// Needed to change permissions or remove protection. Empty = same as the user password.
    pub owner_password: String,
    pub allow: Allow,
}

/// Encryption details of an open document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Security {
    pub encrypted: bool,
    /// e.g. "AES-256", "AES-128", "RC4 128-bit".
    pub method: String,
    pub allow: Allow,
}

fn encryption_method(s: &EncryptionState) -> String {
    let filters = format!("{s:?}");
    match s.version() {
        5 => "AES-256".into(),
        4 if filters.contains("Aes128") => "AES-128".into(),
        4 => "RC4 128-bit".into(),
        2 => format!("RC4 {}-bit", s.key_length().unwrap_or(40)),
        v => format!("Standard (V{v})"),
    }
}

fn random_key<const N: usize>() -> [u8; N] {
    let mut k = [0u8; N];
    rand::rng().fill(&mut k);
    k
}

fn random_password() -> String {
    random_key::<16>().iter().map(|b| format!("{b:02x}")).collect()
}

impl Document {
    /// Encryption status and permissions.
    pub fn security(&self) -> Result<Security> {
        let bytes = self.to_bytes()?;
        let (_, state) = load(&bytes, self.password.as_deref())?;
        Ok(match state {
            None => Security { encrypted: false, method: String::new(), allow: Allow::ALL },
            Some(s) => {
                Security { encrypted: true, method: encryption_method(&s), allow: Allow::from_lopdf(s.permissions()) }
            }
        })
    }

    /// Encrypt with AES-256 (PDF 2.0 / Acrobat X+ standard security handler), replacing any
    /// existing protection.
    pub fn protect(&mut self, p: &Protection) -> Result<()> {
        if p.user_password.is_empty() && p.owner_password.is_empty() {
            return Err(Error::Invalid("give a password to open the document, an owner password, or both".into()));
        }
        // With no owner password, the user password also unlocks permissions. With only an owner
        // password, anyone can open it and permissions apply.
        let owner = match (&p.owner_password, &p.user_password) {
            (o, _) if !o.is_empty() => o.clone(),
            (_, u) if !u.is_empty() => u.clone(),
            _ => random_password(),
        };
        let bytes = self.to_bytes()?;
        let (mut doc, _old) = load(&bytes, self.password.as_deref())?;
        // `load` decrypted everything; drop the old /Encrypt so lopdf will encrypt afresh.
        doc.trailer.remove(b"Encrypt");
        doc.encryption_state = None;
        ensure_file_id(&mut doc);
        let key = random_key::<32>();
        let filter: Arc<dyn CryptFilter> = Arc::new(Aes256CryptFilter);
        let state = EncryptionState::try_from(EncryptionVersion::V5 {
            encrypt_metadata: true,
            crypt_filters: BTreeMap::from([(b"StdCF".to_vec(), filter)]),
            file_encryption_key: &key,
            stream_filter: b"StdCF".to_vec(),
            string_filter: b"StdCF".to_vec(),
            owner_password: &owner,
            user_password: &p.user_password,
            permissions: p.allow.to_lopdf(),
        })
        .map_err(lo_err)?;
        if version_less_than(&doc.version, "1.7") {
            doc.version = "1.7".into();
        }
        let out = save(doc, Some(&state), false)?;
        let pw = (!p.user_password.is_empty()).then(|| p.user_password.clone());
        self.replace_bytes(out, pw)
    }

    /// Remove password protection and permission restrictions. The document must already be open
    /// (i.e. you know its password, or it has none to open it).
    pub fn unprotect(&mut self) -> Result<()> {
        let bytes = self.to_bytes()?;
        let (mut doc, state) = load(&bytes, self.password.as_deref())?;
        if state.is_none() {
            return Ok(());
        }
        doc.trailer.remove(b"Encrypt");
        doc.encryption_state = None;
        let out = save(doc, None, false)?;
        self.replace_bytes(out, None)
    }
}

/// AES-256 requires a document /ID; add one if missing.
fn ensure_file_id(doc: &mut lopdf::Document) {
    if doc.trailer.get(b"ID").is_err() {
        let id = Object::String(random_key::<16>().to_vec(), lopdf::StringFormat::Hexadecimal);
        doc.trailer.set("ID", Object::Array(vec![id.clone(), id]));
    }
}

// ------------------------------------------------------------------ compression

/// Settings for [`Document::compress`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CompressOptions {
    /// Downsample images displayed above this resolution. `None` keeps image resolution.
    pub image_dpi: Option<f32>,
    /// JPEG quality (1–100) for recompressed photos.
    pub jpeg_quality: u8,
    /// Re-encode images as JPEG when that makes them smaller.
    pub recompress_images: bool,
    /// Remove XMP metadata, document info, page thumbnails and private application data.
    pub strip_metadata: bool,
}

impl CompressOptions {
    /// Light: lossless clean-up only (stream compression, unused objects, object streams).
    pub const LOSSLESS: CompressOptions =
        CompressOptions { image_dpi: None, jpeg_quality: 90, recompress_images: false, strip_metadata: false };
    /// Good for print: 200 dpi images, high-quality JPEG.
    pub const PRINT: CompressOptions =
        CompressOptions { image_dpi: Some(200.0), jpeg_quality: 85, recompress_images: true, strip_metadata: false };
    /// Good for screen and e-mail: 120 dpi, medium JPEG.
    pub const SCREEN: CompressOptions =
        CompressOptions { image_dpi: Some(120.0), jpeg_quality: 70, recompress_images: true, strip_metadata: false };
    /// Smallest: 72 dpi, low JPEG, metadata stripped.
    pub const SMALLEST: CompressOptions =
        CompressOptions { image_dpi: Some(72.0), jpeg_quality: 50, recompress_images: true, strip_metadata: true };

    pub fn preset(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "lossless" => Some(Self::LOSSLESS),
            "print" | "high" => Some(Self::PRINT),
            "screen" | "medium" | "ebook" => Some(Self::SCREEN),
            "smallest" | "low" => Some(Self::SMALLEST),
            _ => None,
        }
    }
}

impl Default for CompressOptions {
    fn default() -> Self {
        Self::SCREEN
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CompressReport {
    pub before: usize,
    pub after: usize,
    pub images_recompressed: usize,
    pub images_total: usize,
}

impl Document {
    /// Shrink the document. Never makes it bigger: if nothing helps, it is left unchanged.
    pub fn compress(&mut self, o: &CompressOptions) -> Result<CompressReport> {
        let bytes = self.to_bytes()?;
        let before = bytes.len();
        let (mut doc, state) = load(&bytes, self.password.as_deref())?;
        doc.trailer.remove(b"Encrypt");
        doc.encryption_state = None;

        let mut report = CompressReport { before, ..Default::default() };
        let display = image_display_sizes(&doc);
        // Soft masks (transparency) are images too, but are handled with their parent image.
        let masks: std::collections::HashSet<ObjectId> = doc
            .objects
            .values()
            .filter_map(|o| o.as_stream().ok()?.dict.get(b"SMask").ok()?.as_reference().ok())
            .collect();
        let images: Vec<ObjectId> = doc
            .objects
            .iter()
            .filter(|(id, o)| {
                !masks.contains(id) && o.as_stream().is_ok_and(|s| is_name(s.dict.get(b"Subtype").ok(), b"Image"))
            })
            .map(|(id, _)| *id)
            .collect();
        report.images_total = images.len();
        if o.recompress_images || o.image_dpi.is_some() {
            for id in images {
                let size = display.get(&id).copied();
                if recompress_image(&mut doc, id, size, o).unwrap_or(false) {
                    report.images_recompressed += 1;
                }
            }
        }
        if o.strip_metadata {
            strip_metadata(&mut doc);
        }
        doc.compress();
        doc.prune_objects();
        doc.renumber_objects();
        let out = save(doc, state.as_ref(), true)?;
        if out.len() < before {
            report.after = out.len();
            let pw = self.password.clone();
            self.replace_bytes(out, pw)?;
        } else {
            report.after = before;
        }
        Ok(report)
    }
}

fn is_name(o: Option<&Object>, name: &[u8]) -> bool {
    o.and_then(|o| o.as_name().ok()) == Some(name)
}

/// 2D affine matrix [a b c d e f].
type Mat = [f32; 6];

fn mul(m: &Mat, n: &Mat) -> Mat {
    [
        m[0] * n[0] + m[1] * n[2],
        m[0] * n[1] + m[1] * n[3],
        m[2] * n[0] + m[3] * n[2],
        m[2] * n[1] + m[3] * n[3],
        m[4] * n[0] + m[5] * n[2] + n[4],
        m[4] * n[1] + m[5] * n[3] + n[5],
    ]
}

fn num(o: &Object) -> f32 {
    o.as_float().or_else(|_| o.as_i64().map(|i| i as f32)).unwrap_or(0.0)
}

/// Largest size (in points) each image is drawn at anywhere in the document, found by walking
/// page and form XObject content streams and tracking the transformation matrix.
fn image_display_sizes(doc: &lopdf::Document) -> HashMap<ObjectId, (f32, f32)> {
    let mut out = HashMap::new();
    for page_id in doc.get_pages().into_values() {
        let content = doc.get_page_content(page_id);
        let xobjects = page_xobjects(doc, page_id);
        walk_content(doc, &content, &xobjects, [1.0, 0.0, 0.0, 1.0, 0.0, 0.0], 0, &mut out);
    }
    out
}

fn page_xobjects(doc: &lopdf::Document, page_id: ObjectId) -> HashMap<Vec<u8>, ObjectId> {
    let mut map = HashMap::new();
    let Ok((inline, ids)) = doc.get_page_resources(page_id) else { return map };
    let dicts = inline.into_iter().chain(ids.iter().filter_map(|id| doc.get_dictionary(*id).ok()));
    for res in dicts {
        collect_xobjects(doc, res, &mut map);
    }
    map
}

fn collect_xobjects(doc: &lopdf::Document, res: &Dictionary, map: &mut HashMap<Vec<u8>, ObjectId>) {
    let Ok(xo) = res.get(b"XObject") else { return };
    let Ok((_, Object::Dictionary(xo))) = doc.dereference(xo) else { return };
    for (name, v) in xo.iter() {
        if let Ok(id) = v.as_reference() {
            map.entry(name.clone()).or_insert(id);
        }
    }
}

fn walk_content(
    doc: &lopdf::Document,
    content: &[u8],
    xobjects: &HashMap<Vec<u8>, ObjectId>,
    base: Mat,
    depth: usize,
    out: &mut HashMap<ObjectId, (f32, f32)>,
) {
    if depth > 8 {
        return;
    }
    let Ok(ops) = Content::decode(content) else { return };
    let mut ctm = base;
    let mut stack = Vec::new();
    for op in ops.operations {
        match op.operator.as_str() {
            "q" => stack.push(ctm),
            "Q" => ctm = stack.pop().unwrap_or(base),
            "cm" if op.operands.len() == 6 => {
                let m: Vec<f32> = op.operands.iter().map(num).collect();
                ctm = mul(&[m[0], m[1], m[2], m[3], m[4], m[5]], &ctm);
            }
            "Do" => {
                let Some(id) = op.operands.first().and_then(|o| o.as_name().ok()).and_then(|n| xobjects.get(n)) else {
                    continue;
                };
                let Ok(stream) = doc.get_object(*id).and_then(Object::as_stream) else { continue };
                if is_name(stream.dict.get(b"Subtype").ok(), b"Image") {
                    let w = (ctm[0] * ctm[0] + ctm[1] * ctm[1]).sqrt();
                    let h = (ctm[2] * ctm[2] + ctm[3] * ctm[3]).sqrt();
                    let e = out.entry(*id).or_insert((0.0, 0.0));
                    *e = (e.0.max(w), e.1.max(h));
                } else if is_name(stream.dict.get(b"Subtype").ok(), b"Form") {
                    let m = stream
                        .dict
                        .get(b"Matrix")
                        .and_then(Object::as_array)
                        .ok()
                        .filter(|a| a.len() == 6)
                        .map(|a| [num(&a[0]), num(&a[1]), num(&a[2]), num(&a[3]), num(&a[4]), num(&a[5])])
                        .unwrap_or([1.0, 0.0, 0.0, 1.0, 0.0, 0.0]);
                    let mut inner = xobjects.clone();
                    if let Ok(res) = stream.dict.get(b"Resources").and_then(|r| doc.dereference(r))
                        && let (_, Object::Dictionary(res)) = res
                    {
                        let mut own = HashMap::new();
                        collect_xobjects(doc, res, &mut own);
                        inner.extend(own); // the form's own names take precedence
                    }
                    if let Ok(c) = stream.decompressed_content() {
                        walk_content(doc, &c, &inner, mul(&m, &ctm), depth + 1, out);
                    }
                }
            }
            _ => {}
        }
    }
}

/// Number of colour components for colour spaces we can safely re-encode as JPEG.
fn components(doc: &lopdf::Document, cs: Option<&Object>) -> Option<u8> {
    let (_, cs) = doc.dereference(cs?).ok()?;
    match cs {
        Object::Name(n) if n == b"DeviceRGB" => Some(3),
        Object::Name(n) if n == b"DeviceGray" => Some(1),
        Object::Array(a) if a.len() == 2 && is_name(a.first(), b"ICCBased") => {
            let (_, icc) = doc.dereference(&a[1]).ok()?;
            match icc.as_stream().ok()?.dict.get(b"N").ok()?.as_i64().ok()? {
                3 => Some(3),
                1 => Some(1),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Returns Ok(true) if the image was replaced with a smaller version.
fn recompress_image(
    doc: &mut lopdf::Document,
    id: ObjectId,
    display_pt: Option<(f32, f32)>,
    o: &CompressOptions,
) -> Result<bool> {
    let Ok(stream) = doc.get_object(id).and_then(Object::as_stream) else { return Ok(false) };
    let d = &stream.dict;
    let int = |k: &[u8]| d.get(k).and_then(Object::as_i64).ok();
    let (Some(w), Some(h)) = (int(b"Width"), int(b"Height")) else { return Ok(false) };
    let old_len = stream.content.len();
    // Leave small images, masks, unusual encodings and colour-keyed images alone.
    if w < 32 || h < 32 || old_len < 8 * 1024 || int(b"BitsPerComponent") != Some(8) {
        return Ok(false);
    }
    if d.get(b"ImageMask").and_then(Object::as_bool).unwrap_or(false)
        || d.get(b"Decode").is_ok()
        || d.get(b"Mask").is_ok_and(|m| m.as_array().is_ok())
    {
        return Ok(false);
    }
    let Some(n) = components(doc, d.get(b"ColorSpace").ok()) else { return Ok(false) };
    let filters: Vec<Vec<u8>> = match d.get(b"Filter") {
        Ok(Object::Name(f)) => vec![f.clone()],
        Ok(Object::Array(a)) => a.iter().filter_map(|f| f.as_name().ok().map(<[u8]>::to_vec)).collect(),
        _ => vec![],
    };
    let (w, h) = (w as u32, h as u32);
    let img: image::DynamicImage = if filters == [b"DCTDecode".to_vec()] {
        match image::load_from_memory_with_format(&stream.content, image::ImageFormat::Jpeg) {
            Ok(i) if i.width() == w && i.height() == h => i,
            _ => return Ok(false),
        }
    } else if filters.iter().all(|f| f == b"FlateDecode") {
        let Ok(raw) = stream.decompressed_content() else { return Ok(false) };
        if raw.len() != (w * h * n as u32) as usize {
            return Ok(false);
        }
        match n {
            3 => image::RgbImage::from_raw(w, h, raw).map(image::DynamicImage::ImageRgb8),
            _ => image::GrayImage::from_raw(w, h, raw).map(image::DynamicImage::ImageLuma8),
        }
        .ok_or_else(|| Error::Invalid("bad image data".into()))?
    } else {
        return Ok(false);
    };

    // Target pixel size from the display size and requested dpi.
    let (mut nw, mut nh) = (w, h);
    if let (Some(dpi), Some((pw, ph))) = (o.image_dpi, display_pt)
        && pw > 0.0
        && ph > 0.0
    {
        let tw = (pw / 72.0 * dpi).ceil() as u32;
        let th = (ph / 72.0 * dpi).ceil() as u32;
        // Only downsample when it is worthwhile (>15 % smaller in each direction).
        if (tw as f32) < w as f32 * 0.85 && (th as f32) < h as f32 * 0.85 {
            let s = (tw as f32 / w as f32).max(th as f32 / h as f32);
            nw = ((w as f32 * s).round() as u32).max(1);
            nh = ((h as f32 * s).round() as u32).max(1);
        }
    }
    if (nw, nh) == (w, h) && !o.recompress_images {
        return Ok(false);
    }
    let img = if (nw, nh) != (w, h) { img.resize_exact(nw, nh, image::imageops::FilterType::Lanczos3) } else { img };
    let mut jpeg = Vec::new();
    let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, o.jpeg_quality.clamp(1, 100));
    let res = match n {
        3 => enc.encode_image(&img.to_rgb8()),
        _ => enc.encode_image(&img.to_luma8()),
    };
    if res.is_err() || jpeg.len() as f32 > old_len as f32 * 0.9 {
        return Ok(false);
    }
    let Ok(stream) = doc.get_object_mut(id).and_then(Object::as_stream_mut) else { return Ok(false) };
    stream.dict.set("Filter", Object::Name(b"DCTDecode".to_vec()));
    stream.dict.remove(b"DecodeParms");
    stream.dict.set("Width", nw as i64);
    stream.dict.set("Height", nh as i64);
    stream.set_content(jpeg);
    stream.allows_compression = false;
    Ok(true)
}

fn strip_metadata(doc: &mut lopdf::Document) {
    if let Ok(cat) = doc.catalog_mut() {
        cat.remove(b"Metadata");
        cat.remove(b"PieceInfo");
    }
    doc.trailer.remove(b"Info");
    let pages: Vec<ObjectId> = doc.get_pages().into_values().collect();
    for id in pages {
        if let Ok(p) = doc.get_dictionary_mut(id) {
            p.remove(b"Thumb");
            p.remove(b"PieceInfo");
            p.remove(b"Metadata");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare() {
        assert!(version_less_than("1.4", "1.5"));
        assert!(!version_less_than("1.7", "1.5"));
        assert!(!version_less_than("2.0", "1.7"));
    }

    #[test]
    fn matrices_compose() {
        let scale = [2.0, 0.0, 0.0, 3.0, 0.0, 0.0];
        let shift = [1.0, 0.0, 0.0, 1.0, 10.0, 20.0];
        // Scale then translate: (1,1) -> (2,3) -> (12,23)
        let m = mul(&scale, &shift);
        assert_eq!((m[0] + m[4], m[3] + m[5]), (12.0, 23.0));
    }

    #[test]
    fn permissions_round_trip() {
        let a = Allow { print: true, copy: false, modify: false, annotate: false, fill_forms: true, assemble: false };
        assert_eq!(Allow::from_lopdf(a.to_lopdf()), a);
        assert_eq!(Allow::from_lopdf(Allow::ALL.to_lopdf()), Allow::ALL);
    }
}
