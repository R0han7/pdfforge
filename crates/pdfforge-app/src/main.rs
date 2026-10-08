//! PDF Forge desktop app: continuous-scroll PDF viewer with thumbnails, bookmarks, search,
//! and page tools (rotate, delete, reorder, insert, extract, split, merge) with undo.

mod sign;
mod view;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use eframe::egui::{
    self, Align, Color32, ColorImage, Id, Key, KeyboardShortcut, Modifiers, Pos2, Rect, RichText, ScrollArea, Sense,
    Stroke, StrokeKind, TextureHandle, TextureOptions, Vec2,
};
use pdfforge_core::{
    Allow, CompressOptions, Document, Error, FieldKind, FieldValue, FormField, NormRect, OutlineItem, Protection,
    Rotation, SearchHit, parse_page_ranges,
};

use sign::{SignDialog, SignOutcome, Signature};

use view::{GAP, Layout, Selection, moved_order, positions_of};

const THUMB_W: f32 = 120.0;
const THUMB_ROW: f32 = THUMB_W * 1.42 + 26.0;
const UNDO_LIMIT: usize = 30;
/// Pages rendered per frame at most; the rest are picked up on following frames so scrolling
/// through a long document never blocks the UI for long.
const RENDER_BUDGET: usize = 2;
const THUMB_BUDGET: usize = 4;

fn main() -> eframe::Result {
    let path = std::env::args_os().nth(1).map(PathBuf::from);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1400.0, 900.0])
            .with_min_inner_size([640.0, 400.0])
            .with_drag_and_drop(true),
        ..Default::default()
    };
    eframe::run_native("PDF Forge", options, Box::new(move |cc| Ok(Box::new(App::new(&cc.egui_ctx, path)))))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    Pages,
    Bookmarks,
    Search,
}

/// Something that would discard unsaved changes, waiting for confirmation.
#[derive(Clone)]
enum Pending {
    Open(Option<PathBuf>),
    Close,
    Quit,
}

struct Tex {
    tex: TextureHandle,
    /// Render scale (pixels per PDF point) the texture was made at.
    scale: f32,
}

#[derive(Default)]
struct SearchState {
    query: String,
    match_case: bool,
    whole_word: bool,
    hits: Vec<SearchHit>,
    by_page: HashMap<usize, Vec<usize>>,
    current: Option<usize>,
    /// Query the current hits belong to, to tell "search again" from "next result".
    searched: Option<(String, bool, bool)>,
    focus: bool,
}

/// An open document plus everything derived from it.
struct Open {
    doc: Document,
    path: Option<PathBuf>,
    /// Page sizes in PDF points, rotation applied.
    sizes: Vec<(f32, f32)>,
    outline: Vec<OutlineItem>,
    pages: HashMap<usize, Tex>,
    thumbs: HashMap<usize, TextureHandle>,
    selection: Selection,
    /// Snapshots for undo/redo: document bytes and the password needed to open them.
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    dirty: bool,
    fields: Vec<FormField>,
    encrypted: bool,
}

type Snapshot = (Vec<u8>, Option<String>);

impl Open {
    fn new(doc: Document, path: Option<PathBuf>) -> Self {
        let mut o = Self {
            outline: doc.outline(),
            doc,
            path,
            sizes: Vec::new(),
            pages: HashMap::new(),
            thumbs: HashMap::new(),
            selection: Selection::new(0),
            undo: Vec::new(),
            redo: Vec::new(),
            dirty: false,
            fields: Vec::new(),
            encrypted: false,
        };
        o.refresh();
        o
    }

    /// Recompute derived data after the document changed.
    fn refresh(&mut self) {
        let n = self.doc.page_count();
        self.sizes = (0..n).map(|i| self.doc.page_size(i).unwrap_or((612.0, 792.0))).collect();
        self.pages.clear();
        self.thumbs.clear();
        self.selection = Selection::new(n);
        self.fields = if self.doc.has_form() { self.doc.form_fields().unwrap_or_default() } else { Vec::new() };
        self.encrypted = self.doc.info().encrypted;
    }

    fn snapshot(&self) -> pdfforge_core::Result<Snapshot> {
        Ok((self.doc.to_bytes()?, self.doc.password().map(str::to_string)))
    }

    fn name(&self) -> String {
        self.path
            .as_deref()
            .and_then(Path::file_name)
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Untitled.pdf".into())
    }
}

struct Status {
    text: String,
    error: bool,
    at: Instant,
}

struct App {
    open: Option<Open>,
    zoom: f32,
    fit: Option<Fit>,
    layout: Layout,
    layout_key: (f32, usize, u64),
    generation: u64,
    current_page: usize,
    goto: Option<usize>,
    page_field: String,
    side: Side,
    search: SearchState,
    status: Option<Status>,
    pending: Option<Pending>,
    allow_close: bool,
    password_prompt: Option<(PathBuf, String, bool)>,
    split_dialog: Option<String>,
    info_open: bool,
    viewport_size: Vec2,
    load_error: Option<String>,
    // Forms
    show_fields: bool,
    editing: Option<TextEditing>,
    choice: Option<ChoicePopup>,
    // Signatures
    sign_dialog: Option<SignDialog>,
    /// Where a signature goes once created: a signature field's box, or `None` to place by hand.
    sign_target: Option<(usize, NormRect)>,
    placing: Option<Signature>,
    // Security / optimisation dialogs
    protect_dialog: Option<ProtectDialog>,
    /// Selected preset and the document size when the dialog opened.
    compress_dialog: Option<(usize, usize)>,
    last_title: String,
}

/// A text field being edited in place.
struct TextEditing {
    name: String,
    page: usize,
    rect: NormRect,
    text: String,
    multiline: bool,
    focus: bool,
}

/// An open dropdown / list field menu.
struct ChoicePopup {
    name: String,
    options: Vec<String>,
    current: String,
    pos: Pos2,
    width: f32,
    opened: bool,
}

struct ProtectDialog {
    user: String,
    confirm: String,
    owner: String,
    allow: Allow,
    error: Option<String>,
}

const COMPRESS_PRESETS: [(&str, &str, CompressOptions); 4] = [
    ("Screen", "120 dpi images — e-mail and on-screen reading", CompressOptions::SCREEN),
    ("Print", "200 dpi images, high quality — printing", CompressOptions::PRINT),
    ("Smallest", "72 dpi images, lower quality, metadata removed", CompressOptions::SMALLEST),
    ("Lossless", "No image changes — only removes waste", CompressOptions::LOSSLESS),
];

/// Something the user did on a page, applied after drawing.
enum PageAction {
    Toggle(String, bool),
    Radio(String, String),
    EditText(TextEditing),
    Choose(ChoicePopup),
    SignField(usize, NormRect),
    Place(usize, NormRect),
    CommitText,
    CancelText,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Fit {
    Width,
    Page,
}

impl App {
    fn new(ctx: &egui::Context, path: Option<PathBuf>) -> Self {
        // We use Ctrl +/- for document zoom, not UI scaling.
        ctx.options_mut(|o| o.zoom_with_keyboard = false);
        let mut app = Self {
            open: None,
            zoom: 1.0,
            fit: Some(Fit::Width),
            layout: Layout::default(),
            layout_key: (0.0, 0, 0),
            generation: 0,
            current_page: 0,
            goto: None,
            page_field: String::new(),
            side: Side::Pages,
            search: SearchState::default(),
            status: None,
            pending: None,
            allow_close: false,
            password_prompt: None,
            split_dialog: None,
            info_open: false,
            viewport_size: Vec2::ZERO,
            load_error: pdfforge_core::pdfium().err().map(|e| e.to_string()),
            show_fields: true,
            editing: None,
            choice: None,
            sign_dialog: None,
            sign_target: None,
            placing: None,
            protect_dialog: None,
            compress_dialog: None,
            last_title: String::new(),
        };
        if let Some(p) = path {
            app.load(p, None);
        }
        app
    }

    fn say(&mut self, text: impl Into<String>) {
        self.status = Some(Status { text: text.into(), error: false, at: Instant::now() });
    }

    fn fail(&mut self, text: impl Into<String>) {
        self.status = Some(Status { text: text.into(), error: true, at: Instant::now() });
    }

    fn dirty(&self) -> bool {
        self.open.as_ref().is_some_and(|o| o.dirty)
    }

    // ------------------------------------------------------------ files

    fn load(&mut self, path: PathBuf, password: Option<&str>) {
        match Document::open(&path, password) {
            Ok(doc) => {
                let n = doc.page_count();
                self.open = Some(Open::new(doc, Some(path.clone())));
                self.generation += 1;
                self.fit = Some(Fit::Width);
                self.current_page = 0;
                self.goto = Some(0);
                self.search.hits.clear();
                self.search.by_page.clear();
                self.search.current = None;
                self.search.searched = None;
                self.side = if self.open.as_ref().is_some_and(|o| !o.outline.is_empty()) {
                    Side::Bookmarks
                } else {
                    Side::Pages
                };
                self.say(format!("Opened {} ({n} pages)", path.display()));
            }
            Err(Error::PasswordRequired) => {
                let wrong = password.is_some();
                self.password_prompt = Some((path, String::new(), wrong));
            }
            Err(e) => self.fail(format!("Could not open {}: {e}", path.display())),
        }
    }

    /// Run `action` now, or ask first if it would discard unsaved changes.
    fn request(&mut self, action: Pending) {
        if self.dirty() {
            self.pending = Some(action);
        } else {
            self.perform(action);
        }
    }

