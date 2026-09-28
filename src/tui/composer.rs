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

    /// The Delete key: the character after the caret.
    pub fn delete_forward(&mut self) {
        if self.cursor >= self.text.len() {
            return;
        }
        let end = next_boundary(&self.text, self.cursor);
        self.text.replace_range(self.cursor..end, "");
    }

    /// Ctrl+W and Alt+Backspace: back to the start of the previous word.
    pub fn delete_word_back(&mut self) {
        let start = word_start(&self.text, self.cursor);
        self.text.replace_range(start..self.cursor, "");
        self.cursor = start;
    }

    /// Ctrl+U. Like Ctrl+K, what it removes can be yanked back.
    pub fn kill_to_start(&mut self) {
        let start = line_start(&self.text, self.cursor);
        self.kill = self.text[start..self.cursor].to_string();
        self.text.replace_range(start..self.cursor, "");
        self.cursor = start;
    }

    pub fn move_left(&mut self) {
        self.cursor = prev_boundary(&self.text, self.cursor);
    }

    pub fn move_right(&mut self) {
        self.cursor = next_boundary(&self.text, self.cursor);
    }

    pub fn move_home(&mut self) {
        self.cursor = line_start(&self.text, self.cursor);
    }

    pub fn move_end(&mut self) {
        self.cursor = line_end(&self.text, self.cursor);
    }

    pub fn move_word_left(&mut self) {
        self.cursor = word_start(&self.text, self.cursor);
    }

    pub fn move_word_right(&mut self) {
        let rest = &self.text[self.cursor..];
        let skip_space = rest.len() - rest.trim_start().len();
        let word = rest[skip_space..]
            .find(char::is_whitespace)
            .unwrap_or(rest.len() - skip_space);
        self.cursor += skip_space + word;
    }

    /// Up inside a multi-line draft. False on the first line, so the caller
    /// can fall through to history.
    pub fn move_up(&mut self) -> bool {
        let start = line_start(&self.text, self.cursor);
        if start == 0 {
            return false;
        }
        let column = self.text[start..self.cursor].chars().count();
        let above = line_start(&self.text, start - 1);
        self.cursor = column_offset(&self.text, above, start - 1, column);
        true
    }

    /// Down inside a multi-line draft. False on the last line.
    pub fn move_down(&mut self) -> bool {
        let end = line_end(&self.text, self.cursor);
        if end >= self.text.len() {
            return false;
        }
        let column = self.text[line_start(&self.text, self.cursor)..self.cursor]
            .chars()
            .count();
        let below = end + 1;
        self.cursor = column_offset(&self.text, below, line_end(&self.text, below), column);
        true
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

fn line_start(text: &str, cursor: usize) -> usize {
    text[..cursor].rfind('\n').map(|at| at + 1).unwrap_or(0)
}

/// Skip spaces backwards, then the word before them.
fn word_start(text: &str, cursor: usize) -> usize {
    let before = &text[..cursor];
    let trimmed = before.trim_end();
    trimmed
        .rfind(char::is_whitespace)
        .map(|at| at + trimmed[at..].chars().next().map_or(1, char::len_utf8))
        .unwrap_or(0)
}

/// The byte offset `column` characters into the line `start..end`, or its end.
fn column_offset(text: &str, start: usize, end: usize, column: usize) -> usize {
    text[start..end]
        .char_indices()
        .nth(column)
        .map(|(at, _)| start + at)
        .unwrap_or(end)
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
    fn backspace_and_delete_work_mid_line() {
        let mut composer = Composer::default();
        composer.insert_paste("héllo");
        composer.move_left();
        composer.move_left();
        composer.backspace();
        assert_eq!(composer.text(), "hélo");
        composer.delete_forward();
        assert_eq!(composer.text(), "héo");
        composer.move_home();
        composer.delete_forward();
        assert_eq!(composer.text(), "éo");
        composer.move_end();
        composer.delete_forward();
        assert_eq!(composer.text(), "éo");
    }

    #[test]
    fn words_are_jumped_and_deleted() {
        let mut composer = Composer::default();
        composer.insert_paste("run the tests  ");
        composer.delete_word_back();
        assert_eq!(composer.text(), "run the ");
        composer.move_word_left();
        assert_eq!(composer.cursor(), 4);
        composer.move_word_left();
        assert_eq!(composer.cursor(), 0);
        composer.move_word_right();
        assert_eq!(composer.cursor(), 3);
    }

    #[test]
    fn kill_to_start_can_be_yanked_back() {
        let mut composer = Composer::default();
        composer.insert_paste("abc def");
        composer.move_left();
        composer.move_left();
        composer.kill_to_start();
        assert_eq!(composer.text(), "ef");
        composer.move_end();
        composer.yank();
        assert_eq!(composer.text(), "efabc d");
    }

    #[test]
    fn up_and_down_move_between_lines_before_history() {
        let mut composer = Composer::default();
        composer.insert_paste("first line\nsecond");
        assert!(composer.move_up());
        assert_eq!(composer.cursor(), 6);
        assert!(!composer.move_up());
        assert!(composer.move_down());
        assert_eq!(composer.cursor(), composer.text().len());
        assert!(!composer.move_down());
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
