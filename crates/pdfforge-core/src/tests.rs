use super::*;

/// Page labels: each sample page contains "Page N" so we can check order after edits.
fn doc(n: usize) -> Document {
    let texts: Vec<String> = (1..=n).map(|i| format!("Page {i}\nshared word apple")).collect();
    let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
    sample_document(&refs).unwrap()
}

fn labels(d: &Document) -> Vec<String> {
    (0..d.page_count()).map(|i| d.text(i).unwrap().lines().next().unwrap_or("").trim().to_string()).collect()
}

/// Round-trip through bytes so we test what is actually written to disk.
fn reload(d: &Document) -> Document {
    Document::from_bytes(d.to_bytes().unwrap(), None).unwrap()
}

#[test]
fn creates_and_reads_text() {
    let d = reload(&doc(3));
    assert_eq!(d.page_count(), 3);
    assert_eq!(labels(&d), ["Page 1", "Page 2", "Page 3"]);
    let (w, h) = d.page_size(0).unwrap();
    assert!((w - 612.0).abs() < 1.0 && (h - 792.0).abs() < 1.0, "US Letter, got {w}x{h}");
    assert_eq!(d.info().pages, 3);
}

#[test]
fn renders_pages() {
    let d = doc(1);
    let b = d.render(0, 1.0).unwrap();
    assert_eq!((b.width, b.height), (612, 792));
    assert_eq!(b.rgba.len(), 612 * 792 * 4);
    // Some pixels are dark (the text), most are white.
    let dark = b.rgba.chunks(4).filter(|p| p[0] < 100).count();
    assert!(dark > 50 && dark < 612 * 792 / 10, "dark pixels: {dark}");
    let t = d.render_thumbnail(0, 200).unwrap();
    assert_eq!(t.height, 200);
}

#[test]
fn rotates_pages() {
    let mut d = doc(2);
    d.rotate_pages(&[1], Rotation::R90).unwrap();
    let d = reload(&d);
    assert_eq!(d.rotation(0).unwrap(), Rotation::R0);
    assert_eq!(d.rotation(1).unwrap(), Rotation::R90);
    // PDFium may order lines differently on a rotated page, but none of the text is lost.
    let t = d.text(1).unwrap();
    assert!(t.contains("Page 2") && t.contains("shared word apple"), "text survives a 90° rotation: {t:?}");
    assert_eq!(d.search("Page 2", true, false).unwrap().len(), 1);
    let (w, h) = d.page_size(1).unwrap();
    assert!(w > h, "rotated page should be landscape: {w}x{h}");
    let b = d.render(1, 0.5).unwrap();
    assert!(b.width > b.height);
    let mut d = d;
    d.rotate_pages(&[1], Rotation::R270).unwrap();
    assert_eq!(d.rotation(1).unwrap(), Rotation::R0);
}

#[test]
fn deletes_pages() {
    let mut d = doc(5);
    d.delete_pages(&[3, 0, 3]).unwrap();
    assert_eq!(labels(&reload(&d)), ["Page 2", "Page 3", "Page 5"]);
    assert!(matches!(d.delete_pages(&[0, 1, 2]), Err(Error::InvalidPages(_))));
    assert!(matches!(d.delete_pages(&[9]), Err(Error::InvalidPages(_))));
}

#[test]
fn extracts_and_reorders() {
    let d = doc(4);
    let e = d.extract(&[3, 1, 2]).unwrap();
    assert_eq!(labels(&reload(&e)), ["Page 4", "Page 2", "Page 3"]);
    let mut d = d;
    d.reorder(&[3, 2, 1, 0]).unwrap();
    assert_eq!(labels(&reload(&d)), ["Page 4", "Page 3", "Page 2", "Page 1"]);
    assert!(d.reorder(&[0, 0, 1, 2]).is_err());
    assert!(d.reorder(&[0, 1]).is_err());
}

