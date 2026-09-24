//! The transcript, the in-flight cell, and the bottom pane. Drawing is the
//! only thing here that knows about a terminal; keys and notices are a state
//! machine a test can drive.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use crate::runtime::ApprovalChoice;

use super::composer::Composer;
use super::diff_render::{self, DiffKind};
use super::footer::{self, FooterInput, FooterMode};
use super::history::Cell;
use super::markdown::{self, Block as Md, Table};
use super::shimmer::{self, Emphasis};
use super::styles::{Palette, Swatch};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Enter,
    Backspace,
    Esc,
    Tab,
    Up,
    Down,
    Left,
    Right,
    PageUp,
    PageDown,
    Ctrl(char),
}

pub enum Effect {
    None,
    Submit(String),
    Choice(ApprovalChoice),
    Cancel,
    Quit,
}

#[derive(Clone)]
struct Active {
    name: String,
    arguments: String,
}

enum Overlay {
    None,
    Help,
    Tail,
    Approval {
        flagged: bool,
        tool: String,
        preview: String,
        concerns: Vec<String>,
    },
}

pub struct ChatWidget {
    history: Vec<Cell>,
    active: Option<Active>,
    composer: Composer,
    overlay: Overlay,
    running: bool,
    commentary: bool,
    thinking: bool,
    esc_armed: bool,
    quit_armed: bool,
    scroll: usize,
    frame: usize,
    live_tail: String,
    /// The answer as it arrives, before the finished cell replaces it.
    streaming: String,
    queued: Vec<String>,
    last_user: Option<String>,
    pub model: String,
    pub sandbox: String,
    palette: Palette,
}

impl ChatWidget {
    pub fn new(model: String, sandbox: String, palette: Palette) -> Self {
        Self {
            history: Vec::new(),
            active: None,
            composer: Composer::default(),
            overlay: Overlay::None,
            running: false,
            commentary: false,
            thinking: false,
            esc_armed: false,
            quit_armed: false,
            scroll: 0,
            frame: 0,
            live_tail: String::new(),
            streaming: String::new(),
            queued: Vec::new(),
            last_user: None,
            model,
            sandbox,
            palette,
        }
    }

    pub fn tick(&mut self) {
        self.frame = self.frame.wrapping_add(1);
    }

    pub fn disarm_quit(&mut self) {
        self.quit_armed = false;
    }

    pub fn quit_armed(&self) -> bool {
        self.quit_armed
    }

    pub fn take_queued(&mut self) -> Option<String> {
        match self.queued.is_empty() {
            true => None,
            false => Some(self.queued.remove(0)),
        }
    }

    pub fn on_key(&mut self, key: Key) -> Effect {
        if !matches!(key, Key::Esc | Key::Ctrl('c')) {
            self.esc_armed = false;
            self.quit_armed = false;
        }

        if let Overlay::Approval { flagged, .. } = &self.overlay {
            let flagged = *flagged;
            return match key {
                Key::Char('1') => self.answer(ApprovalChoice::Once),
                Key::Char('2') => self.answer(match flagged {
                    true => ApprovalChoice::Once,
                    false => ApprovalChoice::Always,
                }),
                Key::Char('3') | Key::Esc => self.answer(ApprovalChoice::No),
                _ => Effect::None,
            };
        }

        if matches!(self.overlay, Overlay::Help) {
            self.overlay = Overlay::None;
            return Effect::None;
        }

        if self.composer.searching().is_some() {
            return self.on_search(key);
        }

        match key {
            Key::Ctrl('c') => {
                if self.quit_armed {
                    return Effect::Quit;
                }
                self.quit_armed = true;
                Effect::None
            }
            Key::Esc => self.on_esc(),
            Key::Ctrl('t') => {
                self.overlay = match self.overlay {
                    Overlay::Tail => Overlay::None,
                    _ => Overlay::Tail,
                };
                Effect::None
            }
            Key::Char('?') if self.composer.is_empty() => {
                self.overlay = Overlay::Help;
                Effect::None
            }
            Key::Ctrl('r') => {
                self.composer.search_start();
                Effect::None
            }
            Key::Ctrl('k') => {
                self.composer.kill_to_end();
                Effect::None
            }
            Key::Ctrl('y') => {
                self.composer.yank();
                Effect::None
            }
            Key::Enter | Key::Tab => self.submit_or_queue(),
            Key::Char('\n') => {
                self.composer.newline();
                Effect::None
            }
            Key::Backspace => {
                self.composer.backspace();
                Effect::None
            }
            Key::Left => {
                self.composer.move_left();
                Effect::None
            }
            Key::Right => {
                self.composer.move_right();
                Effect::None
            }
            Key::Up => {
                self.composer.history_prev();
                Effect::None
            }
            Key::Down => {
                self.composer.history_next();
                Effect::None
            }
            Key::PageUp => {
                self.scroll = self.scroll.saturating_add(8);
                Effect::None
            }
            Key::PageDown => {
                self.scroll = self.scroll.saturating_sub(8);
                Effect::None
            }
            Key::Char(ch) => {
                self.composer.insert_char(ch);
                Effect::None
            }
            Key::Ctrl(_) => Effect::None,
        }
    }

