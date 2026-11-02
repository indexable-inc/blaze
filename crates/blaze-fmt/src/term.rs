//! The few calls rustfmt makes into the `term` crate, answered with ANSI escapes. rustfmt colours
//! whenever terminfo knows `$TERM`, pipes included; this colours only a terminal, so piped or
//! captured output (`--check` diffs in CI) is plain text.

use std::io::{self, IsTerminal, Write};

pub(crate) mod color {
    pub(crate) type Color = u8;

    pub(crate) const RED: Color = 1;
    pub(crate) const GREEN: Color = 2;
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Attr {
    Bold,
}

pub(crate) struct AnsiTerminal<T: Write> {
    out: T,
}

impl<T: Write> Write for AnsiTerminal<T> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.out.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.out.flush()
    }
}

impl<T: Write> AnsiTerminal<T> {
    pub(crate) fn fg(&mut self, color: color::Color) -> io::Result<()> {
        write!(self.out, "\x1b[3{color}m")
    }

    pub(crate) fn reset(&mut self) -> io::Result<()> {
        write!(self.out, "\x1b[0m")
    }

    pub(crate) fn supports_color(&self) -> bool {
        true
    }

    pub(crate) fn supports_attr(&self, attr: Attr) -> bool {
        match attr {
            Attr::Bold => true,
        }
    }
}

fn colour_allowed() -> bool {
    std::env::var_os("TERM").is_some_and(|term| term != "dumb")
}

pub(crate) fn stdout() -> Option<AnsiTerminal<io::Stdout>> {
    let out = io::stdout();
    (colour_allowed() && out.is_terminal()).then(|| AnsiTerminal { out })
}

pub(crate) fn stderr() -> Option<AnsiTerminal<io::Stderr>> {
    let out = io::stderr();
    (colour_allowed() && out.is_terminal()).then(|| AnsiTerminal { out })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colours_are_ansi_escapes_around_the_text() {
        let mut t = AnsiTerminal { out: Vec::new() };
        assert!(t.supports_color() && t.supports_attr(Attr::Bold));
        t.fg(color::GREEN).unwrap();
        write!(t, "+a").unwrap();
        t.reset().unwrap();
        t.fg(color::RED).unwrap();
        assert_eq!(t.out, b"\x1b[32m+a\x1b[0m\x1b[31m");
    }
}
