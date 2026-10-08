//! Pure helpers for the viewer: page layout, zoom steps, page selection and reordering.
//! Kept free of egui and PDFium so they can be unit-tested directly.

/// egui points per PDF point at 100 % zoom (PDF is 72 dpi, screens are nominally 96 dpi), so
/// 100 % shows a page at its physical size.
pub const PT_TO_UI: f32 = 96.0 / 72.0;
/// Vertical gap between pages and margin around them, in egui points.
pub const GAP: f32 = 16.0;

pub const ZOOM_STEPS: &[f32] =
    &[0.25, 0.33, 0.5, 0.67, 0.75, 0.9, 1.0, 1.1, 1.25, 1.5, 1.75, 2.0, 2.5, 3.0, 4.0, 6.0, 8.0];
pub const MIN_ZOOM: f32 = 0.1;
pub const MAX_ZOOM: f32 = 8.0;

pub fn zoom_in(z: f32) -> f32 {
    ZOOM_STEPS.iter().copied().find(|&s| s > z + 0.001).unwrap_or(MAX_ZOOM)
}

pub fn zoom_out(z: f32) -> f32 {
    ZOOM_STEPS.iter().rev().copied().find(|&s| s < z - 0.001).unwrap_or(MIN_ZOOM)
}

/// Vertical layout of every page in a continuous scroll.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Layout {
    /// Top of each page relative to the top of the scroll content.
    pub tops: Vec<f32>,
    /// Displayed size of each page in egui points.
    pub sizes: Vec<(f32, f32)>,
    pub total_height: f32,
    pub max_width: f32,
}

impl Layout {
    /// `page_sizes` are in PDF points (rotation already applied).
    pub fn new(page_sizes: &[(f32, f32)], zoom: f32) -> Self {
        let mut y = GAP;
        let mut l = Layout::default();
        for &(w, h) in page_sizes {
            let s = (w * zoom * PT_TO_UI, h * zoom * PT_TO_UI);
            l.tops.push(y);
            l.sizes.push(s);
            l.max_width = l.max_width.max(s.0);
            y += s.1 + GAP;
        }
        l.total_height = y;
        l
    }

    /// Pages intersecting the vertical range `[y0, y1]` (content coordinates).
    pub fn visible(&self, y0: f32, y1: f32) -> std::ops::Range<usize> {
        let first = self.tops.partition_point(|&t| t < y0).saturating_sub(1);
        let mut first = first;
        // The page before `first` may still reach into view.
        while first < self.tops.len() && self.tops[first] + self.sizes[first].1 < y0 {
            first += 1;
        }
        let last = self.tops.partition_point(|&t| t <= y1);
        first..last.max(first)
    }

    /// The page considered "current" for a viewport: the one covering the line a third of the
    /// way down the view, or the nearest one above it.
    pub fn current(&self, y0: f32, y1: f32) -> usize {
        let probe = y0 + (y1 - y0) / 3.0;
        self.tops.partition_point(|&t| t <= probe).saturating_sub(1)
    }

    /// Zoom that fits the widest page into `available_width`.
    pub fn fit_width_zoom(page_sizes: &[(f32, f32)], available_width: f32) -> f32 {
        let widest = page_sizes.iter().map(|s| s.0).fold(0.0_f32, f32::max).max(1.0);
        ((available_width - 2.0 * GAP) / (widest * PT_TO_UI)).clamp(MIN_ZOOM, MAX_ZOOM)
    }

    /// Zoom that fits the whole of page `page` into the viewport.
    pub fn fit_page_zoom(page_sizes: &[(f32, f32)], page: usize, w: f32, h: f32) -> f32 {
        let Some(&(pw, ph)) = page_sizes.get(page) else { return 1.0 };
        let zw = (w - 2.0 * GAP) / (pw.max(1.0) * PT_TO_UI);
        let zh = (h - 2.0 * GAP) / (ph.max(1.0) * PT_TO_UI);
        zw.min(zh).clamp(MIN_ZOOM, MAX_ZOOM)
    }
}

/// A set of selected pages with an anchor for shift-click range selection.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Selection {
    pages: Vec<bool>,
    anchor: Option<usize>,
}

impl Selection {
    pub fn new(count: usize) -> Self {
        Self { pages: vec![false; count], anchor: None }
    }

    pub fn is_selected(&self, i: usize) -> bool {
        self.pages.get(i).copied().unwrap_or(false)
    }

    /// Selected pages in ascending order.
    pub fn pages(&self) -> Vec<usize> {
        self.pages.iter().enumerate().filter(|(_, s)| **s).map(|(i, _)| i).collect()
    }

    pub fn clear(&mut self) {
        self.pages.iter_mut().for_each(|s| *s = false);
        self.anchor = None;
    }

    pub fn select_all(&mut self) {
        self.pages.iter_mut().for_each(|s| *s = true);
    }

    pub fn set(&mut self, pages: &[usize]) {
        self.clear();
        for &p in pages {
            if let Some(s) = self.pages.get_mut(p) {
                *s = true;
            }
        }
        self.anchor = pages.first().copied();
    }