    pub fn paste(&mut self, text: &str) {
        self.composer.insert_paste(text);
    }

    pub fn collapse_burst(&mut self, tail: &str) {
        self.composer.collapse_tail(tail);
    }

    pub fn thinking(&mut self) {
        self.running = true;
        self.thinking = true;
        self.commentary = false;
    }

    /// A piece of the answer. Shown as plain text: re-laying out markdown on
    /// every delta would reflow tables and code blocks as they grow, which
    /// reads worse than waiting. The finished cell renders properly.
    pub fn delta(&mut self, text: String) {
        self.thinking = false;
        self.commentary = true;
        self.streaming.push_str(&text);
    }

    pub fn assistant(&mut self, text: String) {
        self.commentary = true;
        self.thinking = false;
        // The finished answer replaces what was streamed, rather than being
        // appended to it.
        self.streaming.clear();
        self.push_assistant(text);
    }

    pub fn tool_start(&mut self, name: String, arguments: String) {
        // Commentary before a tool call has been delivered as its own
        // assistant cell already; anything still buffered was that text.
        self.streaming.clear();
        self.commentary = false;
        self.thinking = false;
        self.live_tail = arguments.clone();
        self.active = Some(Active { name, arguments });
    }

    pub fn tool_done(&mut self, name: String, output: String, failed: bool) {
        let arguments = self
            .active
            .take()
            .filter(|active| active.name == name)
            .map(|active| active.arguments)
            .unwrap_or_default();
        self.live_tail = output.clone();
        self.history
            .push(Cell::tool(name, arguments, output, failed));
    }

    pub fn finished(&mut self, ok: bool, text: String) {
        self.running = false;
        self.thinking = false;
        self.active = None;
        self.streaming.clear();
        if ok {
            self.scroll = 0;
            self.push_assistant(text);
        } else if !text.is_empty() {
            self.history.push(Cell::Error(text));
        }
    }

