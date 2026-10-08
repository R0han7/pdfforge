//! PDF Forge core: open, render, search and edit PDF documents.
//!
//! Built on Google's [PDFium](https://pdfium.googlesource.com/pdfium/) via
//! [`pdfium-render`](https://crates.io/crates/pdfium-render). PDFium is loaded at runtime;
//! see [`library_candidates`] for where it is looked for.
//!
//! Page numbers in this API are zero-based indices unless a function says otherwise
//! (user-facing page ranges like `"1-3,7"` are one-based, see [`parse_page_ranges`]).
//!
//! ```no_run
//! use pdfforge_core::Document;
//!
//! let a = Document::open("a.pdf", None)?;
//! let b = Document::open("b.pdf", None)?;
//! let merged = Document::merge(&[&a, &b])?;
//! merged.save("merged.pdf")?;
//! # Ok::<(), pdfforge_core::Error>(())
//! ```

mod forms;
mod lowlevel;
mod ranges;
mod stamp;

use std::cell::Cell;
use std::fmt;
use std::mem::ManuallyDrop;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

use pdfium_render::prelude::*;

pub use forms::{FieldKind, FieldValue, FormField};
pub use lowlevel::{Allow, CompressOptions, CompressReport, Protection, Security};
pub use ranges::{format_page_ranges, parse_page_ranges};
pub use stamp::{Ink, StampFont};

// ------------------------------------------------------------------ errors

