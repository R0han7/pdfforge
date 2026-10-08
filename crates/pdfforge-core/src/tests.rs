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
