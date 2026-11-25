//! A cell's style, as ghostty's style.zig holds it: three colours, each unset, a palette index
//! or RGB (the source is kept, so a palette change re-colours what was drawn with an index),
//! and on/off attributes. The screen interns styles (one id a distinct style); SGR edits the
//! cursor's style; DECRQSS reports it.

/// An RGB colour.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

/// A colour as SGR set it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub enum Color {
    /// The default for its role (foreground, background, underline: the foreground).
    #[default]
    None,
    /// An index into the 256-entry palette, read when drawn.
    Palette(u8),
    Rgb(Rgb),
}

/// SGR 4's styles (`4:0`–`4:5`; SGR 21 is double).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub enum Underline {
    #[default]
    None,
    Single,
    Double,
    Curly,
    Dotted,
    Dashed,
}

/// The on/off attributes.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct Flags {
    pub bold: bool,
    pub italic: bool,
    pub faint: bool,
    pub blink: bool,
    pub inverse: bool,
    pub invisible: bool,
    pub strikethrough: bool,
    pub overline: bool,
    pub underline: Underline,
}

/// A whole style. `Style::default()` is the default style, the one id 0 names.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct Style {
    pub fg: Color,
    pub bg: Color,
    pub underline_color: Color,
    pub flags: Flags,
}

impl Style {
    pub fn is_default(&self) -> bool {
        *self == Style::default()
    }
}
