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
    Bullet(Vec<String>),
    Table(Table),
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
    let mut bullets = Vec::new();
    let lines: Vec<&str> = src.lines().collect();
    let mut index = 0;

    let flush_paragraph = |paragraph: &mut String, blocks: &mut Vec<Block>| {
        if !paragraph.is_empty() {
            blocks.push(Block::Paragraph(std::mem::take(paragraph)));
        }
    };
    let flush_bullets = |bullets: &mut Vec<String>, blocks: &mut Vec<Block>| {
        if !bullets.is_empty() {
            blocks.push(Block::Bullet(std::mem::take(bullets)));
        }
    };

    while index < lines.len() {
        let line = lines[index];
        if line.starts_with("```") {
            flush_paragraph(&mut paragraph, &mut blocks);
            flush_bullets(&mut bullets, &mut blocks);
            let lang = line.trim_start_matches('`').trim().to_string();
            index += 1;
            let mut body = String::new();
            while index < lines.len() && !lines[index].starts_with("```") {
                if !body.is_empty() {
                    body.push('\n');
                }
                body.push_str(lines[index]);
                index += 1;
            }
            if index < lines.len() {
                index += 1;
            }
            blocks.push(Block::Code { lang, body });
            continue;
        }
        if line.starts_with('|') {
            flush_paragraph(&mut paragraph, &mut blocks);
            flush_bullets(&mut bullets, &mut blocks);
            let mut raw = Vec::new();
            while index < lines.len() && lines[index].starts_with('|') {
                raw.push(lines[index]);
                index += 1;
            }
            blocks.push(Block::Table(layout_table(&raw, width)));
            continue;
        }
        if let Some(text) = line.strip_prefix("# ") {
            flush_paragraph(&mut paragraph, &mut blocks);
            flush_bullets(&mut bullets, &mut blocks);
            blocks.push(Block::Heading {
                level: 1,
                text: text.to_string(),
            });
            index += 1;
            continue;
        }
        if let Some(text) = line.strip_prefix("## ") {
            flush_paragraph(&mut paragraph, &mut blocks);
            flush_bullets(&mut bullets, &mut blocks);
            blocks.push(Block::Heading {
                level: 2,
                text: text.to_string(),
            });
            index += 1;
            continue;
        }
        if let Some(text) = line.strip_prefix("- ") {
            flush_paragraph(&mut paragraph, &mut blocks);
            bullets.push(text.to_string());
            index += 1;
            continue;
        }
        if line.trim().is_empty() {
            flush_paragraph(&mut paragraph, &mut blocks);
            flush_bullets(&mut bullets, &mut blocks);
            index += 1;
            continue;
        }
        flush_bullets(&mut bullets, &mut blocks);
        if !paragraph.is_empty() {
            paragraph.push(' ');
        }
        paragraph.push_str(line.trim());
        index += 1;
    }
    flush_paragraph(&mut paragraph, &mut blocks);
    flush_bullets(&mut bullets, &mut blocks);
    blocks
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
}
