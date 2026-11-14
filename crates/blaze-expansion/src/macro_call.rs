//! The record of one macro call (target decision 7).
//!
//! **Key**: the core's `Expansion` key (`blaze_keyed::kind::ExpansionInputs`), computed before
//! the expander runs, from:
//!
//! 1. the expander's code identity (`Expander`): for a proc macro, the content digest of its
//!    compiled wasm module, the export it runs, its kind, and the host interface's digest
//!    (the version of blaze-macro-eval's server, shims and the pinned bridge's protocol); for a
//!    declarative macro, [`crate::tokens::definition_identity`] of its definition;
//! 2. the input tokens, position-free ([`crate::tokens::stream_digest`] with the call's three
//!    sites first);
//! 3. the attribute's own arguments (an attribute macro), or the derive's helper context;
//! 4. the call's context ([`call_context`]): this crate's [`CALL_LAYOUT`], the call's edition,
//!    whether keywords of every edition are forced (blaze-macro-eval's `forced_keywords`), and
//!    the state the expander starts from (`StartState`).
//!
//! Nothing else is in the key. What an expander reads besides (a variable, a file, a span's
//! line) is an entry of its read list ([`crate::host`]); a declarative macro reads nothing, so
//! its list is always empty and its key is its validity.
//!
//! **Value** ([`MacroValue`]): the output (tokens against the call's numbering, the marks the
//! expansion made) or the panic's message, the diagnostics, and the state the call left
//! ([`StateOut`]), with the bytes of that state when a next call is to start from it.

use std::{alloc::Global, fmt};

use blaze_keyed::kind::{CallContext, Expansion, StartState};

use crate::{
    flat::{Corrupt, Reader, Why, Writer},
    tokens::{OutputRef, OutputWriter},
};

/// The version of what this crate keys and writes of a call (how input, attribute and spans are
/// laid out): the `layout` of every call's context.
pub const CALL_LAYOUT: u32 = 1;

/// A call's context as this crate keys it: [`CALL_LAYOUT`], the call's edition (its year), and
/// whether keywords of every edition are forced.
///
/// The state an expander's instance starts from: a module whose crates write no static (its
/// link's `blaze.statics` list names none outside the standard library) is always
/// `StartState::Fresh`. One that does is run with what the call before it, in its calling
/// crate's source order, left ([`crate::chain`]): the bytes of its statics
/// (`StartState::Statics`), or, when they hold an address of the instance's heap, which no fresh
/// instance has, the calls before it run again first (`StartState::Replayed`, by the digest of
/// their keys in order).
pub fn call_context(edition: u16, forced_keywords: bool, state: StartState) -> CallContext {
    CallContext { layout: CALL_LAYOUT, edition, forced_keywords, state }
}

fn put_state(state: &StartState, w: &mut Writer) {
    match state {
        StartState::Fresh => w.u8(0),
        StartState::Statics(d) => w.u8(1).digest(d),
        StartState::Replayed(d) => w.u8(2).digest(d),
    };
}

fn read_state(r: &mut Reader<'_>) -> Result<StartState, Corrupt> {
    Ok(match r.u8()? {
        0 => StartState::Fresh,
        1 => StartState::Statics(r.digest()?),
        2 => StartState::Replayed(r.digest()?),
        tag => return Err(r.corrupt(Why::Tag(tag))),
    })
}

/// The state a call left.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StateOut {
    /// As it found it: the call commutes with every other.
    Unchanged,
    /// Another state, which a next call starts from.
    Moved(StartState),
}

/// A diagnostic's level.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Level {
    Error,
    Warning,
    Note,
    Help,
}

impl Level {
    fn byte(self) -> u8 {
        match self {
            Level::Error => 0,
            Level::Warning => 1,
            Level::Note => 2,
            Level::Help => 3,
        }
    }