    fn perform(&mut self, action: Pending) {
        match action {
            Pending::Open(Some(p)) => self.load(p, None),
            Pending::Open(None) => {
                if let Some(p) = rfd::FileDialog::new().add_filter("PDF", &["pdf", "PDF"]).pick_file() {
                    self.load(p, None);
                }
            }
            Pending::Close => {
                self.open = None;
                self.generation += 1;
            }
            Pending::Quit => {}
        }
    }

    fn save(&mut self, ctx: &egui::Context, save_as: bool) {
        self.commit_text();
        let Some(o) = &self.open else { return };
        let target = match (&o.path, save_as) {
            (Some(p), false) => Some(p.clone()),
            _ => rfd::FileDialog::new().add_filter("PDF", &["pdf"]).set_file_name(o.name()).save_file(),
        };
        let Some(path) = target else { return };
        let path = with_pdf_extension(path);
        match o.doc.save(&path) {
            Ok(()) => {
                let o = self.open.as_mut().expect("checked above");
                o.path = Some(path.clone());
                o.dirty = false;
                self.say(format!("Saved {}", path.display()));
                ctx.send_viewport_cmd(egui::ViewportCommand::Title(self.title()));
            }
            Err(e) => self.fail(format!("Could not save {}: {e}", path.display())),
        }
    }

    fn title(&self) -> String {
        match &self.open {
            Some(o) => format!("{}{} — PDF Forge", if o.dirty { "• " } else { "" }, o.name()),
            None => "PDF Forge".into(),
        }
    }

    // ------------------------------------------------------------ editing

    /// Apply an edit with an undo snapshot. `f` returns the pages to select afterwards.
    fn edit(&mut self, what: &str, f: impl FnOnce(&mut Document) -> pdfforge_core::Result<Vec<usize>>) {
        let Some(o) = &mut self.open else { return };
        let snapshot = match o.snapshot() {
            Ok(b) => b,
            Err(e) => return self.fail(format!("{what} failed: {e}")),
        };
        match f(&mut o.doc) {
            Ok(select) => {
                o.undo.push(snapshot);
                if o.undo.len() > UNDO_LIMIT {
                    o.undo.remove(0);
                }
                o.redo.clear();
                o.dirty = true;
                o.refresh();
                o.selection.set(&select);
                self.after_change();
                self.say(what.to_string());
            }
            Err(e) => self.fail(format!("{what} failed: {e}")),
        }
    }

    fn after_change(&mut self) {
        self.generation += 1;
        self.editing = None;
        self.choice = None;
        let n = self.open.as_ref().map_or(0, |o| o.doc.page_count());
        self.current_page = self.current_page.min(n.saturating_sub(1));
        // Search results point at old page positions.
        self.search.hits.clear();
        self.search.by_page.clear();
        self.search.current = None;
        self.search.searched = None;
    }

    fn undo_redo(&mut self, redo: bool) {
        let Some(o) = &mut self.open else { return };
        if (redo && o.redo.is_empty()) || (!redo && o.undo.is_empty()) {
            return;
        }
        let current = match o.snapshot() {
            Ok(b) => b,
            Err(e) => return self.fail(format!("Undo failed: {e}")),
        };
        let (from, to) = if redo { (&mut o.redo, &mut o.undo) } else { (&mut o.undo, &mut o.redo) };
        let Some((bytes, password)) = from.pop() else { return };
        match Document::from_bytes(bytes, password.as_deref()) {
            Ok(doc) => {
                to.push(current);
                o.doc = doc;
                o.dirty = true;
                o.refresh();
                self.after_change();
                self.say(if redo { "Redo" } else { "Undo" });
            }
            Err(e) => self.fail(format!("Undo failed: {e}")),
        }
    }

    /// Selected pages, or the current page when nothing is selected.
    fn target_pages(&self) -> Vec<usize> {
        let Some(o) = &self.open else { return vec![] };
        let s = o.selection.pages();
        if s.is_empty() && o.doc.page_count() > 0 { vec![self.current_page] } else { s }
    }

    fn rotate(&mut self, r: Rotation) {
        let pages = self.target_pages();
        let what = format!("Rotated {}", describe(&pages));
        self.edit(&what, |d| d.rotate_pages(&pages, r).map(|()| pages.clone()));
    }

    fn delete(&mut self) {
        let pages = self.target_pages();
        let what = format!("Deleted {}", describe(&pages));
        self.edit(&what, |d| d.delete_pages(&pages).map(|()| vec![]));
    }

    fn move_pages(&mut self, up: bool) {
        let pages = self.target_pages();
        let Some(o) = &self.open else { return };
        let Some(order) = moved_order(o.doc.page_count(), &pages, up) else { return };
        let new_pos = positions_of(&order, &pages);
        if let Some(&first) = new_pos.first() {
            self.current_page = first;
        }
        self.edit(&format!("Moved {}", describe(&pages)), |d| d.reorder(&order).map(|()| new_pos));
    }

    fn insert_blank(&mut self) {
        let at = self.target_pages().last().map_or(0, |p| p + 1);
        self.edit(&format!("Inserted a blank page at {}", at + 1), |d| d.insert_blank_page(at).map(|()| vec![at]));
    }

    /// Insert other PDFs after the target pages (or at the end with `at_end`).
    fn insert_files(&mut self, at_end: bool) {
        let Some(files) = rfd::FileDialog::new().add_filter("PDF", &["pdf", "PDF"]).pick_files() else { return };
        let mut others = Vec::new();
        for f in &files {
            match Document::open(f, None) {
                Ok(d) => others.push(d),
                Err(e) => return self.fail(format!("Could not open {}: {e}", f.display())),
            }
        }
        let n = self.open.as_ref().map_or(0, |o| o.doc.page_count());
        let at = if at_end { n } else { self.target_pages().last().map_or(n, |p| p + 1) };
        let total: usize = others.iter().map(Document::page_count).sum();
        let what = format!("Inserted {total} pages from {} file(s)", others.len());
        self.edit(&what, |d| {
            let mut pos = at;
            for o in &others {
                d.insert_document(o, pos)?;
                pos += o.page_count();
            }
            Ok((at..pos).collect())
        });
    }

    fn extract(&mut self) {
        let pages = self.target_pages();
        let Some(o) = &self.open else { return };
        let stem = o.name().trim_end_matches(".pdf").to_string();
        let Some(path) =
            rfd::FileDialog::new().add_filter("PDF", &["pdf"]).set_file_name(format!("{stem}-pages.pdf")).save_file()
        else {
            return;
        };
        let path = with_pdf_extension(path);
        match o.doc.extract(&pages).and_then(|d| d.save(&path)) {
            Ok(()) => self.say(format!("Extracted {} to {}", describe(&pages), path.display())),
            Err(e) => self.fail(format!("Extract failed: {e}")),
        }
    }

    fn split(&mut self, spec: &str) {
        let Some(o) = &self.open else { return };
        let n = o.doc.page_count();
        let groups: Vec<Vec<usize>> = match spec.trim().parse::<usize>() {
            Ok(every) if every > 0 => (0..n).collect::<Vec<_>>().chunks(every).map(<[usize]>::to_vec).collect(),
            _ => match spec.split(';').filter(|g| !g.trim().is_empty()).map(|g| parse_page_ranges(g, n)).collect() {
                Ok(g) => g,
                Err(e) => return self.fail(format!("Split: {e}")),
            },
        };
        if groups.is_empty() {
            return self.fail("Split: nothing to split");
        }
        let Some(dir) = rfd::FileDialog::new().set_title("Choose a folder for the split files").pick_folder() else {
            return;
        };
        let stem = o.name().trim_end_matches(".pdf").to_string();
        let width = groups.len().to_string().len();
        let result = o.doc.split_groups(&groups).and_then(|parts| {
            for (i, p) in parts.iter().enumerate() {
                p.save(dir.join(format!("{stem}-part{:0width$}.pdf", i + 1)))?;
            }
            Ok(parts.len())
        });
        match result {
            Ok(k) => self.say(format!("Wrote {k} files to {}", dir.display())),
            Err(e) => self.fail(format!("Split failed: {e}")),
        }
    }

    // ------------------------------------------------------------ forms, signatures, security

    fn set_field(&mut self, name: &str, value: FieldValue) {
        let what = format!("Filled “{name}”");
        let pages = self.target_pages();
        let v = vec![(name.to_string(), value)];
        self.edit(&what, |d| d.fill_fields(&v).map(|()| pages.clone()));
        if let Some(o) = &mut self.open {
            o.selection.clear();
        }
    }

    fn flatten_form(&mut self) {
        self.edit("Flattened the form (fields are now part of the page)", |d| d.flatten(None).map(|()| vec![]));
    }

    /// Stamp a signature into `rect`; the signature keeps its proportions inside the box.
    fn place_signature(&mut self, page: usize, rect: NormRect, sig: &Signature) {
        let Some(o) = &self.open else { return };
        let Some(&(pw, ph)) = o.sizes.get(page) else { return };
        let rect = fit_aspect(rect, sig.aspect(), pw, ph);
        self.edit(&format!("Signed page {}", page + 1), |d| {
            match sig {
                Signature::Ink { ink, .. } => d.stamp_ink(page, rect, ink)?,
                Signature::Text { text, font, color } => d.stamp_text(page, rect, text, *font, *color)?,
                Signature::Image { image, .. } => d.stamp_image(page, rect, image)?,
            }
            Ok(vec![])
        });
    }

    fn start_signing(&mut self, target: Option<(usize, NormRect)>) {
        self.sign_target = target;
        self.sign_dialog.get_or_insert_with(SignDialog::default);
    }

