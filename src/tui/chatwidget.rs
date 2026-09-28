//! The transcript, the in-flight cell, and the bottom pane. Drawing is the
//! only thing here that knows about a terminal; keys and notices are a state
//! machine a test can drive.

use std::time::{Duration, Instant};

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Clear, Padding, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::runtime::ApprovalChoice;

use super::composer::Composer;
use super::diff_render;
use super::footer::{self, FooterInput, FooterMode};
use super::highlight;
use super::history::Cell;
use super::render::{self, Row, badge, muted, on, paint};
use super::shimmer::{self, Emphasis};
use super::styles::{Depth, Palette, Swatch};

/// How long the caret stays solid after a key, before it starts blinking.
/// A caret that blinks out mid-word reads as a dropped keystroke.
const CARET_SOLID: Duration = Duration::from_millis(500);
const CARET_PERIOD_MS: u128 = 530;
/// Rows of draft shown before the composer scrolls.
const COMPOSER_ROWS: usize = 6;
/// Lines of tool output kept in the transcript; ctrl-t shows the rest.
const TOOL_OUTPUT_LINES: usize = 8;
const INDENT: usize = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Enter,
    Backspace,
    Delete,
    Esc,
    Tab,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    WordLeft,
    WordRight,
    DeleteWord,
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
    /// When the caret last had a reason to be solid.
    caret_epoch: Instant,
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
            caret_epoch: Instant::now(),
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
        self.caret_epoch = Instant::now();
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
            Key::Ctrl('u') => {
                self.composer.kill_to_start();
                Effect::None
            }
            Key::Ctrl('y') => {
                self.composer.yank();
                Effect::None
            }
            Key::Ctrl('a') | Key::Home => {
                self.composer.move_home();
                Effect::None
            }
            Key::Ctrl('e') | Key::End => {
                self.composer.move_end();
                Effect::None
            }
            Key::Ctrl('w') | Key::DeleteWord => {
                self.composer.delete_word_back();
                Effect::None
            }
            Key::WordLeft => {
                self.composer.move_word_left();
                Effect::None
            }
            Key::WordRight => {
                self.composer.move_word_right();
                Effect::None
            }
            Key::Enter | Key::Tab => self.submit_or_queue(),
            Key::Char('\n') | Key::Ctrl('j') => {
                self.composer.newline();
                Effect::None
            }
            Key::Backspace | Key::Ctrl('h') => {
                self.composer.backspace();
                Effect::None
            }
            Key::Delete => {
                self.composer.delete_forward();
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
                if !self.composer.move_up() {
                    self.composer.history_prev();
                }
                Effect::None
            }
            Key::Down => {
                if !self.composer.move_down() {
                    self.composer.history_next();
                }
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
        self.caret_epoch = Instant::now();
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
        let composer_height = self.composer_height(area.width);
        let chunks = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(composer_height),
            Constraint::Length(1),
        ])
        .split(area);

        let hint = self.draw_transcript(frame, chunks[1]);
        self.draw_header(frame, chunks[0], &hint);
        self.draw_composer(frame, chunks[2]);
        self.draw_footer(frame, chunks[3]);

        match &self.overlay {
            Overlay::None => {}
            Overlay::Help => {
                let rows = help_rows(&self.palette);
                self.popup(frame, area, "keys", self.palette.brand, rows);
            }
            Overlay::Tail => {
                let rows = self.tail_rows(area.height.saturating_sub(6) as usize);
                self.popup(frame, area, "live tail", self.palette.info, rows);
            }
            Overlay::Approval {
                flagged,
                tool,
                preview,
                concerns,
            } => {
                let room = area.height.saturating_sub(14) as usize;
                let rows = approval_rows(
                    *flagged,
                    tool,
                    preview,
                    concerns,
                    room.max(3),
                    &self.palette,
                );
                let border = match flagged {
                    true => self.palette.error,
                    false => self.palette.warning,
                };
                self.popup(frame, area, "approval needed", border, rows);
            }
        }
    }

    /// The transcript, pinned to its end unless paged back. Returns the
    /// scroll hint for the header.
    fn draw_transcript(&self, frame: &mut Frame, area: Rect) -> String {
        let body = Rect {
            x: area.x + 1,
            y: area.y + 1,
            width: area.width.saturating_sub(3),
            height: area.height.saturating_sub(1),
        };
        let width = body.width as usize;
        let height = body.height as usize;

        if self.history.is_empty() && self.streaming.is_empty() && !self.running {
            let lines = welcome_lines(&self.palette, &self.model, &self.sandbox, self.frame);
            let top = height.saturating_sub(lines.len()) / 2;
            let mut padded = vec![Line::raw(""); top];
            padded.extend(lines);
            frame.render_widget(
                Paragraph::new(padded).alignment(ratatui::layout::Alignment::Center),
                body,
            );
            return String::new();
        }

        let lines = self.transcript_lines(width.max(1));
        let total = lines.len();
        let start = view_start(total, height, self.scroll);
        let visible: Vec<_> = lines.into_iter().skip(start).take(height).collect();
        frame.render_widget(Paragraph::new(visible), body);

        if total > height && area.width > 2 {
            let mut state = ScrollbarState::new(total.saturating_sub(height)).position(start);
            let bar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None)
                .track_symbol(Some("│"))
                .thumb_symbol("┃")
                .track_style(paint(self.palette.border))
                .thumb_style(paint(self.palette.brand));
            frame.render_stateful_widget(
                bar,
                Rect {
                    width: body.width + 1,
                    ..body
                },
                &mut state,
            );
        }
        scroll_hint(start, height, total)
    }

    fn draw_header(&self, frame: &mut Frame, area: Rect, hint: &str) {
        let palette = &self.palette;
        let phase = match self.running {
            true => self.frame as f32 * 0.03,
            false => 0.0,
        };
        let mut spans = vec![Span::raw(" ")];
        spans.extend(render::gradient_spans(
            "◆ token",
            palette,
            phase,
            Modifier::BOLD,
        ));
        spans.push(Span::styled("  │  ", paint(palette.border)));
        spans.push(Span::styled("◇ ", paint(palette.info)));
        spans.push(Span::styled(
            self.model.clone(),
            paint(palette.info).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled("  │  ", paint(palette.border)));
        let sandbox_swatch = match self.sandbox.as_str() {
            "off" => palette.error,
            "read-only" => palette.info,
            _ => palette.success,
        };
        spans.push(Span::styled("▣ ", paint(sandbox_swatch)));
        spans.push(Span::styled(self.sandbox.clone(), paint(sandbox_swatch)));
        let hint = hint.trim();
        if !hint.is_empty() {
            let used = render::width_of(&spans);
            let pad = (area.width as usize).saturating_sub(used + hint.width() + 1);
            spans.push(Span::raw(" ".repeat(pad.max(2))));
            spans.push(Span::styled(hint.to_string(), muted(palette)));
        }
        frame.render_widget(
            Paragraph::new(Line::from(spans)).style(on(palette.surface)),
            area,
        );
    }

    fn draw_composer(&self, frame: &mut Frame, area: Rect) {
        let palette = &self.palette;
        let focused = matches!(self.overlay, Overlay::None);
        let title = match self.composer.searching() {
            Some(_) => " search history ",
            None => " message ",
        };
        let right = match (self.running, self.queued.len()) {
            (true, 0) => " working · type to queue ".to_string(),
            (true, queued) => format!(" {queued} queued "),
            (false, _) if !self.composer.is_empty() => {
                " enter ⏎ send · alt+enter newline ".to_string()
            }
            (false, _) => String::new(),
        };
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(paint(palette.border))
            .title(Line::from(Span::styled(
                title,
                paint(palette.brand).add_modifier(Modifier::BOLD),
            )))
            .title_top(Line::from(Span::styled(right, muted(palette))).right_aligned());
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if focused {
            self.tint_border(frame, area);
        }
        if inner.width < 3 || inner.height == 0 {
            return;
        }

        let text_width = inner.width.saturating_sub(2) as usize;
        let caret_on = focused && self.caret_visible();
        let caret_style = paint(palette.brand).add_modifier(Modifier::REVERSED);
        let prompt = Span::styled("❯ ", paint(palette.brand).add_modifier(Modifier::BOLD));

        if let Some(query) = self.composer.searching() {
            let mut spans = vec![
                prompt,
                Span::styled(
                    "search ",
                    paint(palette.accent).add_modifier(Modifier::BOLD),
                ),
                Span::raw(query.to_string()),
            ];
            spans.push(Span::styled(
                " ",
                if caret_on {
                    caret_style
                } else {
                    Style::default()
                },
            ));
            match self.composer.search_preview() {
                Some(preview) => {
                    spans.push(Span::styled("  → ", muted(palette)));
                    spans.push(Span::styled(preview.replace('\n', " ⏎ "), muted(palette)));
                }
                None if !query.is_empty() => {
                    spans.push(Span::styled("  no match", paint(palette.error)));
                }
                None => {}
            }
            frame.render_widget(Paragraph::new(Line::from(spans)), inner);
            return;
        }

        if self.composer.is_empty() {
            let placeholder = match self.running {
                true => "token is working. Type to queue a follow-up",
                false => "Ask token anything, or press ? for keys",
            };
            let caret = Span::styled(
                " ",
                if caret_on {
                    caret_style
                } else {
                    Style::default()
                },
            );
            let line = Line::from(vec![
                prompt,
                caret,
                Span::styled(placeholder, muted(palette).add_modifier(Modifier::ITALIC)),
            ]);
            frame.render_widget(Paragraph::new(line), inner);
            return;
        }

        let (rows, (caret_row, caret_col)) =
            layout_draft(self.composer.text(), self.composer.cursor(), text_width);
        let height = inner.height as usize;
        let top = (caret_row + 1).saturating_sub(height);
        let lines: Vec<Line> = rows
            .iter()
            .enumerate()
            .skip(top)
            .take(height)
            .map(|(index, (text, starts_line))| {
                let lead = match (index, starts_line) {
                    (0, _) => prompt.clone(),
                    (_, true) => Span::styled("┆ ", paint(palette.border)),
                    (_, false) => Span::raw("  "),
                };
                let mut spans = vec![lead];
                if index == caret_row {
                    let (before, under, after) = split_at_column(text, caret_col);
                    spans.push(Span::styled(before, paint(palette.text)));
                    let under = if under.is_empty() {
                        " ".to_string()
                    } else {
                        under
                    };
                    spans.push(Span::styled(
                        under,
                        if caret_on {
                            caret_style
                        } else {
                            paint(palette.text)
                        },
                    ));
                    spans.push(Span::styled(after, paint(palette.text)));
                } else {
                    spans.push(Span::styled(text.clone(), paint(palette.text)));
                }
                Line::from(spans)
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), inner);
    }

    /// Paints the composer's frame along the brand gradient. Only border
    /// glyphs are recoloured, so the titles keep their own colours. While a
    /// task runs the gradient drifts, which says "busy" without a word.
    fn tint_border(&self, frame: &mut Frame, area: Rect) {
        if area.width < 2 || area.height < 2 {
            return;
        }
        let phase = match self.running {
            true => self.frame as f32 * 0.02,
            false => 0.0,
        };
        let span = (area.width - 1) as f32;
        let buffer = frame.buffer_mut();
        for y in [area.y, area.bottom() - 1] {
            for x in area.x..area.right() {
                let t = (x - area.x) as f32 / span;
                let t = if phase == 0.0 {
                    t
                } else {
                    render::wave(t * 0.6 + phase)
                };
                let color = render::color_of(self.palette.gradient(t));
                if let Some(cell) = buffer.cell_mut((x, y)) {
                    if matches!(cell.symbol(), "─" | "╭" | "╮" | "╰" | "╯") {
                        cell.set_style(Style::default().fg(color));
                    }
                }
            }
        }
        for y in area.y + 1..area.bottom() - 1 {
            for (x, t) in [(area.x, 0.0), (area.right() - 1, 1.0)] {
                let t = if phase == 0.0 {
                    t
                } else {
                    render::wave(t * 0.6 + phase)
                };
                let color = render::color_of(self.palette.gradient(t));
                if let Some(cell) = buffer.cell_mut((x, y)) {
                    cell.set_style(Style::default().fg(color));
                }
            }
        }
    }

    fn draw_footer(&self, frame: &mut Frame, area: Rect) {
        let palette = &self.palette;
        let mode = footer_mode(self);
        let label = match (self.quit_armed, mode) {
            (true, _) => (" QUIT? ".to_string(), palette.error),
            (_, FooterMode::Running) => (
                format!(" {} WORKING ", shimmer::spinner(self.frame)),
                palette.brand,
            ),
            (_, FooterMode::Approval) => (" APPROVE ".to_string(), palette.warning),
            _ => (" READY ".to_string(), palette.success),
        };
        let pill = badge(&label.0, label.1, palette);
        let pill_width = pill.content.width() + 1;
        let pet = shimmer::pet_frame(self.frame / 8);
        let text = footer::footer_line(&FooterInput {
            width: (area.width as usize).saturating_sub(pill_width + 1),
            mode,
            quit_armed: self.quit_armed,
            queued: self.queued.len(),
            model: &self.model,
            sandbox: &self.sandbox,
            pet,
            show_pet: !self.running && matches!(mode, FooterMode::Empty | FooterMode::Draft),
        });
        let line = Line::from(vec![
            Span::raw(" "),
            pill,
            Span::raw(" "),
            Span::styled(text, muted(palette)),
        ]);
        frame.render_widget(Paragraph::new(line), area);
    }

    fn caret_visible(&self) -> bool {
        let elapsed = self.caret_epoch.elapsed();
        elapsed < CARET_SOLID || (elapsed.as_millis() / CARET_PERIOD_MS) % 2 == 0
    }

    fn composer_height(&self, width: u16) -> u16 {
        let text_width = width.saturating_sub(4) as usize;
        let rows = match self.composer.searching() {
            Some(_) => 1,
            None => layout_draft(self.composer.text(), self.composer.cursor(), text_width)
                .0
                .len(),
        };
        rows.clamp(1, COMPOSER_ROWS) as u16 + 2
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
            Key::Backspace | Key::Ctrl('h') => self.composer.search_backspace(),
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
            self.scroll = 0;
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

    fn transcript_lines(&self, width: usize) -> Vec<Line<'static>> {
        render::lay_out(&self.transcript_rows(width), width)
    }

    fn transcript_rows(&self, width: usize) -> Vec<Row> {
        let palette = &self.palette;
        let mut rows = Vec::new();
        // The name goes on the first reply after each question, not on every
        // piece of commentary between tool calls.
        let mut needs_label = true;
        for cell in &self.history {
            match cell {
                Cell::User(text) => rows.extend(user_rows(text, palette)),
                Cell::Assistant(text) => {
                    if needs_label {
                        rows.push(assistant_label(palette));
                    }
                    if diff_render::looks_like_diff(text) {
                        rows.extend(render::diff_rows(text, palette, INDENT));
                    } else {
                        rows.extend(render::markdown_rows(text, width, palette, INDENT));
                    }
                }
                Cell::Tool {
                    name,
                    arguments,
                    output,
                    failed,
                } => rows.extend(tool_rows(name, arguments, output, *failed, palette)),
                Cell::Error(text) => {
                    let mut body = vec![badge("ERROR", palette.error, palette), Span::raw(" ")];
                    body.push(Span::styled(text.clone(), paint(palette.error)));
                    rows.push(Row::new(body).prefix(vec![Span::raw("  ")]));
                }
            }
            rows.push(Row::blank());
            needs_label = matches!(cell, Cell::User(_));
        }

        if !self.streaming.is_empty() {
            if needs_label {
                rows.push(assistant_label(palette));
            }
            let lines: Vec<&str> = self.streaming.split('\n').collect();
            let last = lines.len() - 1;
            for (index, line) in lines.into_iter().enumerate() {
                let mut body = vec![Span::styled(line.to_string(), paint(palette.text))];
                if index == last && (self.frame / 4) % 2 == 0 {
                    body.push(Span::styled("▌", paint(palette.accent)));
                }
                rows.push(Row::new(body).prefix(vec![Span::raw("  ")]));
            }
            rows.push(Row::blank());
            needs_label = false;
        }
        if self.thinking && !self.commentary && self.active.is_none() {
            if needs_label {
                rows.push(assistant_label(palette));
            }
            let mut body = vec![Span::styled(
                format!("{} ", shimmer::spinner(self.frame)),
                paint(palette.accent),
            )];
            body.extend(shimmer_spans("thinking…", self.frame, palette.brand));
            rows.push(Row::new(body).prefix(vec![Span::raw("  ")]));
        }
        if let Some(active) = &self.active {
            let mut body = vec![Span::styled(
                format!("{} ", shimmer::spinner(self.frame)),
                paint(palette.info),
            )];
            body.extend(shimmer_spans(&active.name, self.frame, palette.info));
            let summary = render::tool_summary(&active.arguments);
            if !summary.is_empty() {
                body.push(Span::styled(format!("  {summary}"), muted(palette)));
            }
            rows.push(Row::new(body).hanging(vec![Span::raw("  ")], vec![Span::raw("    ")]));
        }
        rows
    }

    fn tail_rows(&self, room: usize) -> Vec<Row> {
        let lines: Vec<&str> = self.live_tail.lines().collect();
        if lines.is_empty() {
            return vec![Row::new(vec![Span::styled(
                "nothing has run yet",
                muted(&self.palette).add_modifier(Modifier::ITALIC),
            )])];
        }
        let skip = lines.len().saturating_sub(room.max(1));
        let mut rows = Vec::new();
        if skip > 0 {
            rows.push(Row::new(vec![Span::styled(
                format!("… {skip} earlier lines"),
                muted(&self.palette).add_modifier(Modifier::ITALIC),
            )]));
        }
        rows.extend(
            lines[skip..]
                .iter()
                .map(|line| Row::new(vec![Span::raw(line.to_string())]).hard()),
        );
        rows
    }

    fn popup(&self, frame: &mut Frame, area: Rect, title: &str, border: Swatch, rows: Vec<Row>) {
        let width = area.width.saturating_sub(4).clamp(20, 80).min(area.width);
        let inner_width = width.saturating_sub(4) as usize;
        let lines = render::lay_out(&rows, inner_width.max(1));
        let height = (lines.len() as u16 + 2).clamp(4, area.height.saturating_sub(2).max(4));
        let x = area.x + area.width.saturating_sub(width) / 2;
        let y = area.y + area.height.saturating_sub(height) / 2;
        let rect = Rect::new(x, y, width, height.min(area.height));
        frame.render_widget(Clear, rect);
        frame.render_widget(
            Paragraph::new(lines).block(
                Block::bordered()
                    .border_type(BorderType::Rounded)
                    .border_style(paint(border))
                    .padding(Padding::horizontal(1))
                    .title(Line::from(Span::styled(
                        format!(" {title} "),
                        paint(border).add_modifier(Modifier::BOLD),
                    ))),
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
fn view_start(len: usize, height: usize, offset: usize) -> usize {
    if height == 0 || len <= height {
        return 0;
    }
    (len - height).saturating_sub(offset)
}

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

/// The draft wrapped to `width` columns, each row marked with whether it
/// starts a line the user typed, and the caret as a row and column of
/// cells. Columns count display cells, not bytes, or a caret after any
/// non-ASCII text would be drawn too far right. A caret at the end of a full
/// row moves to the start of the next, never past the edge.
fn layout_draft(text: &str, cursor: usize, width: usize) -> (Vec<(String, bool)>, (usize, usize)) {
    let width = width.max(1);
    let mut rows = vec![(String::new(), true)];
    let mut used = 0;
    let mut caret = None;
    for (at, ch) in text.char_indices() {
        let cells = ch.width().unwrap_or(0);
        if ch != '\n' && used + cells > width {
            rows.push((String::new(), false));
            used = 0;
        }
        if at == cursor {
            caret = Some((rows.len() - 1, used));
        }
        if ch == '\n' {
            rows.push((String::new(), true));
            used = 0;
            continue;
        }
        rows.last_mut().expect("never empty").0.push(ch);
        used += cells;
    }
    let caret = caret.unwrap_or_else(|| {
        if used >= width {
            rows.push((String::new(), false));
            (rows.len() - 1, 0)
        } else {
            (rows.len() - 1, used)
        }
    });
    (rows, caret)
}

/// The text before the caret, the character under it, and the rest.
fn split_at_column(text: &str, column: usize) -> (String, String, String) {
    let mut used = 0;
    for (at, ch) in text.char_indices() {
        if used >= column {
            let end = at + ch.len_utf8();
            return (
                text[..at].to_string(),
                text[at..end].to_string(),
                text[end..].to_string(),
            );
        }
        used += ch.width().unwrap_or(0);
    }
    (text.to_string(), String::new(), String::new())
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

const KEYMAP: [(&str, &str); 19] = [
    ("enter", "send, or queue while a task is running"),
    ("tab", "send, or queue while a task is running"),
    ("alt+enter", "new line in the message"),
    ("← →", "move the cursor"),
    ("alt+← →", "move a word at a time"),
    ("home end", "start or end of the line (also ctrl-a, ctrl-e)"),
    ("↑ ↓", "move between lines, then walk history"),
    ("backspace", "delete before the cursor"),
    ("delete", "delete under the cursor"),
    ("ctrl-w", "delete the word before the cursor"),
    ("ctrl-u ctrl-k", "kill to start or end of line"),
    ("ctrl-y", "yank what was killed"),
    ("ctrl-r", "search history"),
    ("pgup pgdn", "scroll the transcript"),
    ("esc", "edit the previous task"),
    ("esc esc", "interrupt the running task"),
    ("ctrl-t", "live tail of the tool in flight"),
    ("ctrl-c", "press twice to quit"),
    ("?", "this overlay, when the composer is empty"),
];

fn help_rows(palette: &Palette) -> Vec<Row> {
    let key_width = KEYMAP.iter().map(|(key, _)| key.width()).max().unwrap_or(0) + 2;
    KEYMAP
        .iter()
        .map(|(key, description)| {
            let pad = key_width - key.width();
            Row::new(vec![Span::styled(
                description.to_string(),
                paint(palette.text),
            )])
            .hanging(
                vec![
                    Span::styled(
                        key.to_string(),
                        paint(palette.accent).add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(" ".repeat(pad)),
                ],
                vec![Span::raw(" ".repeat(key_width))],
            )
        })
        .collect()
}

/// The three choices, numbered and spelled out. Option 2 keeps its number on
/// a flagged call and says why it is unavailable, rather than vanishing.
fn approval_options(flagged: bool) -> [&'static str; 3] {
    [
        "1) Allow Once",
        match flagged {
            true => "2) Allow Always - not available here, this must be answered each time",
            false => "2) Allow Always - allows every change for the rest of this session",
        },
        "3) No",
    ]
}

fn approval_rows(
    flagged: bool,
    tool: &str,
    preview: &str,
    concerns: &[String],
    room: usize,
    palette: &Palette,
) -> Vec<Row> {
    let mut rows = vec![Row::new(vec![
        Span::styled("⚙ ", paint(palette.warning)),
        Span::styled(
            tool.to_string(),
            paint(palette.text).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" wants to run", muted(palette)),
    ])];
    rows.push(Row::blank());

    let lang = match tool {
        "terminal" => "sh",
        _ => "",
    };
    let bar = vec![Span::styled("│ ", paint(palette.border))];
    let lines: Vec<&str> = preview.lines().collect();
    if diff_render::looks_like_diff(preview) {
        let diff = render::diff_rows(preview, palette, 0);
        let hidden = diff.len().saturating_sub(room);
        rows.extend(diff.into_iter().take(room).map(|row| Row {
            first: bar.clone(),
            rest: bar.clone(),
            ..row
        }));
        if hidden > 0 {
            rows.push(more_row(hidden, palette));
        }
    } else {
        let highlighted = highlight::highlight(lang, preview);
        for tokens in highlighted.into_iter().take(room) {
            let body = tokens
                .into_iter()
                .map(|(tok, text)| {
                    let style = match (lang, tok) {
                        ("", _) => paint(palette.text),
                        (_, tok) => render::token_style(tok, palette),
                    };
                    Span::styled(text, style)
                })
                .collect();
            rows.push(Row::new(body).prefix(bar.clone()).hard());
        }
        if lines.len() > room {
            rows.push(more_row(lines.len() - room, palette));
        }
    }

    if !concerns.is_empty() {
        rows.push(Row::blank());
    }
    for concern in concerns {
        rows.push(
            Row::new(vec![Span::styled(
                concern.clone(),
                paint(palette.error).add_modifier(Modifier::BOLD),
            )])
            .hanging(
                vec![Span::styled("⚠ ", paint(palette.error))],
                vec![Span::raw("  ")],
            ),
        );
    }
    rows.push(Row::blank());

    for (index, option) in approval_options(flagged).into_iter().enumerate() {
        let (number, label) = option.split_once(") ").unwrap_or(("", option));
        let unavailable = flagged && index == 1;
        let swatch = match index {
            0 => palette.success,
            1 if unavailable => palette.muted,
            1 => palette.warning,
            _ => palette.error,
        };
        let label_style = match unavailable {
            true => muted(palette),
            false => paint(palette.text).add_modifier(Modifier::BOLD),
        };
        rows.push(
            Row::new(vec![Span::styled(label.to_string(), label_style)]).hanging(
                vec![badge(number, swatch, palette), Span::raw(" ")],
                vec![Span::raw("    ")],
            ),
        );
    }
    rows
}

fn more_row(hidden: usize, palette: &Palette) -> Row {
    Row::new(vec![Span::styled(
        format!("… {hidden} more lines"),
        muted(palette).add_modifier(Modifier::ITALIC),
    )])
}

fn assistant_label(palette: &Palette) -> Row {
    Row::new(render::gradient_spans(
        "◆ token",
        palette,
        0.0,
        Modifier::BOLD,
    ))
}

/// The question, on its own tinted band so a long transcript can be scanned
/// for where each turn began.
fn user_rows(text: &str, palette: &Palette) -> Vec<Row> {
    let fill = on(palette.user_bg);
    let body = vec![Span::styled(
        text.to_string(),
        paint(palette.text).add_modifier(Modifier::BOLD),
    )];
    let question = Row::new(body)
        .hanging(
            vec![Span::styled(
                " ❯ ",
                paint(palette.user).add_modifier(Modifier::BOLD),
            )],
            vec![Span::raw("   ")],
        )
        .fill(fill);
    match palette.depth {
        Depth::Ansi16 => vec![question],
        _ => vec![Row::blank().fill(fill), question, Row::blank().fill(fill)],
    }
}

fn tool_rows(
    name: &str,
    arguments: &str,
    output: &str,
    failed: bool,
    palette: &Palette,
) -> Vec<Row> {
    let (icon, swatch) = match failed {
        true => ("✗", palette.error),
        false => ("✓", palette.success),
    };
    let mut head = vec![
        Span::styled(
            format!("{icon} "),
            paint(swatch).add_modifier(Modifier::BOLD),
        ),
        Span::styled(name.to_string(), paint(swatch).add_modifier(Modifier::BOLD)),
    ];
    let summary = render::tool_summary(arguments);
    if !summary.is_empty() {
        head.push(Span::styled(format!("  {summary}"), muted(palette)));
    }
    let mut rows = vec![Row::new(head).hanging(vec![Span::raw("  ")], vec![Span::raw("    ")])];

    if diff_render::looks_like_diff(output) {
        rows.extend(render::diff_rows(output, palette, 4));
        return rows;
    }
    let lines: Vec<&str> = output.lines().collect();
    let gutter = vec![Span::raw("    "), Span::styled("│ ", paint(palette.border))];
    let style = match failed {
        true => paint(palette.error),
        false => muted(palette),
    };
    for line in lines.iter().take(TOOL_OUTPUT_LINES) {
        rows.push(
            Row::new(vec![Span::styled(line.to_string(), style)])
                .prefix(gutter.clone())
                .hard(),
        );
    }
    if lines.len() > TOOL_OUTPUT_LINES {
        rows.push(
            Row::new(vec![Span::styled(
                format!(
                    "… {} more lines · ctrl-t for the tail",
                    lines.len() - TOOL_OUTPUT_LINES
                ),
                muted(palette).add_modifier(Modifier::ITALIC),
            )])
            .prefix(gutter),
        );
    }
    rows
}

fn welcome_lines(
    palette: &Palette,
    model: &str,
    sandbox: &str,
    frame: usize,
) -> Vec<Line<'static>> {
    const LOGO: [&str; 3] = [
        "▀█▀ █▀█ █▄▀ █▀▀ █▄ █",
        " █  █ █ █▀▄ █▀▀ █ ▀█",
        " ▀  ▀▀▀ ▀ ▀ ▀▀▀ ▀  ▀",
    ];
    let phase = frame as f32 * 0.01;
    let mut lines: Vec<Line<'static>> = LOGO
        .iter()
        .map(|row| {
            Line::from(render::gradient_spans(
                row,
                palette,
                phase.max(0.001),
                Modifier::BOLD,
            ))
        })
        .collect();
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        "a coding agent in your terminal",
        muted(palette).add_modifier(Modifier::ITALIC),
    ));
    lines.push(Line::raw(""));
    lines.push(Line::from(vec![
        Span::styled(
            model.to_string(),
            paint(palette.info).add_modifier(Modifier::BOLD),
        ),
        Span::styled("  ·  ", paint(palette.border)),
        Span::styled(format!("sandbox {sandbox}"), paint(palette.success)),
    ]));
    lines.push(Line::raw(""));
    let key = |text: &str| {
        Span::styled(
            text.to_string(),
            paint(palette.accent).add_modifier(Modifier::BOLD),
        )
    };
    let say = |text: &str| Span::styled(text.to_string(), muted(palette));
    lines.push(Line::from(vec![
        key("enter"),
        say(" send   "),
        key("alt+enter"),
        say(" new line   "),
        key("?"),
        say(" keys   "),
        key("ctrl-c ctrl-c"),
        say(" quit"),
    ]));
    lines
}

fn shimmer_spans(text: &str, phase: usize, swatch: Swatch) -> Vec<Span<'static>> {
    let chars: Vec<char> = text.chars().collect();
    chars
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
        .collect()
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
    fn editing_keys_reach_the_composer() {
        let mut widget = widget();
        for ch in "hello world".chars() {
            widget.on_key(Key::Char(ch));
        }
        widget.on_key(Key::Backspace);
        assert_eq!(widget.composer.text(), "hello worl");
        widget.on_key(Key::WordLeft);
        widget.on_key(Key::Left);
        widget.on_key(Key::Backspace);
        assert_eq!(widget.composer.text(), "hell worl");
        widget.on_key(Key::Home);
        widget.on_key(Key::Delete);
        assert_eq!(widget.composer.text(), "ell worl");
        widget.on_key(Key::End);
        widget.on_key(Key::Ctrl('w'));
        assert_eq!(widget.composer.text(), "ell ");
        widget.on_key(Key::Ctrl('h'));
        assert_eq!(widget.composer.text(), "ell");
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
        let options = approval_options(true);
        assert_eq!(options[0], "1) Allow Once");
        assert!(options[1].starts_with("2) Allow Always - not available"));
        assert_eq!(options[2], "3) No");
        assert!(approval_options(false)[1].contains("rest of this session"));
        assert!(matches!(
            widget.on_key(Key::Char('3')),
            Effect::Choice(ApprovalChoice::No)
        ));
    }

    #[test]
    fn the_approval_overlay_shows_every_option_and_concern() {
        let palette = palette(Theme::Dark, Depth::Truecolor);
        let rows = approval_rows(
            true,
            "terminal",
            "rm .env",
            &["names a secret".into()],
            5,
            &palette,
        );
        let text: String = render::lay_out(&rows, 70)
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
                    + "\n"
            })
            .collect();
        assert!(text.contains("rm .env"));
        assert!(text.contains("names a secret"));
        assert!(text.contains("Allow Once"));
        assert!(text.contains("not available here"));
        assert!(text.contains(" No"));
    }

    #[test]
    fn a_long_preview_is_cut_before_the_options() {
        let palette = palette(Theme::Dark, Depth::Truecolor);
        let preview = (0..50)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let rows = approval_rows(false, "write_file", &preview, &[], 5, &palette);
        let text: String = render::lay_out(&rows, 70)
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
                    + "\n"
            })
            .collect();
        assert!(text.contains("45 more lines"));
        assert!(text.contains("Allow Once"));
    }

    #[test]
    fn the_caret_tracks_the_draft() {
        assert_eq!(layout_draft("", 0, 80).1, (0, 0));
        assert_eq!(layout_draft("hello", 5, 80).1, (0, 5));
        assert_eq!(layout_draft("hello", 2, 80).1, (0, 2));
        assert_eq!(layout_draft("one\ntwo", 7, 80).1, (1, 3));
        assert_eq!(layout_draft("one\ntwo", 4, 80).1, (1, 0));
    }

    #[test]
    fn the_caret_counts_characters_not_bytes() {
        // Six bytes, two characters: a byte column would put the caret four
        // cells past where the text ends.
        assert_eq!(layout_draft("héllo", "héllo".len(), 80).1, (0, 5));
        assert_eq!(layout_draft("✓✓", "✓✓".len(), 80).1, (0, 2));
    }

    #[test]
    fn a_long_draft_wraps_and_the_caret_follows() {
        // The old composer did not wrap: past the box's width the text and
        // the caret went off the edge, which read as keys being ignored.
        let (rows, caret) = layout_draft("abcdefgh", 8, 4);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].0, "abcd");
        assert!(!rows[1].1, "a wrapped row does not start a typed line");
        assert_eq!(caret, (2, 0));
        assert_eq!(layout_draft("abcdefgh", 5, 4).1, (1, 1));
    }

    #[test]
    fn the_caret_splits_its_row() {
        assert_eq!(
            split_at_column("héllo", 1),
            ("h".to_string(), "é".to_string(), "llo".to_string())
        );
        assert_eq!(
            split_at_column("ab", 2),
            ("ab".to_string(), String::new(), String::new())
        );
    }

    #[test]
    fn the_question_is_in_the_transcript_before_the_answer() {
        let mut widget = widget();
        widget.paste("what is this");
        let _ = widget.on_key(Key::Enter);
        widget.delta("an answer".into());

        let text: Vec<String> = widget
            .transcript_lines(80)
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
    fn a_finished_answer_is_labelled_once_per_turn() {
        let mut widget = widget();
        widget.paste("q");
        let _ = widget.on_key(Key::Enter);
        widget.assistant("looking".into());
        widget.tool_start("terminal".into(), r#"{"command":"ls"}"#.into());
        widget.tool_done("terminal".into(), "src".into(), false);
        widget.finished(true, "**done**".into());
        let labels = widget
            .transcript_lines(80)
            .iter()
            .filter(|line| {
                line.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
                    == "◆ token"
            })
            .count();
        assert_eq!(labels, 1);
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
