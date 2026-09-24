//! The input state machine: text, history, search, the kill buffer, and
//! pastes too large to leave inline.

const PASTE_LIMIT: usize = 1000;
const BURST_GAP_MS: u128 = 15;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    display: String,
    pastes: Vec<String>,
}

#[derive(Debug)]
pub struct Composer {
    text: String,
    cursor: usize,
    kill: String,
    history: Vec<Entry>,
    /// Index into `history` while walking it. `None` is the live draft.
    walking: Option<usize>,
    draft: String,
    search: Option<String>,
    pastes: Vec<String>,
}

impl Default for Composer {
    fn default() -> Self {
        Self {
            text: String::new(),
            cursor: 0,
            kill: String::new(),
            history: Vec::new(),
            walking: None,
            draft: String::new(),
            search: None,
            pastes: Vec::new(),
        }
    }
}

impl Composer {
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Byte offset of the caret within `text`.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn searching(&self) -> Option<&str> {
        self.search.as_deref()
    }

    pub fn search_preview(&self) -> Option<&str> {
        let query = self.search.as_ref()?;
        self.history
            .iter()
            .rev()
            .find(|entry| entry.display.contains(query.as_str()))
            .map(|entry| entry.display.as_str())
    }

    pub fn insert_char(&mut self, ch: char) {
        self.insert_plain(&ch.to_string());
    }

    /// A bracketed paste, or a burst already joined into one string.
    pub fn insert_paste(&mut self, text: &str) {
        if text.chars().count() > PASTE_LIMIT {
            self.remember_paste(text);
        } else {
            self.insert_plain(text);
        }
    }

    /// A burst that was typed in as ordinary keys and then recognised as a paste.
    pub fn collapse_tail(&mut self, tail: &str) {
        if tail.chars().count() <= PASTE_LIMIT || !self.text.ends_with(tail) {
            return;
        }
        let at = self.text.len() - tail.len();
        self.text.replace_range(at.., "");
        self.cursor = at;
        self.remember_paste(tail);
    }

    fn remember_paste(&mut self, text: &str) {
        self.pastes.push(text.to_string());
        let marker = format!("[Pasted Content {} chars]", text.chars().count());
        self.insert_plain(&marker);
    }

    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let start = prev_boundary(&self.text, self.cursor);
        self.text.replace_range(start..self.cursor, "");
        self.cursor = start;
    }

    pub fn newline(&mut self) {
        self.insert_plain("\n");
    }

    /// Ctrl+K. The killed text stays available across later edits.
    pub fn kill_to_end(&mut self) {
        let end = line_end(&self.text, self.cursor);
        self.kill = self.text[self.cursor..end].to_string();
        self.text.replace_range(self.cursor..end, "");
    }

    /// Ctrl+Y.
    pub fn yank(&mut self) {
        if self.kill.is_empty() {
            return;
        }
        let killed = self.kill.clone();
        self.insert_plain(&killed);
    }

    pub fn move_left(&mut self) {
        self.cursor = prev_boundary(&self.text, self.cursor);
    }

    pub fn move_right(&mut self) {
        self.cursor = next_boundary(&self.text, self.cursor);
    }

    pub fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let index = match self.walking {
            None => {
                self.draft = self.text.clone();
                self.history.len() - 1
            }
            Some(0) => return,
            Some(index) => index - 1,
        };
        self.load_entry(index);
    }

    pub fn history_next(&mut self) {
        let Some(index) = self.walking else {
            return;
        };
        if index + 1 >= self.history.len() {
            self.walking = None;
            self.text = self.draft.clone();
            self.cursor = self.text.len();
            return;
        }
        self.load_entry(index + 1);
    }

    pub fn search_start(&mut self) {
        self.search = Some(String::new());
    }

    pub fn search_push(&mut self, ch: char) {
        if let Some(query) = &mut self.search {
            query.push(ch);
        }
    }

    pub fn search_backspace(&mut self) {
        if let Some(query) = &mut self.search {
            query.pop();
        }
    }

    /// Enter during search: take the preview, leave search.
    pub fn search_accept(&mut self) -> bool {
        let Some(preview) = self.search_preview().map(str::to_string) else {
            self.search = None;
            return false;
        };
        if let Some(index) = self
            .history
            .iter()
            .position(|entry| entry.display == preview)
        {
            self.load_entry(index);
        }
        self.search = None;
        true
    }

    pub fn search_cancel(&mut self) {
        self.search = None;
    }

    pub fn load(&mut self, text: &str) {
        self.text = text.to_string();
        self.cursor = self.text.len();
        self.walking = None;
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.pastes.clear();
        self.walking = None;
    }

    /// Remember this draft and return the text the model should see, with
    /// paste bodies put back in place of their placeholders.
    pub fn commit(&mut self) -> String {
        let expanded = self.expand();
        if !self.text.is_empty() {
            self.history.push(Entry {
                display: self.text.clone(),
                pastes: self.pastes.clone(),
            });
        }
        self.clear();
        expanded
    }

    fn expand(&self) -> String {
        let mut text = self.text.clone();
        for paste in &self.pastes {
            let marker = format!("[Pasted Content {} chars]", paste.chars().count());
            if let Some(at) = text.find(&marker) {
                text.replace_range(at..at + marker.len(), paste);
            }
        }
        text
    }

    fn load_entry(&mut self, index: usize) {
        let entry = &self.history[index];
        self.text = entry.display.clone();
        self.pastes = entry.pastes.clone();
        self.cursor = self.text.len();
        self.walking = Some(index);
    }

    fn insert_plain(&mut self, extra: &str) {
        self.text.insert_str(self.cursor, extra);
        self.cursor += extra.len();
    }
}

