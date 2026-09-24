//! A single-line text field with a small subset of vi key bindings.
//!
//! Normal mode: `h` `l` `w` `b` `0` `$` move, `x` deletes, `cw` changes a word, `i` `a` `I` `A` insert.
//! Insert mode: typed text goes in at the cursor, Escape returns to normal mode.

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    #[default]
    Normal,
    Insert,
}

#[derive(Debug, Default)]
pub struct LineEdit {
    pub text: String,
    /// Byte offset of the cursor, always on a char boundary. In normal mode it sits on a character,
    /// so it is short of the end unless the text is empty.
    pub caret: usize,
    pub mode: Mode,
    /// First key of a two-key command (`c` of `cw`).
    pending: Option<char>,
}

/// vi's word classes: blanks, keyword characters, and runs of any other punctuation.
fn class(c: char) -> u8 {
    if c.is_whitespace() {
        0
    } else if c.is_alphanumeric() || c == '_' {
        1
    } else {
        2
    }
}

impl LineEdit {
    /// Starts in normal mode with the cursor on the last character.
    pub fn new(text: String) -> LineEdit {
        let mut e = LineEdit { caret: text.len(), text, ..LineEdit::default() };
        e.clamp();
        e
    }

    fn char_at(&self, i: usize) -> Option<char> {
        self.text[i..].chars().next()
    }

    fn prev_of(&self, i: usize) -> usize {
        self.text[..i].chars().next_back().map_or(0, |c| i - c.len_utf8())
    }

    fn next_of(&self, i: usize) -> usize {
        self.char_at(i).map_or(i, |c| i + c.len_utf8())
    }

    /// Keep a normal-mode cursor on a character.
    fn clamp(&mut self) {
        if self.mode == Mode::Normal && self.caret >= self.text.len() {
            self.caret = self.prev_of(self.text.len());
        }
    }

    /// End of the run of same-class characters starting at `i`.
    fn run_end(&self, i: usize) -> usize {
        let Some(k) = self.char_at(i).map(class) else { return i };
        let mut j = i;
        while self.char_at(j).is_some_and(|c| class(c) == k) {
            j = self.next_of(j);
        }
        j
    }

    /// Start of the next word (`w`).
    fn next_word(&self) -> usize {
        let mut j = self.caret;
        if self.char_at(j).is_some_and(|c| class(c) != 0) {
            j = self.run_end(j);
        }
        while self.char_at(j).is_some_and(|c| class(c) == 0) {
            j = self.next_of(j);
        }
        j
    }

    /// Start of this word, or of the previous one if already there (`b`).
    fn prev_word(&self) -> usize {
        let mut j = self.caret;
        while j > 0 && class(self.char_at(self.prev_of(j)).unwrap()) == 0 {
            j = self.prev_of(j);
        }
        let Some(k) = (j > 0).then(|| class(self.char_at(self.prev_of(j)).unwrap())) else { return 0 };
        while j > 0 && class(self.char_at(self.prev_of(j)).unwrap()) == k {
            j = self.prev_of(j);
        }
        j
    }

    fn insert_mode(&mut self, caret: usize) {
        self.caret = caret;
        self.mode = Mode::Insert;
    }

    /// A normal-mode key. Unknown keys are ignored, and cancel a pending `c`.
    pub fn command(&mut self, key: &str) {
        if self.pending.take() == Some('c') {
            if key == "w" {
                // Like vi, `cw` stops at the end of the word rather than eating the gap after it.
                let end = if self.char_at(self.caret).is_some_and(|c| class(c) == 0) {
                    self.next_of(self.caret)
                } else {
                    self.run_end(self.caret)
                };
                self.text.replace_range(self.caret..end, "");
                self.mode = Mode::Insert;
            }
            return;
        }
        match key {
            "h" => self.caret = self.prev_of(self.caret),
            "l" => self.caret = self.next_of(self.caret),
            "w" => self.caret = self.next_word(),
            "b" => self.caret = self.prev_word(),
            "0" => self.caret = 0,
            "$" => self.caret = self.text.len(),
            "x" => {
                let end = self.next_of(self.caret);
                self.text.replace_range(self.caret..end, "");
            }
            "i" => self.insert_mode(self.caret),
            "a" => self.insert_mode(self.next_of(self.caret)),
            "I" => self.insert_mode(0),
            "A" => self.insert_mode(self.text.len()),
            "c" => self.pending = Some('c'),
            _ => {}
        }
        self.clamp();
    }

