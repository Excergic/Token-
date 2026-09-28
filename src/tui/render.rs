//! Styled rows for the transcript: markdown, code, tables, diffs and the
//! wrapping that keeps every span's style when a line breaks. Colours come
//! from the palette; nothing here reads a terminal.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::diff_render::{self, DiffKind};
use super::highlight::{self, Tok};
use super::markdown::{self, Block as Md, Inline, Item, Table};
use super::styles::{Depth, Palette, Swatch};

pub fn color_of(swatch: Swatch) -> Color {
    match swatch {
        Swatch::Default => Color::Reset,
        Swatch::Green => Color::Green,
        Swatch::Red => Color::Red,
        Swatch::Cyan => Color::Cyan,
        Swatch::Magenta => Color::Magenta,
        Swatch::Yellow => Color::Yellow,
        Swatch::Blue => Color::Blue,
        Swatch::Indexed(index) => Color::Indexed(index),
        Swatch::Rgb(red, green, blue) => Color::Rgb(red, green, blue),
    }
}

/// A foreground. `Default` sets nothing, so it can be laid over another
/// style without resetting the colour underneath it.
pub fn paint(swatch: Swatch) -> Style {
    match swatch {
        Swatch::Default => Style::default(),
        swatch => Style::default().fg(color_of(swatch)),
    }
}

pub fn on(swatch: Swatch) -> Style {
    match swatch {
        Swatch::Default => Style::default(),
        swatch => Style::default().bg(color_of(swatch)),
    }
}

/// Secondary text. A 16-colour terminal has no grey to spare, so it dims.
pub fn muted(palette: &Palette) -> Style {
    match palette.muted {
        Swatch::Default => Style::default().add_modifier(Modifier::DIM),
        swatch => paint(swatch),
    }
}

/// A coloured label. With no backgrounds to spend it is bold colour instead.
pub fn badge(label: &str, swatch: Swatch, palette: &Palette) -> Span<'static> {
    match palette.depth {
        Depth::Ansi16 => Span::styled(
            label.to_string(),
            paint(swatch).add_modifier(Modifier::BOLD),
        ),
        _ => Span::styled(
            format!(" {} ", label.trim()),
            on(swatch)
                .fg(color_of(palette.code_bg))
                .add_modifier(Modifier::BOLD),
        ),
    }
}

/// One logical line before wrapping. `first` prefixes the first screen row
/// and `rest` every continuation, so a bullet's text hangs under itself.
/// `fill` pads the row to the full width with a background; `hard` wraps on
/// characters rather than words and keeps leading spaces, which code needs.
#[derive(Debug, Clone, Default)]
pub struct Row {
    pub first: Vec<Span<'static>>,
    pub rest: Vec<Span<'static>>,
    pub body: Vec<Span<'static>>,
    pub fill: Option<Style>,
    pub hard: bool,
}

impl Row {
    pub fn new(body: Vec<Span<'static>>) -> Self {
        Self {
            body,
            ..Self::default()
        }
    }

    pub fn blank() -> Self {
        Self::default()
    }

    pub fn prefix(mut self, prefix: Vec<Span<'static>>) -> Self {
        self.rest = prefix.clone();
        self.first = prefix;
        self
    }

    pub fn hanging(mut self, first: Vec<Span<'static>>, rest: Vec<Span<'static>>) -> Self {
        self.first = first;
        self.rest = rest;
        self
    }

    pub fn fill(mut self, style: Style) -> Self {
        self.fill = Some(style);
        self
    }

    pub fn hard(mut self) -> Self {
        self.hard = true;
        self
    }
}

