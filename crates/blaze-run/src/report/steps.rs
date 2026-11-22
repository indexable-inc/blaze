//! The step trace of a timed-out or failed end-to-end test (docs/v2/plan/design/test.md, "End
//! state: property, differential and end-to-end tests", C, "A failure names the step"; piece
//! 18, the formatter half): a pure formatter over values the capture hands it. Capturing them
//! (the stacks at the deadline, zygote.rs `Effect::Stacks`, symbolised; each thread's blocking
//! call, read by the control thread; the open spans of blaze_core's waits; the test crate's own
//! functions the run entered; each process's output tail) is the test child's, not this
//! file's: nothing here reads a clock, a process or a file.
//!
//! The text view, exactly (a thread of the test's own process is named alone, one of a launched
//! process after its program's name):
//!
//! ```text
//! FAILED tests::place_order: deadline 60 s
//!   step   place_order > wait_ready (still inside)
//!   waits  main: read(fd 7: pipe from `shop-server` pid 412) for 59.8 s
//!          shop-server main: span "load payment form" deadline 30 s, expired
//!   output shop-server: "error: payment config missing: config/payment.toml"
//!   stack  main: shop::tests::wait_ready
//!                shop::tests::place_order
//!          shop-server main: shop_server::payment::load
//! ```
//!
//! Sections in this order, each printed only when it has a line, except `step` and `waits`,
//! which say so when empty (`none of the test's own functions was entered`, `no thread is in a
//! blocking call or an open span`), so a trace is never only "timed out". Processes in order:
//! the test's own, then the launched ones as given (the capture lists them by launch). Output:
//! the last [`OUTPUT_LINES`] lines of each process's tail, a trailing `\r` dropped, a process
//! with no output skipped. A time is `60 s` when whole, `59.8 s` (tenths, cut) from a second
//! up, `250 ms` below.
//!
//! The JSON view: one line, keys always present and in this order: `{"event":"steps","test":
//! (64 hex digits),"path","deadline_ns","steps":[names],"inside":(index or null),"processes":
//! [{"name","pid","own","threads":[{"name","blocked":({"call","on":{"kind",..},"for_ns"} or
//! null),"spans":[{"what","deadline_ns","open_ns","expired"}],"frames":[names]}],"output":
//! [lines]}]}`; `on`'s kinds: `pipe` (`fd`, `from`: `{"name","pid"}` or null), `connect` and
//! `accept` (`fd`, `port`), `child` (`pid`, `name`), `lock` (`name`), `futex` (`address`),
//! `file` (`fd`, `path`), `fd` (`fd`), `sleep`.

#![blaze::sans_io]

use core::fmt::{self, Write};

use super::{Format, ReportError, Style, hex, json_str, lossy, paint, RED};
use crate::{Nanos, TestId};

/// The most lines of one process's output a step trace shows.
pub const OUTPUT_LINES: usize = 20;

/// The test crate's own functions a run entered, in item order, and which of them the run is
/// still inside: the innermost of them on the test thread's stack.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Steps<'r> {
    entered: &'r [&'r str],
    inside: Option<usize>,
}

/// Why a [`Steps`] was not made.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StepsError {
    /// `inside` names no entered function.
    Inside { inside: usize, entered: usize },
}

impl fmt::Display for StepsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            StepsError::Inside { inside, entered } => write!(
                f,
                "a step trace says the run is inside step {inside}, but only {entered} of the test's own functions were entered"
            ),
        }
    }
}

impl core::error::Error for StepsError {}

impl<'r> Steps<'r> {
    pub const fn new(entered: &'r [&'r str], inside: Option<usize>) -> Result<Steps<'r>, StepsError> {
        match inside {
            Some(at) if at >= entered.len() => Err(StepsError::Inside { inside: at, entered: entered.len() }),
            _ => Ok(Steps { entered, inside }),
        }
    }

    pub const fn entered(&self) -> &'r [&'r str] {
        self.entered
    }

    pub const fn inside(&self) -> Option<usize> {
        self.inside
    }
}

/// A process at the other end of a pipe or a wait.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Peer<'r> {
    /// Its program's name, as the trace prints it (`shop-server`).
    pub name: &'r str,
    pub pid: u32,
}

