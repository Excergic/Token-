//! Semantic colours. Nothing in the screen hardcodes a raw ANSI code at the
//! call site: a role is resolved through the palette for the detected theme
//! and the deepest colour depth the terminal actually has.

/// Light or dark paper. The screen asks the terminal; tests pass this in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Theme {
    Dark,
    Light,
}

/// How many colours we are willing to spend. A truecolor pastel must not be
/// sent to a 16-colour terminal, where it would come out as an accident.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Depth {
    /// Names only: green, red, cyan, magenta. No backgrounds.
    Ansi16,
    Indexed,
    Truecolor,
}

/// A colour we know how to draw at every depth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Swatch {
    /// Leave the terminal's own foreground or background.
    Default,
    Green,
    Red,
    Cyan,
    Magenta,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    pub user: Swatch,
    pub success: Swatch,
    pub error: Swatch,
    pub brand: Swatch,
    pub muted: Swatch,
    pub diff_add: Swatch,
    pub diff_del: Swatch,
    pub diff_add_bg: Swatch,
    pub diff_del_bg: Swatch,
}

/// `COLORFGBG` is `fg;bg` using the 16-colour indexes. A light paper is a
/// high background index. Anything we cannot read stays dark, which is the
/// common case and the one the pastels below were drawn for first.
pub fn theme_from_colorfgbg(value: Option<&str>) -> Theme {
    let Some(bg) = value.and_then(|raw| raw.split(';').nth(1)) else {
        return Theme::Dark;
    };
    let bg = bg.trim().parse::<u8>().unwrap_or(0);
    match bg {
        7 | 15 => Theme::Light,
        _ => Theme::Dark,
    }
}

pub fn depth_from_env(colorterm: Option<&str>, term: Option<&str>, no_color: bool) -> Depth {
    if no_color {
        return Depth::Ansi16;
    }
    match colorterm {
        Some("truecolor") | Some("24bit") => Depth::Truecolor,
        _ if term.is_some_and(|term| term.contains("256color")) => Depth::Indexed,
        _ => Depth::Ansi16,
    }
}

pub fn palette(theme: Theme, depth: Depth) -> Palette {
    let (add_bg, del_bg) = backgrounds(theme, depth);
    Palette {
        user: Swatch::Cyan,
        success: Swatch::Green,
        error: Swatch::Red,
        brand: Swatch::Magenta,
        muted: match depth {
            Depth::Ansi16 => Swatch::Default,
            Depth::Indexed => Swatch::Indexed(245),
            Depth::Truecolor => Swatch::Rgb(140, 140, 140),
        },
        diff_add: Swatch::Green,
        diff_del: Swatch::Red,
        diff_add_bg: add_bg,
        diff_del_bg: del_bg,
    }
}

/// GitHub-style pastels on light paper, deeper ones on dark. ANSI-16 gets no
/// background at all: a 16-colour terminal paints a "pastel" as a solid block.
fn backgrounds(theme: Theme, depth: Depth) -> (Swatch, Swatch) {
    match (theme, depth) {
        (_, Depth::Ansi16) => (Swatch::Default, Swatch::Default),
        (Theme::Light, Depth::Indexed) => (Swatch::Indexed(194), Swatch::Indexed(224)),
        (Theme::Dark, Depth::Indexed) => (Swatch::Indexed(22), Swatch::Indexed(52)),
        (Theme::Light, Depth::Truecolor) => {
            (Swatch::Rgb(230, 255, 236), Swatch::Rgb(255, 235, 233))
        }
        (Theme::Dark, Depth::Truecolor) => (Swatch::Rgb(20, 60, 40), Swatch::Rgb(70, 24, 28)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colorfgbg_picks_the_paper() {
        assert_eq!(theme_from_colorfgbg(Some("15;0")), Theme::Dark);
        assert_eq!(theme_from_colorfgbg(Some("0;15")), Theme::Light);
        assert_eq!(theme_from_colorfgbg(Some("0;7")), Theme::Light);
        assert_eq!(theme_from_colorfgbg(None), Theme::Dark);
        assert_eq!(theme_from_colorfgbg(Some("nonsense")), Theme::Dark);
    }

    #[test]
    fn depth_follows_the_terminal_and_no_color_wins() {
        assert_eq!(
            depth_from_env(Some("truecolor"), None, false),
            Depth::Truecolor
        );
        assert_eq!(
            depth_from_env(None, Some("xterm-256color"), false),
            Depth::Indexed
        );
        assert_eq!(depth_from_env(Some("truecolor"), None, true), Depth::Ansi16);
        assert_eq!(depth_from_env(None, Some("dumb"), false), Depth::Ansi16);
    }

    #[test]
    fn ansi16_diffs_have_no_background() {
        let palette = palette(Theme::Light, Depth::Ansi16);
        assert_eq!(palette.diff_add_bg, Swatch::Default);
        assert_eq!(palette.diff_del_bg, Swatch::Default);
        assert_eq!(palette.success, Swatch::Green);
        assert_eq!(palette.error, Swatch::Red);
        assert_eq!(palette.user, Swatch::Cyan);
        assert_eq!(palette.brand, Swatch::Magenta);
    }

    #[test]
    fn truecolor_light_diffs_are_the_pale_ones() {
        let palette = palette(Theme::Light, Depth::Truecolor);
        assert_eq!(palette.diff_add_bg, Swatch::Rgb(230, 255, 236));
        assert_eq!(palette.diff_del_bg, Swatch::Rgb(255, 235, 233));
    }
}
