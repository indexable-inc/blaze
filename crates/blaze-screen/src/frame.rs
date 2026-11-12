//! What the screen hands back: a frame as data. Nothing here is a byte of an escape sequence;
//! [`crate::encode`] makes those for one terminal's capabilities.

use alloc::{string::String, vec::Vec};

use crate::text;

/// What a span of text means. The encoder gives each a colour; none is told apart by colour
/// alone: a state's glyph and word say it too.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Role {
    /// As the terminal draws text.
    Plain,
    /// The one thing to read on its line.
    Strong,
    Running,
    Done,
    Waiting,
    Cached,
    Failed,
    /// Something is off and the build goes on.
    Warned,
    /// Detail: read second.
    Dim,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Span {
    pub text: String,
    pub role: Role,
    /// Where it leads, on a terminal with hyperlinks: a `file://` URL, a page. The text is
    /// the same with and without.
    pub link: Option<String>,
}

/// One line.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Line {
    Spans(Vec<Span>),
    /// Text drawn elsewhere for this client (a diagnostic, a test's output), written as it
    /// is, with the parts of it that lead somewhere.
    Raw { text: String, links: Vec<Mark> },
}

/// A part of a raw line that is a link: its bytes `from..to`, and where it leads.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Mark {
    pub from: usize,
    pub to: usize,
    pub url: String,
}

impl Line {
    pub fn new() -> Line {
        Line::Spans(Vec::new()) // alloc: none until a span is pushed
    }

    #[must_use]
    pub fn with(mut self, role: Role, text: impl Into<String>) -> Line {
        self.push(role, text);
        self
    }

    pub fn push(&mut self, role: Role, text: impl Into<String>) {
        if let Line::Spans(spans) = self {
            spans.push(Span { text: text.into(), role, link: None }); // alloc: a row's span
        }
    }

    pub fn push_link(&mut self, role: Role, text: impl Into<String>, link: String) {
        if let Line::Spans(spans) = self {
            spans.push(Span { text: text.into(), role, link: Some(link) }); // alloc: as above
        }
    }

    /// The columns it takes; a raw line's are its text's with no escape counted out.
    pub fn width(&self) -> usize {
        match self {
            Line::Spans(spans) => spans.iter().map(|it| text::width(&it.text)).sum(),
            Line::Raw { text, .. } => text::width(text),
        }
    }

    /// Its text, with no role or link.
    pub fn text(&self) -> String {
        match self {
            Line::Spans(spans) => spans.iter().map(|it| it.text.as_str()).collect(), // alloc: tests and the plain form
            Line::Raw { text, .. } => text.clone(), // clone: as above
        }
    }
}

impl Default for Line {
    fn default() -> Line {
        Line::new()
    }
}

/// The terminal's own progress report (OSC 9;4), where it has one.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Progress {
    #[default]
    Clear,
    /// How far is not known yet.
    Unknown,
    Percent(u8),
    Failed(u8),
}

/// One frame: what the driver writes, in this order.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Frame {
    /// The rows of the live region drawn last, as the terminal holds them at its width now:
    /// erased first.
    pub erase: u16,
    /// Lines that stay, appended to the scrollback.
    pub above: Vec<Line>,
    /// The live region, redrawn under them; empty when nothing is live.
    pub region: Vec<Line>,
    /// The cursor is hidden (`Some(true)`) or shown again, when that changes.
    pub hide_cursor: Option<bool>,
    /// The terminal's progress report, when it changes.
    pub progress: Option<Progress>,
    /// A notification: its title and body. Only for a long build's end.
    pub notify: Option<(String, String)>,
}

impl Frame {
    /// What puts a terminal back when the command is ended from outside (an interrupt, a
    /// panic) with `rows` of a region on it: the region erased, the cursor shown, the
    /// terminal's progress report cleared.
    pub fn restore(rows: u16) -> Frame {
        Frame {
            erase: rows,
            hide_cursor: Some(false),
            progress: Some(Progress::Clear),
            ..Frame::default()
        }
    }

    pub fn is_empty(&self) -> bool {
        *self == Frame::default()
    }
}