#[test]
fn merges_splits_and_inserts() {
    let a = doc(2);
    let b = doc(3);
    let m = Document::merge(&[&a, &b]).unwrap();
    assert_eq!(labels(&reload(&m)), ["Page 1", "Page 2", "Page 1", "Page 2", "Page 3"]);

    let parts = m.split_every(2).unwrap();
    assert_eq!(parts.iter().map(Document::page_count).collect::<Vec<_>>(), [2, 2, 1]);

    let groups = m.split_groups(&[vec![0], vec![2, 3, 4]]).unwrap();
    assert_eq!(labels(&groups[1]), ["Page 1", "Page 2", "Page 3"]);

    let mut a = a;
    a.insert_document(&b, 1).unwrap();
    assert_eq!(labels(&a), ["Page 1", "Page 1", "Page 2", "Page 3", "Page 2"]);
    a.insert_blank_page(0).unwrap();
    assert_eq!(a.page_count(), 6);
    assert_eq!(a.text(0).unwrap().trim(), "");
    assert!(a.insert_blank_page(99).is_err());
}

#[test]
fn searches_text() {
    let d = doc(3);
    let hits = d.search("apple", false, false).unwrap();
    assert_eq!(hits.len(), 3);
    assert_eq!(hits.iter().map(|h| h.page).collect::<Vec<_>>(), [0, 1, 2]);
    let r = hits[0].rects[0];
    // Text is at x=72pt (of 612) and y≈680pt from the bottom (of 792) → near the top-left.
    assert!(r.x0 > 0.1 && r.x0 < 0.5 && r.x1 > r.x0, "{r:?}");
    assert!(r.y0 > 0.05 && r.y0 < 0.25 && r.y1 > r.y0, "{r:?}");
    assert!(hits[0].snippet.contains("apple"));
    assert_eq!(d.search("Page 2", true, false).unwrap().len(), 1);
    assert_eq!(d.search("APPLE", true, false).unwrap().len(), 0);
    assert!(d.search("   ", false, false).unwrap().is_empty());
}

