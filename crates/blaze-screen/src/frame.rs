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
    /// Code or an identifier. Where there is no colour it is put between backticks.
    Code,
    /// A command the reader can run as it is. Between backticks where there is no colour.
    Command,
    Path,
    /// The figure of a line: a count, a time.
    Number,
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

    /// `text`'s spans at the end of the line, each with the role its type gives it; words take
    /// `words`. A path is shown from `root` and leads to its file.
    pub fn push_text(&mut self, text: &blaze_report::Text, words: Role, root: Option<&str>) {
        use blaze_report::Span as S;
        for span in &text.0 {
            match span {
                S::Plain(it) => self.push(words, it.as_str()),
                S::Code(it) | S::Var(it) => self.push(Role::Code, it.as_str()),
                S::Command(it) => self.push(Role::Command, it.as_str()),
                S::Strong(it) => self.push(Role::Strong, it.as_str()),
                S::Path(it) => {
                    let shown = root
                        .and_then(|root| it.strip_prefix(root))
                        .map_or(it.as_str(), |below| below.trim_start_matches('/'));
                    match it.starts_with('/') {
                        true => self.push_link(Role::Path, shown, crate::file_url(it)),
                        false => self.push(Role::Path, shown),
                    }
                }
                S::Package { name, version } => {
                    self.push(Role::Strong, name.as_str());
                    if let Some(version) = version {
                        self.push(Role::Dim, alloc::format!(" {version}")); // alloc: a span
                    }
                }
                S::Version(it) => self.push(Role::Dim, it.as_str()),
                S::Hash(it) => self.push(Role::Dim, it.get(..8).unwrap_or(it)),
                S::Count(n) => self.push(Role::Number, alloc::format!("{n}")), // alloc: a span
                S::Took(ns) => self.push(Role::Number, text::time(ns / 1_000_000)),
            }
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

/// A build's end, for a notification of it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Ended {
    pub title: String,
    pub body: String,
    pub failed: bool,
    pub took_ms: u64,
}

/// One frame: what the driver writes, in this order.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Frame {
    /// For a terminal whose region is redrawn ([`crate::Caps::live`]). A frame that is not
    /// is lines that stay and nothing else: the encoder writes no sequence for it but a
    /// colour asked for by name.
    pub live: bool,
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
    /// The build ended: what a notification of it says, where the terminal has them and its
    /// settings ask for one of a build that failed or took this long.
    pub ended: Option<Ended>,
}

impl Frame {
    /// What puts a terminal back when the command is ended from outside (an interrupt, a
    /// panic) with `rows` of a region on it: the region erased, the cursor shown, the
    /// terminal's progress report cleared.
    pub fn restore(rows: u16) -> Frame {
        Frame {
            live: true,
            erase: rows,
            hide_cursor: Some(false),
            progress: Some(Progress::Clear),
            ..Frame::default()
        }
    }

    /// Nothing to write.
    pub fn is_empty(&self) -> bool {
        *self == Frame { live: self.live, ..Frame::default() }
    }
}