#[derive(Debug)]
pub enum Error {
    /// PDFium could not be found or loaded. Contains every path that was tried.
    LibraryNotFound(Vec<PathBuf>),
    /// The document is encrypted and needs a (different) password.
    PasswordRequired,
    /// A page index or page range was out of bounds or malformed.
    InvalidPages(String),
    /// A request that cannot be carried out (bad argument, unsupported field, ...).
    Invalid(String),
    Io(std::io::Error),
    Pdfium(PdfiumError),
    /// Error from the low-level PDF object layer (encryption, compression, form values).
    Lopdf(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::LibraryNotFound(tried) => {
                write!(f, "PDFium library not found (run scripts/fetch-pdfium.sh or set PDFIUM_LIB_PATH). Tried:")?;
                for p in tried {
                    write!(f, "\n  {}", p.display())?;
                }
                Ok(())
            }
            Error::PasswordRequired => write!(f, "this PDF is password protected; a valid password is required"),
            Error::InvalidPages(m) => write!(f, "invalid pages: {m}"),
            Error::Invalid(m) => write!(f, "{m}"),
            Error::Lopdf(m) => write!(f, "PDF error: {m}"),
            Error::Io(e) => write!(f, "{e}"),
            Error::Pdfium(e) => write!(f, "PDFium error: {e:?}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<PdfiumError> for Error {
    fn from(e: PdfiumError) -> Self {
        match e {
            PdfiumError::PdfiumLibraryInternalError(PdfiumInternalError::PasswordError) => Error::PasswordRequired,
            e => Error::Pdfium(e),
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

// ------------------------------------------------------------------ library loading

static PDFIUM: OnceLock<std::result::Result<Pdfium, Vec<PathBuf>>> = OnceLock::new();

// PDFium is not thread-safe, even across separate documents, and pdfium-render only serialises
// individual FFI calls. Every operation that touches PDFium holds this process-wide lock for
// its whole duration. It is re-entrant per thread because operations call each other.
static LOCK: Mutex<()> = Mutex::new(());

thread_local! {
    static HELD: Cell<usize> = const { Cell::new(0) };
}

struct Guard(#[allow(dead_code)] Option<MutexGuard<'static, ()>>);

fn lock() -> Guard {
    HELD.with(|h| {
        let g = (h.get() == 0).then(|| LOCK.lock().unwrap_or_else(|e| e.into_inner()));
        h.set(h.get() + 1);
        Guard(g)
    })
}

impl Drop for Guard {
    fn drop(&mut self) {
        HELD.with(|h| h.set(h.get() - 1));
        // The mutex guard (if this was the outermost lock) is released after this body.
    }
}

/// Places PDFium is searched for, in order:
/// 1. `$PDFIUM_LIB_PATH` (a library file or a directory containing it)
/// 2. next to the running executable, and `../lib` relative to it
/// 3. `vendor/pdfium/lib` in this source tree (for `cargo run` during development)
/// 4. the system library search path
pub fn library_candidates() -> Vec<PathBuf> {
    let name = Pdfium::pdfium_platform_library_name();
    let mut v = Vec::new();
    if let Some(p) = std::env::var_os("PDFIUM_LIB_PATH").map(PathBuf::from) {
        if p.is_dir() { v.push(p.join(&name)) } else { v.push(p) }
    }
    if let Some(dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)) {
        v.push(dir.join(&name));
        v.push(dir.join("../lib").join(&name));
    }
    v.push(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vendor/pdfium/lib").join(&name));
    v.push(PathBuf::from(&name));
    v
}

/// The shared PDFium instance, loaded on first use.
pub fn pdfium() -> Result<&'static Pdfium> {
    PDFIUM
        .get_or_init(|| {
            let candidates = library_candidates();
            for c in &candidates {
                // Bare names go through the system loader; paths must exist.
                if (c.components().count() > 1 && !c.exists()) || c.as_os_str().is_empty() {
                    continue;
                }
                if let Ok(b) = Pdfium::bind_to_library(c) {
                    return Ok(Pdfium::new(b));
                }
            }
            Err(candidates)
        })
        .as_ref()
        .map_err(|tried| Error::LibraryNotFound(tried.clone()))
}

// ------------------------------------------------------------------ types

/// Document metadata and summary.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DocInfo {
    pub title: String,
    pub author: String,
    pub subject: String,
    pub keywords: String,
    pub creator: String,
    pub producer: String,
    pub created: String,
    pub modified: String,
    pub pages: usize,
    pub version: String,
    pub encrypted: bool,
}

/// An RGBA8 bitmap of a rendered page.
#[derive(Clone)]
pub struct Bitmap {
    pub width: u32,
    pub height: u32,
    /// Straight RGBA, row-major, `width * height * 4` bytes.
    pub rgba: Vec<u8>,
}

impl fmt::Debug for Bitmap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Bitmap({}x{})", self.width, self.height)
    }
}

/// A rectangle as fractions of the displayed page (0..1), origin top-left, page rotation applied.
/// Multiply by the on-screen page size to get pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NormRect {
    pub x0: f32,
    pub y0: f32,
    pub x1: f32,
    pub y1: f32,
}

/// One occurrence of a search term.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchHit {
    pub page: usize,
    /// One rect per text segment (a match can wrap across lines).
    pub rects: Vec<NormRect>,
    /// A little context around the match, for result lists.
    pub snippet: String,
}

/// A bookmark / table-of-contents entry, flattened in reading order.
#[derive(Debug, Clone, PartialEq)]
pub struct OutlineItem {
    pub title: String,
    pub page: Option<usize>,
    pub depth: usize,
}

/// Clockwise rotation in quarter turns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rotation {
    R0,
    R90,
    R180,
    R270,
}

impl Rotation {
    pub fn from_degrees(deg: i32) -> Option<Self> {
        match deg.rem_euclid(360) {
            0 => Some(Rotation::R0),
            90 => Some(Rotation::R90),
            180 => Some(Rotation::R180),
            270 => Some(Rotation::R270),
            _ => None,
        }
    }

    pub fn degrees(self) -> i32 {
        match self {
            Rotation::R0 => 0,
            Rotation::R90 => 90,
            Rotation::R180 => 180,
            Rotation::R270 => 270,
        }
    }