    fn protect(&mut self, p: Protection) {
        let what = if p.user_password.is_empty() {
            "Restricted permissions (owner password set)"
        } else {
            "Protected with a password — it is encrypted when you save"
        };
        self.edit(what, |d| d.protect(&p).map(|()| vec![]));
    }

    fn unprotect(&mut self) {
        self.edit("Removed the password and restrictions", |d| d.unprotect().map(|()| vec![]));
    }

    fn compress(&mut self, opts: CompressOptions) {
        let mut report = None;
        self.edit("Compressed", |d| {
            report = Some(d.compress(&opts)?);
            Ok(vec![])
        });
        if let Some(r) = report {
            if r.after >= r.before {
                self.say("This file is already as small as this setting can make it");
            } else {
                self.say(format!(
                    "Compressed {} -> {} ({:.0}% smaller, {} of {} images) — save to keep it",
                    human(r.before),
                    human(r.after),
                    100.0 * (1.0 - r.after as f64 / r.before.max(1) as f64),
                    r.images_recompressed,
                    r.images_total
                ));
            }
        }
    }

    fn open_compress_dialog(&mut self) {
        let size = self.open.as_ref().and_then(|o| o.doc.to_bytes().ok()).map_or(0, |b| b.len());
        self.compress_dialog = Some((0, size));
    }

    fn apply_page_action(&mut self, a: PageAction) {
        match a {
            PageAction::Toggle(name, on) => self.set_field(&name, FieldValue::Checked(on)),
            PageAction::Radio(name, state) => self.set_field(&name, FieldValue::Choice(state)),
            PageAction::EditText(e) => {
                self.commit_text();
                self.editing = Some(e);
            }
            PageAction::Choose(c) => {
                self.commit_text();
                self.choice = Some(c);
            }
            PageAction::SignField(page, rect) => self.start_signing(Some((page, rect))),
            PageAction::Place(page, rect) => {
                if let Some(sig) = self.placing.take() {
                    self.place_signature(page, rect, &sig);
                }
            }
            PageAction::CommitText => self.commit_text(),
            PageAction::CancelText => self.editing = None,
        }
    }

    fn commit_text(&mut self) {
        let Some(e) = self.editing.take() else { return };
        let unchanged = self
            .open
            .as_ref()
            .and_then(|o| o.fields.iter().find(|f| f.name == e.name))
            .is_some_and(|f| f.value == e.text);
        if !unchanged {
            self.set_field(&e.name, FieldValue::Text(e.text));
        }
    }

    // ------------------------------------------------------------ search

    fn run_search(&mut self, backwards: bool) {
        let key = (self.search.query.clone(), self.search.match_case, self.search.whole_word);
        if self.search.searched.as_ref() == Some(&key) && !self.search.hits.is_empty() {
            let n = self.search.hits.len();
            let cur = self.search.current.unwrap_or(0);
            let next = if backwards { (cur + n - 1) % n } else { (cur + 1) % n };
            self.select_hit(next);
            return;
        }
        let Some(o) = &self.open else { return };
        let started = Instant::now();
        match o.doc.search(&key.0, key.1, key.2) {
            Ok(hits) => {
                self.search.by_page.clear();
                for (i, h) in hits.iter().enumerate() {
                    self.search.by_page.entry(h.page).or_default().push(i);
                }
                let n = hits.len();
                // Start at the first hit at or after the current page.
                let first = hits.iter().position(|h| h.page >= self.current_page).unwrap_or(0);
                self.search.hits = hits;
                self.search.searched = Some(key);
                self.search.current = None;
                self.side = Side::Search;
                if n > 0 {
                    self.select_hit(first);
                }
                self.say(format!(
                    "{n} match{} for \"{}\" ({} ms)",
                    if n == 1 { "" } else { "es" },
                    self.search.query,
                    started.elapsed().as_millis()
                ));
            }
            Err(e) => self.fail(format!("Search failed: {e}")),
        }
    }

    fn select_hit(&mut self, i: usize) {
        if let Some(h) = self.search.hits.get(i) {
            self.search.current = Some(i);
            self.goto = Some(h.page);
        }
    }

    // ------------------------------------------------------------ rendering

    /// Ensure page textures exist for the visible pages at the right resolution.
    fn render_visible(&mut self, ctx: &egui::Context, visible: std::ops::Range<usize>) -> bool {
        let Some(o) = &mut self.open else { return false };
        let ppp = ctx.pixels_per_point();
        let max_side = ctx.input(|i| i.max_texture_side) as f32;
        let mut budget = RENDER_BUDGET;
        let mut more = false;
        for i in visible.clone() {
            let (w, h) = o.sizes[i];
            // Pixels per PDF point, capped so the texture fits the GPU limit.
            let scale = (self.zoom * view::PT_TO_UI * ppp).min(max_side / w.max(h).max(1.0));
            let fresh = o.pages.get(&i).is_some_and(|t| (t.scale - scale).abs() < 0.01);
            if fresh {
                continue;
            }
            if budget == 0 {
                more = true;
                continue;
            }
            budget -= 1;
            match o.doc.render(i, scale) {
                Ok(b) => {
                    let img = ColorImage::from_rgba_unmultiplied([b.width as usize, b.height as usize], &b.rgba);
                    let tex = ctx.load_texture(format!("page-{i}"), img, TextureOptions::LINEAR);
                    o.pages.insert(i, Tex { tex, scale });
                }
                Err(_) => {
                    // Keep a 1×1 placeholder so a broken page isn't retried every frame.
                    let tex = ctx.load_texture(
                        format!("page-{i}"),
                        ColorImage::new([1, 1], vec![Color32::WHITE]),
                        TextureOptions::LINEAR,
                    );
                    o.pages.insert(i, Tex { tex, scale });
                }
            }
        }
        // Evict textures far from the view to bound memory on long documents.
        if o.pages.len() > 24 {
            let (a, b) = (visible.start.saturating_sub(4), visible.end + 4);
            o.pages.retain(|&k, _| k >= a && k < b);
        }
        more
    }

    fn thumbnail(&mut self, ctx: &egui::Context, i: usize, budget: &mut usize) -> Option<TextureHandle> {
        let o = self.open.as_mut()?;
        if let Some(t) = o.thumbs.get(&i) {
            return Some(t.clone());
        }
        if *budget == 0 {
            return None;
        }
        *budget -= 1;
        let px = (THUMB_W * ctx.pixels_per_point()) as u32;
        let b = o.doc.render_thumbnail(i, px).ok()?;
        let img = ColorImage::from_rgba_unmultiplied([b.width as usize, b.height as usize], &b.rgba);
        let tex = ctx.load_texture(format!("thumb-{i}"), img, TextureOptions::LINEAR);
        if o.thumbs.len() > 300 {
            o.thumbs.retain(|&k, _| k.abs_diff(i) < 100);
        }
        o.thumbs.insert(i, tex.clone());
        Some(tex)
    }

    // ------------------------------------------------------------ input

    fn shortcuts(&mut self, ctx: &egui::Context) {
        let typing = ctx.egui_wants_keyboard_input();
        let cmd = |key, shift: bool| {
            let m = if shift { Modifiers::COMMAND | Modifiers::SHIFT } else { Modifiers::COMMAND };
            ctx.input_mut(|i| i.consume_shortcut(&KeyboardShortcut::new(m, key)))
        };
        if cmd(Key::O, false) {
            self.request(Pending::Open(None));
        }
        if cmd(Key::S, true) {
            self.save(ctx, true);
        } else if cmd(Key::S, false) {
            self.save(ctx, false);
        }
        if cmd(Key::W, false) {
            self.request(Pending::Close);
        }
        if cmd(Key::F, false) {
            self.side = Side::Search;
            self.search.focus = true;
        }
        if self.open.is_none() {
            return;
        }
        if cmd(Key::Z, true) || cmd(Key::Y, false) {
            self.undo_redo(true);
        } else if cmd(Key::Z, false) {
            self.undo_redo(false);
        }
        if cmd(Key::Equals, false) || cmd(Key::Plus, false) {
            self.set_zoom(view::zoom_in(self.zoom));
        }
        if cmd(Key::Minus, false) {
            self.set_zoom(view::zoom_out(self.zoom));
        }
        if cmd(Key::Num0, false) {
            self.fit = Some(Fit::Width);
        }
        if cmd(Key::Num1, false) {
            self.set_zoom(1.0);
        }
        if cmd(Key::R, true) {
            self.rotate(Rotation::R270);
        } else if cmd(Key::R, false) {
            self.rotate(Rotation::R90);
        }
        if typing {
            return;
        }
        if cmd(Key::A, false)
            && let Some(o) = &mut self.open
        {
            o.selection.select_all();
        }
        let n = self.open.as_ref().map_or(0, |o| o.doc.page_count());
        let key = |k| ctx.input_mut(|i| i.consume_key(Modifiers::NONE, k));
        if key(Key::PageDown) || key(Key::ArrowRight) {
            self.goto = Some((self.current_page + 1).min(n.saturating_sub(1)));
        }
        if key(Key::PageUp) || key(Key::ArrowLeft) {
            self.goto = Some(self.current_page.saturating_sub(1));
        }
        if key(Key::Home) {
            self.goto = Some(0);
        }
        if key(Key::End) {
            self.goto = Some(n.saturating_sub(1));
        }
        if key(Key::Delete) {
            self.delete();
        }
        if key(Key::Escape) {
            if self.placing.take().is_some() {
                self.say("Signature placement cancelled");
            } else if self.choice.take().is_none()
                && let Some(o) = &mut self.open
            {
                o.selection.clear();
            }
        }
        // Ctrl + mouse wheel zooms around the current page.
        let zd = ctx.input(|i| i.zoom_delta());
        if (zd - 1.0).abs() > 0.001 {
            self.set_zoom(self.zoom * zd);
        }
    }

