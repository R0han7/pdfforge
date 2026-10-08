use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use pdfforge_core::{Document, Rotation, format_page_ranges, parse_page_ranges};

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

PAGES (one-based): 1-3,5  8-  -4  last  odd  even  all  (default: all)

Options:
  -o, --output <path>   Output file (or directory for render/split)
      --in-place        Overwrite the input file (edit commands)
      --password <pw>   Password for encrypted PDFs
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
        other => return Err(format!("unknown command '{other}' (see --help)")),
    }
    Ok(())
}

fn open(a: &Args, i: usize) -> Result<Document, String> {
    let path = Path::new(&a.pos[i]);
    Document::open(path, a.password.as_deref()).map_err(|e| match e {
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
