use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use pdfforge_core::{
    Allow, CompressOptions, Document, FieldKind, FieldValue, NormRect, Protection, Rotation, StampFont,
    format_page_ranges, parse_page_ranges,
};

const USAGE: &str = "\
Usage: pdfforge <command> [args] [options]

Inspect:
  info    <file.pdf>                     Metadata, page count and page sizes
  text    <file.pdf> [-p PAGES]          Print the text of pages
  search  <file.pdf> <query> [--case] [--word]
                                         List matches with page numbers
  outline <file.pdf>                     Print bookmarks
  render  <file.pdf> [-p PAGES] [--dpi N] [-o DIR] [--jpg]
                                         Save pages as PNG (or JPEG) images

Edit (writes <name>-<command>.pdf next to the input unless -o or --in-place):
  merge   <a.pdf> <b.pdf>... -o <out.pdf>
                                         Concatenate documents
  extract <file.pdf> <PAGES>             Keep only these pages, in this order
  delete  <file.pdf> <PAGES>             Remove pages
  rotate  <file.pdf> <PAGES> <90|180|270|-90>
                                         Rotate pages clockwise
  reorder <file.pdf> <ORDER>             New page order, e.g. \"3,1,2\" or \"last-1\"
  insert  <file.pdf> <other.pdf> --at N  Insert another PDF before page N
  blank   <file.pdf> --at N              Insert a blank page before page N
  split   <file.pdf> (--every N | --ranges \"1-3;4-6\") [-o DIR]
                                         Split into several files

Forms, signatures and security (edit commands, same output rules):
  fields  <file.pdf>                     List form fields, their types, values and options
  fill    <file.pdf> NAME=VALUE... [--flatten]
                                         Fill form fields (checkboxes: yes/no; radio and
                                         dropdowns: the option name)
  flatten <file.pdf> [-p PAGES]          Make form fields and annotations part of the page
  sign    <file.pdf> (--image sig.png | --text \"Your Name\") [-p PAGES]
          [--rect X,Y,W,H] [--font NAME] [--color #rrggbb]
                                         Place a visual signature (default: bottom right of
                                         the last page). --rect is in points from the
                                         top-left of the page. Not a certificate signature.
  protect <file.pdf> [--user-password PW] [--owner-password PW] [--allow LIST]
                                         Encrypt with AES-256. LIST: print,copy,modify,
                                         annotate,forms,assemble or none (default: all)
  unprotect <file.pdf> --password PW     Remove the password and restrictions
  compress <file.pdf> [--preset screen|print|smallest|lossless] [--dpi N] [--quality N]
                                         Shrink the file (default preset: screen)

PAGES (one-based): 1-3,5  8-  -4  last  odd  even  all  (default: all)

Options:
  -o, --output <path>   Output file (or directory for render/split)
      --in-place        Overwrite the input file (edit commands)
      --password <pw>   Password for encrypted PDFs
                        A password of \"-\" is read from standard input instead (passwords on
                        the command line can be seen by other users of this computer).
  -h, --help            Show this help

PDFium is loaded from $PDFIUM_LIB_PATH, next to the executable, or the system path.";

/// Error value used when stdout is closed early (e.g. `pdfforge text x.pdf | head`).
const BROKEN_PIPE: &str = "\0broken pipe";

/// `println!` that propagates write errors instead of panicking.
macro_rules! out {
    ($($t:tt)*) => {{
        use std::io::Write as _;
        writeln!(std::io::stdout().lock(), $($t)*).map_err(|e| {
            if e.kind() == std::io::ErrorKind::BrokenPipe { BROKEN_PIPE.to_string() } else { e.to_string() }
        })?
    }};
}

fn main() -> ExitCode {
    match run(std::env::args_os().skip(1).collect()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) if e == BROKEN_PIPE => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Parsed command line: positional arguments plus options.
#[derive(Default)]
struct Args {
    pos: Vec<OsString>,
    output: Option<PathBuf>,
    pages: Option<String>,
    password: Option<String>,
    in_place: bool,
    dpi: Option<f32>,
    jpg: bool,
    case: bool,
    word: bool,
    every: Option<usize>,
    ranges: Option<String>,
    at: Option<usize>,
    flatten: bool,
    image: Option<PathBuf>,
    text: Option<String>,
    font: Option<String>,
    rect: Option<String>,
    color: Option<String>,
    user_password: Option<String>,
    owner_password: Option<String>,
    allow: Option<String>,
    preset: Option<String>,
    quality: Option<u8>,
}

fn parse_args(raw: Vec<OsString>) -> Result<Args, String> {
    let mut a = Args::default();
    let mut it = raw.into_iter();
    let val = |it: &mut std::vec::IntoIter<OsString>, flag: &str| {
        it.next().ok_or_else(|| format!("missing value for {flag}"))
    };
    let num = |s: OsString, flag: &str| -> Result<usize, String> {
        s.to_str().and_then(|v| v.parse().ok()).ok_or_else(|| format!("{flag} needs a whole number"))
    };
    let text = |s: OsString, flag: &str| s.into_string().map_err(|_| format!("{flag} value is not valid UTF-8"));
    let mut only_positional = false;
    while let Some(arg) = it.next() {
        if only_positional {
            a.pos.push(arg);
            continue;
        }
        match arg.to_str() {
            Some("--") => only_positional = true,
            Some("-o" | "--output") => a.output = Some(val(&mut it, "--output")?.into()),
            Some("-p" | "--pages") => a.pages = Some(text(val(&mut it, "--pages")?, "--pages")?),
            Some("--password") => a.password = Some(text(val(&mut it, "--password")?, "--password")?),
            Some("--in-place") => a.in_place = true,
            Some("--dpi") => {
                let d = num(val(&mut it, "--dpi")?, "--dpi")?;
                if !(10..=1200).contains(&d) {
                    return Err("--dpi must be between 10 and 1200".into());
                }
                a.dpi = Some(d as f32);
            }
            Some("--jpg" | "--jpeg") => a.jpg = true,
            Some("--case") => a.case = true,
            Some("--word") => a.word = true,
            Some("--every") => a.every = Some(num(val(&mut it, "--every")?, "--every")?),
            Some("--ranges") => a.ranges = Some(text(val(&mut it, "--ranges")?, "--ranges")?),
            Some("--at") => a.at = Some(num(val(&mut it, "--at")?, "--at")?),
            Some("--flatten") => a.flatten = true,
            Some("--image") => a.image = Some(val(&mut it, "--image")?.into()),
            Some("--text") => a.text = Some(text(val(&mut it, "--text")?, "--text")?),
            Some("--font") => a.font = Some(text(val(&mut it, "--font")?, "--font")?),
            Some("--rect") => a.rect = Some(text(val(&mut it, "--rect")?, "--rect")?),
            Some("--color") => a.color = Some(text(val(&mut it, "--color")?, "--color")?),
            Some("--user-password") => {
                a.user_password = Some(text(val(&mut it, "--user-password")?, "--user-password")?)
            }
            Some("--owner-password") => {
                a.owner_password = Some(text(val(&mut it, "--owner-password")?, "--owner-password")?)
            }
            Some("--allow") => a.allow = Some(text(val(&mut it, "--allow")?, "--allow")?),
            Some("--preset") => a.preset = Some(text(val(&mut it, "--preset")?, "--preset")?),
            Some("--quality") => {
                let q = num(val(&mut it, "--quality")?, "--quality")?;
                if !(1..=100).contains(&q) {
                    return Err("--quality must be between 1 and 100".into());
                }
                a.quality = Some(q as u8);
            }
            Some(s) if s.starts_with('-') && s.len() > 1 && s.parse::<i32>().is_err() => {
                return Err(format!("unknown option {s} (see --help)"));
            }
            _ => a.pos.push(arg),
        }
    }
    Ok(a)
}

fn run(raw: Vec<OsString>) -> Result<(), String> {
    if raw.is_empty() || raw.iter().any(|a| a == "-h" || a == "--help") {
        out!("{USAGE}");
        return Ok(());
    }
    let mut a = parse_args(raw)?;
    let cmd = a.pos.remove(0).into_string().map_err(|_| "invalid command".to_string())?;
    let e = |e: pdfforge_core::Error| e.to_string();

    // Positional argument helpers.
    let need = |a: &Args, n: usize, what: &str| -> Result<(), String> {
        if a.pos.len() < n { Err(format!("{cmd}: missing {what} (see --help)")) } else { Ok(()) }
    };
    let pos_str = |a: &Args, i: usize| a.pos[i].to_string_lossy().into_owned();

    match cmd.as_str() {
        "info" => {
            need(&a, 1, "input file")?;
            let d = open(&a, 0)?;
            let i = d.info();
            let row = |k: &str, v: &str| -> Result<(), String> {
                if !v.is_empty() {
                    out!("{k:<10} {v}");
                }
                Ok(())
            };
            row("File", &a.pos[0].to_string_lossy())?;
            row("Pages", &i.pages.to_string())?;
            row("Version", &i.version)?;
            row("Title", &i.title)?;
            row("Author", &i.author)?;
            row("Subject", &i.subject)?;
            row("Keywords", &i.keywords)?;
            row("Creator", &i.creator)?;
            row("Producer", &i.producer)?;
            row("Created", &i.created)?;
            row("Modified", &i.modified)?;
            row("Encrypted", if i.encrypted { "yes" } else { "no" })?;
            // Group consecutive pages of the same size so long documents stay readable.
            let mut groups: Vec<(Vec<usize>, (f32, f32))> = Vec::new();
            for p in 0..d.page_count() {
                let s = d.page_size(p).map_err(e)?;
                match groups.last_mut() {
                    Some((pages, size)) if (size.0 - s.0).abs() < 0.5 && (size.1 - s.1).abs() < 0.5 => pages.push(p),
                    _ => groups.push((vec![p], s)),
                }
            }
            for (pages, (w, h)) in groups {
                out!("{:<10} {:.0} x {:.0} pt ({}){}", "Page size", w, h, paper_name(w, h), {
                    let r = format_page_ranges(&pages);
                    if pages.len() == d.page_count() { String::new() } else { format!("  pages {r}") }
                });
            }
        }
        "text" => {
            need(&a, 1, "input file")?;
            let d = open(&a, 0)?;
            let pages = pages(a.pages.as_deref(), &d)?;
            for (n, p) in pages.iter().enumerate() {
                if pages.len() > 1 {
                    if n > 0 {
                        out!();
                    }
                    out!("--- page {} ---", p + 1);
                }
                out!("{}", d.text(*p).map_err(e)?.trim_end());
            }
        }
        "search" => {
            need(&a, 2, "input file and query")?;
            let d = open(&a, 0)?;
            let q = pos_str(&a, 1);
            let hits = d.search(&q, a.case, a.word).map_err(e)?;
            for h in &hits {
                out!("p.{:<5} {}", h.page + 1, h.snippet);
            }
            eprintln!("{} match{}", hits.len(), if hits.len() == 1 { "" } else { "es" });
        }
        "outline" => {
            need(&a, 1, "input file")?;
            let d = open(&a, 0)?;
            let items = d.outline();
            if items.is_empty() {
                eprintln!("no bookmarks");
            }
            for it in items {
                let page = it.page.map(|p| (p + 1).to_string()).unwrap_or_else(|| "-".into());
                out!("{:>5}  {}{}", page, "  ".repeat(it.depth), it.title);
            }
        }
        "render" => {
            need(&a, 1, "input file")?;
            let input = PathBuf::from(&a.pos[0]);
            let d = open(&a, 0)?;
            let pages = pages(a.pages.as_deref(), &d)?;
            let dir = a.output.clone().unwrap_or_else(|| input.parent().unwrap_or(Path::new(".")).to_path_buf());
            std::fs::create_dir_all(&dir).map_err(|err| format!("cannot create {}: {err}", dir.display()))?;
            let scale = a.dpi.unwrap_or(150.0) / 72.0;
            let stem = stem(&input);
            let width = d.page_count().to_string().len();
            for p in pages {
                let bmp = d.render(p, scale).map_err(e)?;
                let ext = if a.jpg { "jpg" } else { "png" };
                let out = dir.join(format!("{stem}-{:0width$}.{ext}", p + 1));
                save_image(&bmp, &out, a.jpg)?;
                out!("Wrote {} ({}x{})", out.display(), bmp.width, bmp.height);
            }
        }
        "merge" => {
            need(&a, 2, "at least two input files")?;
            let out = a.output.clone().ok_or("merge: -o <out.pdf> is required")?;
            let docs: Vec<Document> = (0..a.pos.len()).map(|i| open(&a, i)).collect::<Result<_, _>>()?;
            let refs: Vec<&Document> = docs.iter().collect();
            let m = Document::merge(&refs).map_err(e)?;
            m.save(&out).map_err(e)?;
            out!("Wrote {} ({} pages from {} files)", out.display(), m.page_count(), docs.len());
        }
        "extract" | "delete" | "rotate" | "reorder" => {
            let what = if cmd == "rotate" { "input file, pages and angle" } else { "input file and pages" };
            need(&a, if cmd == "rotate" { 3 } else { 2 }, what)?;
            let mut d = open(&a, 0)?;
            let sel = pages(Some(&pos_str(&a, 1)), &d)?;
            match cmd.as_str() {
                "extract" => d = d.extract(&sel).map_err(e)?,
                "delete" => d.delete_pages(&sel).map_err(e)?,
                "reorder" => d.reorder(&sel).map_err(e)?,
                _ => {
                    let deg: i32 = pos_str(&a, 2).parse().map_err(|_| "rotate: angle must be 90, 180, 270 or -90")?;
                    let r = Rotation::from_degrees(deg).ok_or("rotate: angle must be a multiple of 90")?;
                    d.rotate_pages(&sel, r).map_err(e)?;
                }
            }
            save_edit(&a, &cmd, &d)?;
        }
        "insert" => {
            need(&a, 2, "input file and the PDF to insert")?;
            let mut d = open(&a, 0)?;
            let other = open(&a, 1)?;
            let at = insert_pos(&a, &d)?;
            d.insert_document(&other, at).map_err(e)?;
            save_edit(&a, &cmd, &d)?;
        }
        "blank" => {
            need(&a, 1, "input file")?;
            let mut d = open(&a, 0)?;
            let at = insert_pos(&a, &d)?;
            d.insert_blank_page(at).map_err(e)?;
            save_edit(&a, &cmd, &d)?;
        }
        "split" => {
            need(&a, 1, "input file")?;
            let input = PathBuf::from(&a.pos[0]);
            let d = open(&a, 0)?;
            let (parts, labels): (Vec<Document>, Vec<String>) = match (a.every, &a.ranges) {
                (Some(n), None) => {
                    let parts = d.split_every(n).map_err(e)?;
                    let labels = (0..parts.len())
                        .map(|i| format_page_ranges(&((i * n)..((i + 1) * n).min(d.page_count())).collect::<Vec<_>>()))
                        .collect();
                    (parts, labels)
                }
                (None, Some(spec)) => {
                    let groups: Vec<Vec<usize>> = spec
                        .split(';')
                        .filter(|g| !g.trim().is_empty())
                        .map(|g| parse_page_ranges(g, d.page_count()).map_err(e))
                        .collect::<Result<_, _>>()?;
                    let labels = groups.iter().map(|g| format_page_ranges(g)).collect();
                    (d.split_groups(&groups).map_err(e)?, labels)
                }
                _ => return Err("split: give exactly one of --every N or --ranges \"1-3;4-6\"".into()),
            };
            let dir = a.output.clone().unwrap_or_else(|| input.parent().unwrap_or(Path::new(".")).to_path_buf());
            std::fs::create_dir_all(&dir).map_err(|err| format!("cannot create {}: {err}", dir.display()))?;
            let stem = stem(&input);
            let width = parts.len().to_string().len();
            for (i, (part, label)) in parts.iter().zip(labels).enumerate() {
                let out = dir.join(format!("{stem}-part{:0width$}.pdf", i + 1));
                part.save(&out).map_err(e)?;
                out!("Wrote {} ({} {label})", out.display(), if part.page_count() == 1 { "page" } else { "pages" });
            }
        }
        "fields" => {
            need(&a, 1, "input file")?;
            let d = open(&a, 0)?;
            let fields = d.form_fields().map_err(e)?;
            if fields.is_empty() {
                eprintln!("no form fields");
            }
            for f in &fields {
                let page = f.widgets.first().map(|w| (w.page + 1).to_string()).unwrap_or_else(|| "-".into());
                let mut extra = Vec::new();
                if !f.options.is_empty() {
                    extra.push(format!("options: {}", f.options.join(" | ")));
                }
                if matches!(f.kind, FieldKind::Radio) {
                    let states: Vec<String> = f.widgets.iter().filter_map(|w| w.on_state.clone()).collect();
                    extra.push(format!("options: {}", states.join(" | ")));
                }
                if f.read_only {
                    extra.push("read-only".into());
                }
                if f.required {
                    extra.push("required".into());
                }
                let value = match f.kind {
                    FieldKind::Checkbox => if f.checked { "yes" } else { "no" }.to_string(),
                    _ => format!("{:?}", f.value),
                };
                let extra = if extra.is_empty() { String::new() } else { format!("  [{}]", extra.join("; ")) };
                out!("p.{page:<4} {:<9} {} = {value}{extra}", f.kind.label(), f.name);
            }
        }
        "fill" => {
            need(&a, 2, "input file and at least one NAME=VALUE")?;
            let mut d = open(&a, 0)?;
            let fields = d.form_fields().map_err(e)?;
            let mut values = Vec::new();
            for arg in &a.pos[1..] {
                let arg = arg.to_string_lossy();
                let (name, value) = arg.split_once('=').ok_or_else(|| format!("'{arg}' should be NAME=VALUE"))?;
                let f = fields.iter().find(|f| f.name == name).ok_or_else(|| {
                    let names: Vec<&str> = fields.iter().map(|f| f.name.as_str()).collect();
                    format!("no field named '{name}' (fields: {})", names.join(", "))
                })?;
                values.push((name.to_string(), FieldValue::parse_for(&f.kind, value).map_err(e)?));
            }
            d.fill_fields(&values).map_err(e)?;
            if a.flatten {
                d.flatten(None).map_err(e)?;
            }
            save_edit(&a, &cmd, &d)?;
        }
        "flatten" => {
            need(&a, 1, "input file")?;
            let mut d = open(&a, 0)?;
            let sel = pages(a.pages.as_deref(), &d)?;
            d.flatten(Some(&sel)).map_err(e)?;
            save_edit(&a, &cmd, &d)?;
        }
        "sign" => {
            need(&a, 1, "input file")?;
            let mut d = open(&a, 0)?;
            let n = d.page_count();
            let sel = match &a.pages {
                Some(p) => pages(Some(p), &d)?,
                None => vec![n.saturating_sub(1)],
            };
            let color = match &a.color {
                Some(c) => parse_color(c)?,
                None => [0, 0, 0],
            };
            let image = match &a.image {
                Some(p) => Some(image::open(p).map_err(|err| format!("cannot read {}: {err}", p.display()))?),
                None => None,
            };
            if image.is_some() == a.text.is_some() {
                return Err("sign: give exactly one of --image FILE or --text \"Your Name\"".into());
            }
            let font = match &a.font {
                Some(f) => StampFont::ALL
                    .into_iter()
                    .find(|x| x.name().eq_ignore_ascii_case(f) || x.name().replace(' ', "").eq_ignore_ascii_case(f))
                    .ok_or_else(|| {
                        let names: Vec<&str> = StampFont::ALL.iter().map(|f| f.name()).collect();
                        format!("unknown font '{f}' (fonts: {})", names.join(", "))
                    })?,
                None => StampFont::default(),
            };
            for p in sel {
                let (pw, ph) = d.page_size(p).map_err(e)?;
                let aspect = image.as_ref().map(|i| i.height() as f32 / i.width().max(1) as f32);
                let rect = sign_rect(a.rect.as_deref(), pw, ph, aspect)?;
                match &image {
                    Some(img) => d.stamp_image(p, rect, img).map_err(e)?,
                    None => d.stamp_text(p, rect, a.text.as_deref().unwrap_or(""), font, color).map_err(e)?,
                }
            }
            save_edit(&a, &cmd, &d)?;
        }
        "protect" => {
            need(&a, 1, "input file")?;
            let mut d = open(&a, 0)?;
            let user = a.user_password.clone().map(read_secret).transpose()?.unwrap_or_default();
            let owner = a.owner_password.clone().map(read_secret).transpose()?.unwrap_or_default();
            if user.is_empty() && owner.is_empty() {
                return Err(
                    "protect: give --user-password (to open) and/or --owner-password (to change permissions)".into()
                );
            }
            let allow = match &a.allow {
                Some(list) => parse_allow(list)?,
                None => Allow::ALL,
            };
            d.protect(&Protection { user_password: user, owner_password: owner, allow }).map_err(e)?;
            save_edit(&a, &cmd, &d)?;
        }
        "unprotect" => {
            need(&a, 1, "input file")?;
            let mut d = open(&a, 0)?;
            if !d.security().map_err(e)?.encrypted {
                eprintln!("note: {} is not protected", a.pos[0].to_string_lossy());
            }
            d.unprotect().map_err(e)?;
            save_edit(&a, &cmd, &d)?;
        }
        "compress" => {
            need(&a, 1, "input file")?;
            let mut d = open(&a, 0)?;
            let name = a.preset.as_deref().unwrap_or("screen");
            let mut opts = CompressOptions::preset(name)
                .ok_or_else(|| format!("unknown preset '{name}' (screen, print, smallest, lossless)"))?;
            if let Some(dpi) = a.dpi {
                opts.image_dpi = Some(dpi);
                opts.recompress_images = true;
            }
            if let Some(q) = a.quality {
                opts.jpeg_quality = q;
                opts.recompress_images = true;
            }
            let r = d.compress(&opts).map_err(e)?;
            eprintln!(
                "{} -> {} ({:.0}% smaller), {} of {} images recompressed",
                human(r.before),
                human(r.after),
                100.0 * (1.0 - r.after as f64 / r.before.max(1) as f64),
                r.images_recompressed,
                r.images_total
            );
            save_edit(&a, &cmd, &d)?;
        }
        other => return Err(format!("unknown command '{other}' (see --help)")),
    }
    Ok(())
}

fn open(a: &Args, i: usize) -> Result<Document, String> {
    let path = Path::new(&a.pos[i]);
    let pw = a.password.clone().map(read_secret).transpose()?;
    Document::open(path, pw.as_deref()).map_err(|e| match e {
        pdfforge_core::Error::Io(err) => format!("cannot read {}: {err}", path.display()),
        pdfforge_core::Error::Pdfium(_) => format!("{} is not a valid PDF ({e})", path.display()),
        e => format!("{}: {e}", path.display()),
    })
}

fn pages(spec: Option<&str>, d: &Document) -> Result<Vec<usize>, String> {
    parse_page_ranges(spec.unwrap_or("all"), d.page_count()).map_err(|e| e.to_string())
}

/// `--at N` (one-based, N = page count + 1 appends) → zero-based insert index.
fn insert_pos(a: &Args, d: &Document) -> Result<usize, String> {
    let n = a.at.unwrap_or(d.page_count() + 1);
    if n == 0 || n > d.page_count() + 1 {
        return Err(format!("--at must be between 1 and {} (to append)", d.page_count() + 1));
    }
    Ok(n - 1)
}

fn stem(p: &Path) -> String {
    p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "page".into())
}

fn save_edit(a: &Args, cmd: &str, d: &Document) -> Result<(), String> {
    let input = PathBuf::from(&a.pos[0]);
    let out = match (&a.output, a.in_place) {
        (Some(_), true) => return Err("use either -o or --in-place, not both".into()),
        (Some(o), false) => o.clone(),
        (None, true) => input.clone(),
        (None, false) => input.with_file_name(format!("{}-{cmd}.pdf", stem(&input))),
    };
    d.save(&out).map_err(|e| format!("cannot write {}: {e}", out.display()))?;
    out!("Wrote {} ({} pages)", out.display(), d.page_count());
    Ok(())
}

fn save_image(bmp: &pdfforge_core::Bitmap, out: &Path, jpg: bool) -> Result<(), String> {
    let img = image::RgbaImage::from_raw(bmp.width, bmp.height, bmp.rgba.clone()).ok_or("bad bitmap")?;
    let res = if jpg {
        // JPEG has no alpha channel.
        image::DynamicImage::ImageRgba8(img).to_rgb8().save_with_format(out, image::ImageFormat::Jpeg)
    } else {
        img.save_with_format(out, image::ImageFormat::Png)
    };
    res.map_err(|e| format!("cannot write {}: {e}", out.display()))
}

/// "-" means: read one line from standard input.
fn read_secret(v: String) -> Result<String, String> {
    if v != "-" {
        return Ok(v);
    }
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).map_err(|e| format!("cannot read password: {e}"))?;
    Ok(line.trim_end_matches(['\r', '\n']).to_string())
}

fn parse_color(s: &str) -> Result<[u8; 3], String> {
    let h = s.trim().trim_start_matches('#');
    let bad = || format!("'{s}' is not a colour like #1a2b3c");
    if h.len() != 6 || !h.is_ascii() {
        return Err(bad());
    }
    let c = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).map_err(|_| bad());
    Ok([c(0)?, c(2)?, c(4)?])
}

