//! Application state machine: browse, focus peek and review screens.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use winit::application::ApplicationHandler;
use winit::event::{ElementState, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow};
use winit::keyboard::{Key, ModifiersState};
use winit::window::{Fullscreen, Window, WindowId};

use crate::catalog::Catalog;
use crate::delete;
use crate::layout::{fit_rect, peek_rect, Grid};
use crate::loader::{JobKind, Loaded, Loader, Payload, Rgba};
use crate::meta::Meta;
use crate::render::{srgb, Align, Frame, Gpu, Rect, Texture};
use crate::state::{self, State};

/// Screen-fit images kept around the cursor, in the direction of travel / behind it.
const AHEAD: usize = 3;
const BEHIND: usize = 1;

const WHITE: [u8; 4] = [235, 235, 235, 255];
const MUTED: [u8; 4] = [170, 170, 170, 255];
const RED: [u8; 4] = [255, 80, 70, 255];
const GREEN: [u8; 4] = [110, 220, 120, 255];
const AMBER: [u8; 4] = [255, 190, 60, 255];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mark {
    None,
    Delete,
    Edit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Browse,
    Peek,
    Review,
}

struct Cached {
    tex: Texture,
    thumb: Option<Rgba>,
    full_w: u32,
    full_h: u32,
}

#[derive(Default)]
struct Review {
    /// Shot indices that were marked (either way) when the review screen opened.
    items: Vec<usize>,
    /// Items the user chose to keep after all.
    keep: Vec<bool>,
    sel: usize,
    scroll: f32,
}

pub struct App {
    dir: PathBuf,
    catalog: Catalog,
    marks: Vec<Mark>,
    cursor: usize,
    forward: bool,
    mode: Mode,
    windowed: bool,

    loader: Loader,
    gpu: Option<Gpu>,
    cache: HashMap<usize, Cached>,
    errors: HashMap<usize, String>,
    pending: HashSet<(usize, JobKind)>,
    full: Option<(usize, Texture)>,
    thumbs: HashMap<usize, Texture>,
    /// EXIF metadata; tiny, so kept for every shot that has been decoded.
    meta: HashMap<usize, Meta>,

    peek_center: (f32, f32),
    review: Review,
    modifiers: ModifiersState,
    mouse: (f32, f32),
    dragging: bool,
    show_help: bool,
    show_meta: bool,

    /// Message printed to the terminal after the window closes.
    pub exit_message: Option<String>,
}

impl App {
    pub fn new(dir: PathBuf, catalog: Catalog, windowed: bool, loader: Loader) -> App {
        let saved = state::load(&dir);
        let marks = catalog
            .shots
            .iter()
            .map(|s| {
                if saved.marked.contains(&s.stem) {
                    Mark::Delete
                } else if saved.edit.contains(&s.stem) {
                    Mark::Edit
                } else {
                    Mark::None
                }
            })
            .collect();
        let cursor = saved
            .cursor
            .and_then(|stem| catalog.shots.iter().position(|s| s.stem == stem))
            .unwrap_or(0);
        App {
            dir,
            catalog,
            marks,
            cursor,
            forward: true,
            mode: Mode::Browse,
            windowed,
            loader,
            gpu: None,
            cache: HashMap::new(),
            errors: HashMap::new(),
            pending: HashSet::new(),
            full: None,
            thumbs: HashMap::new(),
            meta: HashMap::new(),
            peek_center: (0.5, 0.5),
            review: Review::default(),
            modifiers: ModifiersState::empty(),
            mouse: (0.0, 0.0),
            dragging: false,
            show_help: false,
            show_meta: false,
            exit_message: None,
        }
    }

    fn n(&self) -> usize {
        self.catalog.shots.len()
    }

    fn count(&self, mark: Mark) -> usize {
        self.marks.iter().filter(|&&m| m == mark).count()
    }

    fn stems(&self, mark: Mark) -> std::collections::BTreeSet<String> {
        self.catalog.shots.iter().zip(&self.marks).filter(|(_, m)| **m == mark).map(|(s, _)| s.stem.clone()).collect()
    }