/// What a blocking call waits on, as the control thread read it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum On<'r> {
    /// A pipe, and the process at its other end when the capture knows it.
    Pipe { fd: i32, from: Option<Peer<'r>> },
    /// A connect to a loopback port.
    Connect { fd: i32, port: u16 },
    /// An accept on a loopback port.
    Accept { fd: i32, port: u16 },
    /// A wait for a child process.
    Child { child: Peer<'r> },
    /// A lock a `blaze_core::lock` names.
    Lock { name: &'r str },
    /// A futex no named lock owns.
    Futex { address: u64 },
    /// A file, by its path from the workspace root.
    File { fd: i32, path: &'r str },
    /// A descriptor whose kind the capture did not name.
    Fd { fd: i32 },
    /// A sleep: nothing to wait on but time.
    Sleep,
}

/// A thread's blocking call in flight at the deadline.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Blocked<'r> {
    /// The call's name (`read`, `connect`, `wait4`, `__psynch_mutexwait`).
    pub call: &'r str,
    pub on: On<'r>,
    /// How long the thread had been in it.
    pub since: Nanos,
}

/// A span still open at the deadline (rules 15, 44: each names what it waits for).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct OpenSpan<'r> {
    pub what: &'r str,
    pub deadline: Option<Nanos>,
    /// How long it had been open.
    pub open: Nanos,
}

impl OpenSpan<'_> {
    pub const fn expired(&self) -> bool {
        match self.deadline {
            Some(deadline) => self.open.0 >= deadline.0,
            None => false,
        }
    }
}

/// One thread at the deadline.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Thread<'r> {
    /// As printed: `main`, or the thread's name.
    pub name: &'r str,
    pub blocked: Option<Blocked<'r>>,
    /// Outermost first.
    pub spans: &'r [OpenSpan<'r>],
    /// Its stack, symbolised, innermost first.
    pub frames: &'r [&'r str],
}

/// One process: the test's own, or one it launched.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Process<'r> {
    pub name: &'r str,
    pub pid: u32,
    pub threads: &'r [Thread<'r>],
    /// The tail of its captured output.
    pub output: &'r [u8],
}

/// Everything a step trace prints, as the capture hands it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct StepTrace<'r> {
    pub test: TestId,
    /// As printed: `module::name`.
    pub path: &'r str,
    pub deadline: Nanos,
    pub steps: Steps<'r>,
    /// The test's own process.
    pub own: Process<'r>,
    /// The processes it launched, in launch order.
    pub launched: &'r [Process<'r>],
}

/// Writes a timed-out test's report: in text, the `FAILED <path>: deadline <time>` line and
/// the trace; in JSON, the trace's one object.
pub fn timed_out(out: &mut dyn Write, style: Style, trace: &StepTrace<'_>) -> Result<(), ReportError> {
    match style.format {
        Format::Text => {
            paint(out, style, RED, "FAILED")?;
            write!(out, " {}: deadline ", trace.path)?;
            time(out, trace.deadline)?;
            out.write_char('\n')?;
            trace_text(out, trace)?;
        }
        Format::Json => trace_json(out, trace)?,
    }
    Ok(())
}

/// Writes the trace alone (its indented lines; in JSON its object): what follows a failed
/// end-to-end test's `FAILED` line from [`super::failure`].
pub fn step_trace(out: &mut dyn Write, style: Style, trace: &StepTrace<'_>) -> Result<(), ReportError> {
    match style.format {
        Format::Text => trace_text(out, trace)?,
        Format::Json => trace_json(out, trace)?,
    }
    Ok(())
}

/// The nine columns before a section's text: `  step   `, `  waits  `, `  output `, `  stack  `.
const INDENT: &str = "         ";

/// Opens a section's line: its word the first time, the indent after.
fn section(out: &mut dyn Write, word: &str, first: &mut bool) -> fmt::Result {
    if *first {
        *first = false;
        write!(out, "  {word:<7}")
    } else {
        out.write_str(INDENT)
    }
}

/// A thread as the trace names it: alone in the test's process, after the program's name in a
/// launched one. Returns the width written.
fn thread_label(out: &mut dyn Write, process: &Process<'_>, own: bool, thread: &Thread<'_>) -> Result<usize, fmt::Error> {
    if own {
        write!(out, "{}: ", thread.name)?;
        Ok(thread.name.chars().count() + 2)
    } else {
        write!(out, "{} {}: ", process.name, thread.name)?;
        Ok(process.name.chars().count() + thread.name.chars().count() + 3)
    }
}

fn processes<'t, 'r>(trace: &'t StepTrace<'r>) -> impl Iterator<Item = (&'t Process<'r>, bool)> {
    core::iter::once((&trace.own, true)).chain(trace.launched.iter().map(|process| (process, false)))
}