fn parse_allow(list: &str) -> Result<Allow, String> {
    let mut a = Allow::NONE;
    for item in list.split(',').map(|s| s.trim().to_ascii_lowercase()).filter(|s| !s.is_empty()) {
        match item.as_str() {
            "none" => {}
            "all" => a = Allow::ALL,
            "print" => a.print = true,
            "copy" => a.copy = true,
            "modify" | "edit" => a.modify = true,
            "annotate" | "comment" => a.annotate = true,
            "forms" | "fill" => a.fill_forms = true,
            "assemble" | "pages" => a.assemble = true,
            other => {
                return Err(format!(
                    "unknown permission '{other}' (print, copy, modify, annotate, forms, assemble, none)"
                ));
            }
        }
    }
    Ok(a)
}

/// Signature box on a `pw` × `ph` pt page: `--rect X,Y,W,H` in points from the top-left, or a
/// 2.5 × 0.75 in box at the bottom right. Images keep their aspect ratio inside the box.
fn sign_rect(spec: Option<&str>, pw: f32, ph: f32, aspect: Option<f32>) -> Result<NormRect, String> {
    let (x, y, mut w, mut h) = match spec {
        Some(s) => {
            let v: Vec<f32> = s
                .split(',')
                .map(|n| n.trim().parse::<f32>())
                .collect::<Result<_, _>>()
                .map_err(|_| format!("--rect '{s}' should be X,Y,W,H in points, e.g. 350,650,180,50"))?;
            if v.len() != 4 || v[2] <= 0.0 || v[3] <= 0.0 {
                return Err(format!("--rect '{s}' should be X,Y,W,H with positive width and height"));
            }
            (v[0], v[1], v[2], v[3])
        }
        None => (pw - 54.0 - 180.0, ph - 54.0 - 54.0, 180.0, 54.0),
    };
    if let Some(a) = aspect {
        // Fit the image inside the box.
        if h / w > a { h = w * a } else { w = h / a }
    }
    if x < 0.0 || y < 0.0 || x + w > pw + 0.5 || y + h > ph + 0.5 {
        return Err(format!("signature box {x},{y},{w},{h} is outside the {pw:.0} x {ph:.0} pt page"));
    }
    Ok(NormRect { x0: x / pw, y0: y / ph, x1: (x + w) / pw, y1: (y + h) / ph })
}