    fn to_pdfium(self) -> PdfPageRenderRotation {
        match self {
            Rotation::R0 => PdfPageRenderRotation::None,
            Rotation::R90 => PdfPageRenderRotation::Degrees90,
            Rotation::R180 => PdfPageRenderRotation::Degrees180,
            Rotation::R270 => PdfPageRenderRotation::Degrees270,
        }
    }

    fn from_pdfium(r: PdfPageRenderRotation) -> Self {
        match r {
            PdfPageRenderRotation::None => Rotation::R0,
            PdfPageRenderRotation::Degrees90 => Rotation::R90,
            PdfPageRenderRotation::Degrees180 => Rotation::R180,
            PdfPageRenderRotation::Degrees270 => Rotation::R270,
        }
    }
}

// ------------------------------------------------------------------ document

/// An open PDF document.
pub struct Document {
    // Dropped explicitly in `Drop` while holding the PDFium lock.
    doc: ManuallyDrop<PdfDocument<'static>>,
    path: Option<PathBuf>,
    /// Password the document was opened with (needed to re-open it after low-level edits).
    password: Option<String>,
    /// Exact bytes from the last low-level edit (e.g. compression with object streams), served
    /// by `to_bytes` until PDFium modifies the document again. PDFium's own writer would undo
    /// some of that work.
    cached: Option<Vec<u8>>,
}

impl fmt::Debug for Document {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let _g = lock();
        f.debug_struct("Document").field("path", &self.path).field("pages", &self.page_count()).finish()
    }
}

impl Drop for Document {
    fn drop(&mut self) {
        let _g = lock();
        // SAFETY: `doc` is never used again after this.
        unsafe { ManuallyDrop::drop(&mut self.doc) };
    }
}

impl Document {
    /// Open a PDF file. `password` is only needed for encrypted documents.
    pub fn open(path: impl AsRef<Path>, password: Option<&str>) -> Result<Self> {
        let _g = lock();
        let path = path.as_ref();
        let bytes = std::fs::read(path)?;
        let mut d = Self::from_bytes(bytes, password)?;
        d.path = Some(path.to_path_buf());
        d.password = password.map(str::to_string);
        Ok(d)
    }

    /// Load a PDF from memory.
    pub fn from_bytes(bytes: Vec<u8>, password: Option<&str>) -> Result<Self> {
        let _g = lock();
        let doc = pdfium()?.load_pdf_from_byte_vec(bytes, password)?;
        Ok(Self { doc: ManuallyDrop::new(doc), path: None, password: password.map(str::to_string), cached: None })
    }

    /// A new document with no pages.
    pub fn new_empty() -> Result<Self> {
        let _g = lock();
        Ok(Self { doc: ManuallyDrop::new(pdfium()?.create_new_pdf()?), path: None, password: None, cached: None })
    }

    /// The file this document was opened from, if any.
    pub fn path(&self) -> Option<&Path> {
        let _g = lock();
        self.path.as_deref()
    }

    pub fn page_count(&self) -> usize {
        let _g = lock();
        self.doc.pages().len().max(0) as usize
    }

    fn check(&self, page: usize) -> Result<i32> {
        if page < self.page_count() {
            Ok(page as i32)
        } else {
            Err(Error::InvalidPages(format!("page {} does not exist (document has {})", page + 1, self.page_count())))
        }
    }

    fn check_all(&self, pages: &[usize]) -> Result<()> {
        pages.iter().try_for_each(|&p| self.check(p).map(drop))
    }

    /// Displayed page size in points (1/72 inch), with the page's rotation applied.
    pub fn page_size(&self, page: usize) -> Result<(f32, f32)> {
        let _g = lock();
        let p = self.doc.pages().get(self.check(page)?)?;
        Ok((p.width().value, p.height().value))
    }

    pub fn rotation(&self, page: usize) -> Result<Rotation> {
        let _g = lock();
        let p = self.doc.pages().get(self.check(page)?)?;
        Ok(Rotation::from_pdfium(p.rotation()?))
    }

