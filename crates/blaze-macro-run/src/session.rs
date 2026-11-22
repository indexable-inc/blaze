//! THE SESSION: the macro's process's side of one call, a sans-IO state machine between three
//! parties its driver (piece 4's launcher) connects: blaze, over blaze-comptime's frames; the
//! pinned `proc_macro` client, whose requests it answers from the [`Server`]; and the process's
//! own standard library, whose system calls (`Env`, `Look`, ...) it forwards as they are.
//!
//! ```text
//! start            -> Send(Hello)
//! Frame(hello)     -> Run { which, kind, input }        the driver runs the client
//! Request(bytes)   -> Reply(bytes)                      most methods, from the server
//!                  -> Send(TrackEnv | TrackPath | Bridge span_file) -> Frame(answer) -> Reply
//! Sys(call)        -> Send(call) -> Frame(answer) -> Answered (read it with `answer`)
//! Returned(output) -> Last(Return)                      write it and exit
//! ```
//!
//! Anything the server stops on (a malformed request, a limit, a refused `span_file`) ends the
//! call with a stopped outcome in the `Return`: the process says why, blaze reports it, and
//! nothing else of the run is taken.

use core::fmt;

use blaze_comptime::{Answer, Call as SysCall, CallKind, Guest, GuestEffect, GuestError, GuestEvent, GuestOpts, Nanos, PROTOCOL, WireLimits};
use blaze_expand::ProcKind;
use blaze_own::{AVec, Alloc, Vec};

use crate::bridge::{Reader, Writer};
use crate::call::{CallView, MacroId};
use crate::method::Method;
use crate::outcome::write_stopped;
use crate::server::{Ask, Dispatched, Server, ServerError, ServerLimits, put_place};

pub struct SessionOpts<'c, 'a> {
    /// The call's bytes ([`crate::Call::encode`]), as the process was started on them.
    pub call: &'c [u8],
    pub limits: ServerLimits = ServerLimits::SHIPPED,
    pub wire: WireLimits = WireLimits::SHIPPED,
    pub alloc: Alloc<'a>,
}

