//! Markdown the screen can lay out without a terminal.
//!
//! Tables go through one pipeline: drop spillover cells, pad short rows,
//! give each column a kind, then either keep columns or fall back to
//! key/value pairs when the width cannot hold them. Spillover is appended
//! after the table so a value is never thrown away.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
    Heading { level: u8, text: String },
    Paragraph(String),
    Code { lang: String, body: String },
    Bullet(Vec<Item>),
    Table(Table),
    Quote(String),
    Rule,
}

/// One list entry. `number` is the marker of an ordered item ("3."), and
/// `None` is a bullet. `depth` counts two-space indents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub depth: usize,
    pub number: Option<String>,
    pub text: String,
}

/// What an inline run of an answer is, before any colour is chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Inline {
    Plain,
    Strong,
    Emphasis,
    StrongEmphasis,
    Code,
    Strike,
    Link,
    /// The target of a web link, shown after its label.
    Url,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Table {
    pub headers: Vec<String>,
    pub rows: Vec<Vec<String>>,
    pub spillover: Vec<String>,
    pub pairs: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnKind {
    /// Short tokens. Shrink these last.
    Compact,
    /// Long unbroken tokens. Shrink before compact.
    TokenHeavy,
    /// Sentences. Shrink these first.
    Narrative,
}

pub fn parse(src: &str, width: usize) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut paragraph = String::new();
    let mut bullets: Vec<Item> = Vec::new();
    let mut quote = String::new();
    let lines: Vec<&str> = src.lines().collect();
    let mut index = 0;

    let flush = |paragraph: &mut String,
                 bullets: &mut Vec<Item>,
                 quote: &mut String,
                 blocks: &mut Vec<Block>| {
        if !paragraph.is_empty() {
            blocks.push(Block::Paragraph(std::mem::take(paragraph)));
        }
        if !bullets.is_empty() {
            blocks.push(Block::Bullet(std::mem::take(bullets)));
        }
        if !quote.is_empty() {
            blocks.push(Block::Quote(std::mem::take(quote)));
        }
    };

    while index < lines.len() {
        let line = lines[index];
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            flush(&mut paragraph, &mut bullets, &mut quote, &mut blocks);
            let fence = &trimmed[..3];
            let lang = trimmed.trim_start_matches(['`', '~']).trim().to_string();
            index += 1;
            let mut body = String::new();
            let mut first = true;
            while index < lines.len() && !lines[index].trim_start().starts_with(fence) {
                if !first {
                    body.push('\n');
                }
                first = false;
                body.push_str(lines[index]);
                index += 1;
            }
            if index < lines.len() {
                index += 1;
            }
            blocks.push(Block::Code { lang, body });
            continue;
        }
        if trimmed.starts_with('|') {
            flush(&mut paragraph, &mut bullets, &mut quote, &mut blocks);
            let mut raw = Vec::new();
            while index < lines.len() && lines[index].trim_start().starts_with('|') {
                raw.push(lines[index].trim_start());
                index += 1;
            }
            blocks.push(Block::Table(layout_table(&raw, width)));
            continue;
        }
        if let Some((level, text)) = heading(line) {
            flush(&mut paragraph, &mut bullets, &mut quote, &mut blocks);
            blocks.push(Block::Heading {
                level,
                text: text.to_string(),
            });
            index += 1;
            continue;
        }
        if is_horizontal_rule(line) {
            flush(&mut paragraph, &mut bullets, &mut quote, &mut blocks);
            blocks.push(Block::Rule);
            index += 1;
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix('>') {
            if !paragraph.is_empty() || !bullets.is_empty() {
                flush(
                    &mut paragraph,
                    &mut bullets,
                    &mut String::new(),
                    &mut blocks,
                );
            }
            if !quote.is_empty() {
                quote.push('\n');
            }
            quote.push_str(rest.trim());
            index += 1;
            continue;
        }
        if let Some(item) = list_item(line) {
            if !paragraph.is_empty() || !quote.is_empty() {
                flush(&mut paragraph, &mut Vec::new(), &mut quote, &mut blocks);
            }
            bullets.push(item);
            index += 1;
            continue;
        }
        if line.trim().is_empty() {
            flush(&mut paragraph, &mut bullets, &mut quote, &mut blocks);
            index += 1;
            continue;
        }
        // An indented line under a list item continues that item.
        if line.starts_with(' ') {
            if let Some(last) = bullets.last_mut() {
                last.text.push(' ');
                last.text.push_str(line.trim());
                index += 1;
                continue;
            }
        }
        if !bullets.is_empty() || !quote.is_empty() {
            flush(&mut String::new(), &mut bullets, &mut quote, &mut blocks);
        }
        if !paragraph.is_empty() {
            paragraph.push(' ');
        }
        paragraph.push_str(line.trim());
        index += 1;
    }
    flush(&mut paragraph, &mut bullets, &mut quote, &mut blocks);
    blocks
}

