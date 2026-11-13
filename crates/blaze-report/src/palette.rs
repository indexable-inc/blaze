//! The palette: what a colour means, once. Code names a [`Token`] (success, a command, a
//! path), never a colour; the value is picked here for how many colours the terminal has
//! ([`Depth`]) and whether its background is dark or light ([`Ground`]).
//!
//! The truecolor values were held to WCAG AA (a contrast of 4.5 or more) against the
//! backgrounds terminals ship with: black, `#1c1c1c`, `#1e1e1e`, `#282c34` and Solarized's
//! `#002b36` for the dark set (the least is `cached` at 4.55), white, `#f5f5f5`, `#fefefe`
//! and Solarized's `#fdf6e3` for the light set (the least is `path` at 5.84). The 256-colour
//! values are the nearest of xterm's cube picked by hand so no two tokens share one. With 16
//! colours the terminal's own theme gives the value, so only its eight basic colours are
//! named, with bold and underline for weight.
//!
//! No token is the only carrier of what it says. Simulated protanopia and deuteranopia
//! (Machado's matrices) leave `success` and `failure`, and `running` and `waiting`, a few
//! ΔE apart: each such pair is told apart by its glyph and word (`✓` and `✗`, `▶` and `◦`)
//! and `failure` is bold.

/// What a colour says.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Token {
    Success,
    Failure,
    Warning,
    Running,
    Waiting,
    Cached,
    /// Code or an identifier: a function's path, a flag, a variable.
    Code,
    /// A command the reader can run as it is.
    Command,
    Path,
    /// A number and its unit: the one figure of a line.
    Number,
    /// Detail, read second.
    Dim,
    Emphasis,
}

impl Token {
    pub const ALL: [Token; 12] = [
        Token::Success,
        Token::Failure,
        Token::Warning,
        Token::Running,
        Token::Waiting,
        Token::Cached,
        Token::Code,
        Token::Command,
        Token::Path,
        Token::Number,
        Token::Dim,
        Token::Emphasis,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            Token::Success => "success",
            Token::Failure => "failure",
            Token::Warning => "warning",
            Token::Running => "running",
            Token::Waiting => "waiting",
            Token::Cached => "cached",
            Token::Code => "code",
            Token::Command => "command",
            Token::Path => "path",
            Token::Number => "number",
            Token::Dim => "dim",
            Token::Emphasis => "emphasis",
        }
    }
}

/// How many colours the terminal draws.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Depth {
    /// None: no sequence is written.
    #[default]
    None,
    /// The eight basic colours, bold, faint and underline.
    Ansi16,
    Ansi256,
    /// 24-bit: `COLORTERM` says `truecolor` or `24bit`, or the terminal is one known to.
    True,
}

/// The terminal's background.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Ground {
    #[default]
    Dark,
    Light,
}

/// The SGR parameters of `token` (what goes between `ESC [` and `m`); empty for no style.
pub const fn sgr(token: Token, depth: Depth, ground: Ground) -> &'static str {
    use Token::*;
    match depth {
        Depth::None => "",
        Depth::Ansi16 => match token {
            Success => "32",
            Failure => "1;31",
            Warning => "1;33",
            Running => "36",
            Waiting => "35",
            Cached | Dim => "2",
            Code | Number | Emphasis => "1",
            Command => "1;4",
            Path => "4",
        },
        Depth::Ansi256 => match (token, ground) {
            (Dim, _) => "2",
            (Number | Emphasis, _) => "1",
            (Success, Ground::Dark) => "38;5;77",
            (Failure, Ground::Dark) => "1;38;5;203",
            (Warning, Ground::Dark) => "38;5;208",
            (Running, Ground::Dark) => "38;5;81",
            (Waiting, Ground::Dark) => "38;5;183",
            (Cached, Ground::Dark) => "38;5;245",
            (Code, Ground::Dark) => "38;5;180",
            (Command, Ground::Dark) => "4;38;5;111",
            (Path, Ground::Dark) => "38;5;116",
            (Success, Ground::Light) => "38;5;22",
            (Failure, Ground::Light) => "1;38;5;124",
            (Warning, Ground::Light) => "38;5;130",
            (Running, Ground::Light) => "38;5;24",
            (Waiting, Ground::Light) => "38;5;61",
            (Cached, Ground::Light) => "38;5;241",
            (Code, Ground::Light) => "38;5;58",
            (Command, Ground::Light) => "4;38;5;25",
            (Path, Ground::Light) => "38;5;30",
        },
        Depth::True => match (token, ground) {
            (Dim, _) => "2",
            (Number | Emphasis, _) => "1",
            (Success, Ground::Dark) => "38;2;63;185;80",
            (Failure, Ground::Dark) => "1;38;2;255;123;114",
            (Warning, Ground::Dark) => "38;2;240;136;62",
            (Running, Ground::Dark) => "38;2;86;194;230",
            (Waiting, Ground::Dark) => "38;2;200;162;242",
            (Cached, Ground::Dark) => "38;2;139;148;158",
            (Code, Ground::Dark) => "38;2;230;192;123",
            (Command, Ground::Dark) => "4;38;2;121;192;255",
            (Path, Ground::Dark) => "38;2;126;224;210",
            (Success, Ground::Light) => "38;2;17;99;41",
            (Failure, Ground::Light) => "1;38;2;179;38;30",
            (Warning, Ground::Light) => "38;2;138;70;0",
            (Running, Ground::Light) => "38;2;0;96;122",
            (Waiting, Ground::Light) => "38;2;102;57;186",
            (Cached, Ground::Light) => "38;2;87;96;106",
            (Code, Ground::Light) => "38;2;77;90;0",
            (Command, Ground::Light) => "4;38;2;5;80;174",
            (Path, Ground::Light) => "38;2;15;107;98",
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// No two tokens that carry a hue share a value at any depth above 16 colours, and every
    /// token has a style wherever there is colour.
    #[test]
    fn every_token_has_its_own_value() {
        for depth in [Depth::Ansi16, Depth::Ansi256, Depth::True] {
            for ground in [Ground::Dark, Ground::Light] {
                let hued = [
                    Token::Success,
                    Token::Failure,
                    Token::Warning,
                    Token::Running,
                    Token::Waiting,
                    Token::Code,
                    Token::Command,
                    Token::Path,
                ];
                for token in Token::ALL {
                    assert!(!sgr(token, depth, ground).is_empty(), "{token:?} at {depth:?}");
                    assert_eq!(sgr(token, Depth::None, ground), "");
                }
                for (at, a) in hued.iter().enumerate() {
                    for b in &hued[at + 1..] {
                        assert_ne!(sgr(*a, depth, ground), sgr(*b, depth, ground), "{a:?} and {b:?} at {depth:?} on {ground:?}");
                    }
                }
            }
        }
    }
}