/// What the driver hands the session.
#[derive(Clone, Copy, Debug)]
pub enum Input<'i> {
    /// An answer frame's body from blaze ([`blaze_comptime::wire::split_frame`] cut it).
    Frame(&'i [u8]),
    /// A request of the client, in the bridge's bytes.
    Request(&'i [u8]),
    /// The client's run output: `Result<TokenStream, PanicMessage>` in the bridge's bytes.
    Returned(&'i [u8]),
    /// A system call of the process's own library.
    Sys(SysCall<'i>),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InputKind {
    Frame,
    Request,
    Returned,
    Sys,
}

impl Input<'_> {
    pub fn kind(&self) -> InputKind {
        match self {
            Input::Frame(_) => InputKind::Frame,
            Input::Request(_) => InputKind::Request,
            Input::Returned(_) => InputKind::Returned,
            Input::Sys(_) => InputKind::Sys,
        }
    }
}

/// What the driver does next.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Output<'s> {
    /// Write this frame to blaze; its answer comes back as [`Input::Frame`].
    Send { frame: &'s [u8] },
    /// Run the client of macro `which` (an index of the crate's `#[rustc_proc_macro_decls]`)
    /// on `input` (`ExpnGlobals`, then the stream handles), handing its requests back as
    /// [`Input::Request`] and its output as [`Input::Returned`].
    Run { which: MacroId, kind: ProcKind, input: &'s [u8] },
    /// The reply to the client's request.
    Reply { bytes: &'s [u8] },
    /// The system call is answered: read it with [`Session::answer`].
    Answered,
    /// Write this frame (the `Return`) to blaze, then exit.
    Last { frame: &'s [u8] },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SessionState {
    New,
    /// `Hello` sent.
    Hello,
    /// The client runs.
    Running,
    /// A `track_*` ask is with blaze.
    AskingTrack,
    /// A `span_file` ask is with blaze.
    AskingFile,
    /// A system call is with blaze.
    Sys,
    Ended,
}

/// A session driven wrong, or blaze's side of the protocol broken: the process exits naming
/// it, and blaze sees the process gone (blaze-comptime `Crash::Gone`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SessionError {
    Order { state: SessionState, input: InputKind },
    Guest(GuestError),
    /// blaze answered `Hello` with another protocol's version.
    Protocol { got: u32 },
    /// An answer the call cannot have (the guest checks fit; this is its shape here).
    Answer { on: CallKind },
    /// A `span_file` reply that is not `Result<String, ()>`.
    FileReply,
    /// The guest made no frame for a call it took.
    NoFrame,
}

impl fmt::Display for SessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SessionError::Order { state, input } => write!(f, "the macro's session was handed {input:?} while {state:?}"),
            SessionError::Guest(error) => write!(f, "the macro's session's protocol failed: {error:?}"),
            SessionError::Protocol { got } => write!(f, "blaze answered the macro's session with protocol {got}, not {PROTOCOL}"),
            SessionError::Answer { on } => write!(f, "blaze answered the macro's session's {on:?} with an answer it cannot have"),
            SessionError::FileReply => f.write_str("blaze answered `span_file` with bytes that are not `Result<String, ()>`"),
            SessionError::NoFrame => f.write_str("the macro's session took a call and made no frame of it"),
        }
    }
}

/// What a step emits, before it borrows the session.
#[derive(Clone, Copy)]
enum Emit {
    Send,
    Run,
    Reply,
    Answered,
    Last,
}

pub struct Session<'c, 'a> {
    call: &'c [u8],
    limits: ServerLimits,
    guest: Guest<'a>,
    server: Option<Server<'a>>,
    which: MacroId,
    kind: ProcKind,
    state: SessionState,
    /// The client's run input.
    run_input: AVec<'a, u8>,
    /// The last reply to the client.
    reply: AVec<'a, u8>,
    /// A `span_file` ask's place, then the outcome's bytes.
    out: AVec<'a, u8>,
    alloc: Alloc<'a>,
}

/// The guest reads no clock (blaze-comptime guest.rs `step` ignores it): the session waits on
/// nothing but its answers, which blaze bounds.
const NO_CLOCK: Nanos = Nanos(0);

impl<'c, 'a> Session<'c, 'a> {
    pub fn new(opts: SessionOpts<'c, 'a>) -> Session<'c, 'a> {
        Session {
            call: opts.call,
            limits: opts.limits,
            guest: Guest::new(GuestOpts { wire: opts.wire, alloc: opts.alloc }),
            server: None,
            which: MacroId(0),
            kind: ProcKind::FnLike,
            state: SessionState::New,
            run_input: Vec::new_in(opts.alloc),
            reply: Vec::new_in(opts.alloc),
            out: Vec::new_in(opts.alloc),
            alloc: opts.alloc,
        }
    }

    pub fn state(&self) -> SessionState {
        self.state
    }

    /// The `Hello` frame.
    pub fn start(&mut self) -> Result<Output<'_>, SessionError> {
        if self.state != SessionState::New {
            return Err(SessionError::Order { state: self.state, input: InputKind::Frame });
        }
        self.guest.call(&SysCall::Hello { protocol: PROTOCOL }).map_err(SessionError::Guest)?;
        self.state = SessionState::Hello;
        self.emit(Emit::Send)
    }

    pub fn step(&mut self, input: Input<'_>) -> Result<Output<'_>, SessionError> {
        let emit = self.advance(input)?;
        self.emit(emit)
    }

    /// The answer to the last system call ([`Output::Answered`]).
    pub fn answer(&self) -> Result<Answer<'_>, SessionError> {
        self.guest.answer().map_err(SessionError::Guest)
    }

    fn advance(&mut self, input: Input<'_>) -> Result<Emit, SessionError> {
        let order = SessionError::Order { state: self.state, input: input.kind() };
        match (self.state, input) {
            (SessionState::Hello, Input::Frame(body)) => {
                self.guest.step(NO_CLOCK, GuestEvent::Frame(body)).map_err(SessionError::Guest)?;
                match self.guest.answer().map_err(SessionError::Guest)? {
                    Answer::Hello { protocol } if protocol == PROTOCOL => {}
                    Answer::Hello { protocol } => return Err(SessionError::Protocol { got: protocol }),
                    _ => return Err(SessionError::Answer { on: CallKind::Hello }),
                }
                self.begin()
            }
            (SessionState::Running, Input::Request(request)) => {
                let Some(server) = self.server.as_mut() else { return Err(order) };
                match server.dispatch(request, &mut self.reply) {
                    Ok(Dispatched::Replied) => Ok(Emit::Reply),
                    Ok(Dispatched::Ask(ask)) => self.ask(ask),
                    Err(error) => self.stop(error),
                }
            }
            (SessionState::Running, Input::Sys(call)) => {
                self.guest.call(&call).map_err(SessionError::Guest)?;
                self.state = SessionState::Sys;
                Ok(Emit::Send)
            }
            (SessionState::Running, Input::Returned(output)) => {
                let Some(server) = self.server.as_mut() else { return Err(order) };
                server.outcome(output, &mut self.out);
                self.last()
            }
            (SessionState::AskingTrack, Input::Frame(body)) => {
                self.guest.step(NO_CLOCK, GuestEvent::Frame(body)).map_err(SessionError::Guest)?;
                // `Done`, or a refused path's error: the call goes on, and blaze refuses it by
                // the path's name when it ends (host.rs `finish`).
                match self.guest.answer().map_err(SessionError::Guest)? {
                    Answer::Done | Answer::Error(_) => {}
                    _ => return Err(SessionError::Answer { on: CallKind::TrackPath }),
                }
                let Some(server) = self.server.as_ref() else { return Err(order) };
                server.done_reply(&mut self.reply);
                self.state = SessionState::Running;
                Ok(Emit::Reply)
            }
            (SessionState::AskingFile, Input::Frame(body)) => {
                self.guest.step(NO_CLOCK, GuestEvent::Frame(body)).map_err(SessionError::Guest)?;
                let Answer::Bridge { reply } = self.guest.answer().map_err(SessionError::Guest)? else {
                    return Err(SessionError::Answer { on: CallKind::Bridge });
                };
                let mut r = Reader::new(reply);
                if !r.ok().map_err(|_| SessionError::FileReply)? {
                    r.end().map_err(|_| SessionError::FileReply)?;
                    return self.stop(ServerError::FileRefused);
                }
                let file = r.str().map_err(|_| SessionError::FileReply)?;
                r.end().map_err(|_| SessionError::FileReply)?;
                let Some(server) = self.server.as_ref() else { return Err(order) };
                server.file_reply(file, &mut self.reply);
                self.state = SessionState::Running;
                Ok(Emit::Reply)
            }
            (SessionState::Sys, Input::Frame(body)) => {
                self.guest.step(NO_CLOCK, GuestEvent::Frame(body)).map_err(SessionError::Guest)?;
                self.state = SessionState::Running;
                Ok(Emit::Answered)
            }
            _ => Err(order),
        }
    }

    /// The server of the call, and the client's run; a call the process cannot serve ends at
    /// once with its stopped outcome.
    fn begin(&mut self) -> Result<Emit, SessionError> {
        let view = match CallView::read(self.call) {
            Ok(view) => view,
            Err(what) => return self.stop(ServerError::Call(what)),
        };
        match Server::new(&view, self.limits, self.alloc, &mut self.run_input) {
            Ok(server) => {
                self.server = Some(server);
                self.which = view.which;
                self.kind = view.kind;
                self.state = SessionState::Running;
                Ok(Emit::Run)
            }
            Err(error) => self.stop(error),
        }
    }

    fn ask(&mut self, ask: Ask<'_>) -> Result<Emit, SessionError> {
        let (call, state) = match ask {
            Ask::TrackEnv { name, value } => {
                (SysCall::TrackEnv { name: name.as_bytes(), value: value.map(str::as_bytes) }, SessionState::AskingTrack)
            }
            Ask::TrackPath { path } => (SysCall::TrackPath { path: path.as_bytes() }, SessionState::AskingTrack),
            Ask::SpanFile { place } => {
                self.out.clear();
                put_place(&mut Writer { out: &mut self.out }, place);
                (SysCall::Bridge { method: Method::SpanFile.tag(), args: &self.out }, SessionState::AskingFile)
            }
        };
        self.guest.call(&call).map_err(SessionError::Guest)?;
        self.state = state;
        Ok(Emit::Send)
    }

    /// Ends the call with the stopped outcome of `error`.
    fn stop(&mut self, error: ServerError) -> Result<Emit, SessionError> {
        write_stopped(&mut self.out, error);
        self.last()
    }

    /// Sends the outcome in `out` as the `Return`.
    fn last(&mut self) -> Result<Emit, SessionError> {
        self.guest.call(&SysCall::Return { output: &self.out }).map_err(SessionError::Guest)?;
        self.state = SessionState::Ended;
        Ok(Emit::Last)
    }

    fn emit(&mut self, emit: Emit) -> Result<Output<'_>, SessionError> {
        Ok(match emit {
            Emit::Run => Output::Run { which: self.which, kind: self.kind, input: &self.run_input },
            Emit::Reply => Output::Reply { bytes: &self.reply },
            Emit::Answered => Output::Answered,
            Emit::Send | Emit::Last => {
                let last = matches!(emit, Emit::Last);
                match self.guest.next_effect() {
                    Some(GuestEffect::Send { frame }) if last => Output::Last { frame },
                    Some(GuestEffect::Send { frame }) => Output::Send { frame },
                    None => return Err(SessionError::NoFrame),
                }
            }
        })
    }
}