fn heading(line: &str) -> Option<(u8, &str)> {
    let hashes = line.chars().take_while(|ch| *ch == '#').count();
    if !(1..=6).contains(&hashes) {
        return None;
    }
    let text = line[hashes..].strip_prefix(' ')?;
    Some((hashes as u8, text.trim().trim_end_matches('#').trim_end()))
}

fn is_horizontal_rule(line: &str) -> bool {
    let compact: String = line.chars().filter(|ch| !ch.is_whitespace()).collect();
    compact.len() >= 3
        && ["-", "*", "_"]
            .iter()
            .any(|mark| compact.chars().all(|ch| ch.to_string() == *mark))
}

fn list_item(line: &str) -> Option<Item> {
    let indent = line.chars().take_while(|ch| *ch == ' ').count();
    let rest = &line[indent..];
    let depth = indent / 2;
    for marker in ["- ", "* ", "+ "] {
        if let Some(text) = rest.strip_prefix(marker) {
            return Some(Item {
                depth,
                number: None,
                text: text.trim().to_string(),
            });
        }
    }
    let digits = rest.chars().take_while(char::is_ascii_digit).count();
    if digits == 0 || digits > 3 {
        return None;
    }
    let after = &rest[digits..];
    let text = after
        .strip_prefix(". ")
        .or_else(|| after.strip_prefix(") "))?;
    Some(Item {
        depth,
        number: Some(format!("{}.", &rest[..digits])),
        text: text.trim().to_string(),
    })
}

/// Splits one line of prose into styled runs. Anything that does not close
/// is left as the characters the model wrote, so nothing is dropped.
pub fn inline(text: &str) -> Vec<(Inline, String)> {
    let chars: Vec<char> = text.chars().collect();
    let mut runs: Vec<(Inline, String)> = Vec::new();
    let mut plain = String::new();
    let mut index = 0;

    let push = |runs: &mut Vec<(Inline, String)>, kind: Inline, text: String| {
        if text.is_empty() {
            return;
        }
        match runs.last_mut() {
            Some((last, have)) if *last == kind => have.push_str(&text),
            _ => runs.push((kind, text)),
        }
    };

    while index < chars.len() {
        let ch = chars[index];
        let found = match ch {
            '`' => closing(&chars, index + 1, "`").map(|end| (Inline::Code, index + 1, end, 1)),
            '*' | '_' => emphasis(&chars, index),
            '~' if chars.get(index + 1) == Some(&'~') => {
                closing(&chars, index + 2, "~~").map(|end| (Inline::Strike, index + 2, end, 2))
            }
            '[' => {
                if let Some((label, target, next)) = link(&chars, index) {
                    push(&mut runs, Inline::Plain, std::mem::take(&mut plain));
                    if target.contains("://") {
                        push(&mut runs, Inline::Link, label);
                        push(&mut runs, Inline::Url, format!(" ({target})"));
                    } else {
                        push(&mut runs, Inline::Link, target);
                    }
                    index = next;
                    continue;
                }
                None
            }
            _ => None,
        };
        match found {
            Some((kind, start, end, width)) if end > start => {
                push(&mut runs, Inline::Plain, std::mem::take(&mut plain));
                let body: String = chars[start..end].iter().collect();
                push(&mut runs, kind, body);
                index = end + width;
            }
            _ => {
                plain.push(ch);
                index += 1;
            }
        }
    }
    push(&mut runs, Inline::Plain, plain);
    runs
}

/// `*x*`, `**x**`, `***x***` and their underscore twins. An underscore
/// inside a word is left alone, or `snake_case_name` would go italic.
fn emphasis(chars: &[char], index: usize) -> Option<(Inline, usize, usize, usize)> {
    let mark = chars[index];
    let run = chars[index..]
        .iter()
        .take_while(|ch| **ch == mark)
        .count()
        .min(3);
    if mark == '_' && index > 0 && chars[index - 1].is_alphanumeric() {
        return None;
    }
    let start = index + run;
    if chars.get(start).is_none_or(|ch| ch.is_whitespace()) {
        return None;
    }
    let delimiter: String = std::iter::repeat_n(mark, run).collect();
    let end = closing(chars, start, &delimiter)?;
    if chars[end - 1].is_whitespace() {
        return None;
    }
    if mark == '_' && chars.get(end + run).is_some_and(|ch| ch.is_alphanumeric()) {
        return None;
    }
    let kind = match run {
        1 => Inline::Emphasis,
        2 => Inline::Strong,
        _ => Inline::StrongEmphasis,
    };
    Some((kind, start, end, run))
}