    fn of(byte: u8) -> Option<Level> {
        Some(match byte {
            0 => Level::Error,
            1 => Level::Warning,
            2 => Level::Note,
            3 => Level::Help,
            _ => return None,
        })
    }
}

/// How a run ended, as its value says it.
pub enum Ended<'w> {
    /// It gave this output.
    Output(OutputWriter<'w>),
    /// It panicked with this message: a value like an output, since a run from the same key
    /// panics the same; given to the caller as the macro's error.
    Panicked(&'w str),
}

/// Writes a call's value.
pub struct MacroValue<'w> {
    pub ended: Ended<'w>,
    /// Each diagnostic's level and text, in the order emitted.
    pub diagnostics: &'w [(Level, &'w str)],
    /// Text the expander printed to its stdout and stderr.
    pub printed: &'w [u8],
    pub state_out: StateOut,
    /// The bytes of the state left, when it is `StartState::Statics`: what a next call is given.
    pub state_bytes: &'w [u8],
}

impl MacroValue<'_> {
    pub fn encode(&self) -> Vec<u8, Global> {
        let mut w = Writer::new::<Expansion>();
        match &self.ended {
            Ended::Output(output) => {
                w.u8(0);
                output.write(&mut w);
            }
            Ended::Panicked(message) => {
                w.u8(1).str(message);
            }
        }
        w.u32(self.diagnostics.len() as u32);
        for (level, text) in self.diagnostics {
            w.u8(level.byte()).str(text);
        }
        w.bytes(self.printed);
        match self.state_out {
            StateOut::Unchanged => {
                w.u8(0);
            }
            StateOut::Moved(state) => {
                w.u8(1);
                put_state(&state, &mut w);
            }
        }
        w.bytes(self.state_bytes);
        w.finish()
    }
}

/// A call's value, read in place.
#[derive(Clone)]
pub struct MacroValueRef<'a> {
    pub output: Result<OutputRef<'a>, &'a str>,
    diagnostics: Reader<'a>,
    pub diagnostic_count: u32,
    pub printed: &'a [u8],
    pub state_out: StateOut,
    pub state_bytes: &'a [u8],
}

impl<'a> MacroValueRef<'a> {
    pub fn read(bytes: &'a [u8]) -> Result<MacroValueRef<'a>, Corrupt> {
        let mut r = Reader::open::<Expansion>(bytes)?;
        let output = match r.u8()? {
            0 => Ok(OutputRef::read(&mut r)?),
            1 => Err(r.str()?),
            tag => return Err(r.corrupt(Why::Tag(tag))),
        };
        let diagnostic_count = r.u32()?;
        let diagnostics = r.clone();
        for _ in 0..diagnostic_count {
            let byte = r.u8()?;
            Level::of(byte).ok_or(r.corrupt(Why::Tag(byte)))?;
            r.str()?;
        }
        let printed = r.bytes()?;
        let state_out = match r.u8()? {
            0 => StateOut::Unchanged,
            1 => StateOut::Moved(read_state(&mut r)?),
            tag => return Err(r.corrupt(Why::Tag(tag))),
        };
        let state_bytes = r.bytes()?;
        r.end()?;
        Ok(MacroValueRef { output, diagnostics, diagnostic_count, printed, state_out, state_bytes })
    }

    /// The diagnostics, in the order emitted (checked when read).
    pub fn diagnostics(&self) -> impl Iterator<Item = (Level, &'a str)> + 'a {
        let mut r = self.diagnostics.clone();
        (0..self.diagnostic_count).map(move |_| {
            let level = Level::of(r.u8().expect("checked at read")).expect("checked at read");
            (level, r.str().expect("checked at read"))
        })
    }
}

impl fmt::Debug for MacroValueRef<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MacroValueRef")
            .field("panicked", &self.output.as_ref().err())
            .field("diagnostics", &self.diagnostic_count)
            .field("state_out", &self.state_out)
            .finish_non_exhaustive()
    }
}