pub fn width_of(spans: &[Span<'_>]) -> usize {
    spans.iter().map(|span| span.content.width()).sum()
}

/// Wraps styled text to `width` columns. Words break at spaces when there is
/// one on the row; a word longer than the row is split where it overflows.
pub fn wrap_spans(spans: &[Span<'_>], width: usize, hard: bool) -> Vec<Vec<Span<'static>>> {
    let width = width.max(1);
    let mut rows: Vec<Vec<(Style, char)>> = vec![Vec::new()];
    let mut used = 0;
    for span in spans {
        for ch in span.content.chars() {
            if ch == '\n' {
                rows.push(Vec::new());
                used = 0;
                continue;
            }
            let cells = ch.width().unwrap_or(0);
            while used + cells > width && !rows.last().is_some_and(Vec::is_empty) {
                let row = rows.last_mut().expect("never empty");
                let at = (!hard)
                    .then(|| row.iter().rposition(|(_, have)| *have == ' '))
                    .flatten()
                    .map(|space| space + 1)
                    .filter(|at| *at < row.len());
                let mut carry = match at {
                    Some(at) => row.split_off(at),
                    None => Vec::new(),
                };
                if !hard {
                    while row.last().is_some_and(|(_, have)| *have == ' ') {
                        row.pop();
                    }
                    let lead = carry.iter().take_while(|(_, have)| *have == ' ').count();
                    carry.drain(..lead);
                }
                used = carry
                    .iter()
                    .map(|(_, have)| have.width().unwrap_or(0))
                    .sum();
                rows.push(carry);
            }
            let continuation = rows.len() > 1;
            let row = rows.last_mut().expect("never empty");
            if !hard && ch == ' ' && row.is_empty() && continuation {
                continue;
            }
            row.push((span.style, ch));
            used += cells;
        }
    }
    rows.into_iter().map(group).collect()
}

fn group(row: Vec<(Style, char)>) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut text = String::new();
    let mut current: Option<Style> = None;
    for (style, ch) in row {
        if current.is_some_and(|have| have != style) {
            spans.push(Span::styled(
                std::mem::take(&mut text),
                current.unwrap_or_default(),
            ));
        }
        current = Some(style);
        text.push(ch);
    }
    if !text.is_empty() {
        spans.push(Span::styled(text, current.unwrap_or_default()));
    }
    spans
}

/// Rows to screen lines at `width` columns.
pub fn lay_out(rows: &[Row], width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut lines = Vec::new();
    for row in rows {
        let prefix = width_of(&row.first).max(width_of(&row.rest));
        let room = width.saturating_sub(prefix).max(1);
        let pieces = match row.body.is_empty() {
            true => vec![Vec::new()],
            false => wrap_spans(&row.body, room, row.hard),
        };
        for (index, piece) in pieces.into_iter().enumerate() {
            let mut spans = match index {
                0 => row.first.clone(),
                _ => row.rest.clone(),
            };
            match row.fill {
                Some(fill) => {
                    spans.extend(
                        piece
                            .into_iter()
                            .map(|span| Span::styled(span.content, fill.patch(span.style))),
                    );
                    let pad = width.saturating_sub(width_of(&spans));
                    if pad > 0 {
                        spans.push(Span::styled(" ".repeat(pad), fill));
                    }
                }
                None => spans.extend(piece),
            }
            lines.push(Line::from(spans));
        }
    }
    lines
}

/// Text coloured along the brand gradient, one step per character.
pub fn gradient_spans(
    text: &str,
    palette: &Palette,
    phase: f32,
    extra: Modifier,
) -> Vec<Span<'static>> {
    let chars: Vec<char> = text.chars().collect();
    let span = (chars.len().max(2) - 1) as f32;
    chars
        .iter()
        .enumerate()
        .map(|(index, ch)| {
            let position = index as f32 / span;
            let t = match phase == 0.0 {
                true => position,
                false => wave(position * 0.6 + phase),
            };
            Span::styled(
                ch.to_string(),
                paint(palette.gradient(t)).add_modifier(extra),
            )
        })
        .collect()
}

/// 0 → 1 → 0 as `x` walks one unit, so a moving gradient has no seam.
pub fn wave(x: f32) -> f32 {
    let frac = x.rem_euclid(1.0);
    match frac < 0.5 {
        true => frac * 2.0,
        false => 2.0 - frac * 2.0,
    }
}

/// Inline markdown over a base style: bold gets its own colour, as do
/// italics, code and links.
pub fn inline_spans(text: &str, base: Style, palette: &Palette) -> Vec<Span<'static>> {
    markdown::inline(text)
        .into_iter()
        .map(|(kind, body)| {
            let style = match kind {
                Inline::Plain => base,
                Inline::Strong => base
                    .patch(paint(palette.strong))
                    .add_modifier(Modifier::BOLD),
                Inline::Emphasis => base
                    .patch(paint(palette.emphasis))
                    .add_modifier(Modifier::ITALIC),
                Inline::StrongEmphasis => base
                    .patch(paint(palette.strong))
                    .add_modifier(Modifier::BOLD | Modifier::ITALIC),
                Inline::Code => paint(palette.inline_code).patch(on(palette.inline_code_bg)),
                Inline::Strike => base
                    .patch(muted(palette))
                    .add_modifier(Modifier::CROSSED_OUT),
                Inline::Link => paint(palette.link).add_modifier(Modifier::UNDERLINED),
                Inline::Url => muted(palette),
            };
            Span::styled(body, style)
        })
        .collect()
}