    pub fn draw(&mut self, frame: &mut Frame, area: Rect) {
        let chunks = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(composer_height(&self.composer)),
            Constraint::Length(1),
        ])
        .split(area);

        let lines = self.transcript_lines();
        let width = chunks[1].width as usize;
        let wrapped = wrap_lines(&lines, width.max(1));
        let height = chunks[1].height as usize;
        let start = view_start(wrapped.len(), height, self.scroll);
        let total = wrapped.len();
        let visible: Vec<_> = wrapped.into_iter().skip(start).take(height).collect();
        frame.render_widget(Paragraph::new(visible), chunks[1]);

        let header = format!(
            "token   {}   {}{}",
            self.model,
            self.sandbox,
            scroll_hint(start, height, total)
        );
        frame.render_widget(
            Paragraph::new(Span::styled(header, paint(self.palette.brand))),
            chunks[0],
        );

        let title = match self.composer.searching() {
            Some(query) => format!("search {query}"),
            None => "message".to_string(),
        };
        let draft = Paragraph::new(self.composer.text()).block(
            Block::default()
                .borders(Borders::ALL)
                .title(Span::styled(title, paint(self.palette.brand)))
                .border_style(paint(self.palette.muted)),
        );
        frame.render_widget(draft, chunks[2]);

        // ratatui hides the cursor unless a frame asks for it. Without this
        // the caret is invisible, and an editor with no caret reads as one
        // that ignores the arrow keys even though it does not.
        if matches!(self.overlay, Overlay::None) {
            let (row, column) = caret(self.composer.text(), self.composer.cursor());
            let inner_width = chunks[2].width.saturating_sub(2);
            let inner_height = chunks[2].height.saturating_sub(2);
            frame.set_cursor_position((
                chunks[2].x + 1 + column.min(inner_width.saturating_sub(1)),
                chunks[2].y + 1 + row.min(inner_height.saturating_sub(1)),
            ));
        }

        let mode = footer_mode(self);
        let pet = shimmer::pet_frame(self.frame / 8);
        let footer = footer::footer_line(&FooterInput {
            width: chunks[3].width as usize,
            mode,
            quit_armed: self.quit_armed,
            queued: self.queued.len(),
            model: &self.model,
            sandbox: &self.sandbox,
            pet,
            show_pet: !self.running && matches!(mode, FooterMode::Empty | FooterMode::Draft),
        });
        frame.render_widget(
            Paragraph::new(Span::styled(footer, paint(self.palette.muted))),
            chunks[3],
        );

        match &self.overlay {
            Overlay::None => {}
            Overlay::Help => self.popup(frame, area, "keys", keymap()),
            Overlay::Tail => self.popup(frame, area, "live tail", &self.live_tail),
            Overlay::Approval {
                flagged,
                tool,
                preview,
                concerns,
            } => {
                let body = approval_body(*flagged, tool, preview, concerns);
                self.popup(frame, area, "approve", &body);
            }
        }
    }

    fn on_esc(&mut self) -> Effect {
        if self.esc_armed {
            self.esc_armed = false;
            if self.running {
                return Effect::Cancel;
            }
            self.composer.clear();
            return Effect::None;
        }
        self.esc_armed = true;
        if let Some(previous) = &self.last_user {
            self.composer.load(previous);
        }
        Effect::None
    }

    fn on_search(&mut self, key: Key) -> Effect {
        match key {
            Key::Esc => self.composer.search_cancel(),
            Key::Enter => {
                self.composer.search_accept();
            }
            Key::Backspace => self.composer.search_backspace(),
            Key::Char(ch) => self.composer.search_push(ch),
            _ => {}
        }
        Effect::None
    }

    fn submit_or_queue(&mut self) -> Effect {
        if self.composer.is_empty() {
            return Effect::None;
        }
        let text = self.composer.commit();
        self.last_user = Some(text.clone());
        if self.running {
            self.queued.push(text);
            Effect::None
        } else {
            self.history.push(Cell::User(text.clone()));
            self.running = true;
            Effect::Submit(text)
        }
    }

    fn answer(&mut self, choice: ApprovalChoice) -> Effect {
        self.overlay = Overlay::None;
        Effect::Choice(choice)
    }

    pub fn ask(&mut self, tool: String, preview: String, concerns: Vec<String>, flagged: bool) {
        self.overlay = Overlay::Approval {
            flagged,
            tool,
            preview,
            concerns,
        };
    }

    fn push_assistant(&mut self, text: String) {
        if self
            .history
            .last()
            .is_some_and(|cell| matches!(cell, Cell::Assistant(have) if have == &text))
        {
            return;
        }
        self.history.push(Cell::Assistant(text));
    }

    fn transcript_lines(&self) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        for cell in &self.history {
            lines.extend(render_cell(cell, &self.palette));
            lines.push(Line::raw(""));
        }
        if !self.streaming.is_empty() {
            for line in self.streaming.split('\n') {
                lines.push(Line::raw(line.to_string()));
            }
            lines.push(Line::raw(""));
        }
        if self.thinking && !self.commentary && self.active.is_none() {
            lines.push(shimmer_line("thinking", self.frame, self.palette.brand));
        }
        if let Some(active) = &self.active {
            lines.push(shimmer_line(
                &format!("→ {}", active.name),
                self.frame,
                self.palette.user,
            ));
            lines.push(Line::styled(
                active.arguments.clone(),
                paint(self.palette.muted),
            ));
        }
        lines
    }

    fn popup(&self, frame: &mut Frame, area: Rect, title: &str, body: &str) {
        let width = area.width.clamp(20, 72);
        let height = (body.lines().count() as u16 + 2).clamp(4, area.height.saturating_sub(2));
        let x = area.x + area.width.saturating_sub(width) / 2;
        let y = area.y + area.height.saturating_sub(height) / 2;
        let rect = Rect::new(x, y, width.min(area.width), height.min(area.height));
        frame.render_widget(Clear, rect);
        frame.render_widget(
            Paragraph::new(body).wrap(Wrap { trim: false }).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(Span::styled(title.to_string(), paint(self.palette.brand))),
            ),
            rect,
        );
    }
}