#[test]
fn saves_atomically() {
    let dir = std::env::temp_dir().join(format!("pdfforge-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("out.pdf");
    doc(2).save(&path).unwrap();
    let mut d = Document::open(&path, None).unwrap();
    assert_eq!(d.path(), Some(path.as_path()));
    d.delete_pages(&[0]).unwrap();
    d.save(&path).unwrap(); // overwrite the file we have open
    assert_eq!(Document::open(&path, None).unwrap().page_count(), 1);
    assert!(!path.with_extension("pdfforge-tmp").exists());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn rejects_garbage() {
    assert!(Document::from_bytes(b"not a pdf".to_vec(), None).is_err());
    assert!(matches!(Document::open("/nonexistent/x.pdf", None), Err(Error::Io(_))));
}

#[test]
fn snippets_follow_each_match() {
    let text = "héllo wörld ünïcode Apple pie, then much more filler text here, and a second apple tart";
    let flat = flatten_ws(text);
    let mut cur = 0;
    let a = snippet_after(&flat, &mut cur, "Apple", "apple");
    let b = snippet_after(&flat, &mut cur, "apple", "apple");
    assert!(a.contains("Apple pie"), "{a}");
    assert!(b.contains("apple tart"), "{b}");
    assert_eq!(snippet_after(&flat, &mut cur, "apple", "apple"), "apple"); // no more matches
}

// ------------------------------------------------------------------ forms

fn form() -> Document {
    Document::open(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/form.pdf"), None).unwrap()
}

fn field<'a>(fs: &'a [FormField], name: &str) -> &'a FormField {
    fs.iter().find(|f| f.name == name).unwrap_or_else(|| panic!("no field {name}"))
}

/// Darkness inside a normalised rect of a rendered page (0 = white, 255 = black).
fn ink_in(d: &Document, page: usize, r: NormRect) -> f32 {
    let b = d.render(page, 1.0).unwrap();
    let (x0, x1) = ((r.x0 * b.width as f32) as usize, (r.x1 * b.width as f32) as usize);
    let (y0, y1) = ((r.y0 * b.height as f32) as usize, (r.y1 * b.height as f32) as usize);
    let mut sum = 0.0_f32;
    let mut n = 0.0_f32;
    for y in y0..y1 {
        for x in x0..x1 {
            let i = (y * b.width as usize + x) * 4;
            sum += 255.0 - (b.rgba[i] as f32 + b.rgba[i + 1] as f32 + b.rgba[i + 2] as f32) / 3.0;
            n += 1.0;
        }
    }
    sum / n.max(1.0)
}

#[test]
fn lists_form_fields() {
    let fs = form().form_fields().unwrap();
    let names: Vec<&str> = fs.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, ["name", "notes", "subscribe", "plan", "country"]);
    assert!(matches!(field(&fs, "notes").kind, FieldKind::Text { multiline: true, .. }));
    assert_eq!(field(&fs, "subscribe").kind, FieldKind::Checkbox);
    let plan = field(&fs, "plan");
    assert_eq!(plan.kind, FieldKind::Radio);
    assert_eq!(plan.value, "basic");
    let states: Vec<_> = plan.widgets.iter().map(|w| w.on_state.clone().unwrap()).collect();
    assert_eq!(states, ["basic", "pro"]);
    let country = field(&fs, "country");
    assert_eq!(country.options, ["Japan", "India", "France"]);
    assert_eq!(country.value, "Japan");
    // "name" box is at x=150..400, y=710..732 (pt, bottom-up) on a 612x792 page.
    let r = field(&fs, "name").widgets[0].rect;
    assert!((r.x0 - 150.0 / 612.0).abs() < 0.005 && (r.y0 - (792.0 - 732.0) / 792.0).abs() < 0.005, "{r:?}");
    assert!(Document::new_empty().unwrap().form_fields().unwrap().is_empty());
    assert!(form().has_form());
    assert!(!doc(1).has_form());
}

#[test]
fn fills_and_flattens_forms() {
    let mut d = form();
    let fs = d.form_fields().unwrap();
    let name_rect = field(&fs, "name").widgets[0].rect;
    let box_rect = field(&fs, "subscribe").widgets[0].rect;
    let blank_name = ink_in(&d, 0, name_rect);
    let blank_box = ink_in(&d, 0, box_rect);
    d.fill_fields(&[
        ("name".into(), FieldValue::Text("Rohan Example".into())),
        ("notes".into(), FieldValue::Text("Line one that is long enough to wrap onto the next line\nsecond".into())),
        ("subscribe".into(), FieldValue::Checked(true)),
        ("plan".into(), FieldValue::Choice("pro".into())),
        ("country".into(), FieldValue::Choice("India".into())),
    ])
    .unwrap();
    let d = reload(&d);
    let fs = d.form_fields().unwrap();
    assert_eq!(field(&fs, "name").value, "Rohan Example");
    assert!(field(&fs, "subscribe").checked);
    assert_eq!(field(&fs, "plan").value, "pro");
    assert_eq!(field(&fs, "country").value, "India");
    // The new values are actually drawn (appearance streams were generated).
    assert!(ink_in(&d, 0, name_rect) > blank_name + 3.0, "name text visible");
    assert!(ink_in(&d, 0, box_rect) > blank_box + 3.0, "check mark visible");

    let mut flat = d;
    flat.flatten(None).unwrap();
    let flat = reload(&flat);
    assert!(flat.form_fields().unwrap().is_empty(), "no fields after flattening");
    assert!(ink_in(&flat, 0, name_rect) > blank_name + 3.0, "value kept after flattening");
    assert!(flat.text(0).unwrap().contains("Rohan Example"));
}

#[test]
fn rejects_bad_form_values() {
    let mut d = form();
    assert!(d.fill_fields(&[("nope".into(), FieldValue::Text("x".into()))]).is_err());
    assert!(d.fill_fields(&[("plan".into(), FieldValue::Choice("gold".into()))]).is_err());
    assert!(d.fill_fields(&[("country".into(), FieldValue::Choice("Mars".into()))]).is_err());
    assert!(d.fill_fields(&[("subscribe".into(), FieldValue::Text("x".into()))]).is_err());
    // Nothing was changed by the failed attempts.
    assert_eq!(field(&d.form_fields().unwrap(), "plan").value, "basic");
}

// ------------------------------------------------------------------ security

#[test]
fn protects_and_unprotects() {
    let mut d = doc(2);
    assert!(!d.security().unwrap().encrypted);
    let allow = Allow { print: true, copy: false, ..Allow::NONE };
    d.protect(&Protection { user_password: "open sesame".into(), owner_password: "boss".into(), allow }).unwrap();
    assert_eq!(d.password(), Some("open sesame"));
    let bytes = d.to_bytes().unwrap();
    assert!(matches!(Document::from_bytes(bytes.clone(), None), Err(Error::PasswordRequired)));
    assert!(matches!(Document::from_bytes(bytes.clone(), Some("wrong")), Err(Error::PasswordRequired)));
    let opened = Document::from_bytes(bytes.clone(), Some("open sesame")).unwrap();
    assert_eq!(labels(&opened), ["Page 1", "Page 2"]);
    assert!(opened.info().encrypted);
    let s = opened.security().unwrap();
    assert!(s.encrypted && s.method == "AES-256", "{s:?}");
    assert_eq!(s.allow, allow);
    assert!(Document::from_bytes(bytes, Some("boss")).is_ok(), "owner password opens it too");

    // Edits made through PDFium keep the protection.
    let mut d = opened;
    d.rotate_pages(&[0], Rotation::R90).unwrap();
    let again = d.to_bytes().unwrap();
    assert!(Document::from_bytes(again.clone(), None).is_err());
    assert!(Document::from_bytes(again, Some("open sesame")).is_ok());

    d.unprotect().unwrap();
    let plain = reload(&d);
    assert!(!plain.security().unwrap().encrypted);
    assert!(!plain.info().encrypted);
    assert_eq!(plain.rotation(0).unwrap(), Rotation::R90);
}

#[test]
fn owner_only_protection_opens_without_password() {
    let mut d = doc(1);
    d.protect(&Protection { user_password: String::new(), owner_password: "boss".into(), allow: Allow::NONE }).unwrap();
    let d = Document::from_bytes(d.to_bytes().unwrap(), None).unwrap();
    let s = d.security().unwrap();
    assert!(s.encrypted && !s.allow.print && !s.allow.copy);
    assert!(Document::new_empty().unwrap().protect(&Protection::default()).is_err(), "needs some password");
}

#[test]
fn protected_forms_can_be_filled() {
    let mut d = form();
    d.protect(&Protection { user_password: "pw".into(), ..Default::default() }).unwrap();
    d.fill_fields(&[("name".into(), FieldValue::Text("Secret".into()))]).unwrap();
    let bytes = d.to_bytes().unwrap();
    assert!(Document::from_bytes(bytes.clone(), None).is_err(), "still protected after filling");
    let d = Document::from_bytes(bytes, Some("pw")).unwrap();
    assert_eq!(field(&d.form_fields().unwrap(), "name").value, "Secret");
}

// ------------------------------------------------------------------ stamps and compression

/// A noisy (hard to compress) RGB image.
fn photo(w: u32, h: u32) -> image::DynamicImage {
    let mut seed = 12345u32;
    image::DynamicImage::ImageRgb8(image::RgbImage::from_fn(w, h, |x, y| {
        seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
        let n = (seed >> 24) as u8 / 4;
        image::Rgb([(x * 255 / w) as u8 / 2 + n, (y * 255 / h) as u8 / 2 + n, 128 + n])
    }))
}

const SIG: NormRect = NormRect { x0: 0.55, y0: 0.8, x1: 0.9, y1: 0.9 };

#[test]
fn stamps_signatures() {
    let mut d = doc(2);
    let before = ink_in(&d, 0, SIG);
    let ink =
        Ink { strokes: vec![vec![(0.05, 0.8), (0.3, 0.2), (0.6, 0.8), (0.95, 0.3)]], color: [0, 0, 120], width: 2.5 };
    d.stamp_ink(0, SIG, &ink).unwrap();
    d.stamp_text(1, SIG, "Rohan Example", StampFont::TimesItalic, [0, 0, 0]).unwrap();
    let d = reload(&d);
    assert!(ink_in(&d, 0, SIG) > before + 3.0, "ink visible inside the box");
    let outside = NormRect { x0: 0.05, y0: 0.8, x1: 0.45, y1: 0.9 };
    assert!(ink_in(&d, 0, outside) < 1.0, "nothing drawn outside the box");
    assert!(ink_in(&d, 1, SIG) > before + 3.0, "typed signature visible");
    assert!(d.text(1).unwrap().contains("Rohan Example"));
    assert!(doc(1).stamp_ink(0, SIG, &Ink { strokes: vec![], color: [0; 3], width: 1.0 }).is_err());
    assert!(doc(1).stamp_text(5, SIG, "x", StampFont::Times, [0; 3]).is_err());
}

#[test]
fn stamps_land_where_shown_on_rotated_pages() {
    for rot in [Rotation::R90, Rotation::R180, Rotation::R270] {
        let mut d = doc(1);
        d.rotate_pages(&[0], rot).unwrap();
        let img = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(40, 20, image::Rgb([0, 0, 0])));
        d.stamp_image(0, SIG, &img).unwrap();
        let d = reload(&d);
        let inside = NormRect { x0: SIG.x0 + 0.01, y0: SIG.y0 + 0.01, x1: SIG.x1 - 0.01, y1: SIG.y1 - 0.01 };
        assert!(ink_in(&d, 0, inside) > 240.0, "{rot:?}: box filled where it was placed");
        let left = NormRect { x0: 0.0, y0: 0.0, x1: 0.5, y1: 0.75 };
        assert!(ink_in(&d, 0, left) < 30.0, "{rot:?}: not elsewhere");
    }
}