    /// Escape: leave insert mode or drop a half-typed command. False if there was nothing to cancel.
    pub fn escape(&mut self) -> bool {
        if self.mode == Mode::Insert {
            // vi steps back onto the last inserted character.
            self.mode = Mode::Normal;
            self.caret = self.prev_of(self.caret);
            true
        } else {
            self.pending.take().is_some()
        }
    }

    pub fn left(&mut self) {
        self.caret = self.prev_of(self.caret);
    }

    pub fn right(&mut self) {
        self.caret = self.next_of(self.caret);
        self.clamp();
    }

    /// Insert mode only.
    pub fn insert(&mut self, s: &str) {
        self.text.insert_str(self.caret, s);
        self.caret += s.len();
    }

    /// Insert mode only.
    pub fn backspace(&mut self) {
        let start = self.prev_of(self.caret);
        self.text.replace_range(start..self.caret, "");
        self.caret = start;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PATH: &str = "~/Raw Shots/2026-09-19";

    fn keys(e: &mut LineEdit, keys: &str) {
        for k in keys.chars() {
            e.command(&k.to_string());
        }
    }

    /// Text with `|` marking the cursor.
    fn show(e: &LineEdit) -> String {
        format!("{}|{}", &e.text[..e.caret], &e.text[e.caret..])
    }

    #[test]
    fn starts_on_last_char() {
        let e = LineEdit::new(PATH.to_owned());
        assert_eq!((show(&e).as_str(), e.mode), ("~/Raw Shots/2026-09-1|9", Mode::Normal));
        assert_eq!(LineEdit::new(String::new()).caret, 0);
    }

    #[test]
    fn word_motions() {
        let mut e = LineEdit::new(PATH.to_owned());
        keys(&mut e, "0");
        let mut stops = Vec::new();
        for _ in 0..9 {
            keys(&mut e, "w");
            stops.push(e.caret);
        }
        // ~/ | Raw | Shots | / | 2026 | - | 09 | - | 19, then stuck on the last char.
        assert_eq!(stops, [2, 6, 11, 12, 16, 17, 19, 20, 21]);
        let mut back = Vec::new();
        for _ in 0..9 {
            keys(&mut e, "b");
            back.push(e.caret);
        }
        assert_eq!(back, [20, 19, 17, 16, 12, 11, 6, 2, 0]);
    }

    #[test]
    fn change_word() {
        let mut e = LineEdit::new(PATH.to_owned());
        keys(&mut e, "bcw");
        assert_eq!((show(&e).as_str(), e.mode), ("~/Raw Shots/2026-09-|", Mode::Insert));
        e.insert("20");
        assert!(e.escape());
        assert_eq!((show(&e).as_str(), e.mode), ("~/Raw Shots/2026-09-2|0", Mode::Normal));

        let mut e = LineEdit::new("a b".to_owned());
        keys(&mut e, "0cw");
        assert_eq!(show(&e), "| b", "stops before the blank");

        let mut e = LineEdit::new("ab".to_owned());
        keys(&mut e, "0cx");
        assert_eq!((show(&e).as_str(), e.mode), ("|ab", Mode::Normal), "unknown second key cancels");
        keys(&mut e, "c");
        assert!(e.escape(), "escape drops the pending c");
        assert!(!e.escape());
    }

    #[test]
    fn inserting_and_editing() {
        let mut e = LineEdit::new("~/Pictures".to_owned());
        keys(&mut e, "A");
        e.insert("/2026");
        assert_eq!(show(&e), "~/Pictures/2026|");
        e.backspace();
        e.escape();
        keys(&mut e, "0x$");
        assert_eq!(show(&e), "/Pictures/20|2");
        keys(&mut e, "I");
        e.insert("~");
        e.escape();
        keys(&mut e, "hhhha");
        assert_eq!((show(&e).as_str(), e.mode), ("~|/Pictures/202", Mode::Insert));
    }
}
