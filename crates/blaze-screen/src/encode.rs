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
use core::fmt;
use core::fmt::Write as _;

use blaze_report::{
    palette::Token,
    terminal::{self, Terminal},
};

use crate::frame::{Frame, Line, Progress, Role, Span};
use crate::run_line::{LineBuf, StatusFrame, StatusLine};

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
        if let Some(cols) = frame.fresh_line.filter(|cols| *cols > 1) {
            // A mark, blanks to the line's end and one more column's worth of wrap, then back
            // to the first column and that line erased: at the start of a line this leaves
            // nothing; in the middle of one the mark stays at its end and the cursor is on
            // the next.
            out.push('%');
            out.extend(core::iter::repeat_n(' ', usize::from(cols) - 1));
            out.push_str("\r\x1b[K");
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

/// The run's live line ([`crate::RunStatus`]) as the bytes a terminal takes, onto `out`.
///
/// A draw is one synchronized update at the cursor, which it leaves where it was: the cursor
/// saved (DECSC), autowrap off (DECAWM: a line that met the margin would wrap, and its rest be
/// a line the erase does not reach), the text in at most `cols - 1` columns, the rest of the
/// line erased (EL), autowrap back on, the cursor restored (DECRC). Nothing scrolls and no
/// line feed is written, so the text a program left on the line, mid-line or not, stays where
/// it is. An erase is EL from the cursor: what the draw wrote, and nothing before it.
pub fn encode_status(frame: &StatusFrame<'_>, terminal: &Terminal, out: &mut dyn fmt::Write) -> fmt::Result {
    match frame {
        StatusFrame::Erase => out.write_str("\x1b[K"),
        StatusFrame::Draw(line) => {
            out.write_str("\x1b[?2026h\x1b7\x1b[?7l")?;
            status_text(line, terminal, out)?;
            out.write_str("\x1b[K\x1b[?7h\x1b8\x1b[?2026l")
        }
    }
}

/// The columns `text` takes, its control characters (which are never written) left out.
fn columns(text: &str) -> usize {
    text.chars().filter(|c| !c.is_control()).map(|c| blaze_unicode::char_width(c).unwrap_or(0)).sum()
}

/// `text`'s characters that fit in `room` columns, control characters left out: the columns
/// written.
fn fitting(out: &mut dyn fmt::Write, text: &str, room: usize) -> Result<usize, fmt::Error> {
    let mut used = 0;
    for c in text.chars().filter(|c| !c.is_control()) {
        let w = blaze_unicode::char_width(c).unwrap_or(0);
        if used + w > room {
            break;
        }
        out.write_char(c)?;
        used += w;
    }
    Ok(used)
}

/// `text` in `token`'s colour on `terminal` (none on a terminal without colour).
fn painted(out: &mut dyn fmt::Write, terminal: &Terminal, token: Token, text: &str) -> fmt::Result {
    match terminal.sgr(token) {
        "" => fitting(out, text, usize::MAX).map(drop),
        code => {
            write!(out, "\x1b[{code}m")?;
            fitting(out, text, usize::MAX)?;
            out.write_str("\x1b[0m")
        }
    }
}

/// `path` in `room` columns: whole when it fits, else its first segment, the ellipsis and as
/// much of its end as fits (`std::…stdio::_print`); `cut` when its start was dropped already.
fn fit_path(out: &mut dyn fmt::Write, path: &str, cut: bool, room: usize, ellipsis: &str) -> fmt::Result {
    let (whole, e) = (columns(path), columns(ellipsis));
    if !cut && whole <= room {
        return fitting(out, path, room).map(drop);
    }
    if cut && whole + e <= room {
        out.write_str(ellipsis)?;
        return fitting(out, path, room - e).map(drop);
    }
    let head = match (cut, path.find("::")) {
        (false, Some(at)) if columns(&path[..at + 2]) + e + 4 <= room => &path[..at + 2],
        _ => "",
    };
    let tail_room = room.saturating_sub(columns(head) + e);
    let mut start = path.len();
    let mut used = 0;
    for (at, c) in path.char_indices().rev() {
        let w = if c.is_control() { 0 } else { blaze_unicode::char_width(c).unwrap_or(0) };
        if used + w > tail_room || at < head.len() {
            break;
        }
        used += w;
        start = at;
    }
    fitting(out, head, usize::MAX)?;
    out.write_str(ellipsis)?;
    fitting(out, path[start..].trim_start_matches(':'), tail_room).map(drop)
}

/// The line's text in at most `cols - 1` columns: the glyph and the word, the path, the
/// counts, the time. What does not fit goes: the path first (cut in its middle, then left out
/// when fewer than 8 columns are left for it), then the counts, then the time, then the word's
/// end.
fn status_text(line: &StatusLine<'_>, terminal: &Terminal, out: &mut dyn fmt::Write) -> fmt::Result {
    let budget = usize::from(line.cols).saturating_sub(1);
    let (glyph, sep, ellipsis) = match line.unicode {
        true => ("▶ ", " · ", "…"),
        false => ("> ", " - ", "..."),
    };
    let mut counts = LineBuf::<48>::new();
    match (line.made, line.found) {
        (0, 0) => {}
        (made, 0) => write!(counts, "{made} made")?,
        (0, found) => write!(counts, "{found} found")?,
        (made, found) => write!(counts, "{made} made, {found} found")?,
    }
    let mut time = LineBuf::<24>::new();
    let ms = line.elapsed_ms;
    match ms {
        0..60_000 => write!(time, "{}.{}s", ms / 1000, ms % 1000 / 100)?,
        _ => write!(time, "{}m{:02}s", ms / 60_000, ms % 60_000 / 1000)?,
    }
    let head = columns(glyph) + columns(line.word);
    if head > budget {
        let used = fitting(out, glyph, budget)?;
        return fitting(out, line.word, budget - used).map(drop);
    }
    let sep_w = columns(sep);
    let time_w = sep_w + time.as_str().len();
    let counts_w = match counts.as_str() {
        "" => 0,
        text => sep_w + text.len(),
    };
    let show_time = head + time_w <= budget;
    let show_counts = counts_w > 0 && head + counts_w + if show_time { time_w } else { 0 } <= budget;
    let fixed = head + if show_counts { counts_w } else { 0 } + if show_time { time_w } else { 0 };
    painted(out, terminal, Token::Running, glyph)?;
    painted(out, terminal, Token::Emphasis, line.word)?;
    let room = budget.saturating_sub(fixed + 1);
    if let Some(path) = line.path
        && room >= 8
    {
        out.write_char(' ')?;
        let code = terminal.sgr(Token::Code);
        if !code.is_empty() {
            write!(out, "\x1b[{code}m")?;
        }
        fit_path(out, path, line.cut, room, ellipsis)?;
        if !code.is_empty() {
            out.write_str("\x1b[0m")?;
        }
    }
    if show_counts {
        out.write_str(sep)?;
        painted(out, terminal, Token::Dim, counts.as_str())?;
    }
    if show_time {
        out.write_str(sep)?;
        painted(out, terminal, Token::Dim, time.as_str())?;
    }
    Ok(())
}