/// Where the visible window starts.
///
/// `offset` is measured up from the bottom, so zero is the end of the
/// transcript and the view stays there as output arrives. Anchoring to the
/// start of the turn instead made a finished answer look truncated: the
/// screen filled with its first lines and stopped, which reads as a reply
/// that gave up rather than one that did not fit.
/// Says which way the hidden lines are, and only when some are hidden. The
/// old hint pointed down while the view was pinned to the top, which told the
/// reader the answer continued when what it meant was that it had scrolled.
fn scroll_hint(start: usize, height: usize, total: usize) -> String {
    let above = start > 0;
    let below = start + height < total;
    match (above, below) {
        (true, true) => "    pgup/pgdn for more".to_string(),
        (true, false) => "    pgup for earlier".to_string(),
        (false, true) => "    pgdn for the rest".to_string(),
        (false, false) => String::new(),
    }
}

fn view_start(len: usize, height: usize, offset: usize) -> usize {
    if height == 0 || len <= height {
        return 0;
    }
    (len - height).saturating_sub(offset)
}

fn wrap_lines(lines: &[Line<'_>], width: usize) -> Vec<Line<'static>> {
    let mut wrapped = Vec::new();
    for line in lines {
        wrapped.extend(wrap_one(line, width));
    }
    wrapped
}

fn wrap_one(line: &Line<'_>, width: usize) -> Vec<Line<'static>> {
    let text: String = line
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect();
    let style = line
        .spans
        .first()
        .map(|span| span.style)
        .unwrap_or_default();
    if text.is_empty() || width == 0 {
        return vec![Line::styled(text, style)];
    }
    let mut rows = Vec::new();
    let mut rest = text.as_str();
    while !rest.is_empty() {
        let take = rest
            .chars()
            .take(width)
            .map(char::len_utf8)
            .sum::<usize>()
            .max(1);
        let take = take.min(rest.len());
        let (head, tail) = rest.split_at(take);
        rows.push(Line::styled(head.to_string(), style));
        rest = tail;
    }
    rows
}

/// Where the caret sits in the draft, as a row and column of characters.
/// Columns count characters rather than bytes, or a caret after any non-ASCII
/// text would be drawn too far right.
fn caret(text: &str, cursor: usize) -> (u16, u16) {
    let before = &text[..cursor.min(text.len())];
    let row = before.matches('\n').count() as u16;
    let column = before.rsplit('\n').next().unwrap_or("").chars().count() as u16;
    (row, column)
}

fn composer_height(composer: &Composer) -> u16 {
    let rows = composer.text().lines().count().max(1) as u16;
    (rows + 2).clamp(3, 8)
}

fn footer_mode(widget: &ChatWidget) -> FooterMode {
    if matches!(widget.overlay, Overlay::Approval { .. }) {
        FooterMode::Approval
    } else if widget.running {
        FooterMode::Running
    } else if widget.composer.is_empty() {
        FooterMode::Empty
    } else {
        FooterMode::Draft
    }
}

fn keymap() -> &'static str {
    "\
enter      send, or queue while a task is running\n\
tab        send, or queue while a task is running\n\
esc        edit the previous task\n\
esc esc    interrupt the running task\n\
ctrl-c     press twice to quit\n\
ctrl-t     live tail of the tool in flight\n\
ctrl-r     search history\n\
ctrl-k     kill to end of line\n\
ctrl-y     yank the kill buffer\n\
?          this overlay, when the composer is empty"
}

fn approval_body(flagged: bool, tool: &str, preview: &str, concerns: &[String]) -> String {
    let mut body = format!("{tool}\n{preview}\n");
    for concern in concerns {
        body.push_str(&format!("! {concern}\n"));
    }
    body.push_str("\n1) Allow Once\n");
    if flagged {
        body.push_str("2) Allow Always - not available here, this must be answered each time\n");
    } else {
        body.push_str("2) Allow Always - allows every change for the rest of this session\n");
    }
    body.push_str("3) No");
    body
}

fn render_cell(cell: &Cell, palette: &Palette) -> Vec<Line<'static>> {
    match cell {
        Cell::User(text) => vec![Line::from(vec![
            Span::styled("you  ", paint(palette.user).add_modifier(Modifier::BOLD)),
            Span::styled(text.clone(), paint(palette.user)),
        ])],
        Cell::Assistant(text) => {
            if diff_render::looks_like_diff(text) {
                diff_lines(text, palette)
            } else {
                markdown_lines(text, palette)
            }
        }
        Cell::Tool {
            name,
            arguments,
            output,
            failed,
        } => {
            let role = match failed {
                true => palette.error,
                false => palette.success,
            };
            let mut lines = vec![Line::styled(format!("→ {name}"), paint(role))];
            if !arguments.is_empty() {
                lines.push(Line::styled(arguments.clone(), paint(palette.muted)));
            }
            if diff_render::looks_like_diff(output) {
                lines.extend(diff_lines(output, palette));
            } else if !output.is_empty() {
                for line in output.lines().take(12) {
                    lines.push(Line::raw(line.to_string()));
                }
            }
            lines
        }
        Cell::Error(text) => vec![Line::styled(text.clone(), paint(palette.error))],
    }
}

fn markdown_lines(text: &str, palette: &Palette) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for block in markdown::parse(text, 80) {
        match block {
            Md::Heading { text, .. } => {
                lines.push(Line::styled(
                    text,
                    paint(palette.brand).add_modifier(Modifier::BOLD),
                ));
            }
            Md::Paragraph(text) => lines.push(Line::raw(rewrite_links(&text))),
            Md::Bullet(items) => {
                for item in items {
                    lines.push(Line::raw(format!("• {item}")));
                }
            }
            Md::Code { body, .. } => {
                for line in body.lines() {
                    lines.push(Line::styled(line.to_string(), paint(palette.user)));
                }
            }
            Md::Table(table) => lines.extend(table_lines(&table, palette)),
        }
    }
    if lines.is_empty() {
        lines.push(Line::raw(text.to_string()));
    }
    lines
}