    fn save(&self) {
        let st = State {
            version: 1,
            marked: self.stems(Mark::Delete),
            edit: self.stems(Mark::Edit),
            cursor: Some(self.catalog.shots[self.cursor].stem.clone()),
        };
        if let Err(e) = state::save(&self.dir, &st) {
            eprintln!("cull: could not save state: {e}");
        }
    }

    fn redraw(&self) {
        if let Some(gpu) = &self.gpu {
            gpu.window.request_redraw();
        }
    }

    fn scale(&self) -> f32 {
        self.gpu.as_ref().map_or(1.0, |g| g.window.scale_factor() as f32)
    }

    fn screen(&self) -> (f32, f32) {
        self.gpu.as_ref().map_or((1.0, 1.0), |g| {
            let (w, h) = g.size();
            (w as f32, h as f32)
        })
    }

    /// Which cached indices are worth keeping around the cursor.
    fn in_window(&self, idx: usize) -> bool {
        let (ahead, behind) = if self.forward { (AHEAD, BEHIND) } else { (BEHIND, AHEAD) };
        idx + behind >= self.cursor && idx <= self.cursor + ahead
    }

    /// Evict what's out of range and request what's missing, most important first.
    fn schedule(&mut self) {
        let peeking = self.mode == Mode::Peek;
        self.loader.set_current(self.cursor, peeking);
        let keep: Vec<usize> = self.cache.keys().copied().filter(|&i| self.in_window(i)).collect();
        self.cache.retain(|i, _| keep.contains(i));
        if self.full.as_ref().is_some_and(|(i, _)| *i != self.cursor) {
            self.full = None;
        }

        let mut order = vec![self.cursor];
        let (ahead, behind) = if self.forward { (AHEAD, BEHIND) } else { (BEHIND, AHEAD) };
        for d in 1..=ahead.max(behind) {
            let fwd = (d <= ahead).then(|| self.cursor + d).filter(|&i| i < self.n());
            let back = (d <= behind).then(|| self.cursor.checked_sub(d)).flatten();
            let (first, second) = if self.forward { (fwd, back) } else { (back, fwd) };
            order.extend(first);
            order.extend(second);
        }
        if peeking && self.full.is_none() && !self.errors.contains_key(&self.cursor) {
            self.request(self.cursor, JobKind::Full, true);
        }
        for (rank, idx) in order.into_iter().enumerate() {
            if !self.cache.contains_key(&idx) && !self.errors.contains_key(&idx) {
                self.request(idx, JobKind::Fit, rank == 0);
            }
        }
        if self.mode == Mode::Review {
            for i in 0..self.review.items.len() {
                let idx = self.review.items[i];
                if !self.thumbs.contains_key(&idx) && !self.errors.contains_key(&idx) {
                    self.request(idx, JobKind::Thumb, true);
                }
            }
        }
    }

    fn request(&mut self, idx: usize, kind: JobKind, urgent: bool) {
        if self.pending.insert((idx, kind)) {
            self.loader.request(idx, kind, urgent);
        }
    }

    fn receive(&mut self, loaded: Loaded) {
        self.pending.remove(&(loaded.idx, loaded.kind));
        let idx = loaded.idx;
        let payload = match loaded.result {
            Ok(p) => p,
            Err(e) => {
                self.errors.insert(idx, e);
                return;
            }
        };
        let Some(gpu) = &self.gpu else { return };
        match payload {
            Payload::Fit { fit, thumb, full_width, full_height, meta } => {
                self.meta.insert(idx, meta);
                if self.marks[idx] != Mark::None && !self.thumbs.contains_key(&idx) {
                    self.thumbs.insert(idx, gpu.upload(&thumb));
                }
                if self.in_window(idx) {
                    let tex = gpu.upload(&fit);
                    self.cache.insert(idx, Cached { tex, thumb: Some(thumb), full_w: full_width, full_h: full_height });
                }
            }
            Payload::Full(img) => {
                if idx == self.cursor && self.mode == Mode::Peek {
                    self.full = Some((idx, gpu.upload(&img)));
                }
            }
            Payload::Thumb(img) => {
                if self.marks[idx] != Mark::None {
                    self.thumbs.insert(idx, gpu.upload(&img));
                }
            }
            Payload::Skipped => {}
        }
    }

    fn go_to(&mut self, idx: usize) {
        let idx = idx.min(self.n() - 1);
        if idx != self.cursor {
            self.forward = idx > self.cursor;
            self.cursor = idx;
            self.schedule();
        }
    }