    pub fn info(&self) -> DocInfo {
        let _g = lock();
        let m = self.doc.metadata();
        let tag = |t| m.get(t).map(|v| v.value().to_string()).unwrap_or_default();
        DocInfo {
            title: tag(PdfDocumentMetadataTagType::Title),
            author: tag(PdfDocumentMetadataTagType::Author),
            subject: tag(PdfDocumentMetadataTagType::Subject),
            keywords: tag(PdfDocumentMetadataTagType::Keywords),
            creator: tag(PdfDocumentMetadataTagType::Creator),
            producer: tag(PdfDocumentMetadataTagType::Producer),
            created: tag(PdfDocumentMetadataTagType::CreationDate),
            modified: tag(PdfDocumentMetadataTagType::ModificationDate),
            pages: self.page_count(),
            version: version_string(self.doc.version()),
            // pdfium-render reports AES-256 (revision 6) as an unknown revision, which still
            // means "encrypted".
            encrypted: !matches!(
                self.doc.permissions().security_handler_revision(),
                Ok(PdfSecurityHandlerRevision::Unprotected)
            ),
        }
    }

    // -------------------------------------------------------------- rendering

    /// Render a page at `scale` × 72 dpi (so `scale = 2.0` is 144 dpi). Annotations and form
    /// fields are drawn.
    pub fn render(&self, page: usize, scale: f32) -> Result<Bitmap> {
        let _g = lock();
        let p = self.doc.pages().get(self.check(page)?)?;
        let cfg = PdfRenderConfig::new().scale_page_by_factor(scale.clamp(0.05, 20.0)).render_form_data(true);
        let bmp = p.render_with_config(&cfg)?;
        Ok(Bitmap { width: bmp.width() as u32, height: bmp.height() as u32, rgba: bmp.as_rgba_bytes() })
    }

    /// Render a page so it fits in a `max` × `max` pixel box (for thumbnails).
    pub fn render_thumbnail(&self, page: usize, max: u32) -> Result<Bitmap> {
        let _g = lock();
        let (w, h) = self.page_size(page)?;
        self.render(page, max as f32 / w.max(h).max(1.0))
    }

    // -------------------------------------------------------------- text

    /// All text on a page, in PDFium's reading order.
    pub fn text(&self, page: usize) -> Result<String> {
        let _g = lock();
        let p = self.doc.pages().get(self.check(page)?)?;
        Ok(all_text(&p.text()?))
    }

    /// Find every occurrence of `query` in the document.
    pub fn search(&self, query: &str, match_case: bool, whole_word: bool) -> Result<Vec<SearchHit>> {
        let _g = lock();
        if query.trim().is_empty() {
            return Ok(Vec::new());
        }
        let opts = PdfSearchOptions::new().match_case(match_case).match_whole_word(whole_word);
        let mut hits = Vec::new();
        for i in 0..self.page_count() {
            let page = self.doc.pages().get(i as i32)?;
            let text = page.text()?;
            let (w, h) = (page.width().value.max(1.0), page.height().value.max(1.0));
            // Map page space -> displayed (rotated) pixel space at 1 px per point.
            let cfg = PdfRenderConfig::new().scale_page_by_factor(1.0);
            let to_norm = |x: PdfPoints, y: PdfPoints| -> (f32, f32) {
                page.points_to_pixels(x, y, &cfg).map(|(px, py)| (px as f32 / w, py as f32 / h)).unwrap_or((0.0, 0.0))
            };
            let flat = flatten_ws(&all_text(&text));
            let mut cursor = 0; // byte offset in `flat` after the previous match on this page
            let search = text.search(query, &opts)?;
            while let Some(segments) = search.find_next() {
                let mut rects = Vec::new();
                let mut found = String::new();
                for seg in segments.iter() {
                    let b = seg.bounds();
                    let (ax, ay) = to_norm(b.left(), b.top());
                    let (bx, by) = to_norm(b.right(), b.bottom());
                    rects.push(NormRect { x0: ax.min(bx), y0: ay.min(by), x1: ax.max(bx), y1: ay.max(by) });
                    found.push_str(&seg.text());
                }
                let snippet = snippet_after(&flat, &mut cursor, &found, query);
                hits.push(SearchHit { page: i, rects, snippet });
            }
        }
        Ok(hits)
    }