fn rewrite_links(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find('[') {
        out.push_str(&rest[..start]);
        let Some(end) = rest[start..].find(')') else {
            out.push_str(&rest[start..]);
            break;
        };
        let token = &rest[start..start + end + 1];
        match markdown::link_target(token, ".") {
            Some(path) => out.push_str(&path),
            None => out.push_str(token),
        }
        rest = &rest[start + end + 1..];
    }
    out.push_str(rest);
    out
}

fn table_lines(table: &Table, palette: &Palette) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if table.pairs {
        for row in &table.rows {
            for (header, cell) in table.headers.iter().zip(row.iter()) {
                lines.push(Line::from(vec![
                    Span::styled(format!("{header}: "), paint(palette.brand)),
                    Span::raw(cell.clone()),
                ]));
            }
            lines.push(Line::raw(""));
        }
    } else {
        lines.push(Line::styled(table.headers.join("  "), paint(palette.brand)));
        for row in &table.rows {
            lines.push(Line::raw(row.join("  ")));
        }
    }
    for extra in &table.spillover {
        lines.push(Line::styled(extra.clone(), paint(palette.muted)));
    }
    lines
}

fn diff_lines(text: &str, palette: &Palette) -> Vec<Line<'static>> {
    diff_render::parse_diff(text)
        .into_iter()
        .map(|line| {
            let (fg, bg) = match line.kind {
                DiffKind::Add => (palette.diff_add, palette.diff_add_bg),
                DiffKind::Del => (palette.diff_del, palette.diff_del_bg),
                DiffKind::Hunk => (palette.user, Swatch::Default),
                DiffKind::Meta => (palette.muted, Swatch::Default),
                DiffKind::Context => (Swatch::Default, Swatch::Default),
            };
            Line::styled(line.text, paint(fg).bg(color_of(bg)))
        })
        .collect()
}

