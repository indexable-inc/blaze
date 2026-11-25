//! DCS: the handler fed by the parser's `dcs_hook` / `dcs_put` / `dcs_unhook`, as ghostty's
//! terminal/dcs.zig runs it. It knows three requests and ignores every other DCS (sixel,
//! DECUDK, ReGIS: vt-inventory.md §8, "Deliberately NOT supported"):
//!
//! | hook | request | when it answers |
//! |---|---|---|
//! | `DCS 1000 p` (no intermediates, exactly one parameter) | tmux control mode | [`DcsCommand::Tmux`] at hook (enter), per line, and at unhook (exit); only with [`DcsOpts::tmux_control_mode`] |
//! | `DCS + q` | XTGETTCAP | at unhook: the payload upper-cased, keys split on `;` ([`XtGetTcap::keys`]) |
//! | `DCS $ q` | DECRQSS | at unhook: [`Decrqss`] from at most two payload bytes; a third drops the request with no reply |
//!
//! Bytes are held in one buffer in the allocator passed in, reused across requests, capped by
//! [`DcsOpts::max_bytes`] (ghostty: 1 MiB). A request that passes the cap, or whose buffer the
//! allocator refuses, is dropped with no reply, as ghostty drops it (dcs.zig:115); each drop is
//! counted in [`DcsStats`]. The replies themselves are [`crate::reply`]'s.

pub mod tmux;

#[cfg(test)]
mod tests;

use blaze_own::{Alloc, Vec};

use crate::parser::Dcs;
use tmux::{ControlParser, Notification};

/// Why a request's byte was refused; the request is dropped with no reply (dcs.zig:115,
/// control.zig:71).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Refusal {
    /// The buffer reached the cap (`max_bytes`) before this byte.
    OverCap { max_bytes: usize },
    /// The allocator refused to grow the buffer.
    Alloc,
}

/// The handler's choices and caps.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DcsOpts {
    /// Whether `DCS 1000 p` enters tmux control mode. ghostty's build option
    /// (`tmux_control_mode`, on where oniguruma is: the app); off, the hook is ignored.
    pub tmux_control_mode: bool,
    /// The most bytes one request may hold: an XTGETTCAP payload, a tmux line or block.
    pub max_bytes: usize,
}

impl DcsOpts {
    /// ghostty's cap (dcs.zig:19).
    pub const MAX_BYTES: usize = 1024 * 1024;
}

/// Requests dropped, by cause. ghostty logs these; here a test can see them.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct DcsStats {
    /// Dropped at [`DcsOpts::max_bytes`].
    pub over_cap: u64,
    /// Dropped because the allocator refused.
    pub alloc_refused: u64,
}

/// A DECRQSS setting (dcs.zig:252).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Decrqss {
    /// Not one ghostty answers (`"q`, `"p`, `t`, an empty payload, …): the reply is
    /// `DCS 0 $ r ST`.
    None,
    /// `m`: SGR.
    Sgr,
    /// `SP q`: DECSCUSR.
    Decscusr,
    /// `r`: DECSTBM.
    Decstbm,
    /// `s`: DECSLRM.
    Decslrm,
}

/// An XTGETTCAP payload, upper-cased: `;`-separated hex names (dcs.zig:225).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct XtGetTcap<'h> {
    data: &'h [u8],
}

impl<'h> XtGetTcap<'h> {
    /// The payload, upper-cased.
    pub fn data(&self) -> &'h [u8] {
        self.data
    }

    /// The keys in order, still hex. As ghostty's `next`: an empty payload has none, and a
    /// trailing `;` adds no empty key, but an empty key between two `;` is kept.
    pub fn keys(&self) -> Keys<'h> {
        Keys { rest: self.data }
    }
}

/// [`XtGetTcap::keys`].
#[derive(Clone, Debug)]
pub struct Keys<'h> {
    rest: &'h [u8],
}

impl<'h> Iterator for Keys<'h> {
    type Item = &'h [u8];

    fn next(&mut self) -> Option<&'h [u8]> {
        if self.rest.is_empty() {
            return None;
        }
        let rest = self.rest;
        match rest.iter().position(|&b| b == b';') {
            Some(i) => {
                self.rest = &rest[i + 1..];
                Some(&rest[..i])
            }
            None => {
                self.rest = &[];
                Some(rest)
            }
        }
    }
}