    fn set_zoom(&mut self, z: f32) {
        let z = z.clamp(view::MIN_ZOOM, view::MAX_ZOOM);
        if (z - self.zoom).abs() > 1e-4 {
            self.zoom = z;
            self.fit = None;
            self.goto = Some(self.current_page);
        }
    }

    // ------------------------------------------------------------ UI: chrome

    fn menu_bar(&mut self, ui: &mut egui::Ui) {
        let has_doc = self.open.is_some();
        egui::MenuBar::new().ui(ui, |ui| {
            ui.menu_button("File", |ui| {
                if ui.button("Open…                Ctrl+O").clicked() {
                    self.request(Pending::Open(None));
                }
                ui.add_enabled_ui(has_doc, |ui| {
                    if ui.button("Save                 Ctrl+S").clicked() {
                        self.save(ui.ctx(), false);
                    }
                    if ui.button("Save As…             Ctrl+Shift+S").clicked() {
                        self.save(ui.ctx(), true);
                    }
                    ui.separator();
                    if ui.button("Merge PDFs (append)…").clicked() {
                        self.insert_files(true);
                    }
                    if ui.button("Extract Pages…").clicked() {
                        self.extract();
                    }
                    if ui.button("Split…").clicked() {
                        self.split_dialog = Some("1".into());
                    }
                    ui.separator();
                    if ui.button("Document Properties").clicked() {
                        self.info_open = true;
                    }
                    if ui.button("Close                Ctrl+W").clicked() {
                        self.request(Pending::Close);
                    }
                });
                ui.separator();
                if ui.button("Quit").clicked() {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
            ui.add_enabled_ui(has_doc, |ui| {
                ui.menu_button("Edit", |ui| {
                    let (can_undo, can_redo) =
                        self.open.as_ref().map_or((false, false), |o| (!o.undo.is_empty(), !o.redo.is_empty()));
                    if ui.add_enabled(can_undo, egui::Button::new("Undo                 Ctrl+Z")).clicked() {
                        self.undo_redo(false);
                    }
                    if ui.add_enabled(can_redo, egui::Button::new("Redo                 Ctrl+Shift+Z")).clicked() {
                        self.undo_redo(true);
                    }
                    ui.separator();
                    if ui.button("Find                 Ctrl+F").clicked() {
                        self.side = Side::Search;
                        self.search.focus = true;
                    }
                    if ui.button("Copy Page Text").clicked() {
                        let text = self.open.as_ref().and_then(|o| o.doc.text(self.current_page).ok());
                        if let Some(t) = text {
                            ui.ctx().copy_text(t);
                            self.say(format!("Copied the text of page {}", self.current_page + 1));
                        }
                    }
                    if ui.button("Select All Pages     Ctrl+A").clicked()
                        && let Some(o) = &mut self.open
                    {
                        o.selection.select_all();
                    }
                });
                ui.menu_button("View", |ui| {
                    if ui.button("Zoom In              Ctrl++").clicked() {
                        self.set_zoom(view::zoom_in(self.zoom));
                    }
                    if ui.button("Zoom Out             Ctrl+-").clicked() {
                        self.set_zoom(view::zoom_out(self.zoom));
                    }
                    if ui.button("Actual Size          Ctrl+1").clicked() {
                        self.set_zoom(1.0);
                    }
                    if ui.button("Fit Width            Ctrl+0").clicked() {
                        self.fit = Some(Fit::Width);
                    }
                    if ui.button("Fit Page").clicked() {
                        self.fit = Some(Fit::Page);
                    }
                });
                ui.menu_button("Pages", |ui| self.page_tools_menu(ui));
                ui.menu_button("Tools", |ui| self.tools_menu(ui));
            });
        });
    }

    fn page_tools_menu(&mut self, ui: &mut egui::Ui) {
        if ui.button("Rotate Right         Ctrl+R").clicked() {
            self.rotate(Rotation::R90);
        }
        if ui.button("Rotate Left          Ctrl+Shift+R").clicked() {
            self.rotate(Rotation::R270);
        }
        if ui.button("Rotate 180°").clicked() {
            self.rotate(Rotation::R180);
        }
        ui.separator();
        if ui.button("Move Up").clicked() {
            self.move_pages(true);
        }
        if ui.button("Move Down").clicked() {
            self.move_pages(false);
        }
        ui.separator();
        if ui.button("Insert Blank Page After").clicked() {
            self.insert_blank();
        }
        if ui.button("Insert PDF After…").clicked() {
            self.insert_files(false);
        }
        if ui.button("Extract to New PDF…").clicked() {
            self.extract();
        }
        ui.separator();
        if ui.button("Delete               Del").clicked() {
            self.delete();
        }
    }

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if ui.button("📂 Open").on_hover_text("Open a PDF (Ctrl+O)").clicked() {
                self.request(Pending::Open(None));
            }
            let Some(o) = &self.open else {
                return;
            };
            let (dirty, n) = (o.dirty, o.doc.page_count());
            if ui.add_enabled(dirty, egui::Button::new("💾 Save")).on_hover_text("Save (Ctrl+S)").clicked() {
                self.save(ui.ctx(), false);
            }
            ui.separator();
            if ui.button("⟲").on_hover_text("Rotate left (Ctrl+Shift+R)").clicked() {
                self.rotate(Rotation::R270);
            }
            if ui.button("⟳").on_hover_text("Rotate right (Ctrl+R)").clicked() {
                self.rotate(Rotation::R90);
            }
            if ui.button("⬆").on_hover_text("Move selected pages up").clicked() {
                self.move_pages(true);
            }
            if ui.button("⬇").on_hover_text("Move selected pages down").clicked() {
                self.move_pages(false);
            }
            if ui.button("➕").on_hover_text("Insert a blank page after the selection").clicked() {
                self.insert_blank();
            }
            if ui.button("🗑").on_hover_text("Delete selected pages (Del)").clicked() {
                self.delete();
            }
            ui.separator();
            if ui.button("Sign").on_hover_text("Draw, type or insert a signature and place it on a page").clicked() {
                self.start_signing(None);
            }
            if ui.button("Protect").on_hover_text("Protect with a password (Tools menu)").clicked() {
                self.protect_dialog = Some(ProtectDialog {
                    user: String::new(),
                    confirm: String::new(),
                    owner: String::new(),
                    allow: Allow::ALL,
                    error: None,
                });
            }
            if ui.button("Compress").on_hover_text("Compress to make the file smaller").clicked() {
                self.open_compress_dialog();
            }
            ui.separator();
            if ui.button("◀").on_hover_text("Previous page").clicked() {
                self.goto = Some(self.current_page.saturating_sub(1));
            }
            let field =
                egui::TextEdit::singleline(&mut self.page_field).desired_width(42.0).horizontal_align(Align::Center);
            let r = ui.add(field);
            if !r.has_focus() {
                self.page_field = (self.current_page + 1).to_string();
            }
            if r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                match self.page_field.trim().parse::<usize>() {
                    Ok(p) if (1..=n).contains(&p) => self.goto = Some(p - 1),
                    _ => self.fail(format!("Enter a page number between 1 and {n}")),
                }
            }
            ui.label(format!("/ {n}"));
            if ui.button("▶").on_hover_text("Next page").clicked() {
                self.goto = Some((self.current_page + 1).min(n.saturating_sub(1)));
            }
            ui.separator();
            if ui.button("➖").on_hover_text("Zoom out (Ctrl+-)").clicked() {
                self.set_zoom(view::zoom_out(self.zoom));
            }
            egui::ComboBox::from_id_salt("zoom")
                .width(78.0)
                .selected_text(format!("{:.0}%", self.zoom * 100.0))
                .show_ui(ui, |ui| {
                    if ui.selectable_label(self.fit == Some(Fit::Width), "Fit width").clicked() {
                        self.fit = Some(Fit::Width);
                    }
                    if ui.selectable_label(self.fit == Some(Fit::Page), "Fit page").clicked() {
                        self.fit = Some(Fit::Page);
                    }
                    for &z in view::ZOOM_STEPS {
                        if ui.selectable_label((self.zoom - z).abs() < 0.005, format!("{:.0}%", z * 100.0)).clicked() {
                            self.set_zoom(z);
                        }
                    }
                });
            if ui.button("➕").on_hover_text("Zoom in (Ctrl++)").clicked() {
                self.set_zoom(view::zoom_in(self.zoom));
            }
        });
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if let Some(o) = &self.open {
                let sel = o.selection.pages();
                ui.label(format!("Page {} of {}", self.current_page + 1, o.doc.page_count()));
                if !sel.is_empty() {
                    ui.separator();
                    ui.label(format!("{} selected", sel.len()));
                }
                if let Some(&(w, h)) = o.sizes.get(self.current_page) {
                    ui.separator();
                    ui.label(format!("{:.2} × {:.2} in", w / 72.0, h / 72.0));
                }
            }
            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                if self.placing.is_some() {
                    ui.label(RichText::new("Click on a page to place your signature · Esc to cancel").strong());
                } else if let Some(s) = &self.status {
                    let c = if s.error { ui.visuals().error_fg_color } else { ui.visuals().weak_text_color() };
                    ui.label(RichText::new(&s.text).color(c));
                }
            });
        });
    }

    // ------------------------------------------------------------ UI: side panel

    fn side_panel(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.side, Side::Pages, "Pages");
            ui.selectable_value(&mut self.side, Side::Bookmarks, "Bookmarks");
            ui.selectable_value(&mut self.side, Side::Search, "Search");
        });
        ui.separator();
        match self.side {
            Side::Pages => self.thumbnails(ui),
            Side::Bookmarks => self.bookmarks(ui),
            Side::Search => self.search_panel(ui),
        }
    }

    fn thumbnails(&mut self, ui: &mut egui::Ui) {
        let n = self.open.as_ref().map_or(0, |o| o.doc.page_count());
        ui.label(RichText::new("Click, Ctrl+click or Shift+click to select pages").small().weak());
        let ctx = ui.ctx().clone();
        let mut budget = THUMB_BUDGET;
        let mut missing = false;
        let mut clicked = None;
        let follow = self.goto;
        let mut area = ScrollArea::vertical().id_salt("thumbs").auto_shrink([false, false]);
        if let Some(p) = follow {
            // Keep the thumbnail strip roughly in step with the main view.
            let row = THUMB_ROW + ui.spacing().item_spacing.y;
            area = area.vertical_scroll_offset((p as f32 * row - 2.0 * row).max(0.0));
        }
        area.show_rows(ui, THUMB_ROW, n, |ui, rows| {
            for i in rows {
                let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), THUMB_ROW), Sense::click());
                let selected = self.open.as_ref().is_some_and(|o| o.selection.is_selected(i));
                let current = i == self.current_page;
                let p = ui.painter();
                if selected {
                    p.rect_filled(rect, 4.0, ui.visuals().selection.bg_fill.gamma_multiply(0.5));
                } else if resp.hovered() {
                    p.rect_filled(rect, 4.0, ui.visuals().widgets.hovered.weak_bg_fill);
                }
                let (pw, ph) = self.open.as_ref().map_or((612.0, 792.0), |o| o.sizes[i]);
                let box_h = THUMB_ROW - 26.0;
                let s = (THUMB_W / pw).min(box_h / ph);
                let size = Vec2::new(pw * s, ph * s);
                let img_rect = Rect::from_center_size(Pos2::new(rect.center().x, rect.top() + 4.0 + box_h / 2.0), size);
                match self.thumbnail(&ctx, i, &mut budget) {
                    Some(t) => {
                        ui.painter().image(t.id(), img_rect, uv_full(), Color32::WHITE);
                    }
                    None => {
                        missing = true;
                        ui.painter().rect_filled(img_rect, 0.0, Color32::from_gray(235));
                    }
                }
                let border = if current {
                    Stroke::new(2.0, ui.visuals().selection.stroke.color)
                } else {
                    Stroke::new(1.0, ui.visuals().widgets.noninteractive.bg_stroke.color)
                };
                ui.painter().rect_stroke(img_rect, 0.0, border, StrokeKind::Outside);
                ui.painter().text(
                    Pos2::new(rect.center().x, rect.bottom() - 10.0),
                    egui::Align2::CENTER_CENTER,
                    (i + 1).to_string(),
                    egui::FontId::proportional(12.0),
                    ui.visuals().text_color(),
                );
                if resp.clicked() {
                    clicked = Some((i, ui.input(|inp| inp.modifiers)));
                }
                resp.context_menu(|ui| {
                    if !self.open.as_ref().is_some_and(|o| o.selection.is_selected(i))
                        && let Some(o) = &mut self.open
                    {
                        o.selection.set(&[i]);
                    }
                    self.page_tools_menu(ui);
                });
            }
        });
        if let Some((i, m)) = clicked {
            if let Some(o) = &mut self.open {
                o.selection.click(i, m.command, m.shift);
            }
            self.goto = Some(i);
        }
        if missing {
            ctx.request_repaint();
        }
    }

    fn bookmarks(&mut self, ui: &mut egui::Ui) {
        let Some(o) = &self.open else { return };
        if o.outline.is_empty() {
            ui.label(RichText::new("This document has no bookmarks.").weak());
            return;
        }
        let mut go = None;
        ScrollArea::vertical().id_salt("outline").auto_shrink([false, false]).show_rows(
            ui,
            ui.text_style_height(&egui::TextStyle::Button) + 2.0,
            o.outline.len(),
            |ui, rows| {
                for item in &o.outline[rows] {
                    ui.horizontal(|ui| {
                        ui.add_space(item.depth as f32 * 14.0);
                        let label = match item.page {
                            Some(p) => format!("{}  ·  {}", item.title, p + 1),
                            None => item.title.clone(),
                        };
                        let r = ui.add(egui::Button::new(label).frame(false).truncate());
                        if r.clicked() {
                            go = item.page;
                        }
                    });
                }
            },
        );
        if go.is_some() {
            self.goto = go;
        }
    }

    fn search_panel(&mut self, ui: &mut egui::Ui) {
        let r = ui.add(
            egui::TextEdit::singleline(&mut self.search.query)
                .hint_text("Find in document…")
                .desired_width(f32::INFINITY),
        );
        if self.search.focus {
            r.request_focus();
            self.search.focus = false;
        }
        let enter = r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.search.match_case, "Match case");
            ui.checkbox(&mut self.search.whole_word, "Whole words");
        });
        let mut go = None;
        ui.horizontal(|ui| {
            if ui.button("Find").clicked() || enter {
                go = Some(ui.input(|i| i.modifiers.shift));
            }
            let n = self.search.hits.len();
            if n > 0 {
                if ui.button("◀").clicked() {
                    go = Some(true);
                }
                if ui.button("▶").clicked() {
                    go = Some(false);
                }
                ui.label(format!("{} / {n}", self.search.current.map_or(0, |c| c + 1)));
            }
        });
        if let Some(back) = go {
            self.run_search(back);
            // Keep focus in the box so Enter steps through results.
            r.request_focus();
        }
        ui.separator();
        let mut pick = None;
        let current = self.search.current;
        let row_h = ui.text_style_height(&egui::TextStyle::Body) * 2.0 + 8.0;
        ScrollArea::vertical().id_salt("hits").auto_shrink([false, false]).show_rows(
            ui,
            row_h,
            self.search.hits.len(),
            |ui, rows| {
                for i in rows {
                    let h = &self.search.hits[i];
                    let text = format!("p. {}  {}", h.page + 1, h.snippet);
                    let r = ui.add_sized(
                        [ui.available_width(), row_h],
                        egui::Button::selectable(current == Some(i), RichText::new(text).small()).wrap(),
                    );
                    if r.clicked() {
                        pick = Some(i);
                    }
                }
            },
        );
        if let Some(i) = pick {
            self.select_hit(i);
        }
    }

    // ------------------------------------------------------------ UI: document view

    fn document_view(&mut self, ui: &mut egui::Ui) {
        let Some(o) = &self.open else { return };
        let avail = ui.available_size();
        self.viewport_size = avail;
        match self.fit {
            Some(Fit::Width) => self.zoom = Layout::fit_width_zoom(&o.sizes, avail.x - 14.0).min(2.0),
            Some(Fit::Page) => self.zoom = Layout::fit_page_zoom(&o.sizes, self.current_page, avail.x - 14.0, avail.y),
            None => {}
        }
        let key = (self.zoom, o.sizes.len(), self.generation);
        if key != self.layout_key {
            self.layout = Layout::new(&o.sizes, self.zoom);
            self.layout_key = key;
        }
        let content_w = (self.layout.max_width + 2.0 * GAP).max(avail.x);
        let mut area = ScrollArea::both().id_salt("doc").auto_shrink([false, false]);
        if let Some(p) = self.goto.take()
            && let Some(&top) = self.layout.tops.get(p)
        {
            // For search hits, scroll so the match is in view rather than the page top.
            let hit_y = self
                .search
                .current
                .and_then(|c| self.search.hits.get(c))
                .filter(|h| h.page == p)
                .and_then(|h| h.rects.first())
                .map(|r| r.y0 * self.layout.sizes[p].1 - avail.y / 3.0)
                .unwrap_or(0.0)
                .max(0.0);
            area = area.vertical_scroll_offset((top - GAP + hit_y).max(0.0));
            self.current_page = p;
        }
        let mut actions = Vec::new();
        let out = area.show_viewport(ui, |ui, viewport| {
            let (rect, _) = ui.allocate_exact_size(Vec2::new(content_w, self.layout.total_height), Sense::hover());
            let visible = self.layout.visible(viewport.min.y, viewport.max.y);
            let more = self.render_visible(ui.ctx(), visible.clone());
            let Some(o) = &self.open else { return visible };
            let painter = ui.painter().clone();
            for i in visible.clone() {
                let (w, h) = self.layout.sizes[i];
                let page = Rect::from_min_size(
                    Pos2::new(rect.left() + (content_w - w) / 2.0, rect.top() + self.layout.tops[i]),
                    Vec2::new(w, h),
                );
                painter.rect_filled(page.translate(Vec2::new(2.0, 3.0)), 2.0, Color32::from_black_alpha(60));
                painter.rect_filled(page, 0.0, Color32::WHITE);
                if let Some(t) = o.pages.get(&i) {
                    painter.image(t.tex.id(), page, uv_full(), Color32::WHITE);
                }
                if o.selection.is_selected(i) {
                    painter.rect_stroke(
                        page,
                        0.0,
                        Stroke::new(3.0, ui.visuals().selection.bg_fill),
                        StrokeKind::Outside,
                    );
                }
                if let Some(idx) = self.search.by_page.get(&i) {
                    for &k in idx {
                        let cur = self.search.current == Some(k);
                        let fill = if cur {
                            Color32::from_rgba_unmultiplied(255, 140, 0, 110)
                        } else {
                            Color32::from_rgba_unmultiplied(255, 230, 0, 90)
                        };
                        for r in &self.search.hits[k].rects {
                            let hr = Rect::from_min_max(
                                Pos2::new(page.left() + r.x0 * w, page.top() + r.y0 * h),
                                Pos2::new(page.left() + r.x1 * w, page.top() + r.y1 * h),
                            )
                            .expand(1.5);
                            painter.rect_filled(hr, 2.0, fill);
                        }
                    }
                }
                let to_screen = |r: &NormRect| {
                    Rect::from_min_max(
                        Pos2::new(page.left() + r.x0 * w, page.top() + r.y0 * h),
                        Pos2::new(page.left() + r.x1 * w, page.top() + r.y1 * h),
                    )
                };
                if let Some(sig) = &self.placing {
                    let resp = ui
                        .interact(page, Id::new(("place", i)), Sense::click())
                        .on_hover_cursor(egui::CursorIcon::Crosshair);
                    if let Some(pos) = resp.hover_pos() {
                        let (sw, sh) = sig.default_size_pt();
                        let k = self.zoom * view::PT_TO_UI;
                        let mut ghost = Rect::from_center_size(pos, Vec2::new(sw * k, sh * k));
                        // Keep the whole signature on the page.
                        ghost = ghost.translate(Vec2::new(
                            (page.left() - ghost.left()).max(0.0) + (page.right() - ghost.right()).min(0.0),
                            (page.top() - ghost.top()).max(0.0) + (page.bottom() - ghost.bottom()).min(0.0),
                        ));
                        painter.rect_filled(ghost, 2.0, Color32::from_rgba_unmultiplied(80, 140, 255, 30));
                        painter.rect_stroke(
                            ghost,
                            2.0,
                            Stroke::new(1.0, Color32::from_rgb(60, 110, 220)),
                            StrokeKind::Outside,
                        );
                        sig.paint(&painter, ghost, 190);
                        if resp.clicked() {
                            let n = NormRect {
                                x0: (ghost.left() - page.left()) / w,
                                y0: (ghost.top() - page.top()) / h,
                                x1: (ghost.right() - page.left()) / w,
                                y1: (ghost.bottom() - page.top()) / h,
                            };
                            actions.push(PageAction::Place(i, n));
                        }
                    }
                    continue;
                }
                for f in &o.fields {
                    for (wi, wd) in f.widgets.iter().enumerate().filter(|(_, wd)| wd.page == i) {
                        let fr = to_screen(&wd.rect);
                        if f.read_only || matches!(f.kind, FieldKind::PushButton) || fr.width() < 2.0 {
                            continue;
                        }
                        let editing_this = self.editing.as_ref().is_some_and(|e| e.name == f.name && e.page == i);
                        if editing_this {
                            continue;
                        }
                        let resp = ui.interact(fr, Id::new(("field", &f.name, wi)), Sense::click());
                        let cursor = match f.kind {
                            FieldKind::Text { .. } => egui::CursorIcon::Text,
                            _ => egui::CursorIcon::PointingHand,
                        };
                        let resp = resp.on_hover_cursor(cursor).on_hover_text(field_tooltip(f));
                        if self.show_fields {
                            painter.rect_filled(fr, 1.0, Color32::from_rgba_unmultiplied(80, 140, 255, 26));
                        }
                        if resp.hovered() {
                            painter.rect_stroke(
                                fr,
                                1.0,
                                Stroke::new(1.5, Color32::from_rgb(60, 110, 220)),
                                StrokeKind::Outside,
                            );
                        }
                        if !resp.clicked() {
                            continue;
                        }
                        actions.push(match &f.kind {
                            FieldKind::Checkbox => PageAction::Toggle(f.name.clone(), !f.checked),
                            FieldKind::Radio => match &wd.on_state {
                                Some(s) if *s != f.value => PageAction::Radio(f.name.clone(), s.clone()),
                                _ => continue,
                            },
                            FieldKind::Text { multiline, .. } => PageAction::EditText(TextEditing {
                                name: f.name.clone(),
                                page: i,
                                rect: wd.rect,
                                text: f.value.clone(),
                                multiline: *multiline,
                                focus: true,
                            }),
                            FieldKind::ComboBox { .. } | FieldKind::ListBox { .. } => PageAction::Choose(ChoicePopup {
                                name: f.name.clone(),
                                options: f.options.clone(),
                                current: f.value.clone(),
                                pos: fr.left_bottom(),
                                width: fr.width(),
                                opened: true,
                            }),
                            FieldKind::Signature => PageAction::SignField(i, wd.rect),
                            FieldKind::PushButton => continue,
                        });
                    }
                }
                if let Some(ed) = self.editing.as_mut().filter(|e| e.page == i) {
                    let fr = to_screen(&ed.rect);
                    painter.rect_filled(fr, 0.0, Color32::WHITE);
                    painter.rect_stroke(
                        fr,
                        0.0,
                        Stroke::new(2.0, Color32::from_rgb(60, 110, 220)),
                        StrokeKind::Outside,
                    );
                    let size = if ed.multiline { 12.0 * self.zoom * view::PT_TO_UI * 0.85 } else { fr.height() * 0.62 };
                    let font = egui::FontId::proportional(size.clamp(7.0, 40.0));
                    let edit = if ed.multiline {
                        egui::TextEdit::multiline(&mut ed.text)
                    } else {
                        egui::TextEdit::singleline(&mut ed.text)
                    };
                    let r = ui.put(
                        fr,
                        edit.frame(egui::Frame::NONE)
                            .font(font)
                            .text_color(Color32::BLACK)
                            .margin(egui::Margin::symmetric(3, 1)),
                    );
                    if ed.focus {
                        r.request_focus();
                        ed.focus = false;
                    } else if r.lost_focus() {
                        let esc = ui.input(|inp| inp.key_pressed(Key::Escape));
                        actions.push(if esc { PageAction::CancelText } else { PageAction::CommitText });
                    }
                }
            }
            if more {
                ui.ctx().request_repaint();
            }
            visible
        });
        let off = out.state.offset.y;
        if !self.layout.tops.is_empty() {
            self.current_page = self.layout.current(off, off + out.inner_rect.height());
        }
        for a in actions {
            self.apply_page_action(a);
        }
    }

    fn form_bar(&mut self, ui: &mut egui::Ui) {
        let n = self.open.as_ref().map_or(0, |o| o.fields.len());
        ui.horizontal(|ui| {
            ui.label(format!("📝 This document has a form ({n} fields). Click a field to fill it in."));
            ui.checkbox(&mut self.show_fields, "Highlight fields");
            if ui
                .button("Flatten form")
                .on_hover_text("Make the filled-in values part of the page so they can't be changed")
                .clicked()
            {
                self.flatten_form();
            }
        });
    }

    fn tools_menu(&mut self, ui: &mut egui::Ui) {
        if ui.button("Sign…").clicked() {
            self.start_signing(None);
        }
        let (has_form, encrypted) = self.open.as_ref().map_or((false, false), |o| (!o.fields.is_empty(), o.encrypted));
        ui.add_enabled_ui(has_form, |ui| {
            ui.checkbox(&mut self.show_fields, "Highlight Form Fields");
            if ui.button("Flatten Form").clicked() {
                self.flatten_form();
            }
        });
        ui.separator();
        if ui.button("Protect with Password…").clicked() {
            self.protect_dialog = Some(ProtectDialog {
                user: String::new(),
                confirm: String::new(),
                owner: String::new(),
                allow: Allow::ALL,
                error: None,
            });
        }
        if ui.add_enabled(encrypted, egui::Button::new("Remove Password")).clicked() {
            self.unprotect();
        }
        ui.separator();
        if ui.button("Compress…").clicked() {
            self.open_compress_dialog();
        }
    }

    fn welcome(&mut self, ui: &mut egui::Ui) {
        ui.vertical_centered(|ui| {
            ui.add_space(ui.available_height() * 0.3);
            ui.heading("PDF Forge");
            ui.add_space(6.0);
            if let Some(err) = &self.load_error {
                ui.label(RichText::new(err).color(ui.visuals().error_fg_color));
                return;
            }
            ui.label("Open a PDF to view, search, and rearrange its pages.");
            ui.add_space(10.0);
            if ui.button("📂  Open PDF…").clicked() {
                self.request(Pending::Open(None));
            }
            ui.add_space(6.0);
            ui.label(RichText::new("…or drop a PDF file onto this window").weak());
        });
    }

    // ------------------------------------------------------------ UI: windows

    fn windows(&mut self, ctx: &egui::Context) {
        self.tool_windows(ctx);
        if let Some(action) = self.pending.clone() {
            let mut choice = None;
            egui::Modal::new(Id::new("unsaved")).show(ctx, |ui| {
                ui.heading("Unsaved changes");
                ui.label(format!("Save changes to {} first?", self.open.as_ref().map(Open::name).unwrap_or_default()));
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Save").clicked() {
                        choice = Some(0);
                    }
                    if ui.button("Don't Save").clicked() {
                        choice = Some(1);
                    }
                    if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                        choice = Some(2);
                    }
                });
            });
            match choice {
                Some(0) => {
                    self.save(ctx, false);
                    if !self.dirty() {
                        self.pending = None;
                        self.finish(ctx, action);
                    }
                }
                Some(1) => {
                    self.pending = None;
                    if let Some(o) = &mut self.open {
                        o.dirty = false;
                    }
                    self.finish(ctx, action);
                }
                Some(_) => self.pending = None,
                None => {}
            }
        }

        if let Some((path, pw, wrong)) = &mut self.password_prompt {
            let mut submit = None;
            egui::Modal::new(Id::new("password")).show(ctx, |ui| {
                ui.heading("Password required");
                ui.label(format!("{} is protected.", path.file_name().unwrap_or_default().to_string_lossy()));
                if *wrong {
                    ui.colored_label(ui.visuals().error_fg_color, "That password is incorrect.");
                }
                let r = ui.add(egui::TextEdit::singleline(pw).password(true).hint_text("Password"));
                if !r.has_focus() && !r.lost_focus() {
                    r.request_focus();
                }
                let enter = ui.input(|i| i.key_pressed(Key::Enter));
                ui.horizontal(|ui| {
                    if ui.button("Open").clicked() || enter {
                        submit = Some(true);
                    }
                    if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                        submit = Some(false);
                    }
                });
            });
            match submit {
                Some(true) => {
                    let (path, pw, _) = self.password_prompt.take().expect("prompt is open");
                    self.load(path, Some(&pw));
                }
                Some(false) => self.password_prompt = None,
                None => {}
            }
        }

        if let Some(spec) = &mut self.split_dialog {
            let mut run = None;
            egui::Modal::new(Id::new("split")).show(ctx, |ui| {
                ui.heading("Split document");
                ui.label("Pages per file (e.g. 1), or page ranges separated by ';' (e.g. 1-3; 4-10; 11-)");
                let r = ui.add(egui::TextEdit::singleline(spec).desired_width(320.0));
                ui.horizontal(|ui| {
                    if ui.button("Choose folder & split").clicked()
                        || (r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)))
                    {
                        run = Some(true);
                    }
                    if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                        run = Some(false);
                    }
                });
            });
            match run {
                Some(true) => {
                    let spec = self.split_dialog.take().unwrap_or_default();
                    self.split(&spec);
                }
                Some(false) => self.split_dialog = None,
                None => {}
            }
        }

        if self.info_open {
            let mut open = true;
            if let Some(o) = &self.open {
                let i = o.doc.info();
                egui::Window::new("Document Properties").open(&mut open).resizable(false).collapsible(false).show(
                    ctx,
                    |ui| {
                        egui::Grid::new("props").num_columns(2).spacing([16.0, 4.0]).show(ui, |ui| {
                            let file = o.path.as_ref().map(|p| p.display().to_string()).unwrap_or_default();
                            for (k, v) in [
                                ("File", file.as_str()),
                                ("Title", &i.title),
                                ("Author", &i.author),
                                ("Subject", &i.subject),
                                ("Keywords", &i.keywords),
                                ("Creator", &i.creator),
                                ("Producer", &i.producer),
                                ("Created", &i.created),
                                ("Modified", &i.modified),
                                ("PDF version", &i.version),
                            ] {
                                ui.label(RichText::new(k).strong());
                                ui.label(if v.is_empty() { "—" } else { v });
                                ui.end_row();
                            }
                            ui.label(RichText::new("Pages").strong());
                            ui.label(i.pages.to_string());
                            ui.end_row();
                            ui.label(RichText::new("Encrypted").strong());
                            ui.label(if i.encrypted { "yes" } else { "no" });
                            ui.end_row();
                        });
                    },
                );
            }
            self.info_open = open && self.open.is_some();
        }
    }

    fn tool_windows(&mut self, ctx: &egui::Context) {
        if let Some(d) = &mut self.sign_dialog {
            match d.show(ctx) {
                SignOutcome::Open => {}
                SignOutcome::Cancel => {
                    self.sign_dialog = None;
                    self.sign_target = None;
                }
                SignOutcome::Done(sig) => {
                    self.sign_dialog = None;
                    match self.sign_target.take() {
                        Some((page, rect)) => self.place_signature(page, rect, &sig),
                        None => self.placing = Some(sig),
                    }
                }
            }
        }

        if let Some(c) = &mut self.choice {
            let mut picked = None;
            let area =
                egui::Area::new(Id::new("choice")).order(egui::Order::Foreground).fixed_pos(c.pos).show(ctx, |ui| {
                    egui::Frame::popup(ui.style()).show(ui, |ui| {
                        ui.set_min_width(c.width.max(120.0));
                        egui::ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
                            for opt in &c.options {
                                if ui.selectable_label(*opt == c.current, opt).clicked() {
                                    picked = Some(opt.clone());
                                }
                            }
                        });
                    });
                });
            let clicked_outside =
                !c.opened && ctx.input(|i| i.pointer.any_pressed()) && !area.response.contains_pointer();
            c.opened = false;
            if let Some(v) = picked {
                let name = c.name.clone();
                self.choice = None;
                self.set_field(&name, FieldValue::Choice(v));
            } else if clicked_outside || ctx.input(|i| i.key_pressed(Key::Escape)) {
                self.choice = None;
            }
        }

        if let Some(d) = &mut self.protect_dialog {
            let mut result = None;
            egui::Modal::new(Id::new("protect")).show(ctx, |ui| {
                ui.heading("Protect with a password");
                egui::Grid::new("pw").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                    ui.label("Password to open");
                    ui.add(egui::TextEdit::singleline(&mut d.user).password(true).hint_text("leave empty to allow anyone"));
                    ui.end_row();
                    ui.label("Confirm");
                    ui.add(egui::TextEdit::singleline(&mut d.confirm).password(true));
                    ui.end_row();
                    ui.label("Owner password");
                    ui.add(egui::TextEdit::singleline(&mut d.owner).password(true).hint_text("to change permissions"));
                    ui.end_row();
                });
                ui.add_space(6.0);
                ui.label(RichText::new("People who open it with the first password may:").strong());
                ui.horizontal_wrapped(|ui| {
                    ui.checkbox(&mut d.allow.print, "Print");
                    ui.checkbox(&mut d.allow.copy, "Copy text");
                    ui.checkbox(&mut d.allow.modify, "Edit");
                    ui.checkbox(&mut d.allow.annotate, "Comment");
                    ui.checkbox(&mut d.allow.fill_forms, "Fill forms");
                    ui.checkbox(&mut d.allow.assemble, "Rearrange pages");
                });
                ui.label(
                    RichText::new("Encryption: AES-256. Permissions are honoured by most PDF apps but are not enforced by the encryption itself.")
                        .small()
                        .weak(),
                );
                if let Some(e) = &d.error {
                    ui.colored_label(ui.visuals().error_fg_color, e);
                }
                ui.horizontal(|ui| {
                    if ui.button("Protect").clicked() {
                        result = Some(true);
                    }
                    if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                        result = Some(false);
                    }
                });
            });
            match result {
                Some(true) => {
                    if d.user != d.confirm {
                        d.error = Some("The passwords don't match".into());
                    } else if d.user.is_empty() && d.owner.is_empty() {
                        d.error = Some("Enter a password to open, an owner password, or both".into());
                    } else {
                        let p = Protection {
                            user_password: d.user.clone(),
                            owner_password: d.owner.clone(),
                            allow: d.allow,
                        };
                        self.protect_dialog = None;
                        self.protect(p);
                    }
                }
                Some(false) => self.protect_dialog = None,
                None => {}
            }
        }

        if let Some((sel, size)) = &mut self.compress_dialog {
            let mut run = None;
            egui::Modal::new(Id::new("compress")).show(ctx, |ui| {
                ui.heading("Compress PDF");
                ui.label(format!("Current size: {}", human(*size)));
                ui.add_space(4.0);
                for (i, (name, desc, _)) in COMPRESS_PRESETS.iter().enumerate() {
                    ui.radio_value(sel, i, RichText::new(*name).strong());
                    ui.label(RichText::new(*desc).small().weak());
                }
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button("Compress").clicked() {
                        run = Some(true);
                    }
                    if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                        run = Some(false);
                    }
                });
            });
            match run {
                Some(true) => {
                    let opts = COMPRESS_PRESETS[*sel].2;
                    self.compress_dialog = None;
                    self.compress(opts);
                }
                Some(false) => self.compress_dialog = None,
                None => {}
            }
        }
    }

    fn finish(&mut self, ctx: &egui::Context, action: Pending) {
        if matches!(action, Pending::Quit) {
            self.allow_close = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        } else {
            self.perform(action);
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if ctx.input(|i| i.viewport().close_requested()) && self.dirty() && !self.allow_close {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.pending = Some(Pending::Quit);
        }
        let dropped: Option<PathBuf> = ctx
            .input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).find(|p| !p.as_os_str().is_empty()));
        if let Some(p) = dropped {
            self.request(Pending::Open(Some(p)));
        }
        let modal = self.pending.is_some()
            || self.password_prompt.is_some()
            || self.split_dialog.is_some()
            || self.sign_dialog.is_some()
            || self.protect_dialog.is_some()
            || self.compress_dialog.is_some();
        if !modal {
            self.shortcuts(&ctx);
        }
        // Only send the title when it changes: every viewport command wakes the event loop.
        let title = self.title();
        if title != self.last_title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.last_title = title;
        }
        if self.status.as_ref().is_some_and(|s| s.at.elapsed().as_secs() > 8 && !s.error) {
            self.status = None;
        }

        egui::Panel::top("menu").show(ui, |ui| self.menu_bar(ui));
        egui::Panel::top("toolbar").show(ui, |ui| {
            ui.add_space(2.0);
            self.toolbar(ui);
            ui.add_space(2.0);
        });
        egui::Panel::bottom("status").show(ui, |ui| self.status_bar(ui));
        if self.open.is_some() {
            egui::Panel::left("side").resizable(true).default_size(230.0).show(ui, |ui| self.side_panel(ui));
        }
        if self.open.as_ref().is_some_and(|o| !o.fields.is_empty()) {
            egui::Panel::top("formbar").show(ui, |ui| self.form_bar(ui));
        }
        let bg = if ui.visuals().dark_mode { Color32::from_gray(40) } else { Color32::from_gray(200) };
        egui::CentralPanel::default().frame(egui::Frame::new().fill(bg)).show(ui, |ui| {
            if self.open.is_some() {
                self.document_view(ui);
            } else {
                self.welcome(ui);
            }
        });
        self.windows(&ctx);
    }
}

