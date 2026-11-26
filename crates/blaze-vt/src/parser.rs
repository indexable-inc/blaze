//! The parser: the DEC ANSI state machine of vt100.net as ghostty runs it (Parser.zig,
//! parse_table.zig, and stream.zig's ground routing), events out to a [`Sink`].
//!
//! What it holds to (vt-inventory.md §1, §2):
//! - In ground, bytes are UTF-8. A code point up to U+000F is executed, ESC starts a sequence,
//!   and everything else is printed: 0x10–0x1F other than ESC and DEL print as cells
//!   (Terminal.zig:12561), and raw C1 bytes are invalid UTF-8, printed as U+FFFD.
//! - Inside a sequence, bytes are raw. From any state, CAN and SUB execute and go to ground, ESC
//!   starts over, C1 0x80–0x9A (but 0x90, 0x98) execute as `ESC (c − 0x40)`, ST 0x9C goes to
//!   ground, and 0x90, 0x9B, 0x9D, 0x98/0x9E/0x9F enter DCS, CSI, OSC and SOS/PM/APC. In an OSC
//!   string every byte from 0x20 to 0xFF is data, so 0x9C is data there and only BEL, ESC, CAN
//!   and SUB end it; whatever ends it, the OSC is dispatched ([`Sink::osc_end`]).
//! - At most [`MAX_PARAMS`] parameters: a CSI or DCS with more is dropped whole. At most
//!   [`MAX_INTERMEDIATES`] intermediates: more are dropped and the sequence still dispatches. A
//!   parameter saturates at 65535. A `:` separator is allowed only before final `m`; any other
//!   CSI with one is dropped.
//! - SOS and PM are reported as APC, as ghostty does.

use crate::utf8::{Decoder, Step};

/// Parameters a CSI or DCS may carry; one more drops the sequence (Parser.zig:203).
pub const MAX_PARAMS: usize = 24;
/// Intermediates kept; more are dropped, the sequence still dispatches (Parser.zig:190).
pub const MAX_INTERMEDIATES: usize = 4;

/// How a string sequence ended, which a reply to an OSC query repeats.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Terminator {
    /// BEL (0x07).
    Bel,
    /// ST: `ESC \`, or anything else that ended it (CAN, SUB).
    St,
}

/// `ESC` [intermediates] final.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Esc<'a> {
    pub intermediates: &'a [u8],
    pub final_byte: u8,
}

/// `CSI` [private marker and intermediates] params final.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Csi<'a> {
    /// Private markers (`<`, `=`, `>`, `?`) and intermediates (0x20–0x2F), in order.
    pub intermediates: &'a [u8],
    pub params: &'a [u16],
    /// Bit `i` set: parameter `i` is followed by `:`, not `;` (only ever set when the final is
    /// `m`).
    pub colons: u32,
    pub final_byte: u8,
}

impl Csi<'_> {
    /// Whether parameter `i` is followed by a colon.
    pub fn colon_after(&self, i: usize) -> bool {
        i < MAX_PARAMS && self.colons & (1 << i) != 0
    }
}

/// `DCS` [intermediates] params final, as its hook is reported.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Dcs<'a> {
    pub intermediates: &'a [u8],
    pub params: &'a [u16],
    pub final_byte: u8,
}