    fn step(&mut self, delta: isize) {
        let target = (self.cursor as isize + delta).clamp(0, self.n() as isize - 1) as usize;
        self.go_to(target);
    }

    /// Set the current shot's mark, or clear it if it already has that mark.
    fn toggle_mark(&mut self, mark: Mark) {
        let idx = self.cursor;
        self.marks[idx] = if self.marks[idx] == mark { Mark::None } else { mark };
        if self.marks[idx] != Mark::None {
            if let (Some(gpu), Some(thumb)) = (&self.gpu, self.cache.get(&idx).and_then(|c| c.thumb.as_ref())) {
                self.thumbs.insert(idx, gpu.upload(thumb));
            }
        } else {
            self.thumbs.remove(&idx);
        }
        self.save();
    }

    /// Size of the current image at full resolution, if known yet.
    fn full_dims(&self) -> Option<(f32, f32)> {
        if let Some((_, t)) = &self.full {
            return Some((t.width as f32, t.height as f32));
        }
        self.cache.get(&self.cursor).map(|c| (c.full_w as f32, c.full_h as f32))
    }

    fn pan(&mut self, dx: f32, dy: f32) {
        let Some((iw, ih)) = self.full_dims() else { return };
        let (sw, sh) = self.screen();
        let (u, v) = self.peek_center;
        let (_, c) = peek_rect(iw, ih, sw, sh, u + dx / iw, v + dy / ih);
        self.peek_center = c;
    }

    fn enter_peek(&mut self, center: (f32, f32)) {
        self.mode = Mode::Peek;
        self.peek_center = center;
        self.pan(0.0, 0.0);
        self.schedule();
    }

    fn open_review(&mut self, event_loop: &ActiveEventLoop) {
        let items: Vec<usize> = (0..self.n()).filter(|&i| self.marks[i] != Mark::None).collect();
        if items.is_empty() {
            self.save();
            event_loop.exit();
            return;
        }
        let sel = items.iter().position(|&i| i >= self.cursor).unwrap_or(0);
        self.review = Review { keep: vec![false; items.len()], items, sel, scroll: 0.0 };
        self.mode = Mode::Review;
        self.review.scroll = self.grid().scroll_to(sel, 0.0, self.review.items.len());
        self.schedule();
    }

    fn close_review(&mut self) {
        for (i, &idx) in self.review.items.iter().enumerate() {
            if self.review.keep[i] {
                self.marks[idx] = Mark::None;
                self.thumbs.remove(&idx);
            }
        }
        self.save();
        self.mode = Mode::Browse;
        self.schedule();
    }

    fn grid(&self) -> Grid {
        let (w, h) = self.screen();
        let s = self.scale();
        Grid::new(w, h, 72.0 * s, 220.0 * s, 10.0 * s)
    }

    fn review_move(&mut self, delta: isize) {
        let n = self.review.items.len();
        self.review.sel = (self.review.sel as isize + delta).clamp(0, n as isize - 1) as usize;
        self.review.scroll = self.grid().scroll_to(self.review.sel, self.review.scroll, n);
    }

    fn review_scroll(&mut self, dy: f32) {
        let max = self.grid().max_scroll(self.review.items.len());
        self.review.scroll = (self.review.scroll + dy).clamp(0.0, max);
    }

    /// Review items (shot indices) with `mark` that the user didn't choose to keep.
    fn pending_items(&self, mark: Mark) -> Vec<usize> {
        (0..self.review.items.len())
            .filter(|&i| !self.review.keep[i])
            .map(|i| self.review.items[i])
            .filter(|&idx| self.marks[idx] == mark)
            .collect()
    }

