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
use crate::run_line::{LineBuf, StatusBlock, StatusFrame, StatusGroup, StatusLine, WorkKind};

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
        StatusFrame::EraseLines(rows) => {
            if *rows <= 1 {
                return out.write_str("\x1b[K");
            }
            out.write_str("\x1b[?2026h\r\x1b[2K")?;
            for _ in 1..*rows {
                out.write_str("\r\n\x1b[2K")?;
            }
            write!(out, "\r\x1b[{}A\x1b[?2026l", rows.saturating_sub(1))
        }
        StatusFrame::Draw(line) => {
            out.write_str("\x1b[?2026h\x1b7\x1b[?7l")?;
            status_text(line, terminal, out)?;
            out.write_str("\x1b[K\x1b[?7h\x1b8\x1b[?2026l")
        }
        StatusFrame::DrawBlock(block) => encode_block(block, terminal, out),
    }
}

/// Formats `ms` as a compact live timer (`0.4s`, `42.1s`, `2m05s`).
fn write_timer(out: &mut dyn fmt::Write, ms: u64) -> fmt::Result {
    match ms {
        0..60_000 => write!(out, "{}.{}s", ms / 1000, ms % 1000 / 100),
        _ => write!(out, "{}m{:02}s", ms / 60_000, ms % 60_000 / 1000),
    }
}

/// Renders a multi-line [`StatusBlock`] in place, erasing any extra stale lines left from
/// `block.prev_rows`, and always returning the cursor to row 0, column 0.
fn encode_block(block: &StatusBlock<'_>, terminal: &Terminal, out: &mut dyn fmt::Write) -> fmt::Result {
    let new_rows = block.rows();
    let max_rows = new_rows.max(block.prev_rows);
    if max_rows <= 1 {
        out.write_str("\x1b[?2026h\x1b7\x1b[?7l")?;
        block_summary(block, terminal, out)?;
        return out.write_str("\x1b[K\x1b[?7h\x1b8\x1b[?2026l");
    }
    out.write_str("\x1b[?2026h\x1b[?7l\r")?;
    block_summary(block, terminal, out)?;
    out.write_str("\x1b[K")?;
    let count = usize::from(block.group_count);
    for slot in block.groups.iter().take(count) {
        if let Some(group) = slot {
            out.write_str("\r\n")?;
            block_group_row(block, group, terminal, out)?;
            out.write_str("\x1b[K")?;
        }
    }
    for _ in new_rows..max_rows {
        out.write_str("\r\n\x1b[2K")?;
    }
    write!(out, "\r\x1b[{}A\x1b[?7h\x1b[?2026l", max_rows.saturating_sub(1))
}

/// Line 0 of [`StatusBlock`]: `⠋ blaze check  [████████░░] 482/538  14.2s  (12 active)`.
fn block_summary(block: &StatusBlock<'_>, terminal: &Terminal, out: &mut dyn fmt::Write) -> fmt::Result {
    const SPINNER: [&str; 10] = ["⠋ ", "⠙ ", "⠹ ", "⠸ ", "⠼ ", "⠴ ", "⠦ ", "⠧ ", "⠇ ", "⠏ "];
    let budget = usize::from(block.cols).saturating_sub(1);
    if budget == 0 {
        return Ok(());
    }
    let glyph = if block.unicode {
        let idx = usize::try_from((block.elapsed_ms / 80) % 10).unwrap_or(0);
        *SPINNER.get(idx).unwrap_or(&"▶ ")
    } else {
        "> "
    };
    let mut cmd = LineBuf::<32>::new();
    let _ = write!(cmd, "blaze {}", block.command);

    let mut mid = LineBuf::<48>::new();
    let mid_is_bar = match block.progress {
        Some((done, total)) if total > 0 => {
            let clamped = done.min(total);
            let filled = usize::try_from(u64::from(clamped).saturating_mul(10) / u64::from(total))
                .unwrap_or(0)
                .min(10);
            let (full, empty) = if block.unicode { ('█', '░') } else { ('#', '.') };
            let _ = mid.write_char('[');
            for i in 0..10 {
                let _ = mid.write_char(if i < filled { full } else { empty });
            }
            let _ = write!(mid, "] {clamped}/{total}");
            true
        }
        _ => {
            let _ = mid.write_str(block.verb);
            false
        }
    };

    let mut time = LineBuf::<24>::new();
    let _ = write_timer(&mut time, block.elapsed_ms);

    let mut active = LineBuf::<24>::new();
    if block.active > 0 {
        let _ = write!(active, "({} active)", block.active);
    }

    let glyph_w = columns(glyph);
    let cmd_w = columns(cmd.as_str());
    let head = glyph_w + cmd_w;
    if head > budget {
        let used = fitting(out, glyph, budget)?;
        return fitting(out, cmd.as_str(), budget.saturating_sub(used)).map(drop);
    }

    let time_w = columns(time.as_str());
    let mid_w = columns(mid.as_str());
    let active_w = columns(active.as_str());

    let show_time = head + 2 + time_w <= budget;
    let mut used = head + if show_time { 2 + time_w } else { 0 };
    let show_mid = mid_w > 0 && used + 2 + mid_w <= budget;
    if show_mid {
        used += 2 + mid_w;
    }
    let show_active = active_w > 0 && used + 2 + active_w <= budget;

    painted(out, terminal, Token::Running, glyph)?;
    painted(out, terminal, Token::Emphasis, cmd.as_str())?;
    if show_mid {
        out.write_str("  ")?;
        let token = if mid_is_bar { Token::Number } else { Token::Dim };
        painted(out, terminal, token, mid.as_str())?;
    }
    if show_time {
        out.write_str("  ")?;
        painted(out, terminal, Token::Dim, time.as_str())?;
    }
    if show_active {
        out.write_str("  ")?;
        painted(out, terminal, Token::Dim, active.as_str())?;
    }
    Ok(())
}