#[test]
fn compresses_images_and_keeps_content() {
    let mut d = doc(2);
    // A 2400x1600 photo shown at 4x2.67 in = 600 dpi.
    d.stamp_image(0, NormRect { x0: 0.1, y0: 0.3, x1: 0.57, y1: 0.64 }, &photo(2400, 1600)).unwrap();
    let mut d = reload(&d);
    let before = d.to_bytes().unwrap().len();
    let r = d.compress(&CompressOptions::SCREEN).unwrap();
    assert_eq!(r.before, before);
    assert_eq!((r.images_total, r.images_recompressed), (1, 1));
    assert!(r.after * 4 < r.before, "{r:?}");
    // Saved bytes are the compressed ones, and the document still reads correctly.
    let bytes = d.to_bytes().unwrap();
    assert_eq!(bytes.len(), r.after);
    let d = Document::from_bytes(bytes, None).unwrap();
    assert_eq!(labels(&d), ["Page 1", "Page 2"]);
    assert!(ink_in(&d, 0, NormRect { x0: 0.15, y0: 0.35, x1: 0.5, y1: 0.6 }) > 50.0, "image still shown");

    // Lossless never makes a file bigger and leaves images alone.
    let mut small = doc(1);
    let n = small.to_bytes().unwrap().len();
    let r = small.compress(&CompressOptions::LOSSLESS).unwrap();
    assert!(r.after <= n && r.images_recompressed == 0);
}

#[test]
fn compresses_protected_documents() {
    let mut d = doc(1);
    d.stamp_image(0, SIG, &photo(1200, 400)).unwrap();
    d.protect(&Protection { user_password: "pw".into(), ..Default::default() }).unwrap();
    let r = d.compress(&CompressOptions::SMALLEST).unwrap();
    assert!(r.after < r.before);
    let bytes = d.to_bytes().unwrap();
    assert!(Document::from_bytes(bytes.clone(), None).is_err(), "still protected");
    assert_eq!(labels(&Document::from_bytes(bytes, Some("pw")).unwrap()), ["Page 1"]);
}