    fn confirm(&mut self, event_loop: &ActiveEventLoop) {
        let doomed = self.pending_items(Mark::Delete);
        let edits = self.pending_items(Mark::Edit);
        if doomed.is_empty() && edits.is_empty() {
            self.close_review();
            return;
        }
        if let Some(gpu) = &mut self.gpu {
            let (w, h) = gpu.size();
            let s = gpu.window.scale_factor() as f32;
            let mut frame = Frame::default();
            frame.text("Moving files…", w as f32 / 2.0, h as f32 / 2.0, 22.0 * s, WHITE, Align::Center);
            gpu.render(frame);
        }
        let shots = &self.catalog.shots;
        let trashed = delete::trash_shots(doomed.iter().map(|&i| &shots[i]));
        let dest = self.dir.join(delete::NEEDS_EDIT_DIR);
        let moved = delete::move_shots(edits.iter().map(|&i| &shots[i]), &dest);

        let mut lines = Vec::new();
        if !doomed.is_empty() {
            lines.push(format!("cull: moved {} shot(s), {} file(s) to Trash.", trashed.shots, trashed.files));
        }
        if !edits.is_empty() {
            lines.push(format!("cull: moved {} shot(s), {} file(s) to {}", moved.shots, moved.files, dest.display()));
        }
        let failures: Vec<_> = trashed.failures.iter().chain(&moved.failures).collect();
        if failures.is_empty() {
            state::remove(&self.dir);
        } else {
            lines.push(format!("cull: {} file(s) could not be moved:", failures.len()));
            for (path, err) in &failures {
                lines.push(format!("  {}: {err}", path.display()));
            }
            // Keep marks only for shots that are still here, so a rerun can retry them.
            for (i, &idx) in self.review.items.iter().enumerate() {
                if self.review.keep[i] || !self.catalog.shots[idx].jpeg.exists() {
                    self.marks[idx] = Mark::None;
                }
            }
            self.save();
        }
        self.exit_message = Some(lines.join("\n"));
        event_loop.exit();
    }

    fn on_key(&mut self, event_loop: &ActiveEventLoop, event: &KeyEvent) {
        if event.state != ElementState::Pressed {
            return;
        }
        let shift = self.modifiers.shift_key();
        let key = match &event.logical_key {
            Key::Character(c) if self.modifiers.control_key() && c.as_str() == "c" => {
                self.save();
                event_loop.exit();
                return;
            }
            Key::Character(c) => c.as_str().to_owned(),
            Key::Named(named) => format!("{named:?}"),
            _ => return,
        };
        let key = key.as_str();

        if key == "?" {
            self.show_help = !self.show_help;
            self.redraw();
            return;
        }

        match self.mode {
            Mode::Browse => match key {
                "ArrowLeft" | "ArrowUp" | "h" | "k" => self.step(-1),
                "ArrowRight" | "ArrowDown" | "l" | "j" => self.step(1),
                "Home" | "g" => self.go_to(0),
                "End" | "G" => self.go_to(usize::MAX),
                "PageUp" => self.step(-10),
                "PageDown" => self.step(10),
                "Space" => self.toggle_mark(Mark::Delete),
                "e" => self.toggle_mark(Mark::Edit),
                "f" => self.enter_peek((0.5, 0.5)),
                "m" => self.show_meta = !self.show_meta,
                "q" | "Escape" => {
                    if self.show_help {
                        self.show_help = false;
                    } else {
                        self.open_review(event_loop);
                    }
                }
                _ => return,
            },
            Mode::Peek => {
                let (sw, sh) = self.screen();
                let (sx, sy) = if shift { (sw * 0.5, sh * 0.5) } else { (sw * 0.1, sh * 0.1) };
                match key {
                    "ArrowLeft" | "h" | "H" => self.pan(-sx, 0.0),
                    "ArrowRight" | "l" | "L" => self.pan(sx, 0.0),
                    "ArrowUp" | "k" | "K" => self.pan(0.0, -sy),
                    "ArrowDown" | "j" | "J" => self.pan(0.0, sy),
                    "Space" => self.toggle_mark(Mark::Delete),
                    "e" => self.toggle_mark(Mark::Edit),
                    "m" => self.show_meta = !self.show_meta,
                    "n" | "PageDown" => self.step(1),
                    "p" | "PageUp" => self.step(-1),
                    "f" | "Escape" => {
                        self.mode = Mode::Browse;
                        self.schedule();
                    }
                    _ => return,
                }
            }
            Mode::Review => {
                let cols = self.grid().cols as isize;
                match key {
                    "ArrowLeft" | "h" => self.review_move(-1),
                    "ArrowRight" | "l" => self.review_move(1),
                    "ArrowUp" | "k" => self.review_move(-cols),
                    "ArrowDown" | "j" => self.review_move(cols),
                    "PageUp" => self.review_move(-cols * 3),
                    "PageDown" => self.review_move(cols * 3),
                    "Home" | "g" => self.review_move(isize::MIN / 2),
                    "End" | "G" => self.review_move(isize::MAX / 2),
                    "Space" => {
                        let s = self.review.sel;
                        self.review.keep[s] = !self.review.keep[s];
                    }
                    "y" | "Enter" => self.confirm(event_loop),
                    "n" | "Escape" | "q" => self.close_review(),
                    _ => return,
                }
            }
        }
        self.redraw();
    }