fn spaces(count: usize) -> Span<'static> {
    Span::raw(" ".repeat(count))
}

/// An answer, block by block, with a blank row between blocks.
pub fn markdown_rows(text: &str, width: usize, palette: &Palette, indent: usize) -> Vec<Row> {
    let room = width.saturating_sub(indent);
    let mut rows = Vec::new();
    for (index, block) in markdown::parse(text, room).into_iter().enumerate() {
        if index > 0 {
            rows.push(Row::blank());
        }
        match block {
            Md::Heading { level, text } => {
                rows.extend(heading_rows(level, &text, room, palette, indent))
            }
            Md::Paragraph(text) => rows.push(paragraph_row(&text, palette, indent)),
            Md::Bullet(items) => rows.extend(list_rows(&items, palette, indent)),
            Md::Quote(text) => {
                let bar = vec![spaces(indent), Span::styled("▎ ", paint(palette.brand))];
                let base = paint(palette.quote).add_modifier(Modifier::ITALIC);
                for line in text.lines() {
                    rows.push(Row::new(inline_spans(line, base, palette)).prefix(bar.clone()));
                }
            }
            Md::Rule => rows.push(
                Row::new(vec![Span::styled(
                    "─".repeat(room.max(1)),
                    paint(palette.border),
                )])
                .prefix(vec![spaces(indent)]),
            ),
            Md::Code { lang, body } => rows.extend(code_rows(&lang, &body, width, palette, indent)),
            Md::Table(table) => rows.extend(table_rows(&table, width, palette, indent)),
        }
    }
    if rows.is_empty() && !text.is_empty() {
        rows.push(Row::new(vec![Span::raw(text.to_string())]).prefix(vec![spaces(indent)]));
    }
    rows
}

fn heading_rows(level: u8, text: &str, room: usize, palette: &Palette, indent: usize) -> Vec<Row> {
    let pad = vec![spaces(indent)];
    let bold = Modifier::BOLD;
    match level {
        1 => {
            let plain: String = markdown::inline(text)
                .into_iter()
                .map(|(_, body)| body)
                .collect();
            let rule = "━".repeat(plain.width().clamp(3, room.max(3)));
            vec![
                Row::new(gradient_spans(&plain, palette, 0.0, bold)).prefix(pad.clone()),
                Row::new(gradient_spans(&rule, palette, 0.0, Modifier::empty())).prefix(pad),
            ]
        }
        2 => {
            let base = paint(palette.info).add_modifier(bold);
            vec![Row::new(inline_spans(text, base, palette)).hanging(
                vec![spaces(indent), Span::styled("▍ ", paint(palette.info))],
                vec![spaces(indent + 2)],
            )]
        }
        3 => {
            let base = paint(palette.accent).add_modifier(bold);
            vec![Row::new(inline_spans(text, base, palette)).hanging(
                vec![spaces(indent), Span::styled("▸ ", paint(palette.accent))],
                vec![spaces(indent + 2)],
            )]
        }
        _ => {
            let base = paint(palette.brand).add_modifier(bold);
            vec![Row::new(inline_spans(text, base, palette)).prefix(pad)]
        }
    }
}

/// `Note:`, `Warning:` and friends at the start of a paragraph, as a label.
fn callout(text: &str) -> Option<(&'static str, &str)> {
    const KINDS: [(&str, &str); 10] = [
        ("note", "NOTE"),
        ("info", "INFO"),
        ("hint", "HINT"),
        ("tip", "TIP"),
        ("warning", "WARNING"),
        ("caution", "CAUTION"),
        ("important", "IMPORTANT"),
        ("error", "ERROR"),
        ("danger", "DANGER"),
        ("summary", "SUMMARY"),
    ];
    let body = text.trim_start_matches(['*', '_']);
    let colon = body.find(':')?;
    let word = body[..colon].trim_end_matches(['*', '_']);
    let (_, label) = KINDS
        .iter()
        .find(|(name, _)| word.eq_ignore_ascii_case(name))?;
    let rest = body[colon + 1..]
        .trim_start_matches(['*', '_'])
        .trim_start();
    Some((label, rest))
}