fn closing(chars: &[char], from: usize, delimiter: &str) -> Option<usize> {
    let needle: Vec<char> = delimiter.chars().collect();
    (from..chars.len().saturating_sub(needle.len() - 1))
        .find(|at| chars[*at..].starts_with(&needle))
}

fn link(chars: &[char], index: usize) -> Option<(String, String, usize)> {
    let close = closing(chars, index + 1, "](")?;
    let end = closing(chars, close + 2, ")")?;
    let label: String = chars[index + 1..close].iter().collect();
    let target: String = chars[close + 2..end].iter().collect();
    if label.contains(']') || target.contains(' ') || target.is_empty() {
        return None;
    }
    let target = match link_target(&format!("[{label}]({target})"), ".") {
        Some(path) => path.trim_start_matches("./").to_string(),
        None => target,
    };
    Some((label, target, end + 1))
}

fn layout_table(raw: &[&str], width: usize) -> Table {
    let mut parsed: Vec<Vec<String>> = raw
        .iter()
        .filter(|line| !is_rule(line))
        .map(|line| split_row(line))
        .filter(|row| !row.is_empty())
        .collect();
    if parsed.is_empty() {
        return Table {
            headers: Vec::new(),
            rows: Vec::new(),
            spillover: Vec::new(),
            pairs: false,
        };
    }
    let headers = parsed.remove(0);
    let columns = headers.len();
    let mut spillover = Vec::new();
    let rows = parsed
        .into_iter()
        .map(|mut row| {
            if row.len() > columns {
                spillover.extend(row.split_off(columns));
            }
            row.resize(columns, String::new());
            row
        })
        .collect::<Vec<_>>();

    let kinds: Vec<ColumnKind> = (0..columns)
        .map(|column| {
            let cells: Vec<&str> = rows.iter().map(|row| row[column].as_str()).collect();
            column_kind(&cells)
        })
        .collect();
    let widths = allocate(&kinds, width.saturating_sub(columns.saturating_mul(3)));
    let need: usize = widths.iter().sum::<usize>() + columns.saturating_mul(3);
    let pairs = columns > 1 && (width < 24 || need > width);

    Table {
        headers,
        rows,
        spillover,
        pairs,
    }
}

fn is_rule(line: &str) -> bool {
    let cells = split_row(line);
    !cells.is_empty()
        && cells
            .iter()
            .all(|cell| cell.chars().all(|ch| ch == '-' || ch == ':'))
}

fn split_row(line: &str) -> Vec<String> {
    line.trim()
        .trim_matches('|')
        .split('|')
        .map(|cell| cell.trim().to_string())
        .collect()
}

pub fn column_kind(cells: &[&str]) -> ColumnKind {
    if cells.is_empty() {
        return ColumnKind::Compact;
    }
    let average = cells.iter().map(|cell| cell.chars().count()).sum::<usize>() / cells.len();
    let unbroken = cells
        .iter()
        .any(|cell| !cell.contains(' ') && cell.chars().count() > 24);
    if unbroken || average > 40 {
        ColumnKind::TokenHeavy
    } else if average > 16 {
        ColumnKind::Narrative
    } else {
        ColumnKind::Compact
    }
}

/// Narrative gives up width first, then token-heavy, and compact last.
pub fn allocate(kinds: &[ColumnKind], width: usize) -> Vec<usize> {
    if kinds.is_empty() {
        return Vec::new();
    }
    let floor = 4;
    let mut widths = kinds
        .iter()
        .map(|kind| match kind {
            ColumnKind::Compact => 8,
            ColumnKind::TokenHeavy => 18,
            ColumnKind::Narrative => 24,
        })
        .collect::<Vec<_>>();
    let mut spare = widths.iter().sum::<usize>().saturating_sub(width);
    for kind in [
        ColumnKind::Narrative,
        ColumnKind::TokenHeavy,
        ColumnKind::Compact,
    ] {
        for (index, column) in kinds.iter().enumerate() {
            if spare == 0 {
                break;
            }
            if *column != kind {
                continue;
            }
            let give = (widths[index] - floor).min(spare);
            widths[index] -= give;
            spare -= give;
        }
    }
    widths
}