fn trace_text(out: &mut dyn Write, trace: &StepTrace<'_>) -> fmt::Result {
    // step
    out.write_str("  step   ")?;
    if trace.steps.entered.is_empty() {
        out.write_str("none of the test's own functions was entered")?;
    }
    for (at, name) in trace.steps.entered.iter().enumerate() {
        if at > 0 {
            out.write_str(" > ")?;
        }
        out.write_str(name)?;
        if trace.steps.inside == Some(at) {
            out.write_str(" (still inside)")?;
        }
    }
    out.write_char('\n')?;

    // waits
    let mut first = true;
    for (process, own) in processes(trace) {
        for thread in process.threads {
            if let Some(blocked) = thread.blocked {
                section(out, "waits", &mut first)?;
                thread_label(out, process, own, thread)?;
                write!(out, "{}(", blocked.call)?;
                on_text(out, blocked.on)?;
                out.write_str(") for ")?;
                time(out, blocked.since)?;
                out.write_char('\n')?;
            }
            for span in thread.spans {
                section(out, "waits", &mut first)?;
                thread_label(out, process, own, thread)?;
                write!(out, "span \"{}\"", span.what)?;
                match span.deadline {
                    Some(deadline) => {
                        out.write_str(" deadline ")?;
                        time(out, deadline)?;
                        if span.expired() {
                            out.write_str(", expired")?;
                        } else {
                            out.write_str(", open ")?;
                            time(out, span.open)?;
                        }
                    }
                    None => {
                        out.write_str(", open ")?;
                        time(out, span.open)?;
                    }
                }
                out.write_char('\n')?;
            }
        }
    }
    if first {
        out.write_str("  waits  no thread is in a blocking call or an open span\n")?;
    }

    // output
    let mut first = true;
    for (process, _) in processes(trace) {
        let lines = Lines::of(process.output);
        let skip = lines.count.saturating_sub(OUTPUT_LINES);
        for line in lines.skip(skip) {
            section(out, "output", &mut first)?;
            write!(out, "{}: \"", process.name)?;
            lossy(out, line)?;
            out.write_str("\"\n")?;
        }
    }

    // stack
    let mut first = true;
    for (process, own) in processes(trace) {
        for thread in process.threads {
            let mut width = 0;
            for (at, frame) in thread.frames.iter().enumerate() {
                section(out, "stack", &mut first)?;
                if at == 0 {
                    width = thread_label(out, process, own, thread)?;
                } else {
                    for _ in 0..width {
                        out.write_char(' ')?;
                    }
                }
                out.write_str(frame)?;
                out.write_char('\n')?;
            }
        }
    }
    Ok(())
}

/// What a blocking call waits on, in words (inside its call's parentheses).
fn on_text(out: &mut dyn Write, on: On<'_>) -> fmt::Result {
    match on {
        On::Pipe { fd, from: Some(from) } => write!(out, "fd {fd}: pipe from `{}` pid {}", from.name, from.pid),
        On::Pipe { fd, from: None } => write!(out, "fd {fd}: pipe"),
        On::Connect { fd, port } => write!(out, "fd {fd}: connect to port {port}"),
        On::Accept { fd, port } => write!(out, "fd {fd}: accept on port {port}"),
        On::Child { child } => write!(out, "child `{}` pid {}", child.name, child.pid),
        On::Lock { name } => write!(out, "lock \"{name}\""),
        On::Futex { address } => write!(out, "futex {address:#x}"),
        On::File { fd, path } => write!(out, "fd {fd}: file {path}"),
        On::Fd { fd } => write!(out, "fd {fd}"),
        On::Sleep => Ok(()),
    }
}

/// `60 s` when whole; `59.8 s`, tenths cut, from a second up; `250 ms` below.
fn time(out: &mut dyn Write, amount: Nanos) -> fmt::Result {
    let ns = amount.0;
    if ns >= 1_000_000_000 && ns % 1_000_000_000 == 0 {
        write!(out, "{} s", ns / 1_000_000_000)
    } else if ns >= 1_000_000_000 {
        let tenths = ns / 100_000_000;
        write!(out, "{}.{} s", tenths / 10, tenths % 10)
    } else {
        write!(out, "{} ms", ns / 1_000_000)
    }
}

/// The lines of an output tail: split at `\n`, a final empty line (after a trailing `\n`) not
/// counted, a trailing `\r` dropped from each. `count` is how many it yields.
struct Lines<'o> {
    rest: &'o [u8],
    count: usize,
}

impl<'o> Lines<'o> {
    fn of(tail: &'o [u8]) -> Lines<'o> {
        let body = tail.strip_suffix(b"\n").unwrap_or(tail);
        let count = match tail.is_empty() {
            true => 0,
            false => body.iter().filter(|byte| **byte == b'\n').count() + 1,
        };
        Lines { rest: body, count }
    }
}

impl<'o> Iterator for Lines<'o> {
    type Item = &'o [u8];

    fn next(&mut self) -> Option<&'o [u8]> {
        if self.count == 0 {
            return None;
        }
        self.count -= 1;
        let (line, rest) = match self.rest.iter().position(|byte| *byte == b'\n') {
            Some(at) => (&self.rest[..at], &self.rest[at + 1..]),
            None => (self.rest, &self.rest[self.rest.len()..]),
        };
        self.rest = rest;
        Some(line.strip_suffix(b"\r").unwrap_or(line))
    }
}

