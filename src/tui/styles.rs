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
    Yellow,
    Blue,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

/// Colours for highlighted code, one per token kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Syntax {
    pub plain: Swatch,
    pub keyword: Swatch,
    pub string: Swatch,
    pub comment: Swatch,
    pub number: Swatch,
    pub type_name: Swatch,
    pub function: Swatch,
    pub macro_name: Swatch,
    pub constant: Swatch,
    pub punct: Swatch,
    pub attr: Swatch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    pub depth: Depth,
    pub user: Swatch,
    pub success: Swatch,
    pub error: Swatch,
    pub warning: Swatch,
    pub brand: Swatch,
    pub accent: Swatch,
    pub info: Swatch,
    pub muted: Swatch,
    /// Body text. The terminal's own foreground, so it suits either paper.
    pub text: Swatch,
    /// `**bold**` in an answer: the part the model meant to stand out.
    pub strong: Swatch,
    pub emphasis: Swatch,
    pub link: Swatch,
    pub quote: Swatch,
    pub inline_code: Swatch,
    pub inline_code_bg: Swatch,
    pub code_bg: Swatch,
    /// The header bar.
    pub surface: Swatch,
    pub user_bg: Swatch,
    pub border: Swatch,
    pub diff_add: Swatch,
    pub diff_del: Swatch,
    pub diff_add_bg: Swatch,
    pub diff_del_bg: Swatch,
    pub syntax: Syntax,
    stops: [(u8, u8, u8); 3],
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

/// One role at every depth: the RGB for truecolor, the xterm index for 256,
/// and the name for 16.
type Role = ((u8, u8, u8), u8, Swatch);

fn pick(depth: Depth, (rgb, index, named): Role) -> Swatch {
    match depth {
        Depth::Truecolor => Swatch::Rgb(rgb.0, rgb.1, rgb.2),
        Depth::Indexed => Swatch::Indexed(index),
        Depth::Ansi16 => named,
    }
}

/// A background exists only where a pastel can be drawn as one.
fn pick_bg(depth: Depth, role: Role) -> Swatch {
    match depth {
        Depth::Ansi16 => Swatch::Default,
        _ => pick(depth, role),
    }
}

pub fn palette(theme: Theme, depth: Depth) -> Palette {
    let (add_bg, del_bg) = backgrounds(theme, depth);
    let dark = theme == Theme::Dark;
    let role =
        |dark_role: Role, light_role: Role| pick(depth, if dark { dark_role } else { light_role });
    let bg = |dark_role: Role, light_role: Role| {
        pick_bg(depth, if dark { dark_role } else { light_role })
    };
    use Swatch::*;
    Palette {
        depth,
        user: role(((122, 162, 247), 111, Cyan), ((46, 126, 233), 33, Cyan)),
        success: role(((158, 206, 106), 149, Green), ((88, 117, 57), 64, Green)),
        error: role(((247, 118, 142), 210, Red), ((245, 42, 101), 197, Red)),
        warning: role(
            ((224, 175, 104), 179, Yellow),
            ((140, 108, 62), 136, Yellow),
        ),
        brand: role(
            ((187, 154, 247), 141, Magenta),
            ((120, 71, 189), 97, Magenta),
        ),
        accent: role(
            ((255, 121, 198), 212, Magenta),
            ((190, 50, 140), 162, Magenta),
        ),
        info: role(((125, 207, 255), 117, Cyan), ((0, 113, 151), 31, Cyan)),
        muted: role(
            ((115, 122, 162), 103, Default),
            ((132, 140, 181), 103, Default),
        ),
        text: Default,
        strong: role(((255, 199, 119), 222, Yellow), ((198, 90, 0), 166, Yellow)),
        emphasis: role(((115, 218, 202), 116, Cyan), ((17, 140, 116), 30, Cyan)),
        link: role(((125, 207, 255), 117, Blue), ((46, 126, 233), 33, Blue)),
        quote: role(
            ((154, 165, 206), 146, Default),
            ((104, 112, 154), 60, Default),
        ),
        inline_code: role(((255, 158, 100), 215, Yellow), ((143, 94, 21), 94, Yellow)),
        inline_code_bg: bg(
            ((41, 46, 66), 236, Default),
            ((225, 226, 231), 254, Default),
        ),
        code_bg: bg(
            ((30, 32, 48), 235, Default),
            ((236, 236, 240), 255, Default),
        ),
        surface: bg(
            ((36, 40, 59), 236, Default),
            ((220, 222, 232), 253, Default),
        ),
        user_bg: bg(
            ((41, 46, 72), 237, Default),
            ((216, 226, 245), 189, Default),
        ),
        border: role(
            ((65, 72, 104), 60, Default),
            ((168, 174, 203), 146, Default),
        ),
        diff_add: Green,
        diff_del: Red,
        diff_add_bg: add_bg,
        diff_del_bg: del_bg,
        syntax: Syntax {
            plain: role(
                ((192, 202, 245), 189, Default),
                ((52, 59, 88), 237, Default),
            ),
            keyword: role(
                ((187, 154, 247), 141, Magenta),
                ((152, 84, 241), 98, Magenta),
            ),
            string: role(((158, 206, 106), 149, Green), ((88, 117, 57), 64, Green)),
            comment: role(
                ((86, 95, 137), 60, Default),
                ((132, 140, 181), 103, Default),
            ),
            number: role(((255, 158, 100), 215, Yellow), ((150, 80, 0), 130, Yellow)),
            type_name: role(((42, 195, 222), 44, Cyan), ((0, 113, 151), 31, Cyan)),
            function: role(((122, 162, 247), 111, Blue), ((46, 126, 233), 33, Blue)),
            macro_name: role(((125, 207, 255), 117, Cyan), ((0, 113, 151), 31, Cyan)),
            constant: role(((255, 158, 100), 215, Yellow), ((150, 80, 0), 130, Yellow)),
            punct: role(
                ((137, 221, 255), 117, Default),
                ((104, 112, 154), 60, Default),
            ),
            attr: role(
                ((224, 175, 104), 179, Yellow),
                ((140, 108, 62), 136, Yellow),
            ),
        },
        stops: if dark {
            [(187, 154, 247), (255, 121, 198), (125, 207, 255)]
        } else {
            [(120, 71, 189), (190, 50, 140), (0, 113, 151)]
        },
    }
}