    fn draw(&mut self) {
        let Some(gpu) = &self.gpu else { return };
        let (sw, sh) = {
            let (w, h) = gpu.size();
            (w as f32, h as f32)
        };
        let s = gpu.window.scale_factor() as f32;
        let mut f = Frame::default();
        let shot = &self.catalog.shots[self.cursor];
        let mark = self.marks[self.cursor];
        let dim = if mark == Mark::Delete { 0.85 } else { 0.0 };

        match self.mode {
            Mode::Browse | Mode::Peek => {
                let cached = self.cache.get(&self.cursor);
                if self.mode == Mode::Browse {
                    if let Some(c) = cached {
                        let r = fit_rect(c.tex.width as f32, c.tex.height as f32, sw, sh);
                        f.image(&c.tex, r, dim);
                        match mark {
                            Mark::Delete => outline(gpu, &mut f, r, 4.0 * s, srgb(255, 80, 70, 0.9)),
                            Mark::Edit => outline(gpu, &mut f, r, 4.0 * s, srgb(255, 190, 60, 0.9)),
                            Mark::None => {}
                        }
                    }
                } else if let Some((iw, ih)) = self.full_dims() {
                    let (r, _) = peek_rect(iw, ih, sw, sh, self.peek_center.0, self.peek_center.1);
                    match (&self.full, cached) {
                        (Some((_, t)), _) => f.image(t, r, dim),
                        (None, Some(c)) => {
                            f.image(&c.tex, r, dim);
                            f.text("Loading full resolution…", sw / 2.0, sh / 2.0, 16.0 * s, WHITE, Align::Center);
                        }
                        _ => {}
                    }
                    minimap(gpu, &mut f, iw, ih, r, sw, sh, s);
                    f.bold_text("100%", sw - 16.0 * s, 14.0 * s, 15.0 * s, WHITE, Align::Right);
                }
                if cached.is_none() {
                    let msg = match self.errors.get(&self.cursor) {
                        Some(e) => format!("Could not decode: {e}"),
                        None => "Loading…".to_owned(),
                    };
                    f.text(msg, sw / 2.0, sh / 2.0, 16.0 * s, MUTED, Align::Center);
                }
                if self.show_meta {
                    metadata(gpu, &mut f, self.meta.get(&self.cursor), s);
                }
                match mark {
                    Mark::Delete => f.bold_text("MARKED FOR DELETION", sw / 2.0, 14.0 * s, 20.0 * s, RED, Align::Center),
                    Mark::Edit => f.bold_text("NEEDS EDIT", sw / 2.0, 14.0 * s, 20.0 * s, AMBER, Align::Center),
                    Mark::None => {}
                }

                let name = shot.jpeg.file_name().map(|n| n.to_string_lossy()).unwrap_or_default();
                let kind = if shot.has_raw() {
                    let exts: Vec<String> = shot
                        .raws
                        .iter()
                        .filter_map(|p| p.extension().map(|e| e.to_string_lossy().to_uppercase()))
                        .collect();
                    format!("{} + JPEG", exts.join(", "))
                } else {
                    "JPEG only".to_owned()
                };
                let y = sh - 30.0 * s;
                f.text(
                    format!("{} / {}    {}    {}", self.cursor + 1, self.n(), name, kind),
                    16.0 * s,
                    y,
                    15.0 * s,
                    WHITE,
                    Align::Left,
                );
                let (deletes, edits) = (self.count(Mark::Delete), self.count(Mark::Edit));
                let help_w = 60.0 * s;
                f.text("? help", sw - 16.0 * s, y, 15.0 * s, MUTED, Align::Right);
                f.text(
                    format!("{edits} to edit"),
                    sw - 16.0 * s - help_w,
                    y,
                    15.0 * s,
                    if edits > 0 { AMBER } else { MUTED },
                    Align::Right,
                );
                f.text(
                    format!("{deletes} to delete"),
                    sw - 16.0 * s - help_w - 110.0 * s,
                    y,
                    15.0 * s,
                    if deletes > 0 { RED } else { MUTED },
                    Align::Right,
                );
            }
            Mode::Review => {
                let grid = self.grid();
                for (i, &idx) in self.review.items.iter().enumerate() {
                    let cell = grid.cell_rect(i, self.review.scroll);
                    if cell.y + cell.h < grid.top || cell.y > sh {
                        continue;
                    }
                    let keep = self.review.keep[i];
                    gpu.rect(&mut f, cell, srgb(28, 28, 30, 1.0));
                    if let Some(t) = self.thumbs.get(&idx) {
                        let r = fit_rect(t.width as f32, t.height as f32, cell.w, cell.h);
                        f.image(t, Rect::new(cell.x + r.x, cell.y + r.y, r.w, r.h), if keep { 0.85 } else { 0.0 });
                    } else {
                        let msg = if self.errors.contains_key(&idx) { "error" } else { "…" };
                        f.text(msg, cell.x + cell.w / 2.0, cell.y + cell.h / 2.0, 14.0 * s, MUTED, Align::Center);
                    }
                    if cell.y + cell.h - 24.0 * s > grid.top {
                        let label = &self.catalog.shots[idx].stem;
                        f.text(label.as_str(), cell.x + 8.0 * s, cell.y + cell.h - 24.0 * s, 13.0 * s, WHITE, Align::Left);
                        let (badge, color) = match (keep, self.marks[idx]) {
                            (true, _) => ("KEEP", GREEN),
                            (false, Mark::Edit) => ("EDIT", AMBER),
                            (false, _) => ("DELETE", RED),
                        };
                        f.bold_text(badge, cell.x + cell.w - 8.0 * s, cell.y + cell.h - 24.0 * s, 13.0 * s, color, Align::Right);
                    }
                    if i == self.review.sel {
                        outline(gpu, &mut f, cell, 3.0 * s, srgb(255, 255, 255, 1.0));
                    } else if !keep && self.marks[idx] == Mark::Edit {
                        outline(gpu, &mut f, cell, 2.0 * s, srgb(255, 190, 60, 0.9));
                    }
                }
                // Header drawn last so it covers rows scrolled beneath it.
                gpu.rect(&mut f, Rect::new(0.0, 0.0, sw, grid.top), srgb(18, 18, 20, 0.97));
                let summary = |mark| {
                    let items = self.pending_items(mark);
                    let files: usize = items.iter().map(|&i| self.catalog.shots[i].files().count()).sum();
                    (items.len(), files)
                };
                let (deletes, delete_files) = summary(Mark::Delete);
                let (edits, edit_files) = summary(Mark::Edit);
                let mut parts = Vec::new();
                if deletes > 0 {
                    parts.push(format!("move {deletes} shot(s) ({delete_files} files) to Trash"));
                }
                if edits > 0 {
                    parts.push(format!("move {edits} shot(s) ({edit_files} files) to {}/", delete::NEEDS_EDIT_DIR));
                }
                let title = match parts.join(" and ") {
                    t if t.is_empty() => "Nothing to do (everything kept)".to_owned(),
                    t => format!("{}{}?", t[..1].to_uppercase(), &t[1..]),
                };
                let color = if deletes > 0 { RED } else if edits > 0 { AMBER } else { WHITE };
                f.bold_text(title, 16.0 * s, 14.0 * s, 20.0 * s, color, Align::Left);
                f.text(
                    "y / Enter: confirm     n / Esc: back     Space / click: keep",
                    16.0 * s,
                    44.0 * s,
                    14.0 * s,
                    MUTED,
                    Align::Left,
                );
            }
        }

        if self.show_help {
            help(gpu, &mut f, sw, sh, s);
        }
        let gpu = self.gpu.as_mut().unwrap();
        gpu.render(f);
    }
}

