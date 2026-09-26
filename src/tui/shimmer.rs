//! Motion that does not invent a colour. The highlight is bold against the
//! terminal's own foreground, so it still reads on a 16-colour paper.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Emphasis {
    Dim,
    Normal,
    Bold,
}

/// One cell of a breathing mark walking across `len` glyphs.
pub fn emphasis_at(len: usize, index: usize, phase: usize) -> Emphasis {
    if len == 0 || index >= len {
        return Emphasis::Normal;
    }
    let head = phase % len;
    match index.abs_diff(head) {
        0 => Emphasis::Bold,
        1 => Emphasis::Normal,
        _ => Emphasis::Dim,
    }
}

/// A one-line companion. Four frames, idle only, never a second of content.
pub fn pet_frame(phase: usize) -> &'static str {
    const FRAMES: [&str; 4] = ["(· ·)", "(·‿·)", "(ˇ‿ˇ)", "(·‿·)"];
    FRAMES[phase % FRAMES.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bold_cell_walks_and_wraps() {
        assert_eq!(emphasis_at(4, 0, 0), Emphasis::Bold);
        assert_eq!(emphasis_at(4, 1, 0), Emphasis::Normal);
        assert_eq!(emphasis_at(4, 3, 0), Emphasis::Dim);
        assert_eq!(emphasis_at(4, 0, 4), Emphasis::Bold);
        assert_eq!(emphasis_at(0, 0, 1), Emphasis::Normal);
    }

    #[test]
    fn the_pet_cycles() {
        assert_eq!(pet_frame(0), "(· ·)");
        assert_eq!(pet_frame(4), pet_frame(0));
        assert_ne!(pet_frame(0), pet_frame(1));
    }
}
