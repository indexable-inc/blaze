//! A frame as the bytes one terminal takes: the only place an escape sequence is made. A pure
//! function of the frame and what the terminal does, so the tests read the bytes a driver
//! would write without a terminal.
//!
//! On a terminal a frame is one synchronized update (mode 2026, which a terminal that does
//! not know it ignores): the cursor up over the last region, the screen erased from there,
//! the lines that stay, the new region. Off one it is the lines that stay and nothing else:
//! no cursor movement, no colour unless it was asked for by name, no link, no progress.

use alloc::string::String;
use core::fmt::Write as _;

use blaze_report::terminal::{self, Terminal};

use crate::frame::{Frame, Line, Progress, Role, Span};

/// How a frame is written.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Paint {
    /// A terminal whose region is redrawn ([`crate::Caps::live`]).
    pub live: bool,
    /// Its colour, links, progress report and notifications.
    pub terminal: Terminal,
}

impl Paint {
    /// A pipe: text and newlines.
    pub const PIPE: Paint = Paint { live: false, terminal: Terminal::PIPE };
}

/// The SGR parameters of a role: a palette of the eight colours every terminal has, each
/// readable on a light and on a dark background (no white, no black, no bright variant), with
/// bold and faint for weight.
fn sgr(role: Role) -> &'static str {
    match role {
        Role::Plain => "",
        Role::Strong => "1",
        Role::Running => "36",
        Role::Done => "32",
        Role::Waiting => "33",
        Role::Cached => "34",
        Role::Failed => "1;31",
        Role::Warned => "1;33",
        Role::Dim => "2",
    }
}

fn link_open(out: &mut String, url: &str) {
    out.push_str("\x1b]8;;");
    // A URL holds no control character: one would end the sequence early.
    out.extend(url.chars().filter(|c| !c.is_control()));
    out.push_str("\x1b\\");
}

fn span(out: &mut String, span: &Span, paint: &Paint) {
    let caps = paint.terminal.caps;
    let code = sgr(span.role);
    let linked = caps.links.then_some(span.link.as_deref()).flatten();
    if let Some(url) = linked {
        link_open(out, url);
    }
    match caps.color && !code.is_empty() {
        true => {
            // Writing to a String cannot fail.
            let _ = write!(out, "\x1b[{code}m{}\x1b[0m", span.text);
        }
        false => out.push_str(&span.text),
    }
    if linked.is_some() {
        out.push_str("\x1b]8;;\x1b\\");
    }
}

fn line(out: &mut String, line: &Line, paint: &Paint) {
    match line {
        Line::Spans(spans) => spans.iter().for_each(|it| span(out, it, paint)),
        Line::Raw { text, links } if paint.terminal.caps.links => {
            let mut at = 0;
            for mark in links {
                let (Some(before), Some(inside)) = (text.get(at..mark.from), text.get(mark.from..mark.to))
                else {
                    continue;
                };
                out.push_str(before);
                link_open(out, &mark.url);
                out.push_str(inside);
                out.push_str("\x1b]8;;\x1b\\");
                at = mark.to;
            }
            out.push_str(text.get(at..).unwrap_or(""));
        }
        Line::Raw { text, .. } => out.push_str(text),
    }
    out.push('\n');
}

/// `frame` at the end of `out`.
pub fn encode(frame: &Frame, paint: &Paint, out: &mut String) {
    if !paint.live {
        frame.above.iter().for_each(|it| line(out, it, paint));
        return;
    }
    if frame.is_empty() {
        return;
    }
    out.push_str("\x1b[?2026h");
    if frame.erase > 0 {
        // Writing to a String cannot fail.
        let _ = write!(out, "\r\x1b[{}A\x1b[J", frame.erase);
    }
    match frame.hide_cursor {
        Some(true) => out.push_str("\x1b[?25l"),
        Some(false) => out.push_str("\x1b[?25h"),
        None => {}
    }
    frame.above.iter().for_each(|it| line(out, it, paint));
    frame.region.iter().for_each(|it| line(out, it, paint));
    if let Some(progress) = frame.progress {
        let progress = match progress {
            Progress::Clear => terminal::Progress::Clear,
            Progress::Unknown => terminal::Progress::Indeterminate,
            Progress::Percent(percent) => terminal::Progress::Normal(percent),
            Progress::Failed(percent) => terminal::Progress::Error(percent),
        };
        paint.terminal.progress_into(out, progress);
    }
    if let Some((title, body)) = &frame.notify {
        paint.terminal.notify_into(out, title, body);
    }
    out.push_str("\x1b[?2026l");
}