fn outline(gpu: &Gpu, f: &mut Frame, r: Rect, t: f32, color: [f32; 4]) {
    gpu.rect(f, Rect::new(r.x, r.y, r.w, t), color);
    gpu.rect(f, Rect::new(r.x, r.y + r.h - t, r.w, t), color);
    gpu.rect(f, Rect::new(r.x, r.y + t, t, r.h - 2.0 * t), color);
    gpu.rect(f, Rect::new(r.x + r.w - t, r.y + t, t, r.h - 2.0 * t), color);
}

/// Small overview in the corner showing which part of the image is on screen.
#[allow(clippy::too_many_arguments)]
fn minimap(gpu: &Gpu, f: &mut Frame, iw: f32, ih: f32, img: Rect, sw: f32, sh: f32, s: f32) {
    let size = 140.0 * s;
    let k = size / iw.max(ih);
    let (mw, mh) = (iw * k, ih * k);
    let map = Rect::new(sw - mw - 16.0 * s, sh - mh - 56.0 * s, mw, mh);
    gpu.rect(f, map, srgb(0, 0, 0, 0.5));
    outline(gpu, f, map, 1.0 * s, srgb(200, 200, 200, 0.8));
    let vx = (-img.x).max(0.0) * k;
    let vy = (-img.y).max(0.0) * k;
    let vw = sw.min(iw) * k;
    let vh = sh.min(ih) * k;
    let view = Rect::new(map.x + vx, map.y + vy, vw.min(mw), vh.min(mh));
    gpu.rect(f, view, srgb(255, 255, 255, 0.25));
    outline(gpu, f, view, 1.5 * s, srgb(255, 255, 255, 0.9));
}

