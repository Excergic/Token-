//! A unified diff, line by line, ready for the palette. Parsing stays here so
//! a test can pin an addition without a terminal.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffKind {
    Add,
    Del,
    Hunk,
    Meta,
    Context,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: DiffKind,
    pub text: String,
}

pub fn parse_diff(text: &str) -> Vec<DiffLine> {
    text.lines()
        .map(|line| DiffLine {
            kind: kind_of(line),
            text: line.to_string(),
        })
        .collect()
}

fn kind_of(line: &str) -> DiffKind {
    if line.starts_with("+++") || line.starts_with("---") || line.starts_with("diff ") {
        DiffKind::Meta
    } else if line.starts_with("@@") {
        DiffKind::Hunk
    } else if line.starts_with('+') {
        DiffKind::Add
    } else if line.starts_with('-') {
        DiffKind::Del
    } else {
        DiffKind::Context
    }
}

/// True when the text is worth the diff palette rather than plain markdown.
pub fn looks_like_diff(text: &str) -> bool {
    text.lines().any(|line| line.starts_with("@@"))
        || text.lines().any(|line| line.starts_with("diff "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn additions_and_deletions_are_named() {
        let lines = parse_diff("--- a/f\n+++ b/f\n@@ -1 +1 @@\n-old\n+new\n ctx");
        assert_eq!(
            lines.iter().map(|line| line.kind).collect::<Vec<_>>(),
            vec![
                DiffKind::Meta,
                DiffKind::Meta,
                DiffKind::Hunk,
                DiffKind::Del,
                DiffKind::Add,
                DiffKind::Context,
            ]
        );
    }

    #[test]
    fn a_hunk_header_marks_a_diff() {
        assert!(looks_like_diff("@@ -1,2 +1,3 @@\n+added"));
        assert!(!looks_like_diff("just a paragraph\nwith a + sign later"));
    }
}