impl Palette {
    /// The brand gradient at `t` in 0..=1. A 256-colour terminal gets the
    /// nearest cube entry, and a 16-colour one gets two names, not a smear.
    pub fn gradient(&self, t: f32) -> Swatch {
        let t = t.clamp(0.0, 1.0);
        match self.depth {
            Depth::Ansi16 if t < 0.5 => Swatch::Magenta,
            Depth::Ansi16 => Swatch::Cyan,
            Depth::Indexed => {
                let (r, g, b) = self.blend(t);
                Swatch::Indexed(cube_index(r, g, b))
            }
            Depth::Truecolor => {
                let (r, g, b) = self.blend(t);
                Swatch::Rgb(r, g, b)
            }
        }
    }

    fn blend(&self, t: f32) -> (u8, u8, u8) {
        let (from, to, local) = if t < 0.5 {
            (self.stops[0], self.stops[1], t * 2.0)
        } else {
            (self.stops[1], self.stops[2], (t - 0.5) * 2.0)
        };
        let mix = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * local).round() as u8;
        (mix(from.0, to.0), mix(from.1, to.1), mix(from.2, to.2))
    }
}

/// The nearest entry in the xterm 6x6x6 cube.
fn cube_index(r: u8, g: u8, b: u8) -> u8 {
    const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    let nearest = |value: u8| {
        LEVELS
            .iter()
            .enumerate()
            .min_by_key(|(_, level)| level.abs_diff(value))
            .map(|(index, _)| index as u8)
            .unwrap_or(0)
    };
    16 + 36 * nearest(r) + 6 * nearest(g) + nearest(b)
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
    fn ansi16_has_no_backgrounds_anywhere() {
        let palette = palette(Theme::Dark, Depth::Ansi16);
        for swatch in [
            palette.code_bg,
            palette.inline_code_bg,
            palette.surface,
            palette.user_bg,
        ] {
            assert_eq!(swatch, Swatch::Default);
        }
    }

    #[test]
    fn truecolor_light_diffs_are_the_pale_ones() {
        let palette = palette(Theme::Light, Depth::Truecolor);
        assert_eq!(palette.diff_add_bg, Swatch::Rgb(230, 255, 236));
        assert_eq!(palette.diff_del_bg, Swatch::Rgb(255, 235, 233));
    }

    #[test]
    fn the_gradient_runs_between_its_stops_at_every_depth() {
        let truecolor = palette(Theme::Dark, Depth::Truecolor);
        assert_eq!(truecolor.gradient(0.0), Swatch::Rgb(187, 154, 247));
        assert_eq!(truecolor.gradient(1.0), Swatch::Rgb(125, 207, 255));
        assert!(matches!(
            palette(Theme::Dark, Depth::Indexed).gradient(0.3),
            Swatch::Indexed(16..=231)
        ));
        let ansi = palette(Theme::Dark, Depth::Ansi16);
        assert_eq!(ansi.gradient(0.1), Swatch::Magenta);
        assert_eq!(ansi.gradient(0.9), Swatch::Cyan);
    }

    #[test]
    fn the_cube_lookup_lands_on_the_corners() {
        assert_eq!(cube_index(0, 0, 0), 16);
        assert_eq!(cube_index(255, 255, 255), 231);
        assert_eq!(cube_index(255, 0, 0), 196);
    }
}