fn shimmer_line(text: &str, phase: usize, swatch: Swatch) -> Line<'static> {
    let chars: Vec<char> = text.chars().collect();
    let spans = chars
        .iter()
        .enumerate()
        .map(|(index, ch)| {
            let style = match shimmer::emphasis_at(chars.len(), index, phase / 2) {
                Emphasis::Bold => paint(swatch).add_modifier(Modifier::BOLD),
                Emphasis::Dim => paint(swatch).add_modifier(Modifier::DIM),
                Emphasis::Normal => paint(swatch),
            };
            Span::styled(ch.to_string(), style)
        })
        .collect::<Vec<_>>();
    Line::from(spans)
}

fn paint(swatch: Swatch) -> Style {
    Style::default().fg(color_of(swatch))
}

fn color_of(swatch: Swatch) -> Color {
    match swatch {
        Swatch::Default => Color::Reset,
        Swatch::Green => Color::Green,
        Swatch::Red => Color::Red,
        Swatch::Cyan => Color::Cyan,
        Swatch::Magenta => Color::Magenta,
        Swatch::Indexed(index) => Color::Indexed(index),
        Swatch::Rgb(red, green, blue) => Color::Rgb(red, green, blue),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::styles::{Depth, Theme, palette};

    fn widget() -> ChatWidget {
        ChatWidget::new(
            "gpt-5.5".to_string(),
            "workspace-write".to_string(),
            palette(Theme::Dark, Depth::Ansi16),
        )
    }

    #[test]
    fn enter_submits_and_tab_queues_while_running() {
        let mut widget = widget();
        widget.paste("fix the test");
        let effect = widget.on_key(Key::Enter);
        assert!(matches!(effect, Effect::Submit(text) if text == "fix the test"));
        widget.paste("and the next one");
        assert!(matches!(widget.on_key(Key::Tab), Effect::None));
        assert_eq!(widget.take_queued().as_deref(), Some("and the next one"));
    }

    #[test]
    fn esc_edits_the_previous_task_then_interrupts() {
        let mut widget = widget();
        widget.paste("first");
        widget.on_key(Key::Enter);
        assert!(matches!(widget.on_key(Key::Esc), Effect::None));
        assert_eq!(widget.composer.text(), "first");
        assert!(matches!(widget.on_key(Key::Esc), Effect::Cancel));
    }

    #[test]
    fn a_tool_shows_before_it_finishes_and_commentary_hides_thinking() {
        let mut widget = widget();
        widget.thinking();
        widget.assistant("looking now".to_string());
        assert!(widget.commentary);
        widget.tool_start("terminal".to_string(), "cargo test".to_string());
        assert!(widget.active.is_some());
        assert!(!widget.commentary);
        widget.tool_done("terminal".to_string(), "ok".to_string(), false);
        assert!(widget.active.is_none());
        assert!(
            widget
                .history
                .iter()
                .any(|cell| matches!(cell, Cell::Tool { .. }))
        );
    }

    #[test]
    fn the_approval_menu_keeps_its_numbers() {
        let mut widget = widget();
        widget.ask(
            "terminal".into(),
            "rm .env".into(),
            vec!["secret".into()],
            true,
        );
        let body = approval_body(true, "terminal", "rm .env", &["secret".into()]);
        assert!(body.contains("1) Allow Once"));
        assert!(body.contains("2) Allow Always - not available"));
        assert!(body.contains("3) No"));
        assert!(matches!(
            widget.on_key(Key::Char('3')),
            Effect::Choice(ApprovalChoice::No)
        ));
    }

    #[test]
    fn the_caret_tracks_the_draft() {
        assert_eq!(caret("", 0), (0, 0));
        assert_eq!(caret("hello", 5), (0, 5));
        assert_eq!(caret("hello", 2), (0, 2));
        assert_eq!(caret("one\ntwo", 7), (1, 3));
        assert_eq!(caret("one\ntwo", 4), (1, 0));
    }

    #[test]
    fn the_caret_counts_characters_not_bytes() {
        // Six bytes, two characters: a byte column would put the caret four
        // cells past where the text ends.
        assert_eq!(caret("héllo", "héllo".len()), (0, 5));
        assert_eq!(caret("✓✓", "✓✓".len()), (0, 2));
    }

    #[test]
    fn the_question_is_in_the_transcript_before_the_answer() {
        let mut widget = widget();
        widget.paste("what is this");
        let _ = widget.on_key(Key::Enter);
        widget.delta("an answer".into());

        let text: Vec<String> = widget
            .transcript_lines()
            .iter()
            .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        let question = text.iter().position(|line| line.contains("what is this"));
        let answer = text.iter().position(|line| line.contains("an answer"));
        assert!(question.is_some(), "the question is missing: {text:?}");
        assert!(
            answer > question,
            "the answer must follow the question: {text:?}"
        );
    }

    #[test]
    fn the_view_sits_at_the_end_of_the_transcript() {
        // Reversed deliberately. Opening at the question meant a long answer
        // filled the screen with its first lines and stopped, which reads as
        // a reply that gave up rather than one that did not fit.
        assert_eq!(view_start(100, 10, 0), 90);
    }

    #[test]
    fn paging_up_walks_back_from_the_end() {
        assert_eq!(view_start(100, 10, 8), 82);
        assert_eq!(view_start(100, 10, 90), 0);
    }

    #[test]
    fn paging_up_stops_at_the_beginning() {
        // Without the saturating subtraction this wraps and the view jumps
        // to the far end of the transcript.
        assert_eq!(view_start(100, 10, 500), 0);
    }

    #[test]
    fn a_transcript_that_fits_is_not_scrolled() {
        assert_eq!(view_start(5, 10, 0), 0);
        assert_eq!(view_start(5, 10, 3), 0);
        assert_eq!(view_start(0, 0, 0), 0);
    }

    #[test]
    fn the_hint_points_where_the_hidden_lines_are() {
        // The old hint always pointed down, while the view was pinned to the
        // top: it said the answer continued when it meant it had scrolled.
        assert_eq!(scroll_hint(0, 10, 10), "");
        assert!(scroll_hint(0, 10, 50).contains("pgdn"));
        assert!(scroll_hint(40, 10, 50).contains("pgup"));
        let middle = scroll_hint(20, 10, 50);
        assert!(
            middle.contains("pgup") && middle.contains("pgdn"),
            "{middle}"
        );
    }
}