fn human(n: usize) -> String {
    match n {
        n if n >= 1 << 20 => format!("{:.1} MB", n as f64 / (1 << 20) as f64),
        n if n >= 1 << 10 => format!("{:.0} KB", n as f64 / 1024.0),
        n => format!("{n} B"),
    }
}

/// Common paper names for `info`, matched within 3 pt in either orientation.
fn paper_name(w: f32, h: f32) -> String {
    const SIZES: &[(&str, f32, f32)] = &[
        ("US Letter", 612.0, 792.0),
        ("US Legal", 612.0, 1008.0),
        ("Tabloid", 792.0, 1224.0),
        ("A3", 841.9, 1190.6),
        ("A4", 595.3, 841.9),
        ("A5", 419.5, 595.3),
    ];
    let (short, long) = (w.min(h), w.max(h));
    SIZES
        .iter()
        .find(|(_, a, b)| (short - a).abs() < 3.0 && (long - b).abs() < 3.0)
        .map(|(name, _, _)| if w > h { format!("{name} landscape") } else { name.to_string() })
        .unwrap_or_else(|| "custom".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<OsString> {
        v.iter().map(OsString::from).collect()
    }

    #[test]
    fn parses_options() {
        let a = parse_args(args(&["rotate", "x.pdf", "1-2", "-90", "-o", "y.pdf", "--in-place"])).unwrap();
        assert_eq!(a.pos, args(&["rotate", "x.pdf", "1-2", "-90"]));
        assert_eq!(a.output, Some(PathBuf::from("y.pdf")));
        assert!(a.in_place);
        assert!(parse_args(args(&["info", "--bogus"])).is_err());
        assert!(parse_args(args(&["render", "x.pdf", "--dpi", "5"])).is_err());
        assert!(parse_args(args(&["text", "-p"])).is_err());
        // "-4" is a page range, not an option.
        assert_eq!(parse_args(args(&["extract", "x.pdf", "-4"])).unwrap().pos.len(), 3);
    }

    #[test]
    fn parses_sign_and_security_options() {
        assert_eq!(parse_color("#ff8000").unwrap(), [255, 128, 0]);
        assert!(parse_color("red").is_err());
        let a = parse_allow("print, forms").unwrap();
        assert!(a.print && a.fill_forms && !a.copy && !a.modify);
        assert_eq!(parse_allow("none").unwrap(), Allow::NONE);
        assert!(parse_allow("fly").is_err());
        let r = sign_rect(None, 612.0, 792.0, None).unwrap();
        assert!(r.x1 < 1.0 && r.y1 < 1.0 && r.x0 > 0.5 && r.y0 > 0.8, "{r:?}");
        // A 2:1 image in a 200x50 box becomes 100x50.
        let r = sign_rect(Some("0,0,200,50"), 612.0, 792.0, Some(0.5)).unwrap();
        assert!(((r.x1 - r.x0) * 612.0 - 100.0).abs() < 0.01);
        assert!(sign_rect(Some("600,0,100,50"), 612.0, 792.0, None).is_err());
        assert!(sign_rect(Some("1,2,3"), 612.0, 792.0, None).is_err());
        assert_eq!(human(1536), "2 KB");
    }

    #[test]
    fn end_to_end_forms_and_security() {
        let dir = std::env::temp_dir().join(format!("pdfforge-cli-forms-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = |s: &str| dir.join(s).to_string_lossy().into_owned();
        let fixture = concat!(env!("CARGO_MANIFEST_DIR"), "/../pdfforge-core/tests/fixtures/form.pdf");
        std::fs::copy(fixture, dir.join("f.pdf")).unwrap();

        run(args(&["fill", &p("f.pdf"), "name=Ada", "subscribe=yes", "plan=pro", "-o", &p("filled.pdf")])).unwrap();
        let fs = Document::open(dir.join("filled.pdf"), None).unwrap().form_fields().unwrap();
        assert_eq!(fs[0].value, "Ada");
        assert!(fs[2].checked);
        assert!(run(args(&["fill", &p("f.pdf"), "plan=gold"])).is_err());
        assert!(run(args(&["fill", &p("f.pdf"), "noequals"])).is_err());

        run(args(&["sign", &p("filled.pdf"), "--text", "Ada L", "-p", "1", "-o", &p("signed.pdf")])).unwrap();
        assert!(Document::open(dir.join("signed.pdf"), None).unwrap().text(0).unwrap().contains("Ada L"));
        assert!(run(args(&["sign", &p("filled.pdf")])).is_err(), "needs --text or --image");

        run(args(&["protect", &p("signed.pdf"), "--user-password", "pw", "--allow", "print", "-o", &p("lock.pdf")]))
            .unwrap();
        assert!(Document::open(dir.join("lock.pdf"), None).is_err());
        run(args(&["unprotect", &p("lock.pdf"), "--password", "pw", "-o", &p("open.pdf")])).unwrap();
        assert!(!Document::open(dir.join("open.pdf"), None).unwrap().security().unwrap().encrypted);

        run(args(&["compress", &p("open.pdf"), "--preset", "lossless", "-o", &p("small.pdf")])).unwrap();
        assert!(
            std::fs::metadata(dir.join("small.pdf")).unwrap().len()
                <= std::fs::metadata(dir.join("open.pdf")).unwrap().len()
        );
        assert!(run(args(&["compress", &p("open.pdf"), "--preset", "tiny"])).is_err());

        run(args(&["flatten", &p("filled.pdf"), "-o", &p("flat.pdf")])).unwrap();
        assert!(Document::open(dir.join("flat.pdf"), None).unwrap().form_fields().unwrap().is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn names_paper() {
        assert_eq!(paper_name(612.0, 792.0), "US Letter");
        assert_eq!(paper_name(841.9, 595.3), "A4 landscape");
        assert_eq!(paper_name(100.0, 100.0), "custom");
    }

    #[test]
    fn end_to_end_edit() {
        let dir = std::env::temp_dir().join(format!("pdfforge-cli-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let input = dir.join("in.pdf");
        pdfforge_core::sample_document(&["one", "two", "three"]).unwrap().save(&input).unwrap();
        let p = |s: &str| dir.join(s).to_string_lossy().into_owned();
        let inp = p("in.pdf");

        run(args(&["delete", &inp, "2"])).unwrap();
        assert_eq!(Document::open(dir.join("in-delete.pdf"), None).unwrap().page_count(), 2);

        run(args(&["merge", &inp, &inp, "-o", &p("m.pdf")])).unwrap();
        assert_eq!(Document::open(dir.join("m.pdf"), None).unwrap().page_count(), 6);

        run(args(&["split", &p("m.pdf"), "--every", "4", "-o", &p("parts")])).unwrap();
        assert!(dir.join("parts/m-part1.pdf").exists() && dir.join("parts/m-part2.pdf").exists());

        run(args(&["reorder", &inp, "last-1", "--in-place"])).unwrap();
        let d = Document::open(&input, None).unwrap();
        assert_eq!(d.text(0).unwrap().trim(), "three");

        run(args(&["render", &inp, "-p", "1", "--dpi", "36", "-o", &p("img")])).unwrap();
        let img = image::open(dir.join("img/in-1.png")).unwrap();
        assert_eq!((img.width(), img.height()), (306, 396));

        assert!(run(args(&["delete", &inp, "1-3"])).is_err());
        assert!(run(args(&["rotate", &inp, "1", "45"])).is_err());
        assert!(run(args(&["frobnicate", &inp])).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