fn trace_json(out: &mut dyn Write, trace: &StepTrace<'_>) -> fmt::Result {
    out.write_str("{\"event\":\"steps\",\"test\":")?;
    hex(out, &trace.test.0)?;
    out.write_str(",\"path\":")?;
    json_str(out, trace.path)?;
    write!(out, ",\"deadline_ns\":{},\"steps\":[", trace.deadline.0)?;
    for (at, name) in trace.steps.entered.iter().enumerate() {
        if at > 0 {
            out.write_char(',')?;
        }
        json_str(out, name)?;
    }
    out.write_str("],\"inside\":")?;
    match trace.steps.inside {
        Some(at) => write!(out, "{at}")?,
        None => out.write_str("null")?,
    }
    out.write_str(",\"processes\":[")?;
    for (index, (process, own)) in processes(trace).enumerate() {
        if index > 0 {
            out.write_char(',')?;
        }
        out.write_str("{\"name\":")?;
        json_str(out, process.name)?;
        write!(out, ",\"pid\":{},\"own\":{own},\"threads\":[", process.pid)?;
        for (at, thread) in process.threads.iter().enumerate() {
            if at > 0 {
                out.write_char(',')?;
            }
            thread_json(out, thread)?;
        }
        out.write_str("],\"output\":[")?;
        let lines = Lines::of(process.output);
        let skip = lines.count.saturating_sub(OUTPUT_LINES);
        for (at, line) in lines.skip(skip).enumerate() {
            if at > 0 {
                out.write_char(',')?;
            }
            super::json_bytes(out, line)?;
        }
        out.write_str("]}")?;
    }
    out.write_str("]}\n")
}

fn thread_json(out: &mut dyn Write, thread: &Thread<'_>) -> fmt::Result {
    out.write_str("{\"name\":")?;
    json_str(out, thread.name)?;
    out.write_str(",\"blocked\":")?;
    match thread.blocked {
        None => out.write_str("null")?,
        Some(blocked) => {
            out.write_str("{\"call\":")?;
            json_str(out, blocked.call)?;
            out.write_str(",\"on\":")?;
            on_json(out, blocked.on)?;
            write!(out, ",\"for_ns\":{}}}", blocked.since.0)?;
        }
    }
    out.write_str(",\"spans\":[")?;
    for (at, span) in thread.spans.iter().enumerate() {
        if at > 0 {
            out.write_char(',')?;
        }
        out.write_str("{\"what\":")?;
        json_str(out, span.what)?;
        out.write_str(",\"deadline_ns\":")?;
        match span.deadline {
            Some(deadline) => write!(out, "{}", deadline.0)?,
            None => out.write_str("null")?,
        }
        write!(out, ",\"open_ns\":{},\"expired\":{}}}", span.open.0, span.expired())?;
    }
    out.write_str("],\"frames\":[")?;
    for (at, frame) in thread.frames.iter().enumerate() {
        if at > 0 {
            out.write_char(',')?;
        }
        json_str(out, frame)?;
    }
    out.write_str("]}")
}

fn on_json(out: &mut dyn Write, on: On<'_>) -> fmt::Result {
    match on {
        On::Pipe { fd, from } => {
            write!(out, "{{\"kind\":\"pipe\",\"fd\":{fd},\"from\":")?;
            match from {
                Some(from) => peer_json(out, from)?,
                None => out.write_str("null")?,
            }
            out.write_char('}')
        }
        On::Connect { fd, port } => write!(out, "{{\"kind\":\"connect\",\"fd\":{fd},\"port\":{port}}}"),
        On::Accept { fd, port } => write!(out, "{{\"kind\":\"accept\",\"fd\":{fd},\"port\":{port}}}"),
        On::Child { child } => {
            write!(out, "{{\"kind\":\"child\",\"pid\":{},\"name\":", child.pid)?;
            json_str(out, child.name)?;
            out.write_char('}')
        }
        On::Lock { name } => {
            out.write_str("{\"kind\":\"lock\",\"name\":")?;
            json_str(out, name)?;
            out.write_char('}')
        }
        On::Futex { address } => write!(out, "{{\"kind\":\"futex\",\"address\":{address}}}"),
        On::File { fd, path } => {
            write!(out, "{{\"kind\":\"file\",\"fd\":{fd},\"path\":")?;
            json_str(out, path)?;
            out.write_char('}')
        }
        On::Fd { fd } => write!(out, "{{\"kind\":\"fd\",\"fd\":{fd}}}"),
        On::Sleep => out.write_str("{\"kind\":\"sleep\"}"),
    }
}

fn peer_json(out: &mut dyn Write, peer: Peer<'_>) -> fmt::Result {
    out.write_str("{\"name\":")?;
    json_str(out, peer.name)?;
    write!(out, ",\"pid\":{}}}", peer.pid)
}
