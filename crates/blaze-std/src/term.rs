//! The terminal as a device (docs/v2/plan/design/std.md, "The rest of the set", `Term`): its
//! size, raw mode, styled text and key events. `Out` is only bytes; this is the device behind
//! them, which a program may use only if it was handed one.
//!
//! **Finishing.** Raw mode is a linear guard, [`RawMode`]: a terminal left raw is broken for
//! whoever uses it next, so leaving the guard is a compile error and it ends with
//! [`RawMode::end`]. A panic that unwinds past one runs its abandonment path, its `Drop`, which
//! puts the terminal back without waiting.
//!
//! **Provider.** The simulation only ([`crate::hosted::SimHost::term`]): blaze-core has no
//! effect for a terminal's size, its modes or its input, so a native `Io` does not offer one.

use alloc::{borrow::ToOwned, collections::VecDeque, string::String, vec::Vec};
use core::fmt;

use crate::{
    cx::Cx,
    hosted::{self, Refused},
    io::Io,
};

/// A terminal's size in cells.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Size {
    pub cols: u16,
    pub rows: u16,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Color {
    /// The terminal's own.
    #[default]
    Default,
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
    Cyan,
    White,
}

/// How text is drawn.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Style {
    pub color: Color,
    pub bold: bool,
}

/// Text and how it is drawn: what [`Term::write`] takes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Styled<'a> {
    pub text: &'a str,
    pub style: Style,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Key {
    Char(char),
    Enter,
    Escape,
    Backspace,
    Tab,
    Up,
    Down,
    Left,
    Right,
}

/// One key pressed, with its modifiers.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct KeyEvent {
    pub key: Key,
    pub ctrl: bool,
    pub alt: bool,
}

impl KeyEvent {
    /// `key` with no modifier.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn plain(key: Key) -> KeyEvent {
        KeyEvent { key, ctrl: false, alt: false }
    }
}

/// The simulated terminal.
#[derive(Debug)]
pub struct SimTerm {
    pub size: Size,
    /// Whether it is in raw mode now.
    pub raw: bool,
    /// Everything written, in order.
    pub written: Vec<(String, Style)>,
    /// The keys it will send, in order.
    pub keys: VecDeque<KeyEvent>,
}

impl SimTerm {
    /// 80 by 24, cooked, with nothing written and no key to send.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::nonblocking]
    pub fn new() -> SimTerm {
        SimTerm {
            size: Size { cols: 80, rows: 24 },
            raw: false,
            // alloc: none, the simulated terminal's empty record.
            written: Vec::new(),
            // alloc: none, the simulated terminal's empty script.
            keys: VecDeque::new(),
        }
    }
}

impl Default for SimTerm {
    #[blaze::pure]
#[blaze::nopanic]
#[blaze::nonblocking]
fn default() -> SimTerm {
        SimTerm::new()
    }
}

/// What a terminal call was.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TermOp {
    Size,
    Raw,
    EndRaw,
    Write,
    Key,
}

/// A terminal call that failed: the call (its thing is the terminal) and how.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: an error
pub struct TermError {
    pub op: TermOp,
    pub kind: Refused,
}

impl fmt::Display for TermError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "bstd::term {:?} of the terminal: {}", self.op, self.kind)
    }
}

impl core::error::Error for TermError {}

/// The terminal: from [`Io::term`].
#[derive(Clone)]
pub struct Term {
    io: Io,
}

impl Term {
    #[blaze::nopanic]
#[blaze::noalloc]
#[blaze::nonblocking]
pub(crate) fn new(io: Io) -> Term {
        Term { io }
    }

    /// Its size now.
    pub fn size(&self, cx: &Cx) -> Result<Size, TermError> {
        hosted::call(&self.io, cx, |host| Ok(host.term.size)).map_err(|kind| TermError { op: TermOp::Size, kind })
    }

    /// The terminal put in raw mode (keys come as pressed, nothing is echoed) until the guard
    /// is ended. One that is raw already is refused: two guards would each put it back.
    pub fn raw(&self, cx: &Cx) -> Result<RawMode, TermError> {
        let entered = hosted::call(&self.io, cx, |host| match host.term.raw {
            true => Err(Refused::Host("the terminal is in raw mode already")),
            false => {
                host.term.raw = true;
                Ok(())
            }
        });
        match entered {
            Ok(()) => Ok(RawMode { held: Held { io: self.io.clone(), on: true } }),
            Err(kind) => Err(TermError { op: TermOp::Raw, kind }),
        }
    }

    /// `text` drawn at the cursor, each piece in its style.
    pub fn write(&self, cx: &Cx, text: &[Styled<'_>]) -> Result<(), TermError> {
        hosted::call(&self.io, cx, |host| {
            for piece in text {
                // alloc: the simulated terminal's record of what was written.
                host.term.written.push((piece.text.to_owned(), piece.style));
            }
            Ok(())
        })
        .map_err(|kind| TermError { op: TermOp::Write, kind })
    }

    /// The next key pressed: waits for it until `cx`'s deadline.
    pub fn key(&self, cx: &Cx) -> Result<KeyEvent, TermError> {
        hosted::wait(&self.io, cx, "bstd::term key", |host| host.term.keys.pop_front().map(Ok))
            .map_err(|kind| TermError { op: TermOp::Key, kind })
    }
}

impl fmt::Debug for Term {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Term")
    }
}

/// The terminal held in raw mode, ended with [`RawMode::end`]. Linear (see the module's doc):
/// a panic that drops one puts the terminal back without waiting.
#[blaze::linear]
#[must_use = "raw mode is ended with end(cx)"]
pub struct RawMode {
    held: Held,
}

/// Raw mode's abandonment path.
struct Held {
    io: Io,
    /// Whether the terminal is still to be put back: cleared by [`RawMode::end`].
    on: bool,
}

impl Drop for Held {
    fn drop(&mut self) {
        if self.on {
            let _ = self.io.script(|host| host.term.raw = false);
        }
    }
}

impl RawMode {
    /// The terminal put back as it was. It is put back whatever `cx` says: a stopped task
    /// still leaves the terminal usable. The one way a `RawMode` ends.
    #[blaze::consumes]
    pub fn end(mut self, cx: &Cx) -> Result<(), TermError> {
        self.held.on = false;
        cx.event("bstd::term: raw mode ended");
        match self.held.io.script(|host| host.term.raw = false) {
            Some(()) => Ok(()),
            None => Err(TermError { op: TermOp::EndRaw, kind: Refused::NoProvider }),
        }
    }
}

impl fmt::Debug for RawMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RawMode")
    }
}