/// One category row of [`StatusBlock`], strictly within `cols - 1` columns:
/// `  build.rs    librocksdb-sys · cc/c++ (42.1s), openssl-sys (3.2s)  [external]`.
fn block_group_row(
    block: &StatusBlock<'_>,
    group: &StatusGroup<'_>,
    terminal: &Terminal,
    out: &mut dyn fmt::Write,
) -> fmt::Result {
    let budget = usize::from(block.cols).saturating_sub(1);
    if budget == 0 {
        return Ok(());
    }
    let label = group.kind.label(block.verb);
    let label_token = match group.kind {
        WorkKind::BuildScript => Token::Warning,
        WorkKind::Fetch | WorkKind::ProcMacro => Token::Waiting,
        WorkKind::Blaze => Token::Running,
    };
    let label_w = columns(label);
    let pad = 10usize.saturating_sub(label_w);
    let prefix_w = 2 + label_w + pad + 2;
    if prefix_w >= budget {
        let mut used = fitting(out, "  ", budget)?;
        used += fitting(out, label, budget.saturating_sub(used))?;
        let _ = used;
        return Ok(());
    }

    // Build optional right-hand suffix (`  [external]` for build.rs, `  · <hint>` for Blaze).
    let mut suffix = LineBuf::<64>::new();
    let suffix_token = match group.kind {
        WorkKind::BuildScript => {
            let _ = suffix.write_str("  [external]");
            Token::Warning
        }
        WorkKind::Blaze => {
            if let Some(hint) = block.hint.filter(|s| !s.is_empty()) {
                let sep = if block.unicode { "  · " } else { "  - " };
                let _ = suffix.write_str(sep);
                let _ = suffix.write_str(hint);
            }
            Token::Dim
        }
        WorkKind::Fetch | WorkKind::ProcMacro => Token::Dim,
    };

    let avail = budget - prefix_w;
    let suffix_w = columns(suffix.as_str());
    let show_suffix = suffix_w > 0 && avail >= suffix_w + 16;
    let items_room = if show_suffix { avail - suffix_w } else { avail };

    out.write_str("  ")?;
    painted(out, terminal, label_token, label)?;
    for _ in 0..(pad + 2) {
        out.write_char(' ')?;
    }

    let count = usize::from(group.count);
    let ellipsis = if block.unicode { "…" } else { "..." };
    let ellipsis_w = columns(ellipsis);
    let mut used = 0usize;
    let mut shown = 0usize;
    let item_token = match group.kind {
        WorkKind::BuildScript => Token::Emphasis,
        WorkKind::Fetch | WorkKind::ProcMacro | WorkKind::Blaze => Token::Code,
    };
    let time_token = match group.kind {
        WorkKind::BuildScript => Token::Warning,
        WorkKind::Fetch | WorkKind::ProcMacro | WorkKind::Blaze => Token::Dim,
    };

    for item in group.items.iter().take(count).flatten() {
        let mut timer = LineBuf::<24>::new();
        if item.elapsed_ms >= 100 {
            let _ = timer.write_str(" (");
            let _ = write_timer(&mut timer, item.elapsed_ms);
            let _ = timer.write_char(')');
        }
        let name_w = columns(item.label);
        let timer_w = columns(timer.as_str());
        let item_w = name_w + timer_w;

        if shown == 0 {
            if item_w <= items_room {
                painted(out, terminal, item_token, item.label)?;
                if !timer.as_str().is_empty() {
                    painted(out, terminal, time_token, timer.as_str())?;
                }
                used += item_w;
            } else if timer_w > 0 && items_room >= timer_w + ellipsis_w + 2 {
                let keep = items_room - timer_w - ellipsis_w;
                let code = terminal.sgr(item_token);
                if !code.is_empty() {
                    write!(out, "\x1b[{code}m")?;
                }
                let wrote = fitting(out, item.label, keep)?;
                out.write_str(ellipsis)?;
                if !code.is_empty() {
                    out.write_str("\x1b[0m")?;
                }
                painted(out, terminal, time_token, timer.as_str())?;
                used += wrote + ellipsis_w + timer_w;
            } else if items_room > ellipsis_w {
                let keep = items_room - ellipsis_w;
                let code = terminal.sgr(item_token);
                if !code.is_empty() {
                    write!(out, "\x1b[{code}m")?;
                }
                let wrote = fitting(out, item.label, keep)?;
                out.write_str(ellipsis)?;
                if !code.is_empty() {
                    out.write_str("\x1b[0m")?;
                }
                used += wrote + ellipsis_w;
            } else {
                used += fitting(out, item.label, items_room)?;
            }
            shown += 1;
            continue;
        }

        // Subsequent items: only emit if `, ` + full item fits cleanly.
        if used + 2 + item_w <= items_room {
            out.write_str(", ")?;
            painted(out, terminal, item_token, item.label)?;
            if !timer.as_str().is_empty() {
                painted(out, terminal, time_token, timer.as_str())?;
            }
            used += 2 + item_w;
            shown += 1;
        } else {
            break;
        }
    }

    let omitted = usize::from(group.extra).saturating_add(count.saturating_sub(shown));
    if omitted > 0 {
        let mut plus = LineBuf::<16>::new();
        let _ = write!(plus, " +{omitted}");
        let plus_w = columns(plus.as_str());
        if used + plus_w <= items_room {
            painted(out, terminal, Token::Dim, plus.as_str())?;
        }
    }

    if show_suffix {
        painted(out, terminal, suffix_token, suffix.as_str())?;
    }
    Ok(())
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
    const SPINNER: [&str; 10] = ["⠋ ", "⠙ ", "⠹ ", "⠸ ", "⠼ ", "⠴ ", "⠦ ", "⠧ ", "⠇ ", "⠏ "];
    let budget = usize::from(line.cols).saturating_sub(1);
    let (glyph, sep, ellipsis) = match (line.unicode, line.progress.is_some()) {
        (true, true) => {
            let idx = usize::try_from((line.elapsed_ms / 80) % 10).unwrap_or(0);
            (*SPINNER.get(idx).unwrap_or(&"▶ "), " · ", "…")
        }
        (true, false) => ("▶ ", " · ", "…"),
        (false, _) => ("> ", " - ", "..."),
    };
    let mut counts = LineBuf::<48>::new();
    match line.progress {
        Some((done, total)) if total > 0 => {
            let clamped = done.min(total);
            let filled = usize::try_from(u64::from(clamped).saturating_mul(12) / u64::from(total)).unwrap_or(0).min(12);
            let (full, empty) = if line.unicode { ('█', '░') } else { ('#', '.') };
            counts.write_char('[')?;
            for i in 0..12 {
                counts.write_char(if i < filled { full } else { empty })?;
            }
            write!(counts, "] {clamped}/{total}")?;
        }
        _ => match (line.made, line.found) {
            (0, 0) => {}
            (made, 0) => write!(counts, "{made} made")?,
            (0, found) => write!(counts, "{found} found")?,
            (made, found) => write!(counts, "{made} made, {found} found")?,
        },
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
        text => sep_w + columns(text),
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
