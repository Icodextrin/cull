//! Pure geometry helpers, all in physical pixels.

use crate::render::Rect;

/// Scale an image to fit the screen, centred.
pub fn fit_rect(img_w: f32, img_h: f32, screen_w: f32, screen_h: f32) -> Rect {
    let scale = (screen_w / img_w).min(screen_h / img_h);
    let (w, h) = (img_w * scale, img_h * scale);
    Rect::new(((screen_w - w) / 2.0).round(), ((screen_h - h) / 2.0).round(), w.round(), h.round())
}

/// Place an image at 1:1 with the normalised point `(u, v)` at the screen centre, clamped so the
/// image never scrolls past its edges (and stays centred on axes where it is smaller than the
/// screen). Returns the destination rect and the clamped centre.
pub fn peek_rect(img_w: f32, img_h: f32, screen_w: f32, screen_h: f32, u: f32, v: f32) -> (Rect, (f32, f32)) {
    fn axis(img: f32, screen: f32, c: f32) -> (f32, f32) {
        if img <= screen {
            return (((screen - img) / 2.0).round(), 0.5);
        }
        let offset = (screen / 2.0 - c * img).clamp(screen - img, 0.0).round();
        (offset, (screen / 2.0 - offset) / img)
    }
    let (x, u) = axis(img_w, screen_w, u);
    let (y, v) = axis(img_h, screen_h, v);
    (Rect::new(x, y, img_w, img_h), (u, v))
}

/// Thumbnail grid for the review screen.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Grid {
    pub cols: usize,
    pub cell: f32,
    pub gap: f32,
    /// Y of the first row when not scrolled (below the header).
    pub top: f32,
    pub screen_h: f32,
}

impl Grid {
    pub fn new(screen_w: f32, screen_h: f32, top: f32, target_cell: f32, gap: f32) -> Grid {
        let cols = (((screen_w - gap) / (target_cell + gap)).floor() as usize).max(1);
        let cell = ((screen_w - gap * (cols as f32 + 1.0)) / cols as f32).max(1.0);
        Grid { cols, cell, gap, top, screen_h }
    }

    pub fn cell_rect(&self, i: usize, scroll: f32) -> Rect {
        let (row, col) = (i / self.cols, i % self.cols);
        let pitch = self.cell + self.gap;
        Rect::new(self.gap + col as f32 * pitch, self.top + self.gap + row as f32 * pitch - scroll, self.cell, self.cell)
    }

    pub fn max_scroll(&self, n: usize) -> f32 {
        let rows = n.div_ceil(self.cols) as f32;
        let content = self.gap + rows * (self.cell + self.gap);
        (content - (self.screen_h - self.top)).max(0.0)
    }

    pub fn hit(&self, n: usize, x: f32, y: f32, scroll: f32) -> Option<usize> {
        if y < self.top {
            return None;
        }
        let pitch = self.cell + self.gap;
        let col = ((x - self.gap) / pitch).floor();
        let row = ((y - self.top - self.gap + scroll) / pitch).floor();
        if col < 0.0 || row < 0.0 || col as usize >= self.cols {
            return None;
        }
        let i = row as usize * self.cols + col as usize;
        (i < n && self.cell_rect(i, scroll).contains(x, y)).then_some(i)
    }

    /// Adjust `scroll` so that item `i` is fully visible.
    pub fn scroll_to(&self, i: usize, scroll: f32, n: usize) -> f32 {
        let r = self.cell_rect(i, scroll);
        let mut s = scroll;
        if r.y < self.top + self.gap {
            s -= self.top + self.gap - r.y;
        } else if r.y + r.h > self.screen_h - self.gap {
            s += r.y + r.h - (self.screen_h - self.gap);
        }
        s.clamp(0.0, self.max_scroll(n))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_letterboxes() {
        assert_eq!(fit_rect(6000.0, 4000.0, 3000.0, 3000.0), Rect::new(0.0, 500.0, 3000.0, 2000.0));
        assert_eq!(fit_rect(1000.0, 2000.0, 2000.0, 1000.0), Rect::new(750.0, 0.0, 500.0, 1000.0));
    }

    #[test]
    fn peek_centres_and_clamps() {
        let (r, c) = peek_rect(6000.0, 4000.0, 1000.0, 1000.0, 0.5, 0.5);
        assert_eq!(r, Rect::new(-2500.0, -1500.0, 6000.0, 4000.0));
        assert_eq!(c, (0.5, 0.5));
        // Panning past the left/top edge clamps to the edge.
        let (r, c) = peek_rect(6000.0, 4000.0, 1000.0, 1000.0, -1.0, -1.0);
        assert_eq!((r.x, r.y), (0.0, 0.0));
        assert!((c.0 - 500.0 / 6000.0).abs() < 1e-6);
        // Past the right/bottom edge.
        let (r, _) = peek_rect(6000.0, 4000.0, 1000.0, 1000.0, 2.0, 2.0);
        assert_eq!((r.x, r.y), (-5000.0, -3000.0));
        // Smaller than the screen: centred.
        let (r, c) = peek_rect(500.0, 400.0, 1000.0, 1000.0, 0.9, 0.1);
        assert_eq!((r.x, r.y, c), (250.0, 300.0, (0.5, 0.5)));
    }

    #[test]
    fn grid_layout_and_hits() {
        let g = Grid::new(1000.0, 800.0, 100.0, 200.0, 10.0);
        assert_eq!(g.cols, 4);
        assert!((g.cell - 237.5).abs() < 1e-4);
        let r = g.cell_rect(5, 0.0);
        assert_eq!(g.hit(10, r.x + 1.0, r.y + 1.0, 0.0), Some(5));
        assert_eq!(g.hit(10, 5.0, r.y + 1.0, 0.0), None); // in the gap
        assert_eq!(g.hit(10, r.x + 1.0, 50.0, 0.0), None); // on the header
        assert_eq!(g.hit(5, r.x + 1.0, r.y + 1.0, 0.0), None); // past the end
        // 3 rows of 247.5 + 10 gap = 752.5 content vs 700 visible.
        assert!((g.max_scroll(12) - 52.5).abs() < 1e-4);
        assert_eq!(g.max_scroll(4), 0.0);
        assert!((g.scroll_to(11, 0.0, 12) - 52.5).abs() < 1e-4);
        assert_eq!(g.scroll_to(0, 52.5, 12), 0.0);
    }
}