    /// Whether the document has an interactive (AcroForm) form. Cheap; use it before
    /// [`Document::form_fields`] on large documents.
    pub fn has_form(&self) -> bool {
        let _g = lock();
        self.doc.form().is_some_and(|f| !matches!(f.form_type(), PdfFormType::None))
    }

    /// Bookmarks, flattened depth-first.
    pub fn outline(&self) -> Vec<OutlineItem> {
        let _g = lock();
        fn walk(b: PdfBookmark<'_>, depth: usize, out: &mut Vec<OutlineItem>) {
            if out.len() > 10_000 || depth > 64 {
                return; // guard against malformed, cyclic outlines
            }
            out.push(OutlineItem {
                title: b.title().unwrap_or_default(),
                page: b.destination().and_then(|d| d.page_index().ok()).map(|i| i as usize),
                depth,
            });
            for c in b.iter_direct_children() {
                walk(c, depth + 1, out);
            }
        }
        let mut out = Vec::new();
        if let Some(root) = self.doc.bookmarks().root() {
            for b in root.iter_siblings() {
                walk(b, 0, &mut out);
            }
        }
        out
    }

    // -------------------------------------------------------------- page editing (in place)

    /// Rotate pages clockwise by `by` (relative to their current rotation).
    pub fn rotate_pages(&mut self, pages: &[usize], by: Rotation) -> Result<()> {
        let _g = lock();
        self.touch();
        self.check_all(pages)?;
        for &i in pages {
            let mut p = self.doc.pages().get(i as i32)?;
            let cur = Rotation::from_pdfium(p.rotation()?).degrees();
            let new = Rotation::from_degrees(cur + by.degrees()).unwrap_or(Rotation::R0);
            p.set_rotation(new.to_pdfium());
        }
        Ok(())
    }

    /// Delete pages. Deleting every page is refused, since an empty PDF is not useful.
    pub fn delete_pages(&mut self, pages: &[usize]) -> Result<()> {
        let _g = lock();
        self.touch();
        self.check_all(pages)?;
        let mut sorted = pages.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        if sorted.len() >= self.page_count() {
            return Err(Error::InvalidPages("cannot delete every page".into()));
        }
        for &i in sorted.iter().rev() {
            self.doc.pages().get(i as i32)?.delete()?;
        }
        Ok(())
    }

    /// Copy all pages of `other` into this document before page `at` (`at == page_count()` appends).
    pub fn insert_document(&mut self, other: &Document, at: usize) -> Result<()> {
        let _g = lock();
        self.touch();
        if at > self.page_count() {
            return Err(Error::InvalidPages(format!("insert position {} is past the end", at + 1)));
        }
        if other.page_count() == 0 {
            return Ok(());
        }
        let range = 0..=(other.page_count() as i32 - 1);
        self.doc.pages_mut().copy_page_range_from_document(&other.doc, range, at as i32)?;
        Ok(())
    }

    /// Insert a blank page before `at`, sized like the neighbouring page (or US Letter).
    pub fn insert_blank_page(&mut self, at: usize) -> Result<()> {
        let _g = lock();
        self.touch();
        if at > self.page_count() {
            return Err(Error::InvalidPages(format!("insert position {} is past the end", at + 1)));
        }
        let size = match self.page_count() {
            0 => us_letter(),
            n => {
                let p = self.doc.pages().get(at.min(n - 1) as i32)?;
                PdfPagePaperSize::from_points(p.width(), p.height())
            }
        };
        self.doc.pages_mut().create_page_at_index(size, at as i32)?;
        Ok(())
    }