fn callout_swatch(label: &str, palette: &Palette) -> Swatch {
    match label {
        "TIP" | "SUMMARY" => palette.success,
        "WARNING" | "CAUTION" | "IMPORTANT" => palette.warning,
        "ERROR" | "DANGER" => palette.error,
        _ => palette.info,
    }
}

fn paragraph_row(text: &str, palette: &Palette, indent: usize) -> Row {
    let base = paint(palette.text);
    let pad = vec![spaces(indent)];
    match callout(text) {
        Some((label, rest)) => {
            let swatch = callout_swatch(label, palette);
            let mut body = vec![badge(label, swatch, palette), Span::raw(" ")];
            body.extend(inline_spans(rest, base, palette));
            Row::new(body).prefix(pad)
        }
        None => Row::new(inline_spans(text, base, palette)).prefix(pad),
    }
}

fn list_rows(items: &[Item], palette: &Palette, indent: usize) -> Vec<Row> {
    const BULLETS: [&str; 3] = ["•", "◦", "▪"];
    let colors = [palette.accent, palette.info, palette.brand];
    items
        .iter()
        .map(|item| {
            let depth = item.depth.min(6);
            let (marker, style) = match &item.number {
                Some(number) => (
                    number.clone(),
                    paint(palette.accent).add_modifier(Modifier::BOLD),
                ),
                None => (
                    BULLETS[depth % BULLETS.len()].to_string(),
                    paint(colors[depth % colors.len()]),
                ),
            };
            let lead = indent + depth * 2;
            let mut body = Vec::new();
            let mut text = item.text.as_str();
            if let Some(rest) = text.strip_prefix("[ ] ") {
                body.push(Span::styled("☐ ", muted(palette)));
                text = rest;
            } else if let Some(rest) = text
                .strip_prefix("[x] ")
                .or_else(|| text.strip_prefix("[X] "))
            {
                body.push(Span::styled("☑ ", paint(palette.success)));
                text = rest;
            }
            body.extend(inline_spans(text, paint(palette.text), palette));
            let marker_width = marker.width() + 1;
            Row::new(body).hanging(
                vec![spaces(lead), Span::styled(format!("{marker} "), style)],
                vec![spaces(lead + marker_width)],
            )
        })
        .collect()
}

pub fn token_style(tok: Tok, palette: &Palette) -> Style {
    let syntax = &palette.syntax;
    match tok {
        Tok::Plain => paint(syntax.plain),
        Tok::Keyword => paint(syntax.keyword).add_modifier(Modifier::BOLD),
        Tok::Type => paint(syntax.type_name),
        Tok::Str => paint(syntax.string),
        Tok::Comment => paint(syntax.comment).add_modifier(Modifier::ITALIC),
        Tok::Number => paint(syntax.number),
        Tok::Function => paint(syntax.function),
        Tok::Macro => paint(syntax.macro_name).add_modifier(Modifier::BOLD),
        Tok::Constant => paint(syntax.constant),
        Tok::Punct => paint(syntax.punct),
        Tok::Attr => paint(syntax.attr),
    }
}