/// Shooting info panel in the top-left corner.
fn metadata(gpu: &Gpu, f: &mut Frame, meta: Option<&Meta>, s: f32) {
    let (size, lh, pad) = (14.0 * s, 21.0 * s, 14.0 * s);
    let label_w = 110.0 * s;
    let rows: &[(&str, String)] = match meta {
        Some(m) => &m.rows,
        None => &[("", String::new())],
    };
    // Rough width estimate; values are short so this rarely under-sizes.
    let chars = rows.iter().map(|(_, v)| v.chars().count()).max().unwrap_or(0) as f32;
    let w = (label_w + chars * 7.6 * s + 2.0 * pad).max(220.0 * s);
    let h = rows.len() as f32 * lh + 2.0 * pad - (lh - size * 1.25);
    let r = Rect::new(16.0 * s, 16.0 * s, w, h);
    gpu.rect(f, r, srgb(12, 12, 14, 0.78));
    if meta.is_none() {
        f.text("Loading…", r.x + pad, r.y + pad, size, MUTED, Align::Left);
        return;
    }
    for (i, (label, value)) in rows.iter().enumerate() {
        let y = r.y + pad + i as f32 * lh;
        f.text(*label, r.x + pad, y, size, MUTED, Align::Left);
        f.text(value.as_str(), r.x + pad + label_w, y, size, WHITE, Align::Left);
    }
}

fn help(gpu: &Gpu, f: &mut Frame, sw: f32, sh: f32, s: f32) {
    let lines = [
        ("Browse", ""),
        ("← → / h l", "previous / next"),
        ("Space", "mark / unmark for deletion"),
        ("e", "mark / unmark as needs edit"),
        ("f  or click", "focus peek (100%)"),
        ("g / G", "first / last"),
        ("m", "show / hide metadata"),
        ("q / Esc", "review marked shots and finish"),
        ("", ""),
        ("Focus peek", ""),
        ("arrows / hjkl", "pan (Shift: faster), or drag / scroll"),
        ("n / p", "next / previous shot"),
        ("f / Esc", "back to full screen"),
        ("", ""),
        ("?", "toggle this help"),
    ];
    let lh = 24.0 * s;
    let (w, h) = (460.0 * s, lines.len() as f32 * lh + 40.0 * s);
    let r = Rect::new((sw - w) / 2.0, (sh - h) / 2.0, w, h);
    gpu.rect(f, r, srgb(15, 15, 17, 0.92));
    for (i, (k, v)) in lines.iter().enumerate() {
        let y = r.y + 20.0 * s + i as f32 * lh;
        if v.is_empty() {
            f.bold_text(*k, r.x + 24.0 * s, y, 15.0 * s, WHITE, Align::Left);
        } else {
            f.text(*k, r.x + 24.0 * s, y, 15.0 * s, WHITE, Align::Left);
            f.text(*v, r.x + 180.0 * s, y, 15.0 * s, MUTED, Align::Left);
        }
    }
}

