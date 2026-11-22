//! THE MACRO HOST: blaze's side of one call. blaze-comptime's [`Host`] runs the protocol (the
//! grants, fuel, deadline, the read list, the env facts); this adds what is a proc macro's:
//! the call's grants are checked to be this call's, `span_file` is answered from the call (and
//! recorded as a read), and the outcome is taken only as data the host checks (outcome.rs).
//!
//! The driver moves frames and answers [`Effect::Look`] as for any compile-time run; a bridge
//! question never reaches it, because the host answers it inside [`MacroHost::step`].

use core::fmt;

use blaze_comptime::{Call as SysCall, CallKind, Done, Effect, Event, Grants, Host, HostError, HostOpts, HostState, Nanos, RunKind, WireLimits};
use blaze_expand::{Produced, ReadAnswer, ReadKind, Reads, StreamError};
use blaze_id::ContentId;
use blaze_own::{AVec, Alloc, Vec};

use crate::bridge::{Reader, Sink};
use crate::call::{Call, CallError, MacroDecl, MacroId, MacroProgram};
use crate::method::Method;
use crate::outcome::{Checking, Decoded, OutcomeError, OutcomeLimits, decode};
use crate::refused::{FileWhy, Refused, Why};
use crate::server::{Place, read_place};

/// The domain of a `span_file` read's answer digest.
const FILE_READ: &str = "blaze proc-macro read";

pub struct MacroHostOpts<'h, 's, 'g, 'a> {
    pub program: &'h MacroProgram<'h>,
    pub call: Call<'h, 's>,
    /// The file the call's written tokens are in, as `Span::file` prints it (a path relative to
    /// the package, never one of this machine: rule 25); `None` when the caller does not show
    /// it, and `span_file` is refused.
    pub file: Option<&'h str>,
    /// The run's grants, made with [`Grants::proc_macro`] on this call's bytes
    /// ([`Call::encode`]) and no separate `attr` (the call's bytes hold the arguments).
    pub grants: Grants<'g>,
    pub wire: WireLimits = WireLimits::SHIPPED,
    pub limits: OutcomeLimits = OutcomeLimits::SHIPPED,
    /// Where the outcome is kept.
    pub alloc: Alloc<'a>,
    /// What the checks need only while they run.
    pub scratch: Alloc<'a>,
}

/// A host driven wrong, or a call that cannot be run.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MacroHostError {
    Host(HostError),
    Call(CallError),
    NoSuchMacro { which: MacroId },
    /// A call of another kind than the macro declares.
    Kind { which: MacroId, declared: blaze_expand::ProcKind, called: blaze_expand::ProcKind },
    /// Grants that are not this call's: another macro's name, other bytes, or a separate
    /// `attr` (rule 21: the run's key is the call's bytes).
    NotGranted,
    /// A bridge question with no bridge effect (blaze-comptime broke its own order).
    NoBridge,
}

impl fmt::Display for MacroHostError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MacroHostError::Host(error) => write!(f, "the macro host was driven wrong: {error:?}"),
            MacroHostError::Call(error) => write!(f, "{error}"),
            MacroHostError::NoSuchMacro { which } => write!(f, "the program has no macro {}", which.0),
            MacroHostError::Kind { which, declared, called } => {
                write!(f, "the program's macro {} is a {declared} macro, called as a {called} macro", which.0)
            }
            MacroHostError::NotGranted => {
                f.write_str("the run's grants are not this call's: another name, other bytes or a separate attribute (rule 21)")
            }
            MacroHostError::NoBridge => f.write_str("a bridge question left the run with no bridge effect"),
        }
    }
}

/// What the host refused during the run; the first one is the call's refusal.
#[derive(Clone, Copy, Debug)]
enum Stop {
    File(FileWhy),
    Bridge { method: u8 },
    Reads(StreamError),
}

impl Stop {
    fn why<'x>(self) -> Why<'x> {
        match self {
            Stop::File(why) => Why::File(why),
            Stop::Bridge { method } => Why::Bridge { method },
            Stop::Reads(error) => Why::Outcome(OutcomeError::Stream(error)),
        }
    }
}

/// An expansion taken: blaze-expand's [`Produced`] (its tokens checked, its reads the
/// `span_file` reads) and the run's [`Done`] (its outcome bytes, read list, facts, findings).
pub struct Outcome<'a> {
    pub produced: Produced<'a>,
    pub done: Done<'a>,
}

pub struct MacroHost<'h, 's, 'g, 'a> {
    program: &'h MacroProgram<'h>,
    decl: &'h MacroDecl<'h>,
    call: Call<'h, 's>,
    file: Option<&'h str>,
    host: Host<'g, 'a>,
    wire: WireLimits,
    limits: OutcomeLimits,
    reply: AVec<'a, u8>,
    subject: AVec<'a, u8>,
    reads: Reads<'a>,
    stop: Option<Stop>,
    alloc: Alloc<'a>,
    scratch: Alloc<'a>,
}

