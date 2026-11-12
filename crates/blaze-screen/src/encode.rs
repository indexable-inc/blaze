//! A frame as the bytes one terminal takes: the only place an escape sequence is made. A pure
//! function of the frame and what the terminal does, so the tests read the bytes a driver
//! would write without a terminal.
//!
//! On a terminal a frame is one synchronized update (mode 2026, which a terminal that does
//! not know it ignores): the cursor up over the last region, each of its rows erased (a row
//! at a time, EL: erasing to the end of the screen, ED, from the top-left corner is a clear
//! of the whole screen to tmux, which then pushes what was on it into its history, a copy of
//! the region a frame), the cursor up again, the lines that stay, the new region. Off one it is the lines that stay and nothing else:
//! no cursor movement, no colour unless it was asked for by name, no link, no progress.

use alloc::string::String;
use core::fmt::Write as _;

use blaze_report::{
    palette::Token,
    terminal::{self, Terminal},
};

use crate::frame::{Frame, Line, Progress, Role, Span};

/// The SGR parameters of a role on `terminal`: the palette's value of its token
/// (`blaze_report::palette`), for how many colours the terminal draws and its background.
fn sgr(role: Role, terminal: &Terminal) -> &'static str {
    let token = match role {
        Role::Plain => return "",
        Role::Strong => Token::Emphasis,
        Role::Running => Token::Running,
        Role::Done => Token::Success,
        Role::Waiting => Token::Waiting,
        Role::Cached => Token::Cached,
        Role::Failed => Token::Failure,
        Role::Warned => Token::Warning,
        Role::Dim => Token::Dim,
        Role::Code => Token::Code,
        Role::Command => Token::Command,
        Role::Path => Token::Path,
        Role::Number => Token::Number,
    };
    terminal.sgr(token)
}

fn link_open(out: &mut String, url: &str) {
    out.push_str("\x1b]8;;");
    // A URL holds no control character: one would end the sequence early.
    out.extend(url.chars().filter(|c| !c.is_control()));
    out.push_str("\x1b\\");
}

fn span(out: &mut String, span: &Span, terminal: &Terminal, links: bool) {
    let color = terminal.caps.color;
    let code = sgr(span.role, terminal);
    let linked = links.then_some(span.link.as_deref()).flatten();
    if let Some(url) = linked {
        link_open(out, url);
    }
    match color && !code.is_empty() {
        true => {
            // Writing to a String cannot fail.
            let _ = write!(out, "\x1b[{code}m{}\x1b[0m", span.text);
        }
        // With no colour to say so, code and a command are told from the words around them by
        // backticks.
        false if !color && matches!(span.role, Role::Code | Role::Command) => {
            out.push('`');
            out.push_str(&span.text);
            out.push('`');
        }
        false => out.push_str(&span.text),
    }
    if linked.is_some() {
        out.push_str("\x1b]8;;\x1b\\");
    }
}

fn line(out: &mut String, line: &Line, terminal: &Terminal, linked: bool) {
    match line {
        Line::Spans(spans) => spans.iter().for_each(|it| span(out, it, terminal, linked)),
        Line::Raw { text, links } if linked => {
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

/// `frame` at the end of `out`, for `terminal`: its colour, its links, its progress report
/// and its notifications, each only where it has one.
pub fn encode(frame: &Frame, terminal: &Terminal, out: &mut String) {
    if !frame.live {
        frame.above.iter().for_each(|it| line(out, it, terminal, false));
        return;
    }
    let links = terminal.caps.links;
    if frame.is_empty() {
        return;
    }
    // The update whole, then its brackets: a frame that comes to nothing for this terminal
    // (a notification it has no way to show) writes nothing.
    let mut update = String::new(); // alloc: a frame's bytes, once a frame
    {
        let out = &mut update;
        if frame.erase > 0 {
            // Writing to a String cannot fail.
            let _ = write!(out, "\r\x1b[{}A", frame.erase);
            for _ in 0..frame.erase {
                out.push_str("\x1b[2K\n");
            }
            let _ = write!(out, "\x1b[{}A", frame.erase);
        }
        match frame.hide_cursor {
            Some(true) => out.push_str("\x1b[?25l"),
            Some(false) => out.push_str("\x1b[?25h"),
            None => {}
        }
        frame.above.iter().for_each(|it| line(out, it, terminal, links));
        frame.region.iter().for_each(|it| line(out, it, terminal, links));
        if let Some(progress) = frame.progress {
            let progress = match progress {
                Progress::Clear => terminal::Progress::Clear,
                Progress::Unknown => terminal::Progress::Indeterminate,
                Progress::Percent(percent) => terminal::Progress::Normal(percent),
                Progress::Failed(percent) => terminal::Progress::Error(percent),
            };
            terminal.progress_into(out, progress);
        }
        // A notification is for a build long enough that its person looked away: never for
        // one that ended in the time of a glance, a failure either.
        if let Some(ended) = &frame.ended
            && ended.took_ms / 1000 >= terminal.notify_after_secs
            && terminal.notifies(ended.failed, ended.took_ms / 1000)
        {
            terminal.notify_into(out, &ended.title, &ended.body);
        }
    }
    if update.is_empty() {
        return;
    }
    out.push_str("\x1b[?2026h");
    out.push_str(&update);
    out.push_str("\x1b[?2026l");
}