/// A fenced block in a frame: the language on the top edge, line numbers in
/// a gutter, and the code highlighted on its own background.
pub fn code_rows(
    lang: &str,
    body: &str,
    width: usize,
    palette: &Palette,
    indent: usize,
) -> Vec<Row> {
    let room = width.saturating_sub(indent).max(8);
    let border = paint(palette.border);
    let bg = on(palette.code_bg);
    let label = match lang.trim() {
        "" => "code".to_string(),
        lang => lang.to_string(),
    };
    let top_used = 3 + label.width() + 2;
    let mut rows = vec![
        Row::new(vec![
            Span::styled("╭─ ", border),
            Span::styled(
                label.clone(),
                paint(palette.accent).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" ", border),
            Span::styled("─".repeat(room.saturating_sub(top_used)), border),
        ])
        .prefix(vec![spaces(indent)]),
    ];

    let is_diff = matches!(label.as_str(), "diff" | "patch");
    let lines: Vec<Vec<Span<'static>>> = match is_diff {
        true => body
            .split('\n')
            .map(|line| {
                let (fg, _) = diff_colors(
                    diff_render::parse_diff(line).first().map(|d| d.kind),
                    palette,
                );
                vec![Span::styled(line.to_string(), paint(fg))]
            })
            .collect(),
        false => highlight::highlight(&label, body)
            .into_iter()
            .map(|tokens| {
                tokens
                    .into_iter()
                    .map(|(tok, text)| {
                        Span::styled(text.replace('\t', "    "), token_style(tok, palette))
                    })
                    .collect()
            })
            .collect(),
    };
    let digits = lines.len().to_string().len().max(2);
    for (number, spans) in lines.into_iter().enumerate() {
        let fill = match is_diff {
            true => {
                let text: String = spans.iter().map(|span| span.content.as_ref()).collect();
                let kind = diff_render::parse_diff(&text).first().map(|line| line.kind);
                match diff_colors(kind, palette).1 {
                    Swatch::Default => bg,
                    swatch => on(swatch),
                }
            }
            false => bg,
        };
        let gutter = muted(palette).patch(fill);
        rows.push(
            Row::new(spans)
                .hanging(
                    vec![
                        spaces(indent),
                        Span::styled("│", border.patch(fill)),
                        Span::styled(format!(" {:>digits$} ", number + 1), gutter),
                    ],
                    vec![
                        spaces(indent),
                        Span::styled("│", border.patch(fill)),
                        Span::styled(" ".repeat(digits + 2), gutter),
                    ],
                )
                .fill(fill)
                .hard(),
        );
    }
    rows.push(
        Row::new(vec![Span::styled(
            format!("╰{}", "─".repeat(room.saturating_sub(1))),
            border,
        )])
        .prefix(vec![spaces(indent)]),
    );
    rows
}

fn diff_colors(kind: Option<DiffKind>, palette: &Palette) -> (Swatch, Swatch) {
    match kind {
        Some(DiffKind::Add) => (palette.diff_add, palette.diff_add_bg),
        Some(DiffKind::Del) => (palette.diff_del, palette.diff_del_bg),
        Some(DiffKind::Hunk) => (palette.info, Swatch::Default),
        Some(DiffKind::Meta) => (palette.muted, Swatch::Default),
        _ => (Swatch::Default, Swatch::Default),
    }
}

/// A unified diff outside a fence, coloured line by line.
pub fn diff_rows(text: &str, palette: &Palette, indent: usize) -> Vec<Row> {
    diff_render::parse_diff(text)
        .into_iter()
        .map(|line| {
            let (fg, bg) = diff_colors(Some(line.kind), palette);
            let mut style = paint(fg);
            if matches!(line.kind, DiffKind::Hunk | DiffKind::Meta) {
                style = style.add_modifier(Modifier::BOLD);
            }
            let row = Row::new(vec![Span::styled(line.text, style)])
                .prefix(vec![spaces(indent)])
                .hard();
            match bg {
                Swatch::Default => row,
                swatch => row.fill(on(swatch)),
            }
        })
        .collect()
}