impl<'h, 's, 'g, 'a> MacroHost<'h, 's, 'g, 'a> {
    pub fn new(opts: MacroHostOpts<'h, 's, 'g, 'a>) -> Result<MacroHost<'h, 's, 'g, 'a>, MacroHostError> {
        let which = opts.call.which;
        let decl = opts.program.get(which).ok_or(MacroHostError::NoSuchMacro { which })?;
        if decl.kind != opts.call.kind {
            return Err(MacroHostError::Kind { which, declared: decl.kind, called: opts.call.kind });
        }
        let mut bytes = Vec::new_in(opts.scratch);
        opts.call.encode(&mut bytes).map_err(MacroHostError::Call)?;
        match opts.grants.kind {
            RunKind::ProcMacro { name, input, attr: None } if name == decl.name.as_bytes() && input == &bytes[..] => {}
            _ => return Err(MacroHostError::NotGranted),
        }
        Ok(MacroHost {
            program: opts.program,
            decl,
            call: opts.call,
            file: opts.file,
            host: Host::new(HostOpts { grants: opts.grants, wire: opts.wire, alloc: opts.alloc }),
            wire: opts.wire,
            limits: opts.limits,
            reply: Vec::new_in(opts.alloc),
            subject: Vec::new_in(opts.scratch),
            reads: Reads::new_in(opts.alloc),
            stop: None,
            alloc: opts.alloc,
            scratch: opts.scratch,
        })
    }

    pub fn state(&self) -> HostState {
        self.host.state()
    }

    pub fn next_deadline(&self) -> Option<Nanos> {
        self.host.next_deadline()
    }

    /// The next thing the driver does: send a frame, look a path up, kill the process.
    pub fn next_effect(&mut self) -> Option<Effect<'_>> {
        self.host.next_effect()
    }

    pub fn step(&mut self, now: Nanos, event: Event<'_>) -> Result<(), MacroHostError> {
        let bridge = match event {
            Event::Frame(body) => matches!(blaze_comptime::wire::read_call(body, self.wire), Ok(SysCall::Bridge { .. })),
            _ => false,
        };
        self.host.step(now, event).map_err(MacroHostError::Host)?;
        if !bridge || self.host.state() != HostState::Answering {
            return Ok(());
        }
        let answered = match self.host.next_effect() {
            Some(Effect::Bridge { method, args }) if method == Method::SpanFile.tag() => {
                answer_file(&self.call, self.file, args, &mut self.subject, &mut self.reads, &mut self.reply)
            }
            Some(Effect::Bridge { method, .. }) => Err(Stop::Bridge { method }),
            _ => return Err(MacroHostError::NoBridge),
        };
        if let Err(stop) = answered {
            self.stop.get_or_insert(stop);
            // `Err`: the session stops the call with its own account (server.rs `FileRefused`).
            self.reply.clear();
            self.reply.push(1);
        }
        self.host.step(now, Event::Bridged { reply: &self.reply }).map_err(MacroHostError::Host)
    }

    /// The expansion, or why the call is refused.
    pub fn finish(self) -> Result<Outcome<'a>, Refused<'h, 'a>> {
        let MacroHost { program, decl, call, host, limits, reads, stop, alloc, scratch, .. } = self;
        let refused = |why| Refused { krate: program.krate, name: decl.name, kind: call.kind, why };
        if let Some(stop) = stop {
            return Err(refused(stop.why()));
        }
        let done = host.finish().map_err(|ended| refused(Why::Ended(ended)))?;
        // A path outside the package, tracked or opened: refused by its name (the run was told
        // it is not there, so its outcome is not one blaze may take).
        if let Some(finding) = done.findings.iter().find(|f| matches!(f.call, CallKind::TrackPath | CallKind::Look)) {
            let path = Vec::from_slice_in(finding.name, alloc);
            return Err(refused(Why::Outside { path, grant: finding.grant }));
        }
        let check = Checking { input: call.input, attr: call.attr, edition: call.edition };
        match decode(&done.output, &check, limits, alloc, scratch) {
            Err(error) => Err(refused(Why::Outcome(error))),
            Ok(Decoded::Stopped(error)) => Err(refused(Why::Server(error))),
            Ok(Decoded::Produced(mut produced)) => {
                produced.reads = reads;
                Ok(Outcome { produced, done })
            }
        }
    }
}

/// `span_file` of the place in `args`: the call's file for a token written in it, the reply
/// `Ok(file)` and the read recorded; otherwise why not.
fn answer_file(
    call: &Call<'_, '_>,
    file: Option<&str>,
    args: &[u8],
    subject: &mut AVec<'_, u8>,
    reads: &mut Reads<'_>,
    reply: &mut AVec<'_, u8>,
) -> Result<(), Stop> {
    let mut r = Reader::new(args);
    let place = read_place(&mut r).map_err(|_| Stop::File(FileWhy::Malformed))?;
    r.end().map_err(|_| Stop::File(FileWhy::Malformed))?;
    let (stream, at, side) = match place {
        Place::Site => return Err(Stop::File(FileWhy::Site)),
        Place::Input { lo, .. } => (Some(call.input), lo, "input"),
        Place::Attr { lo, .. } => (call.attr, lo, "attribute"),
    };
    let token = stream
        .and_then(|stream| stream.tokens().get(usize::try_from(at).ok()?))
        .ok_or(Stop::File(FileWhy::Forged { place }))?;
    // A token an expansion made has a context of its stream's table, not a root.
    if token.ctx.node().is_some() {
        return Err(Stop::File(FileWhy::MadeToken { place }));
    }
    let file = file.ok_or(Stop::File(FileWhy::Unknown))?;
    subject.clear();
    // Writing into a vector never fails.
    let _ = fmt::write(&mut Sink { out: &mut *subject }, format_args!("span_file of {side} token {at}"));
    let text = core::str::from_utf8(&subject[..]).unwrap_or("span_file");
    reads.push(ReadKind::ProcMacro, text, ReadAnswer::Digest(ContentId::of(FILE_READ, file.as_bytes()))).map_err(Stop::Reads)?;
    reply.clear();
    reply.push(0);
    crate::bridge::Writer { out: reply }.str(file);
    Ok(())
}
