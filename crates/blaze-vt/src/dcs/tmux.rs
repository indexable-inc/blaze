//! tmux control mode's line protocol, as ghostty parses it (terminal/tmux/control.zig): the
//! bytes inside `DCS 1000 p … ST` become [`Notification`]s. Every line starts with `%`; a
//! `%begin` line opens a block whose raw lines end at a `%end` or `%error` guard line with three
//! numeric fields. A notification borrows the parser's buffer until the next byte is put.
//!
//! ghostty matches each notification with an oniguruma regex; here each is matched by hand with
//! the regex's own greedy choice (each matcher says which), so a line splits where ghostty's
//! splits. Two differences, both where ghostty misbehaves: an id too large for `u64` is an
//! unmatched line here (ghostty's `parseInt … catch unreachable` is undefined behaviour), and
//! bytes are matched as bytes (oniguruma's `.` over invalid UTF-8 is unspecified).

use blaze_own::{Alloc, Vec};

/// A tmux pane id (`%N`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct PaneId(pub u64);
/// A tmux window id (`@N`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct WindowId(pub u64);
/// A tmux session id (`$N`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct SessionId(pub u64);

/// What tmux said (control.zig:498 `Notification`). `B` is how the text is held: borrowed
/// bytes as the parser gives them, or anything [`Notification::map`] makes of them (the
/// terminal's effects hold spans of a buffer).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Notification<B> {
    /// Control mode began (`DCS 1000 p`); ours, not tmux's.
    Enter,
    /// Control mode ended: the DCS ended, or a line did not start with `%`.
    Exit,
    /// A `%begin` block's lines ended by `%end`, trailing CR/LF trimmed.
    BlockEnd(B),
    /// A `%begin` block's lines ended by `%error`.
    BlockErr(B),
    /// `%output %pane data` (data still octal-escaped, as tmux sent it).
    Output { pane: PaneId, data: B },
    /// `%session-changed $id name`.
    SessionChanged { session: SessionId, name: B },
    /// `%sessions-changed`.
    SessionsChanged,
    /// `%layout-change @window layout visible-layout flags`.
    LayoutChange { window: WindowId, layout: B, visible_layout: B, raw_flags: B },
    /// `%window-add @window`.
    WindowAdd { window: WindowId },
    /// `%window-renamed @window name`.
    WindowRenamed { window: WindowId, name: B },
    /// `%window-pane-changed @window %pane`.
    WindowPaneChanged { window: WindowId, pane: PaneId },
    /// `%client-detached client`.
    ClientDetached { client: B },
    /// `%client-session-changed client $id name`.
    ClientSessionChanged { client: B, session: SessionId, name: B },
}

impl<B> Notification<B> {
    /// The same notification with each text `f` of it, in field order.
    pub fn map<C>(self, mut f: impl FnMut(B) -> C) -> Notification<C> {
        use Notification as N;
        match self {
            N::Enter => N::Enter,
            N::Exit => N::Exit,
            N::BlockEnd(b) => N::BlockEnd(f(b)),
            N::BlockErr(b) => N::BlockErr(f(b)),
            N::Output { pane, data } => N::Output { pane, data: f(data) },
            N::SessionChanged { session, name } => N::SessionChanged { session, name: f(name) },
            N::SessionsChanged => N::SessionsChanged,
            N::LayoutChange { window, layout, visible_layout, raw_flags } => {
                let layout = f(layout);
                let visible_layout = f(visible_layout);
                N::LayoutChange { window, layout, visible_layout, raw_flags: f(raw_flags) }
            }
            N::WindowAdd { window } => N::WindowAdd { window },
            N::WindowRenamed { window, name } => N::WindowRenamed { window, name: f(name) },
            N::WindowPaneChanged { window, pane } => N::WindowPaneChanged { window, pane },
            N::ClientDetached { client } => N::ClientDetached { client: f(client) },
            N::ClientSessionChanged { client, session, name } => {
                let client = f(client);
                N::ClientSessionChanged { client, session, name: f(name) }
            }
        }
    }
}