/// Largest rect with `aspect` (width / height in points) centred inside `r` on a `pw` × `ph` page.
fn fit_aspect(r: NormRect, aspect: f32, pw: f32, ph: f32) -> NormRect {
    let (bw, bh) = ((r.x1 - r.x0) * pw, (r.y1 - r.y0) * ph);
    let a = aspect.max(0.01);
    let (w, h) = if bw / bh.max(1e-6) > a { (bh * a, bh) } else { (bw, bw / a) };
    let (cx, cy) = ((r.x0 + r.x1) / 2.0 * pw, (r.y0 + r.y1) / 2.0 * ph);
    NormRect { x0: (cx - w / 2.0) / pw, y0: (cy - h / 2.0) / ph, x1: (cx + w / 2.0) / pw, y1: (cy + h / 2.0) / ph }
}

fn field_tooltip(f: &FormField) -> String {
    let mut t = format!("{} ({})", f.name, f.kind.label());
    if f.required {
        t.push_str(" · required");
    }
    t
}

fn human(n: usize) -> String {
    match n {
        n if n >= 1 << 20 => format!("{:.1} MB", n as f64 / (1 << 20) as f64),
        n if n >= 1 << 10 => format!("{:.0} KB", n as f64 / 1024.0),
        n => format!("{n} B"),
    }
}