impl ApplicationHandler<()> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.gpu.is_some() {
            return;
        }
        event_loop.set_control_flow(ControlFlow::Wait);
        let mut attrs = Window::default_attributes()
            .with_title(format!("cull — {}", self.dir.display()))
            .with_inner_size(winit::dpi::LogicalSize::new(1400.0, 900.0));
        if !self.windowed {
            attrs = attrs.with_fullscreen(Some(Fullscreen::Borderless(None)));
        }
        let window = Arc::new(event_loop.create_window(attrs).expect("create window"));
        let gpu = match pollster::block_on(Gpu::new(window.clone())) {
            Ok(g) => g,
            Err(e) => {
                self.exit_message = Some(format!("cull: GPU init failed: {e}"));
                event_loop.exit();
                return;
            }
        };
        // Decode screen-fit images at monitor resolution so they stay sharp in fullscreen.
        let (mut tw, mut th) = gpu.size();
        if let Some(m) = window.current_monitor() {
            tw = tw.max(m.size().width);
            th = th.max(m.size().height);
        }
        self.loader.set_target(tw, th);
        self.gpu = Some(gpu);
        self.schedule();
        window.request_redraw();
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, _: ()) {
        let mut any = false;
        while let Ok(loaded) = self.loader.results.try_recv() {
            self.receive(loaded);
            any = true;
        }
        if any {
            self.schedule();
            self.redraw();
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => {
                self.save();
                event_loop.exit();
            }
            WindowEvent::Resized(size) => {
                if let Some(gpu) = &mut self.gpu {
                    gpu.resize(size.width, size.height);
                }
                if self.mode == Mode::Peek {
                    self.pan(0.0, 0.0);
                }
                if self.mode == Mode::Review {
                    self.review_scroll(0.0);
                }
                self.redraw();
            }
            WindowEvent::ScaleFactorChanged { .. } => self.redraw(),
            WindowEvent::ModifiersChanged(m) => self.modifiers = m.state(),
            WindowEvent::KeyboardInput { event, .. } => self.on_key(event_loop, &event),
            WindowEvent::CursorMoved { position, .. } => {
                let (x, y) = (position.x as f32, position.y as f32);
                if self.dragging && self.mode == Mode::Peek {
                    self.pan(self.mouse.0 - x, self.mouse.1 - y);
                    self.redraw();
                }
                self.mouse = (x, y);
            }
            WindowEvent::MouseInput { state, button: MouseButton::Left, .. } => {
                let pressed = state == ElementState::Pressed;
                self.dragging = pressed && self.mode == Mode::Peek;
                if !pressed {
                    return;
                }
                match self.mode {
                    Mode::Browse => {
                        // Click to peek at that spot.
                        if let Some(c) = self.cache.get(&self.cursor) {
                            let (sw, sh) = self.screen();
                            let r = fit_rect(c.tex.width as f32, c.tex.height as f32, sw, sh);
                            if r.contains(self.mouse.0, self.mouse.1) {
                                let u = (self.mouse.0 - r.x) / r.w;
                                let v = (self.mouse.1 - r.y) / r.h;
                                self.enter_peek((u, v));
                            }
                        }
                    }
                    Mode::Review => {
                        let n = self.review.items.len();
                        if let Some(i) = self.grid().hit(n, self.mouse.0, self.mouse.1, self.review.scroll) {
                            self.review.sel = i;
                            self.review.keep[i] = !self.review.keep[i];
                        }
                    }
                    Mode::Peek => {}
                }
                self.redraw();
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let (dx, dy) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => (x * 60.0 * self.scale(), y * 60.0 * self.scale()),
                    MouseScrollDelta::PixelDelta(p) => (p.x as f32, p.y as f32),
                };
                match self.mode {
                    Mode::Review => self.review_scroll(-dy),
                    Mode::Peek => self.pan(-dx, -dy),
                    Mode::Browse => return,
                }
                self.redraw();
            }
            WindowEvent::RedrawRequested => self.draw(),
            _ => {}
        }
    }
}