/// Rapid keystrokes with no bracketed-paste markers. A gap ends the burst.
#[derive(Debug, Default)]
pub struct PasteBurst {
    buf: String,
    last_ms: u128,
    open: bool,
}

pub enum Burst {
    /// Still accumulating, or a single ordinary key.
    Held,
    /// The burst ended on a gap. `ready` is what was already typed.
    Flushed { ready: String },
}

impl PasteBurst {
    pub fn push(&mut self, now_ms: u128, ch: char) -> Burst {
        if self.open && now_ms.saturating_sub(self.last_ms) > BURST_GAP_MS {
            let ready = std::mem::take(&mut self.buf);
            self.buf.push(ch);
            self.last_ms = now_ms;
            self.open = true;
            return Burst::Flushed { ready };
        }
        self.buf.push(ch);
        self.last_ms = now_ms;
        self.open = true;
        Burst::Held
    }

    pub fn pending_chars(&self) -> usize {
        self.buf.chars().count()
    }

    pub fn take(&mut self) -> String {
        self.open = false;
        std::mem::take(&mut self.buf)
    }
}

fn prev_boundary(text: &str, cursor: usize) -> usize {
    if cursor == 0 {
        return 0;
    }
    let mut end = cursor;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end]
        .chars()
        .next_back()
        .map(|ch| end - ch.len_utf8())
        .unwrap_or(0)
}

fn next_boundary(text: &str, cursor: usize) -> usize {
    if cursor >= text.len() {
        return text.len();
    }
    let mut start = cursor;
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    text[start..]
        .chars()
        .next()
        .map(|ch| start + ch.len_utf8())
        .unwrap_or(text.len())
}

fn line_end(text: &str, cursor: usize) -> usize {
    text[cursor..]
        .find('\n')
        .map(|at| cursor + at)
        .unwrap_or(text.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_long_paste_becomes_a_placeholder_and_comes_back_on_submit() {
        let mut composer = Composer::default();
        let body = "x".repeat(1001);
        composer.insert_paste(&body);
        assert_eq!(composer.text(), "[Pasted Content 1001 chars]");
        assert_eq!(composer.commit(), body);
    }

    #[test]
    fn a_short_paste_stays_inline() {
        let mut composer = Composer::default();
        composer.insert_paste("hello");
        assert_eq!(composer.text(), "hello");
    }

    #[test]
    fn kill_and_yank_survive_another_edit() {
        let mut composer = Composer::default();
        composer.insert_paste("abcdef");
        composer.move_left();
        composer.move_left();
        composer.kill_to_end();
        assert_eq!(composer.text(), "abcd");
        composer.insert_char('Z');
        composer.yank();
        assert_eq!(composer.text(), "abcdZef");
    }

    #[test]
    fn history_walks_and_restores_the_draft() {
        let mut composer = Composer::default();
        composer.insert_paste("first");
        composer.commit();
        composer.insert_paste("second");
        composer.commit();
        composer.insert_paste("draft");
        composer.history_prev();
        assert_eq!(composer.text(), "second");
        composer.history_prev();
        assert_eq!(composer.text(), "first");
        composer.history_next();
        composer.history_next();
        assert_eq!(composer.text(), "draft");
    }

    #[test]
    fn search_previews_and_accepts() {
        let mut composer = Composer::default();
        composer.insert_paste("cargo test");
        composer.commit();
        composer.insert_paste("cargo build");
        composer.commit();
        composer.search_start();
        composer.search_push('b');
        composer.search_push('u');
        assert_eq!(composer.search_preview(), Some("cargo build"));
        composer.search_accept();
        assert_eq!(composer.text(), "cargo build");
        assert!(composer.searching().is_none());
    }

    #[test]
    fn a_pause_flushes_a_burst() {
        let mut burst = PasteBurst::default();
        assert!(matches!(burst.push(0, 'a'), Burst::Held));
        assert!(matches!(burst.push(5, 'b'), Burst::Held));
        match burst.push(40, 'c') {
            Burst::Flushed { ready } => {
                assert_eq!(ready, "ab");
            }
            Burst::Held => panic!("gap should flush"),
        }
    }
}