    // -------------------------------------------------------------- page editing (new document)

    /// A new document containing `pages` of this one, in the given order (duplicates allowed).
    pub fn extract(&self, pages: &[usize]) -> Result<Document> {
        let _g = lock();
        if pages.is_empty() {
            return Err(Error::InvalidPages("no pages selected".into()));
        }
        self.check_all(pages)?;
        let mut out = Document::new_empty()?;
        // Contiguous ascending runs are imported in one call each so shared resources
        // (fonts, images) are not duplicated more than necessary.
        let mut start = 0;
        while start < pages.len() {
            let mut end = start;
            while end + 1 < pages.len() && pages[end + 1] == pages[end] + 1 {
                end += 1;
            }
            let dest = out.page_count() as i32;
            out.doc.pages_mut().copy_page_range_from_document(
                &self.doc,
                pages[start] as i32..=pages[end] as i32,
                dest,
            )?;
            start = end + 1;
        }
        Ok(out)
    }

    /// Reorder pages: `order[i]` is the current index of the page that should end up at position
    /// `i`. Must be a permutation of `0..page_count()`.
    pub fn reorder(&mut self, order: &[usize]) -> Result<()> {
        let _g = lock();
        let mut seen = vec![false; self.page_count()];
        if order.len() != seen.len() || !order.iter().all(|&i| i < seen.len() && !std::mem::replace(&mut seen[i], true))
        {
            return Err(Error::InvalidPages("new order must list every page exactly once".into()));
        }
        self.touch();
        let mut new = self.extract(order)?;
        std::mem::swap(&mut self.doc, &mut new.doc); // the old pages drop with `new`
        Ok(())
    }

    /// Concatenate documents into a new one.
    pub fn merge(docs: &[&Document]) -> Result<Document> {
        let _g = lock();
        let mut out = Document::new_empty()?;
        for d in docs {
            let at = out.page_count();
            out.insert_document(d, at)?;
        }
        Ok(out)
    }

    /// Split into consecutive chunks of `every` pages.
    pub fn split_every(&self, every: usize) -> Result<Vec<Document>> {
        let _g = lock();
        if every == 0 {
            return Err(Error::InvalidPages("chunk size must be at least 1".into()));
        }
        let all: Vec<usize> = (0..self.page_count()).collect();
        all.chunks(every).map(|c| self.extract(c)).collect()
    }

    /// Split into one document per group of pages (e.g. from several page ranges).
    pub fn split_groups(&self, groups: &[Vec<usize>]) -> Result<Vec<Document>> {
        let _g = lock();
        groups.iter().map(|g| self.extract(g)).collect()
    }

    // -------------------------------------------------------------- saving

    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        let _g = lock();
        if let Some(b) = &self.cached {
            return Ok(b.clone());
        }
        Ok(self.doc.save_to_bytes()?)
    }

    /// Swap in a new serialisation of this document (after a low-level edit), keeping the path.
    fn replace_bytes(&mut self, bytes: Vec<u8>, password: Option<String>) -> Result<()> {
        let _g = lock();
        let mut new = Document::from_bytes(bytes.clone(), password.as_deref())?;
        std::mem::swap(&mut self.doc, &mut new.doc);
        self.password = password;
        self.cached = Some(bytes);
        Ok(())
    }

    /// Call before any PDFium-side modification: cached low-level bytes become stale.
    fn touch(&mut self) {
        self.cached = None;
    }

    /// The password the document was opened (or last protected) with.
    pub fn password(&self) -> Option<&str> {
        self.password.as_deref()
    }

    /// Save to `path`. Writes to a temporary file first and renames, so the original is never
    /// left half-written — this also makes saving over the file you opened safe.
    pub fn save(&self, path: impl AsRef<Path>) -> Result<()> {
        let _g = lock();
        let path = path.as_ref();
        let bytes = self.to_bytes()?;
        let tmp = path.with_extension("pdfforge-tmp");
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, path).inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })?;
        Ok(())
    }
}