fn uv_full() -> Rect {
    Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0))
}

fn with_pdf_extension(p: PathBuf) -> PathBuf {
    if p.extension().is_some_and(|e| e.eq_ignore_ascii_case("pdf")) { p } else { p.with_extension("pdf") }
}

/// "page 3" / "3 pages" for status messages.
fn describe(pages: &[usize]) -> String {
    match pages {
        [p] => format!("page {}", p + 1),
        ps => format!("{} pages", ps.len()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app_with(pages: usize) -> App {
        let ctx = egui::Context::default();
        let mut app = App::new(&ctx, None);
        let texts: Vec<String> = (1..=pages).map(|i| format!("Page {i}")).collect();
        let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
        app.open = Some(Open::new(pdfforge_core::sample_document(&refs).unwrap(), None));
        app
    }

    fn labels(app: &App) -> Vec<String> {
        let d = &app.open.as_ref().unwrap().doc;
        (0..d.page_count()).map(|i| d.text(i).unwrap().trim().to_string()).collect()
    }

    #[test]
    fn edits_with_undo_and_redo() {
        let mut app = app_with(4);
        app.open.as_mut().unwrap().selection.set(&[1, 2]);
        app.move_pages(false);
        assert_eq!(labels(&app), ["Page 1", "Page 4", "Page 2", "Page 3"]);
        assert_eq!(app.open.as_ref().unwrap().selection.pages(), [2, 3], "selection follows the moved pages");
        assert!(app.dirty());

        app.delete();
        assert_eq!(labels(&app), ["Page 1", "Page 4"]);

        app.undo_redo(false);
        assert_eq!(labels(&app), ["Page 1", "Page 4", "Page 2", "Page 3"]);
        app.undo_redo(false);
        assert_eq!(labels(&app), ["Page 1", "Page 2", "Page 3", "Page 4"]);
        app.undo_redo(false); // nothing left: no-op
        app.undo_redo(true);
        assert_eq!(labels(&app), ["Page 1", "Page 4", "Page 2", "Page 3"]);
    }

    #[test]
    fn rotate_and_blank_use_current_page_without_selection() {
        let mut app = app_with(3);
        app.current_page = 1;
        app.rotate(Rotation::R90);
        let o = app.open.as_ref().unwrap();
        assert_eq!(o.doc.rotation(1).unwrap(), Rotation::R90);
        assert!(o.sizes[1].0 > o.sizes[1].1, "sizes refreshed after rotating");
        app.insert_blank();
        assert_eq!(labels(&app), ["Page 1", "Page 2", "", "Page 3"]);
    }

    #[test]
    fn delete_everything_is_refused() {
        let mut app = app_with(2);
        app.open.as_mut().unwrap().selection.select_all();
        app.delete();
        assert_eq!(labels(&app).len(), 2);
        assert!(app.status.as_ref().is_some_and(|s| s.error));
        assert!(app.open.as_ref().unwrap().undo.is_empty(), "failed edits leave no undo entry");
    }

    #[test]
    fn search_steps_through_hits() {
        let mut app = app_with(3);
        app.search.query = "page".into();
        app.run_search(false);
        assert_eq!(app.search.hits.len(), 3);
        assert_eq!(app.search.current, Some(0));
        app.run_search(false);
        assert_eq!(app.search.current, Some(1));
        app.run_search(true);
        app.run_search(true);
        assert_eq!(app.search.current, Some(2), "wraps backwards");
        assert_eq!(app.goto, Some(2));
        app.delete();
        assert!(app.search.hits.is_empty(), "edits invalidate hits");
    }

    fn form_app() -> App {
        let ctx = egui::Context::default();
        let mut app = App::new(&ctx, None);
        let p = concat!(env!("CARGO_MANIFEST_DIR"), "/../pdfforge-core/tests/fixtures/form.pdf");
        app.open = Some(Open::new(Document::open(p, None).unwrap(), None));
        app
    }

    fn value(app: &App, name: &str) -> String {
        app.open.as_ref().unwrap().fields.iter().find(|f| f.name == name).unwrap().value.clone()
    }

    #[test]
    fn fills_form_fields_with_undo() {
        let mut app = form_app();
        assert_eq!(app.open.as_ref().unwrap().fields.len(), 5);
        app.apply_page_action(PageAction::EditText(TextEditing {
            name: "name".into(),
            page: 0,
            rect: NormRect { x0: 0.0, y0: 0.0, x1: 0.1, y1: 0.1 },
            text: "Ada".into(),
            multiline: false,
            focus: false,
        }));
        app.apply_page_action(PageAction::CommitText);
        assert_eq!(value(&app, "name"), "Ada");
        app.apply_page_action(PageAction::Toggle("subscribe".into(), true));
        app.apply_page_action(PageAction::Radio("plan".into(), "pro".into()));
        assert_eq!(value(&app, "plan"), "pro");
        assert!(app.open.as_ref().unwrap().fields.iter().find(|f| f.name == "subscribe").unwrap().checked);
        app.undo_redo(false);
        assert_eq!(value(&app, "plan"), "basic");
        // Unchanged text does not create an undo step.
        let steps = app.open.as_ref().unwrap().undo.len();
        app.editing = Some(TextEditing {
            name: "name".into(),
            page: 0,
            rect: NormRect { x0: 0.0, y0: 0.0, x1: 0.1, y1: 0.1 },
            text: "Ada".into(),
            multiline: false,
            focus: false,
        });
        app.commit_text();
        assert_eq!(app.open.as_ref().unwrap().undo.len(), steps);
        app.flatten_form();
        assert!(app.open.as_ref().unwrap().fields.is_empty());
    }

    #[test]
    fn protects_and_undoes_protection() {
        let mut app = app_with(2);
        app.protect(Protection { user_password: "pw".into(), ..Default::default() });
        let o = app.open.as_ref().unwrap();
        assert!(o.encrypted);
        assert!(Document::from_bytes(o.doc.to_bytes().unwrap(), None).is_err());
        app.rotate(Rotation::R90);
        app.undo_redo(false); // back to protected, unrotated
        assert!(app.open.as_ref().unwrap().encrypted);
        app.undo_redo(false); // back to unprotected
        assert!(!app.open.as_ref().unwrap().encrypted);
        app.undo_redo(true);
        app.unprotect();
        assert!(!app.open.as_ref().unwrap().encrypted);
    }

    #[test]
    fn places_signatures_and_compresses() {
        let mut app = app_with(1);
        let sig =
            Signature::Text { text: "Ada Lovelace".into(), font: pdfforge_core::StampFont::TimesItalic, color: [0; 3] };
        app.placing = Some(sig);
        app.apply_page_action(PageAction::Place(0, NormRect { x0: 0.5, y0: 0.8, x1: 0.9, y1: 0.86 }));
        assert!(app.placing.is_none());
        assert!(labels(&app)[0].contains("Ada Lovelace"));
        app.compress(CompressOptions::LOSSLESS);
        assert!(labels(&app)[0].contains("Ada Lovelace"));
    }

    #[test]
    fn fits_signature_boxes() {
        // A 4:1 signature in a 200x100 pt box becomes 200x50, centred.
        let r = fit_aspect(NormRect { x0: 0.0, y0: 0.0, x1: 0.5, y1: 0.25 }, 4.0, 400.0, 400.0);
        assert!(((r.x1 - r.x0) * 400.0 - 200.0).abs() < 0.01);
        assert!(((r.y1 - r.y0) * 400.0 - 50.0).abs() < 0.01);
        assert!((r.y0 * 400.0 - 25.0).abs() < 0.01);
        assert_eq!(human(3 << 20), "3.0 MB");
    }

    #[test]
    fn describes_pages() {
        assert_eq!(describe(&[2]), "page 3");
        assert_eq!(describe(&[0, 1]), "2 pages");
        assert_eq!(with_pdf_extension("a".into()), PathBuf::from("a.pdf"));
        assert_eq!(with_pdf_extension("a.PDF".into()), PathBuf::from("a.PDF"));
    }
}
