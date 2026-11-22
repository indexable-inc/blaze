//! THE GUEST'S MACHINE: the compile-time process's side, sans-IO (C5), under its standard
//! library's `sys` layer (comptime-native.md, "The std backend"). The library calls
//! [`Guest::call`], writes the frame [`Guest::next_effect`] gives it, reads one frame back and
//! steps it in; [`Guest::answer`] is then the typed answer, checked to fit the call it answers.
//! One call is in flight at a time (call.rs, "Lockstep").

#![blaze::sans_io]

use blaze_own::{AVec, Alloc, Vec};

use crate::call::{Answer, Call, CallKind, SysError};
use crate::host::Nanos;
use crate::wire::{WireError, WireLimits, read_answer, write_call};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GuestState {
    /// No call in flight.
    Ready,
    /// A call sent, its answer not yet in.
    Waiting { on: CallKind },
    /// The answer to `on` is in ([`Guest::answer`]); the next call may be made.
    Answered { on: CallKind },
    /// `Return` or `Exit` sent: nothing more crosses.
    Ended,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GuestEvent<'e> {
    /// One frame's body from the host.
    Frame(&'e [u8]),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GuestEffect<'g> {
    Send { frame: &'g [u8] },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GuestError {
    /// A call while one is in flight, or after the end.
    Busy { state: GuestState },
    /// A frame while none was awaited.
    Unasked,
    /// A frame that is not this protocol's.
    Wire(WireError),
    /// An answer of a kind the call cannot have.
    Mismatch { on: CallKind },
    /// [`Guest::answer`] with no answer in.
    NoAnswer,
}

pub struct GuestOpts<'a> {
    pub wire: WireLimits = WireLimits::SHIPPED,
    pub alloc: Alloc<'a>,
}

pub struct Guest<'a> {
    wire: WireLimits,
    state: GuestState,
    send: AVec<'a, u8>,
    sent: bool,
    /// The last answer frame's body, kept so the answer can borrow it.
    answer: AVec<'a, u8>,
}

impl<'a> Guest<'a> {
    pub fn new(opts: GuestOpts<'a>) -> Guest<'a> {
        Guest { wire: opts.wire, state: GuestState::Ready, send: Vec::new_in(opts.alloc), sent: true, answer: Vec::new_in(opts.alloc) }
    }

    pub fn state(&self) -> GuestState {
        self.state
    }

    /// The guest waits on nothing but its answer, which the host bounds.
    pub fn next_deadline(&self) -> Option<Nanos> {
        None
    }

    pub fn call(&mut self, call: &Call<'_>) -> Result<(), GuestError> {
        match self.state {
            GuestState::Ready | GuestState::Answered { .. } => {}
            state => return Err(GuestError::Busy { state }),
        }
        self.send.clear();
        write_call(call, &mut self.send, self.wire).map_err(GuestError::Wire)?;
        self.sent = false;
        self.state = match call {
            Call::Return { .. } | Call::Exit { .. } => GuestState::Ended,
            other => GuestState::Waiting { on: other.kind() },
        };
        Ok(())
    }

    pub fn next_effect(&mut self) -> Option<GuestEffect<'_>> {
        if self.sent {
            return None;
        }
        self.sent = true;
        Some(GuestEffect::Send { frame: &self.send })
    }

    pub fn step(&mut self, _now: Nanos, event: GuestEvent<'_>) -> Result<(), GuestError> {
        let GuestEvent::Frame(body) = event;
        let GuestState::Waiting { on } = self.state else { return Err(GuestError::Unasked) };
        let answer = read_answer(body, self.wire).map_err(GuestError::Wire)?;
        if !fits(on, &answer) {
            return Err(GuestError::Mismatch { on });
        }
        self.answer.clear();
        self.answer.extend_from_slice(body);
        self.state = GuestState::Answered { on };
        Ok(())
    }

    pub fn answer(&self) -> Result<Answer<'_>, GuestError> {
        match self.state {
            GuestState::Answered { .. } => read_answer(&self.answer, self.wire).map_err(GuestError::Wire),
            _ => Err(GuestError::NoAnswer),
        }
    }
}

/// Whether `answer` is one the call `on` can have.
pub fn fits(on: CallKind, answer: &Answer<'_>) -> bool {
    use CallKind as K;
    match answer {
        Answer::Hello { .. } => on == K::Hello,
        Answer::File { .. } | Answer::Listing { .. } => on == K::Look,
        Answer::Stat { .. } => on == K::Look,
        Answer::Value { .. } => matches!(on, K::Env | K::Cwd),
        Answer::Words { .. } => matches!(on, K::EnvAll | K::Args),
        Answer::Clock { .. } => on == K::Clock,
        Answer::Random { .. } => on == K::Random,
        Answer::Done => matches!(
            on,
            K::Write | K::WriteFile | K::SetLen | K::CreateDir | K::Remove | K::Rename | K::Sleep | K::TrackEnv | K::TrackPath
        ),
        Answer::Spawned { .. } => on == K::Spawn,
        Answer::Bridge { .. } => on == K::Bridge,
        Answer::Error(SysError::TooLarge) => !matches!(on, K::Hello | K::Clock | K::Sleep | K::TrackEnv),
        Answer::Error(_) => !matches!(on, K::Hello | K::EnvAll | K::Args | K::Cwd | K::Clock | K::Random | K::Sleep | K::TrackEnv),
    }
}