fn version_string(v: PdfDocumentVersion) -> String {
    match v {
        PdfDocumentVersion::Unset => String::new(),
        PdfDocumentVersion::Pdf1_0 => "1.0".into(),
        PdfDocumentVersion::Pdf1_1 => "1.1".into(),
        PdfDocumentVersion::Pdf1_2 => "1.2".into(),
        PdfDocumentVersion::Pdf1_3 => "1.3".into(),
        PdfDocumentVersion::Pdf1_4 => "1.4".into(),
        PdfDocumentVersion::Pdf1_5 => "1.5".into(),
        PdfDocumentVersion::Pdf1_6 => "1.6".into(),
        PdfDocumentVersion::Pdf1_7 => "1.7".into(),
        PdfDocumentVersion::Pdf2_0 => "2.0".into(),
        PdfDocumentVersion::Other(n) => format!("{}.{}", n / 10, n % 10),
    }
}

/// All text on a page in content-stream order (including PDFium's generated spaces and line
/// breaks). `PdfPageText::all()` clips to the page box *after* rotation while character
/// positions are unrotated, which drops text on 90°/270° pages, so we read characters directly.
fn all_text(t: &PdfPageText<'_>) -> String {
    t.chars().iter().filter_map(|c| c.unicode_char()).collect()
}

fn flatten_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// ~60 characters of context around the next case-insensitive occurrence of the match in
/// `flat` at or after `*cursor`, advancing the cursor past it so repeated matches on a page
/// each get their own snippet.
fn snippet_after(flat: &str, cursor: &mut usize, found: &str, query: &str) -> String {
    let needle = flatten_ws(if found.trim().is_empty() { query } else { found });
    // Compare char-by-char case-insensitively without changing byte offsets (to_lowercase can).
    let lower_eq = |a: &str, b: &str| a.chars().flat_map(char::to_lowercase).eq(b.chars().flat_map(char::to_lowercase));
    let pos = flat
        .char_indices()
        .map(|(i, _)| i)
        .filter(|&i| i >= *cursor)
        .find(|&i| flat.get(i..i + needle.len()).is_some_and(|w| lower_eq(w, &needle)));
    let Some(pos) = pos else { return needle };
    *cursor = pos + needle.len();
    let start = floor_char(flat, pos.saturating_sub(30));
    let end = floor_char(flat, (pos + needle.len() + 30).min(flat.len()));
    let mut out = String::new();
    if start > 0 {
        out.push('…');
    }
    out.push_str(&flat[start..end]);
    if end < flat.len() {
        out.push('…');
    }
    out
}

fn floor_char(s: &str, mut i: usize) -> usize {
    i = i.min(s.len());
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Build a simple text-only PDF (one entry per page) — handy for tests and examples.
pub fn sample_document(pages: &[&str]) -> Result<Document> {
    let _g = lock();
    let mut d = Document::new_empty()?;
    let font = d.doc.fonts_mut().helvetica();
    for text in pages {
        let mut page = d.doc.pages_mut().create_page_at_end(us_letter())?;
        for (line_no, line) in text.lines().enumerate() {
            page.objects_mut().create_text_object(
                PdfPoints::new(72.0),
                PdfPoints::new(700.0 - 20.0 * line_no as f32),
                line,
                font,
                PdfPoints::new(14.0),
            )?;
        }
    }
    Ok(d)
}

/// Exactly 8.5 × 11 in (612 × 792 pt). pdfium-render's named size is rounded via millimetres.
fn us_letter() -> PdfPagePaperSize {
    PdfPagePaperSize::new_custom(PdfPoints::new(612.0), PdfPoints::new(792.0))
}

#[cfg(test)]
mod tests;