/// A table in a box, columns sized to their content and shrunk widest-first
/// when the screen is narrower, with cells wrapping inside their column.
/// Too narrow for any box, it falls back to one `header: value` per line.
pub fn table_rows(table: &Table, width: usize, palette: &Palette, indent: usize) -> Vec<Row> {
    if table.headers.is_empty() {
        return Vec::new();
    }
    let columns = table.headers.len();
    let room = width.saturating_sub(indent);
    let chrome = columns * 3 + 1;
    let head_style = paint(palette.accent).add_modifier(Modifier::BOLD);
    let cell_style = paint(palette.text);
    let rendered = |text: &str, base: Style| inline_spans(text, base, palette);

    let mut natural: Vec<usize> = table
        .headers
        .iter()
        .map(|header| width_of(&rendered(header, head_style)))
        .collect();
    for row in &table.rows {
        for (column, cell) in row.iter().enumerate().take(columns) {
            natural[column] = natural[column].max(width_of(&rendered(cell, cell_style)));
        }
    }
    let available = room.saturating_sub(chrome);
    if table.pairs || available < columns * 3 {
        return pair_rows(table, palette, indent);
    }
    let mut widths: Vec<usize> = natural.iter().map(|width| (*width).max(1)).collect();
    while widths.iter().sum::<usize>() > available {
        let Some((widest, _)) = widths.iter().enumerate().max_by_key(|(_, width)| **width) else {
            break;
        };
        if widths[widest] <= 3 {
            break;
        }
        widths[widest] -= 1;
    }

    let border = paint(palette.border);
    let edge = |left: &str, middle: &str, right: &str| {
        let body: Vec<String> = widths.iter().map(|width| "─".repeat(width + 2)).collect();
        Row::new(vec![Span::styled(
            format!("{left}{}{right}", body.join(middle)),
            border,
        )])
        .prefix(vec![spaces(indent)])
    };
    let record = |cells: &[String], base: Style| -> Vec<Row> {
        let wrapped: Vec<Vec<Vec<Span<'static>>>> = widths
            .iter()
            .enumerate()
            .map(|(column, width)| {
                let text = cells.get(column).map(String::as_str).unwrap_or("");
                wrap_spans(&rendered(text, base), *width, false)
            })
            .collect();
        let height = wrapped.iter().map(Vec::len).max().unwrap_or(1);
        (0..height)
            .map(|line| {
                let mut body = vec![Span::styled("│", border)];
                for (column, width) in widths.iter().enumerate() {
                    let piece = wrapped[column].get(line).cloned().unwrap_or_default();
                    let pad = width.saturating_sub(width_of(&piece));
                    body.push(Span::raw(" "));
                    body.extend(piece);
                    body.push(spaces(pad + 1));
                    body.push(Span::styled("│", border));
                }
                Row::new(body).prefix(vec![spaces(indent)]).hard()
            })
            .collect()
    };

    let mut rows = vec![edge("┌", "┬", "┐")];
    rows.extend(record(&table.headers, head_style));
    rows.push(edge("├", "┼", "┤"));
    for row in &table.rows {
        rows.extend(record(row, cell_style));
    }
    rows.push(edge("└", "┴", "┘"));
    for extra in &table.spillover {
        rows.push(
            Row::new(vec![Span::styled(extra.clone(), muted(palette))])
                .prefix(vec![spaces(indent)]),
        );
    }
    rows
}

fn pair_rows(table: &Table, palette: &Palette, indent: usize) -> Vec<Row> {
    let mut rows = Vec::new();
    for (index, row) in table.rows.iter().enumerate() {
        if index > 0 {
            rows.push(Row::blank());
        }
        for (header, cell) in table.headers.iter().zip(row.iter()) {
            let mut body = vec![Span::styled(
                format!("{header}: "),
                paint(palette.accent).add_modifier(Modifier::BOLD),
            )];
            body.extend(inline_spans(cell, paint(palette.text), palette));
            rows.push(Row::new(body).prefix(vec![spaces(indent)]));
        }
    }
    for extra in &table.spillover {
        rows.push(
            Row::new(vec![Span::styled(extra.clone(), muted(palette))])
                .prefix(vec![spaces(indent)]),
        );
    }
    rows
}