    /// Apply a click: plain click selects only `i`, ctrl toggles, shift extends from the anchor.
    pub fn click(&mut self, i: usize, ctrl: bool, shift: bool) {
        if i >= self.pages.len() {
            return;
        }
        match (shift, self.anchor) {
            (true, Some(a)) => {
                if !ctrl {
                    self.pages.iter_mut().for_each(|s| *s = false);
                }
                for p in a.min(i)..=a.max(i) {
                    self.pages[p] = true;
                }
            }
            _ if ctrl => {
                self.pages[i] = !self.pages[i];
                self.anchor = Some(i);
            }
            _ => {
                self.pages.iter_mut().for_each(|s| *s = false);
                self.pages[i] = true;
                self.anchor = Some(i);
            }
        }
    }
}

/// New page order after moving the `selected` pages one step (`up = true` → towards page 1).
/// Returns `None` when nothing can move (empty selection or already at the edge).
pub fn moved_order(count: usize, selected: &[usize], up: bool) -> Option<Vec<usize>> {
    let mut order: Vec<usize> = (0..count).collect();
    let mut sel = vec![false; count];
    for &s in selected {
        if s < count {
            sel[s] = true;
        }
    }
    let mut moved = false;
    if up {
        for i in 1..count {
            if sel[i] && !sel[i - 1] {
                order.swap(i, i - 1);
                sel.swap(i, i - 1);
                moved = true;
            }
        }
    } else {
        for i in (0..count.saturating_sub(1)).rev() {
            if sel[i] && !sel[i + 1] {
                order.swap(i, i + 1);
                sel.swap(i, i + 1);
                moved = true;
            }
        }
    }
    moved.then_some(order)
}

/// Where the moved pages end up, given an order produced by [`moved_order`].
pub fn positions_of(order: &[usize], pages: &[usize]) -> Vec<usize> {
    let mut v: Vec<usize> = order.iter().enumerate().filter(|(_, p)| pages.contains(p)).map(|(i, _)| i).collect();
    v.sort_unstable();
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zoom_steps() {
        assert_eq!(zoom_in(1.0), 1.1);
        assert_eq!(zoom_out(1.0), 0.9);
        assert_eq!(zoom_in(1.03), 1.1);
        assert_eq!(zoom_out(1.03), 1.0);
        assert_eq!(zoom_in(8.0), MAX_ZOOM);
        assert_eq!(zoom_out(0.25), MIN_ZOOM);
    }

    #[test]
    fn layout_and_visibility() {
        let l = Layout::new(&[(72.0, 72.0), (72.0, 144.0), (144.0, 72.0)], 1.0);
        let u = PT_TO_UI * 72.0; // 96
        assert_eq!(l.tops, vec![GAP, 2.0 * GAP + u, 3.0 * GAP + 3.0 * u]);
        assert_eq!(l.max_width, 2.0 * u);
        assert_eq!(l.total_height, 4.0 * GAP + 4.0 * u);
        assert!(l.visible(0.0, 10.0).is_empty()); // top margin only
        assert_eq!(l.visible(0.0, GAP + 1.0), 0..1);
        assert_eq!(l.visible(0.0, l.total_height), 0..3);
        assert_eq!(l.visible(GAP + u + 1.0, GAP + u + 5.0), 1..1); // in the gap
        assert_eq!(l.visible(2.0 * GAP + u + 1.0, 2.0 * GAP + u + 2.0), 1..2);
        assert_eq!(l.current(0.0, 30.0), 0);
        assert_eq!(l.current(l.tops[2], l.tops[2] + 300.0), 2);
        assert!(Layout::new(&[], 1.0).visible(0.0, 100.0).is_empty());
    }

    #[test]
    fn fit_zoom() {
        let z = Layout::fit_width_zoom(&[(612.0, 792.0)], 612.0 * PT_TO_UI + 2.0 * GAP);
        assert!((z - 1.0).abs() < 1e-4);
        let z = Layout::fit_page_zoom(&[(612.0, 792.0)], 0, 10_000.0, 792.0 * PT_TO_UI / 2.0 + 2.0 * GAP);
        assert!((z - 0.5).abs() < 1e-4);
    }

    #[test]
    fn selection_clicks() {
        let mut s = Selection::new(6);
        s.click(1, false, false);
        s.click(4, false, true);
        assert_eq!(s.pages(), [1, 2, 3, 4]);
        s.click(2, true, false);
        assert_eq!(s.pages(), [1, 3, 4]);
        s.click(5, false, false);
        assert_eq!(s.pages(), [5]);
        s.click(99, false, false);
        assert_eq!(s.pages(), [5]);
        s.select_all();
        assert_eq!(s.pages().len(), 6);
        s.clear();
        assert!(s.pages().is_empty());
    }

    #[test]
    fn moving_pages() {
        assert_eq!(moved_order(5, &[2], true), Some(vec![0, 2, 1, 3, 4]));
        assert_eq!(moved_order(5, &[2, 3], false), Some(vec![0, 1, 4, 2, 3]));
        assert_eq!(moved_order(5, &[0, 2], true), Some(vec![0, 2, 1, 3, 4])); // page 1 stays
        assert_eq!(moved_order(5, &[0, 1], true), None);
        assert_eq!(moved_order(5, &[4], false), None);
        assert_eq!(moved_order(5, &[], true), None);
        let o = moved_order(5, &[2, 3], false).unwrap();
        assert_eq!(positions_of(&o, &[2, 3]), [3, 4]);
    }
}