/// What a DCS asked for (dcs.zig:203 `Command`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DcsCommand<'h> {
    XtGetTcap(XtGetTcap<'h>),
    Decrqss(Decrqss),
    Tmux(Notification<&'h [u8]>),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    /// No DCS is hooked.
    Inactive,
    /// Hooked, but unknown or dropped: the rest is ignored.
    Ignore,
    XtGetTcap,
    Decrqss {
        data: [u8; 2],
        len: u8,
    },
    Tmux,
}

/// The DCS handler (dcs.zig:13 `Handler`). The parser guarantees hook, puts, unhook in order.
pub struct DcsHandler<'a> {
    opts: DcsOpts,
    state: State,
    /// The XTGETTCAP payload; kept after unhook while the command borrows it.
    buf: Vec<u8, Alloc<'a>>,
    tmux: ControlParser<'a>,
    stats: DcsStats,
}

impl<'a> DcsHandler<'a> {
    pub fn new_in(opts: DcsOpts, alloc: Alloc<'a>) -> DcsHandler<'a> {
        DcsHandler {
            opts,
            state: State::Inactive,
            buf: Vec::new_in(alloc),
            tmux: ControlParser::new_in(opts.max_bytes, alloc),
            stats: DcsStats::default(),
        }
    }

    pub fn stats(&self) -> DcsStats {
        self.stats
    }

    /// Whether a DCS is hooked (and not being ignored).
    pub fn is_active(&self) -> bool {
        !matches!(self.state, State::Inactive | State::Ignore)
    }

    /// `dcs_hook` (dcs.zig:27).
    pub fn hook(&mut self, dcs: &Dcs<'_>) -> Option<DcsCommand<'_>> {
        self.state = match (dcs.intermediates, dcs.final_byte) {
            // Tmux control mode must start with exactly `ESC P 1000 p`.
            ([], b'p') if self.opts.tmux_control_mode && dcs.params == [1000] => {
                self.tmux.reset();
                State::Tmux
            }
            ([b'+'], b'q') => {
                self.buf.clear();
                State::XtGetTcap
            }
            ([b'$'], b'q') => State::Decrqss { data: [0; 2], len: 0 },
            _ => State::Ignore,
        };
        (self.state == State::Tmux).then_some(DcsCommand::Tmux(Notification::Enter))
    }

    /// `dcs_put` (dcs.zig:113). Only tmux control mode answers mid-string.
    pub fn put(&mut self, byte: u8) -> Option<DcsCommand<'_>> {
        match &mut self.state {
            State::Inactive | State::Ignore => None,
            State::Tmux => match self.tmux.step(byte) {
                Ok(ev) => ev.map(|ev| DcsCommand::Tmux(self.tmux.view(ev))),
                Err(why) => {
                    self.refused(why);
                    None
                }
            },
            State::XtGetTcap => {
                if self.buf.len() >= self.opts.max_bytes {
                    self.refused(Refusal::OverCap { max_bytes: self.opts.max_bytes });
                } else if self.buf.try_push(byte).is_err() {
                    self.refused(Refusal::Alloc);
                }
                None
            }
            State::Decrqss { data, len } => {
                if usize::from(*len) < data.len() {
                    data[usize::from(*len)] = byte;
                    *len += 1;
                } else {
                    // A third byte: no setting is that long; dropped, as ghostty's buffer
                    // overflow drops it (dcs.zig:144), with no reply.
                    self.state = State::Ignore;
                }
                None
            }
        }
    }

    /// `dcs_unhook` (dcs.zig:155).
    pub fn unhook(&mut self) -> Option<DcsCommand<'_>> {
        let state = core::mem::replace(&mut self.state, State::Inactive);
        match state {
            State::Inactive | State::Ignore => None,
            State::Tmux => Some(DcsCommand::Tmux(Notification::Exit)),
            State::XtGetTcap => {
                self.buf.make_ascii_uppercase();
                Some(DcsCommand::XtGetTcap(XtGetTcap { data: &self.buf }))
            }
            State::Decrqss { data, len } => {
                Some(DcsCommand::Decrqss(match &data[..usize::from(len)] {
                    b"m" => Decrqss::Sgr,
                    b"r" => Decrqss::Decstbm,
                    b"s" => Decrqss::Decslrm,
                    b" q" => Decrqss::Decscusr,
                    _ => Decrqss::None,
                }))
            }
        }
    }

    /// A request dropped: ignore the rest of it (dcs.zig:115).
    fn refused(&mut self, why: Refusal) {
        match why {
            Refusal::OverCap { .. } => self.stats.over_cap += 1,
            Refusal::Alloc => self.stats.alloc_refused += 1,
        }
        self.state = State::Ignore;
        self.buf.clear();
    }
}