/// A markdown link whose target is a local path. The label is not the path.
pub fn link_target(label: &str, cwd: &str) -> Option<String> {
    let (text, target) = label.split_once("](")?;
    if !text.starts_with('[') {
        return None;
    }
    let target = target.trim_end_matches(')');
    if target.contains("://") {
        return None;
    }
    let path = if target.starts_with('/') {
        target.to_string()
    } else {
        format!("{cwd}/{target}")
    };
    Some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wide_table_becomes_pairs_and_keeps_spillover() {
        let src = "\
| name | notes |\n\
| --- | --- |\n\
| a | one two three four five six seven |\n\
| b | short | extra |\n";
        let blocks = parse(src, 20);
        let Block::Table(table) = &blocks[0] else {
            panic!("expected a table");
        };
        assert!(table.pairs);
        assert_eq!(table.spillover, vec!["extra".to_string()]);
        assert_eq!(table.rows[1].len(), 2);
    }

    #[test]
    fn narrative_shrinks_before_compact() {
        let kinds = [ColumnKind::Compact, ColumnKind::Narrative];
        let widths = allocate(&kinds, 16);
        assert!(widths[1] < 24);
        assert_eq!(widths[0], 8);
    }

    #[test]
    fn column_kinds_follow_the_cells() {
        assert_eq!(column_kind(&["id", "ok"]), ColumnKind::Compact);
        assert_eq!(
            column_kind(&["a sentence with several words inside"]),
            ColumnKind::Narrative
        );
        assert_eq!(
            column_kind(&["sk-abcdefghijklmnopqrstuvwxyz"]),
            ColumnKind::TokenHeavy
        );
    }

    #[test]
    fn a_local_link_shows_the_path_not_the_label() {
        assert_eq!(
            link_target("[the file](src/main.rs)", "/work"),
            Some("/work/src/main.rs".to_string())
        );
        assert_eq!(link_target("[docs](https://example.com)", "/work"), None);
    }

    #[test]
    fn headings_lists_and_code_stay_apart() {
        let blocks = parse("# Title\n\n- one\n- two\n\n```rust\nlet x = 1;\n```\n", 80);
        assert!(matches!(blocks[0], Block::Heading { level: 1, .. }));
        assert!(matches!(blocks[1], Block::Bullet(ref items) if items.len() == 2));
        assert!(matches!(blocks[2], Block::Code { ref lang, .. } if lang == "rust"));
    }

    #[test]
    fn deeper_headings_numbers_quotes_and_rules() {
        let blocks = parse(
            "### Third\n1. first\n2. second\n  - nested\n\n> quoted\n> more\n\n---\n",
            80,
        );
        assert!(matches!(blocks[0], Block::Heading { level: 3, ref text } if text == "Third"));
        let Block::Bullet(items) = &blocks[1] else {
            panic!("expected a list: {blocks:?}");
        };
        assert_eq!(items[0].number.as_deref(), Some("1."));
        assert_eq!(items[2].depth, 1);
        assert_eq!(items[2].number, None);
        assert_eq!(blocks[2], Block::Quote("quoted\nmore".to_string()));
        assert_eq!(blocks[3], Block::Rule);
    }

    #[test]
    fn inline_runs_carry_their_kind() {
        assert_eq!(
            inline("a **bold** and *soft* with `code`"),
            vec![
                (Inline::Plain, "a ".to_string()),
                (Inline::Strong, "bold".to_string()),
                (Inline::Plain, " and ".to_string()),
                (Inline::Emphasis, "soft".to_string()),
                (Inline::Plain, " with ".to_string()),
                (Inline::Code, "code".to_string()),
            ]
        );
    }

    #[test]
    fn snake_case_and_arithmetic_stay_plain() {
        assert_eq!(
            inline("call my_long_name with 2 * 3 * 4"),
            vec![(
                Inline::Plain,
                "call my_long_name with 2 * 3 * 4".to_string()
            )]
        );
    }

    #[test]
    fn an_unclosed_mark_keeps_its_characters() {
        assert_eq!(
            inline("**not closed"),
            vec![(Inline::Plain, "**not closed".to_string())]
        );
    }

    #[test]
    fn web_links_keep_their_url_and_local_links_show_the_path() {
        assert_eq!(
            inline("[docs](https://x.io)"),
            vec![
                (Inline::Link, "docs".to_string()),
                (Inline::Url, " (https://x.io)".to_string()),
            ]
        );
        assert_eq!(
            inline("see [main](src/main.rs)"),
            vec![
                (Inline::Plain, "see ".to_string()),
                (Inline::Link, "src/main.rs".to_string()),
            ]
        );
    }
}