pub use super::Refusal;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    /// Between notifications: the next byte must be `%`. The buffer still holds the last
    /// notification, which the caller may be reading.
    Idle,
    /// Unexpected input or a refusal: everything is dropped.
    Broken,
    /// Inside a `%` line.
    Notification,
    /// Inside a `%begin` block.
    Block,
}

/// The control-mode line parser (control.zig:19 `Parser`).
pub struct ControlParser<'a> {
    state: State,
    buf: Vec<u8, Alloc<'a>>,
    max_bytes: usize,
}

/// A matched line, as ranges into the buffer, so the state can change before the buffer is
/// lent out.
#[derive(Clone, Copy)]
pub(crate) enum Parsed {
    Output(u64, Span),
    SessionChanged(u64, Span),
    SessionsChanged,
    LayoutChange(u64, Span, Span, Span),
    WindowAdd(u64),
    WindowRenamed(u64, Span),
    WindowPaneChanged(u64, u64),
    ClientDetached(Span),
    ClientSessionChanged(Span, u64, Span),
}

#[derive(Clone, Copy)]
pub(crate) struct Span(usize, usize);

impl<'a> ControlParser<'a> {
    /// A parser in the idle state whose buffer may hold `max_bytes` (ghostty: 1 MiB).
    pub fn new_in(max_bytes: usize, alloc: Alloc<'a>) -> ControlParser<'a> {
        ControlParser { state: State::Idle, buf: Vec::new_in(alloc), max_bytes }
    }

    /// Back to idle with an empty buffer, its capacity kept (a new `DCS 1000 p`).
    pub fn reset(&mut self) {
        self.state = State::Idle;
        self.buf.clear();
    }

    /// Whether a refusal or a stray line broke the session.
    pub fn is_broken(&self) -> bool {
        self.state == State::Broken
    }

    /// One byte of control-mode output (control.zig:64 `put`).
    pub fn put(&mut self, byte: u8) -> Result<Option<Notification<&[u8]>>, Refusal> {
        Ok(self.step(byte)?.map(|ev| self.view(ev)))
    }

    /// [`ControlParser::put`] without the borrow: what happened, to be seen with
    /// [`ControlParser::view`], so a caller can act on a refusal with the parser free.
    pub(crate) fn step(&mut self, byte: u8) -> Result<Option<Event>, Refusal> {
        if self.state == State::Broken {
            return Ok(None);
        }
        if self.buf.len() >= self.max_bytes {
            self.broken();
            return Err(Refusal::OverCap { max_bytes: self.max_bytes });
        }
        match self.state {
            State::Broken => return Ok(None),
            State::Idle => {
                if byte != b'%' {
                    self.broken();
                    return Ok(Some(Event::Exit));
                }
                self.buf.clear();
                self.state = State::Notification;
            }
            State::Notification => {
                if byte == b'\n' {
                    return Ok(self.finish_notification());
                }
            }
            State::Block => {
                if byte == b'\n' {
                    let line_at = self.buf.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
                    if let Some(guard) = block_terminator(&self.buf[line_at..]) {
                        let mut end = line_at;
                        while end > 0 && matches!(self.buf[end - 1], b'\r' | b'\n') {
                            end -= 1;
                        }
                        // The buffer is kept: the notification is a view of it.
                        self.state = State::Idle;
                        return Ok(Some(Event::Block(guard, end)));
                    }
                }
            }
        }
        if self.buf.try_push(byte).is_err() {
            return Err(Refusal::Alloc);
        }
        Ok(None)
    }

    fn broken(&mut self) {
        self.state = State::Broken;
        let alloc = *self.buf.allocator();
        self.buf = Vec::new_in(alloc);
    }

    /// A `%` line is complete (control.zig:186 `parseNotification`).
    fn finish_notification(&mut self) -> Option<Event> {
        let mut len = self.buf.len();
        if self.buf[len - 1] == b'\r' {
            len -= 1;
        }
        let line = &self.buf[..len];
        let cmd = &line[..line.iter().position(|&b| b == b' ').unwrap_or(line.len())];
        if cmd == b"%begin" {
            self.state = State::Block;
            self.buf.clear();
            return None;
        }
        let parsed = parse_line(cmd, line);
        self.state = State::Idle;
        // ghostty clears the buffer for the notifications that borrow nothing from it, and for
        // a line it does not know (the next `%` clears it anyway).
        match parsed {
            None
            | Some(
                Parsed::SessionsChanged | Parsed::WindowAdd(_) | Parsed::WindowPaneChanged(..),
            ) => self.buf.clear(),
            Some(_) => {}
        }
        parsed.map(Event::Line)
    }

    /// The notification an [`Event`] from the last [`ControlParser::step`] stands for.
    pub(crate) fn view(&self, ev: Event) -> Notification<&[u8]> {
        let b = &self.buf[..];
        let s = |sp: Span| &b[sp.0..sp.1];
        let parsed = match ev {
            Event::Exit => return Notification::Exit,
            Event::Block(Guard::End, end) => return Notification::BlockEnd(&b[..end]),
            Event::Block(Guard::Err, end) => return Notification::BlockErr(&b[..end]),
            Event::Line(p) => p,
        };
        match parsed {
            Parsed::Output(id, data) => Notification::Output { pane: PaneId(id), data: s(data) },
            Parsed::SessionChanged(id, name) => {
                Notification::SessionChanged { session: SessionId(id), name: s(name) }
            }
            Parsed::SessionsChanged => Notification::SessionsChanged,
            Parsed::LayoutChange(id, l, v, f) => Notification::LayoutChange {
                window: WindowId(id),
                layout: s(l),
                visible_layout: s(v),
                raw_flags: s(f),
            },
            Parsed::WindowAdd(id) => Notification::WindowAdd { window: WindowId(id) },
            Parsed::WindowRenamed(id, name) => {
                Notification::WindowRenamed { window: WindowId(id), name: s(name) }
            }
            Parsed::WindowPaneChanged(w, p) => {
                Notification::WindowPaneChanged { window: WindowId(w), pane: PaneId(p) }
            }
            Parsed::ClientDetached(c) => Notification::ClientDetached { client: s(c) },
            Parsed::ClientSessionChanged(c, id, name) => Notification::ClientSessionChanged {
                client: s(c),
                session: SessionId(id),
                name: s(name),
            },
        }
    }
}

/// What one byte did, without a borrow of the buffer.
#[derive(Clone, Copy)]
pub(crate) enum Event {
    Exit,
    /// A block ended; its output is the buffer up to the offset.
    Block(Guard, usize),
    Line(Parsed),
}

#[derive(Clone, Copy)]
pub(crate) enum Guard {
    End,
    Err,
}

/// A block's last line is exactly `%end|%error <n> <n> <n>` (control.zig:155): one trailing CR
/// dropped, fields split on runs of spaces, each number as Zig's `parseInt(usize, _, 10)` reads
/// it (a sign, `_` between digits).
fn block_terminator(line: &[u8]) -> Option<Guard> {
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    let mut fields = line.split(|&b| b == b' ').filter(|f| !f.is_empty());
    let guard = match fields.next()? {
        b"%end" => Guard::End,
        b"%error" => Guard::Err,
        _ => return None,
    };
    for _ in 0..3 {
        zig_parse_usize(fields.next()?)?;
    }
    if fields.next().is_some() {
        return None;
    }
    Some(guard)
}

/// `std.fmt.parseInt(usize, s, 10)` of Zig 0.15 (fmt.zig:337): an optional `+` or `-` (a
/// negative value overflows, `-0` is 0), digits with single or repeated `_` between them but not
/// first or last.
fn zig_parse_usize(s: &[u8]) -> Option<u64> {
    let (neg, digits) = match s.first()? {
        b'+' => (false, &s[1..]),
        b'-' => (true, &s[1..]),
        _ => (false, s),
    };
    if digits.is_empty() || digits[0] == b'_' || digits[digits.len() - 1] == b'_' {
        return None;
    }
    let mut v: u64 = 0;
    for &c in digits {
        if c == b'_' {
            continue;
        }
        if !c.is_ascii_digit() {
            return None;
        }
        v = v.checked_mul(10)?.checked_add(u64::from(c - b'0'))?;
    }
    if neg && v != 0 {
        return None;
    }
    Some(v)
}

/// `[0-9]+` at the start of `s`: the value and the bytes taken. `None` if there is no digit or
/// the value overflows.
fn digits(s: &[u8]) -> Option<(u64, usize)> {
    let n = s.iter().take_while(|b| b.is_ascii_digit()).count();
    if n == 0 {
        return None;
    }
    let mut v: u64 = 0;
    for &c in &s[..n] {
        v = v.checked_mul(10)?.checked_add(u64::from(c - b'0'))?;
    }
    Some((v, n))
}

/// `prefix` then `[0-9]+`, returning the value and the offset after the digits.
fn id_after(line: &[u8], at: usize, prefix: &[u8]) -> Option<(u64, usize)> {
    let rest = line.get(at..)?.strip_prefix(prefix)?;
    let (v, n) = digits(rest)?;
    Some((v, at + prefix.len() + n))
}

/// `(.+)$` from `at`: a non-empty rest.
fn rest(line: &[u8], at: usize) -> Option<Span> {
    (at < line.len()).then_some(Span(at, line.len()))
}

fn parse_line(cmd: &[u8], line: &[u8]) -> Option<Parsed> {
    match cmd {
        // ^%output %([0-9]+) (.+)$
        b"%output" => {
            let (id, at) = id_after(line, 0, b"%output %")?;
            let at = expect_space(line, at)?;
            Some(Parsed::Output(id, rest(line, at)?))
        }
        // ^%session-changed \$([0-9]+) (.+)$
        b"%session-changed" => {
            let (id, at) = id_after(line, 0, b"%session-changed $")?;
            let at = expect_space(line, at)?;
            Some(Parsed::SessionChanged(id, rest(line, at)?))
        }
        b"%sessions-changed" => (line == b"%sessions-changed").then_some(Parsed::SessionsChanged),
        // ^%layout-change @([0-9]+) (.+) (.+) (.*)$
        b"%layout-change" => {
            let (id, at) = id_after(line, 0, b"%layout-change @")?;
            let at = expect_space(line, at)?;
            let r = &line[at..];
            // Greedy: group 4 is after the last space; group 3 runs from the space before it
            // that leaves groups 2 and 3 both non-empty and group 2 longest.
            let last = r.iter().rposition(|&b| b == b' ')?;
            let mid = (1..last.checked_sub(1)?).rev().find(|&i| r[i] == b' ')?;
            Some(Parsed::LayoutChange(
                id,
                Span(at, at + mid),
                Span(at + mid + 1, at + last),
                Span(at + last + 1, line.len()),
            ))
        }
        // ^%window-add @([0-9]+)$
        b"%window-add" => {
            let (id, at) = id_after(line, 0, b"%window-add @")?;
            (at == line.len()).then_some(Parsed::WindowAdd(id))
        }
        // ^%window-renamed @([0-9]+) (.+)$
        b"%window-renamed" => {
            let (id, at) = id_after(line, 0, b"%window-renamed @")?;
            let at = expect_space(line, at)?;
            Some(Parsed::WindowRenamed(id, rest(line, at)?))
        }
        // ^%window-pane-changed @([0-9]+) %([0-9]+)$
        b"%window-pane-changed" => {
            let (w, at) = id_after(line, 0, b"%window-pane-changed @")?;
            let (p, at) = id_after(line, at, b" %")?;
            (at == line.len()).then_some(Parsed::WindowPaneChanged(w, p))
        }
        // ^%client-detached (.+)$
        b"%client-detached" => {
            Some(Parsed::ClientDetached(rest(line, b"%client-detached ".len())?))
        }
        // ^%client-session-changed (.+) \$([0-9]+) (.+)$
        b"%client-session-changed" => {
            let start = b"%client-session-changed ".len();
            // Group 1 greedy: the last ` $digits ` that leaves group 1 and group 3 non-empty.
            (start + 1..line.len()).rev().find_map(|i| {
                let (id, at) = id_after(line, i, b" $")?;
                let at = expect_space(line, at)?;
                Some(Parsed::ClientSessionChanged(Span(start, i), id, rest(line, at)?))
            })
        }
        _ => None,
    }
}

fn expect_space(line: &[u8], at: usize) -> Option<usize> {
    (line.get(at) == Some(&b' ')).then_some(at + 1)
}