/// A one-line reading of a tool call's JSON arguments: the command for a
/// shell, the path for a file, and short `key=value` pairs for the rest.
/// Long values (a whole file being written) are summarised by size.
pub fn tool_summary(arguments: &str) -> String {
    let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(arguments)
    else {
        return arguments.lines().next().unwrap_or("").to_string();
    };
    if let Some(command) = map.get("command").and_then(|value| value.as_str()) {
        return format!("$ {command}");
    }
    let mut parts = Vec::new();
    if let Some(path) = map.get("path").and_then(|value| value.as_str()) {
        parts.push(path.to_string());
    }
    for (key, value) in &map {
        if key == "path" {
            continue;
        }
        let shown = match value {
            serde_json::Value::String(text) if text.chars().count() > 60 || text.contains('\n') => {
                format!("{key}=({} chars)", text.chars().count())
            }
            serde_json::Value::String(text) => format!("{key}={text}"),
            other => format!("{key}={other}"),
        };
        parts.push(shown);
    }
    parts.join("  ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::styles::{Theme, palette};

    fn text_of(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn wrapping_breaks_at_spaces_and_keeps_styles() {
        let bold = Style::default().add_modifier(Modifier::BOLD);
        let spans = vec![Span::raw("one two "), Span::styled("three four", bold)];
        let rows = wrap_spans(&spans, 9, false);
        let texts: Vec<String> = rows
            .iter()
            .map(|row| row.iter().map(|span| span.content.as_ref()).collect())
            .collect();
        assert_eq!(texts, vec!["one two", "three", "four"]);
        assert!(rows[1].iter().all(|span| span.style == bold));
    }

    #[test]
    fn a_word_longer_than_the_row_is_split() {
        let rows = wrap_spans(&[Span::raw("abcdefghij")], 4, false);
        assert_eq!(rows.len(), 3);
    }

    #[test]
    fn hard_wrapping_keeps_leading_spaces() {
        let rows = wrap_spans(&[Span::raw("    let x = 1;")], 8, true);
        let first: String = rows[0].iter().map(|span| span.content.as_ref()).collect();
        assert_eq!(first, "    let ");
    }

    #[test]
    fn wide_characters_count_as_two_columns() {
        let rows = wrap_spans(&[Span::raw("日本語")], 4, true);
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn continuation_rows_hang_under_the_text() {
        let row = Row::new(vec![Span::raw("alpha beta gamma")])
            .hanging(vec![Span::raw("• ")], vec![Span::raw("  ")]);
        let lines = lay_out(&[row], 10);
        assert_eq!(text_of(&lines[0]), "• alpha");
        assert_eq!(text_of(&lines[1]), "  beta");
    }

    #[test]
    fn a_filled_row_reaches_the_edge() {
        let fill = Style::default().bg(Color::Blue);
        let lines = lay_out(&[Row::new(vec![Span::raw("x")]).fill(fill)], 6);
        assert_eq!(width_of(&lines[0].spans), 6);
    }

    #[test]
    fn bold_text_gets_the_strong_colour() {
        let palette = palette(Theme::Dark, Depth::Truecolor);
        let spans = inline_spans("a **key** point", Style::default(), &palette);
        let strong = spans
            .iter()
            .find(|span| span.content == "key")
            .expect("the bold run");
        assert!(strong.style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(strong.style.fg, Some(color_of(palette.strong)));
    }

    #[test]
    fn code_blocks_are_framed_and_numbered() {
        let palette = palette(Theme::Dark, Depth::Truecolor);
        let rows = code_rows("rust", "fn main() {}\nlet x = 1;", 40, &palette, 0);
        let lines = lay_out(&rows, 40);
        assert!(text_of(&lines[0]).starts_with("╭─ rust "));
        assert!(text_of(&lines[1]).starts_with("│  1 fn main"));
        assert!(text_of(&lines[3]).starts_with("╰"));
    }

    #[test]
    fn a_table_is_boxed_and_fits_the_width() {
        let palette = palette(Theme::Dark, Depth::Truecolor);
        let blocks = markdown::parse("| a | b |\n|---|---|\n| one | a much longer cell |\n", 30);
        let Md::Table(table) = &blocks[0] else {
            panic!("expected a table");
        };
        let lines = lay_out(&table_rows(table, 30, &palette, 0), 30);
        assert!(text_of(&lines[0]).starts_with("┌"));
        assert!(lines.iter().all(|line| width_of(&line.spans) <= 30));
        let all: String = lines.iter().map(text_of).collect();
        assert!(all.contains("much"), "a wrapped cell kept its words: {all}");
    }

    #[test]
    fn callouts_become_labels() {
        assert_eq!(
            callout("Note: this matters"),
            Some(("NOTE", "this matters"))
        );
        assert_eq!(
            callout("**Warning:** careful"),
            Some(("WARNING", "careful"))
        );
        assert_eq!(callout("Nothing: special"), None);
    }

    #[test]
    fn tool_arguments_read_as_one_line() {
        assert_eq!(tool_summary(r#"{"command":"cargo test"}"#), "$ cargo test");
        assert_eq!(tool_summary(r#"{"path":"src/main.rs"}"#), "src/main.rs");
        let body = "x".repeat(200);
        assert_eq!(
            tool_summary(&format!(r#"{{"path":"a.rs","content":"{body}"}}"#)),
            "a.rs  content=(200 chars)"
        );
    }
}