/// What a parser tells its owner. Each call borrows the parser's buffers only for its duration.
pub trait Sink {
    /// A code point to put in a cell (ground).
    fn print(&mut self, c: char);
    /// A run of printable ASCII (0x20–0x7E), the ground fast path. The default prints each.
    fn print_ascii(&mut self, run: &[u8]) {
        for &b in run {
            self.print(char::from(b));
        }
    }
    /// A C0 control (0x00–0x0F in ground; any C0 a sequence state executes). C1 controls come
    /// as [`Sink::esc`] with the 7-bit final, as ghostty's `execute` maps them.
    fn execute(&mut self, c0: u8);
    fn esc(&mut self, esc: &Esc<'_>);
    fn csi(&mut self, csi: &Csi<'_>);
    fn osc_start(&mut self);
    fn osc_put(&mut self, b: u8);
    fn osc_end(&mut self, end: Terminator);
    fn dcs_hook(&mut self, dcs: &Dcs<'_>);
    fn dcs_put(&mut self, b: u8);
    fn dcs_unhook(&mut self);
    fn apc_start(&mut self);
    /// Payload bytes, in runs as long as the input allows (kitty graphics are megabytes).
    fn apc_put(&mut self, bytes: &[u8]);
    fn apc_end(&mut self);
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    Ground,
    Escape,
    EscapeIntermediate,
    CsiEntry,
    CsiIntermediate,
    CsiParam,
    CsiIgnore,
    DcsEntry,
    DcsParam,
    DcsIntermediate,
    DcsPassthrough,
    DcsIgnore,
    OscString,
    SosPmApcString,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Act {
    None,
    Print,
    Execute,
    Collect,
    Param,
    EscDispatch,
    CsiDispatch,
    Put,
    OscPut,
    ApcPut,
}

/// The parser's whole state: fixed size, no allocation.
#[derive(Clone, Debug)]
pub struct Parser {
    state: State,
    utf8: Decoder,
    intermediates: [u8; MAX_INTERMEDIATES],
    n_intermediates: u8,
    params: [u16; MAX_PARAMS],
    colons: u32,
    n_params: u8,
    acc: u16,
    /// A digit has been seen for the parameter being built.
    acc_started: bool,
    /// A DCS hook was reported, so its puts and unhook are too (a DCS with too many parameters
    /// is never hooked; ghostty passes its puts to a handler that has no state for them).
    dcs_hooked: bool,
}

impl Default for Parser {
    fn default() -> Parser {
        Parser::new()
    }
}

impl Parser {
    pub const fn new() -> Parser {
        Parser {
            state: State::Ground,
            utf8: Decoder::new(),
            intermediates: [0; MAX_INTERMEDIATES],
            n_intermediates: 0,
            params: [0; MAX_PARAMS],
            colons: 0,
            n_params: 0,
            acc: 0,
            acc_started: false,
            dcs_hooked: false,
        }
    }

    /// Whether the parser is in ground with no UTF-8 sequence part-way: a point where a driver
    /// may cut the stream (a synchronized-output frame, a resize).
    pub fn at_rest(&self) -> bool {
        self.state == State::Ground && !self.utf8.pending()
    }

    /// Feed bytes. Events go to `sink` in order; nothing is kept but the parser's own state.
    pub fn feed(&mut self, bytes: &[u8], sink: &mut impl Sink) {
        let mut i = 0;
        while i < bytes.len() {
            match self.state {
                State::Ground => {
                    if !self.utf8.pending() {
                        let run = ascii_run(&bytes[i..]);
                        if run > 0 {
                            sink.print_ascii(&bytes[i..i + run]);
                            i += run;
                            continue;
                        }
                    }
                    self.ground_byte(bytes[i], sink);
                    i += 1;
                }
                State::SosPmApcString => {
                    let run = apc_run(&bytes[i..]);
                    if run > 0 {
                        sink.apc_put(&bytes[i..i + run]);
                        i += run;
                        continue;
                    }
                    self.sequence_byte(bytes[i], sink);
                    i += 1;
                }
                _ => {
                    self.sequence_byte(bytes[i], sink);
                    i += 1;
                }
            }
        }
    }

    fn ground_byte(&mut self, b: u8, sink: &mut impl Sink) {
        match self.utf8.next(b) {
            Step::More => {}
            Step::Char(c) => self.ground_char(c, sink),
            Step::Invalid => sink.print(char::REPLACEMENT_CHARACTER),
            Step::InvalidRetry => {
                sink.print(char::REPLACEMENT_CHARACTER);
                // The byte that broke the sequence starts afresh (UTF8Decoder.zig, `consumed`).
                match self.utf8.next(b) {
                    Step::More => {}
                    Step::Char(c) => self.ground_char(c, sink),
                    Step::Invalid | Step::InvalidRetry => sink.print(char::REPLACEMENT_CHARACTER),
                }
            }
        }
    }

    fn ground_char(&mut self, c: char, sink: &mut impl Sink) {
        let cp = u32::from(c);
        if cp <= 0x0F {
            // `cp` is at most 0x0F, so it is the byte.
            sink.execute(c0_byte(cp));
        } else if cp == 0x1B {
            self.state = State::Escape;
            self.clear();
        } else {
            sink.print(c);
        }
    }

    fn sequence_byte(&mut self, b: u8, sink: &mut impl Sink) {
        let (next, act) = transition(self.state, b);
        if next != self.state {
            self.exit(b, sink);
        }
        self.act(act, b, sink);
        if next != self.state {
            self.state = next;
            self.enter(b, sink);
        }
    }

    fn exit(&mut self, b: u8, sink: &mut impl Sink) {
        match self.state {
            State::OscString => {
                sink.osc_end(if b == 0x07 { Terminator::Bel } else { Terminator::St })
            }
            State::DcsPassthrough => {
                if self.dcs_hooked {
                    self.dcs_hooked = false;
                    sink.dcs_unhook();
                }
            }
            State::SosPmApcString => sink.apc_end(),
            _ => {}
        }
    }

    fn enter(&mut self, b: u8, sink: &mut impl Sink) {
        match self.state {
            State::Escape | State::DcsEntry | State::CsiEntry => self.clear(),
            State::OscString => sink.osc_start(),
            State::DcsPassthrough => {
                if usize::from(self.n_params) >= MAX_PARAMS {
                    return;
                }
                self.finish_param();
                self.dcs_hooked = true;
                sink.dcs_hook(&Dcs {
                    intermediates: &self.intermediates[..usize::from(self.n_intermediates)],
                    params: &self.params[..usize::from(self.n_params)],
                    final_byte: b,
                });
            }
            State::SosPmApcString => sink.apc_start(),
            _ => {}
        }
    }

    fn act(&mut self, act: Act, b: u8, sink: &mut impl Sink) {
        match act {
            Act::None => {}
            Act::Print => sink.print(char::from(b)),
            Act::Execute => {
                if b >= 0x80 {
                    sink.esc(&Esc { intermediates: &[], final_byte: b - 0x40 });
                } else {
                    sink.execute(b);
                }
            }
            Act::Collect => {
                let n = usize::from(self.n_intermediates);
                if n < MAX_INTERMEDIATES {
                    self.intermediates[n] = b;
                    self.n_intermediates += 1;
                }
            }
            Act::Param => self.param(b),
            Act::EscDispatch => sink.esc(&Esc {
                intermediates: &self.intermediates[..usize::from(self.n_intermediates)],
                final_byte: b,
            }),
            Act::CsiDispatch => {
                if usize::from(self.n_params) >= MAX_PARAMS {
                    return;
                }
                self.finish_param();
                if b != b'm' && self.colons != 0 {
                    return;
                }
                sink.csi(&Csi {
                    intermediates: &self.intermediates[..usize::from(self.n_intermediates)],
                    params: &self.params[..usize::from(self.n_params)],
                    colons: self.colons,
                    final_byte: b,
                });
            }
            Act::Put => {
                if self.dcs_hooked {
                    sink.dcs_put(b);
                }
            }
            Act::OscPut => sink.osc_put(b),
            Act::ApcPut => sink.apc_put(&[b]),
        }
    }

    fn param(&mut self, b: u8) {
        let n = usize::from(self.n_params);
        if b == b';' || b == b':' {
            if n >= MAX_PARAMS {
                return;
            }
            self.params[n] = self.acc;
            if b == b':' {
                self.colons |= 1 << n;
            }
            self.n_params += 1;
            self.acc = 0;
            self.acc_started = false;
            return;
        }
        if n >= MAX_PARAMS {
            return;
        }
        self.acc = self.acc.saturating_mul(10).saturating_add(u16::from(b - b'0'));
        self.acc_started = true;
    }

    /// The parameter being built joins the list, if a digit was seen for it.
    fn finish_param(&mut self) {
        if self.acc_started {
            self.params[usize::from(self.n_params)] = self.acc;
            self.n_params += 1;
            self.acc = 0;
            self.acc_started = false;
        }
    }

    fn clear(&mut self) {
        self.n_intermediates = 0;
        self.n_params = 0;
        self.colons = 0;
        self.acc = 0;
        self.acc_started = false;
    }
}

fn c0_byte(cp: u32) -> u8 {
    match u8::try_from(cp) {
        Ok(b) => b,
        Err(_) => 0,
    }
}

/// Bytes from the start that are printable ASCII.
fn ascii_run(bytes: &[u8]) -> usize {
    bytes.iter().position(|&b| !(0x20..=0x7E).contains(&b)).unwrap_or(bytes.len())
}

/// Bytes from the start that are APC payload: 0x00–0x7F but CAN, SUB and ESC (parse_table.zig,
/// `sos_pm_apc_string`; stream.zig `consumeApcString`).
fn apc_run(bytes: &[u8]) -> usize {
    bytes.iter().position(|&b| matches!(b, 0x18 | 0x1A | 0x1B | 0x80..=0xFF)).unwrap_or(bytes.len())
}

/// parse_table.zig: the "anywhere" rows first, then each state's own, which override them; a
/// byte no row names stays in the state and does nothing.
fn transition(state: State, b: u8) -> (State, Act) {
    if let Some(t) = own(state, b) {
        return t;
    }
    match b {
        0x18 | 0x1A => (State::Ground, Act::Execute),
        0x80..=0x8F | 0x91..=0x97 | 0x99 | 0x9A => (State::Ground, Act::Execute),
        0x9C => (State::Ground, Act::None),
        0x1B => (State::Escape, Act::None),
        0x98 | 0x9E | 0x9F => (State::SosPmApcString, Act::None),
        0x9B => (State::CsiEntry, Act::None),
        0x90 => (State::DcsEntry, Act::None),
        0x9D => (State::OscString, Act::None),
        _ => (state, Act::None),
    }
}

/// C0 other than CAN, SUB and ESC: 0x00–0x17, 0x19, 0x1C–0x1F.
fn is_c0(b: u8) -> bool {
    matches!(b, 0x00..=0x17 | 0x19 | 0x1C..=0x1F)
}

fn own(state: State, b: u8) -> Option<(State, Act)> {
    use State as S;
    let t = match state {
        S::Ground => match b {
            _ if is_c0(b) => (S::Ground, Act::Execute),
            0x20..=0x7F => (S::Ground, Act::Print),
            _ => return None,
        },
        S::Escape => match b {
            _ if is_c0(b) => (S::Escape, Act::Execute),
            0x7F => (S::Escape, Act::None),
            0x20..=0x2F => (S::EscapeIntermediate, Act::Collect),
            0x58 | 0x5E | 0x5F => (S::SosPmApcString, Act::None),
            0x50 => (S::DcsEntry, Act::None),
            0x5B => (S::CsiEntry, Act::None),
            0x5D => (S::OscString, Act::None),
            0x30..=0x4F | 0x51..=0x57 | 0x59 | 0x5A | 0x5C | 0x60..=0x7E => {
                (S::Ground, Act::EscDispatch)
            }
            _ => return None,
        },
        S::EscapeIntermediate => match b {
            _ if is_c0(b) => (S::EscapeIntermediate, Act::Execute),
            0x20..=0x2F => (S::EscapeIntermediate, Act::Collect),
            0x7F => (S::EscapeIntermediate, Act::None),
            0x30..=0x7E => (S::Ground, Act::EscDispatch),
            _ => return None,
        },
        S::CsiEntry => match b {
            _ if is_c0(b) => (S::CsiEntry, Act::Execute),
            0x7F => (S::CsiEntry, Act::None),
            0x40..=0x7E => (S::Ground, Act::CsiDispatch),
            0x3A => (S::CsiIgnore, Act::None),
            0x20..=0x2F => (S::CsiIntermediate, Act::Collect),
            0x30..=0x39 | 0x3B => (S::CsiParam, Act::Param),
            0x3C..=0x3F => (S::CsiParam, Act::Collect),
            _ => return None,
        },
        S::CsiParam => match b {
            _ if is_c0(b) => (S::CsiParam, Act::Execute),
            0x30..=0x3B => (S::CsiParam, Act::Param),
            0x7F => (S::CsiParam, Act::None),
            0x40..=0x7E => (S::Ground, Act::CsiDispatch),
            0x3C..=0x3F => (S::CsiIgnore, Act::None),
            0x20..=0x2F => (S::CsiIntermediate, Act::Collect),
            _ => return None,
        },
        S::CsiIntermediate => match b {
            _ if is_c0(b) => (S::CsiIntermediate, Act::Execute),
            0x20..=0x2F => (S::CsiIntermediate, Act::Collect),
            0x7F => (S::CsiIntermediate, Act::None),
            0x40..=0x7E => (S::Ground, Act::CsiDispatch),
            0x30..=0x3F => (S::CsiIgnore, Act::None),
            _ => return None,
        },
        S::CsiIgnore => match b {
            _ if is_c0(b) => (S::CsiIgnore, Act::Execute),
            0x20..=0x3F | 0x7F => (S::CsiIgnore, Act::None),
            0x40..=0x7E => (S::Ground, Act::None),
            _ => return None,
        },
        S::DcsEntry => match b {
            _ if is_c0(b) => (S::DcsEntry, Act::None),
            0x7F => (S::DcsEntry, Act::None),
            0x20..=0x2F => (S::DcsIntermediate, Act::Collect),
            0x3A => (S::DcsIgnore, Act::None),
            0x30..=0x39 | 0x3B => (S::DcsParam, Act::Param),
            0x3C..=0x3F => (S::DcsParam, Act::Collect),
            0x40..=0x7E => (S::DcsPassthrough, Act::None),
            _ => return None,
        },
        S::DcsIntermediate => match b {
            _ if is_c0(b) => (S::DcsIntermediate, Act::None),
            0x20..=0x2F => (S::DcsIntermediate, Act::Collect),
            0x7F => (S::DcsIntermediate, Act::None),
            0x30..=0x3F => (S::DcsIgnore, Act::None),
            0x40..=0x7E => (S::DcsPassthrough, Act::None),
            _ => return None,
        },
        S::DcsIgnore => match b {
            _ if is_c0(b) => (S::DcsIgnore, Act::None),
            _ => return None,
        },
        S::DcsParam => match b {
            _ if is_c0(b) => (S::DcsParam, Act::None),
            0x30..=0x39 | 0x3B => (S::DcsParam, Act::Param),
            0x7F => (S::DcsParam, Act::None),
            0x3A | 0x3C..=0x3F => (S::DcsIgnore, Act::None),
            0x20..=0x2F => (S::DcsIntermediate, Act::Collect),
            0x40..=0x7E => (S::DcsPassthrough, Act::None),
            _ => return None,
        },
        S::DcsPassthrough => match b {
            _ if is_c0(b) => (S::DcsPassthrough, Act::Put),
            0x20..=0x7E => (S::DcsPassthrough, Act::Put),
            0x7F => (S::DcsPassthrough, Act::None),
            _ => return None,
        },
        S::OscString => match b {
            0x07 => (S::Ground, Act::None),
            0x00..=0x06 | 0x08..=0x17 | 0x19 | 0x1C..=0x1F => (S::OscString, Act::None),
            0x20..=0xFF => (S::OscString, Act::OscPut),
            _ => return None,
        },
        S::SosPmApcString => match b {
            _ if is_c0(b) => (S::SosPmApcString, Act::ApcPut),
            0x20..=0x7F => (S::SosPmApcString, Act::ApcPut),
            _ => return None,
        },
    };
    Some(t)
}
