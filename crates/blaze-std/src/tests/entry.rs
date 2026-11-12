//! The entry on the simulation ([`crate::entry::run_in`]): a program's `main` run in a
//! simulated world with no process, which is how a program of the blaze edition is tested
//! (docs/v2/plan/design/std.md, "The blaze edition: opting in"). The process's side
//! ([`crate::entry::run`]: the native world, standard error, the exit status) needs a real
//! process and is crates/blaze-engine/tests/repro.rs's.

use crate::{
    cx::Cx,
    entry::{Error, Outcome, Status, run_in},
    io::{Io, Offers},
    out::{HELD, OutError, OutWhy, Stream},
};

/// What the simulated world's standard output holds.
fn written(io: &Io) -> String {
    String::from_utf8(io.script(|host| host.out.clone()).expect("a simulated world")).expect("UTF-8")
}

fn hello(cx: &Cx, io: Io) -> Result<(), Error> {
    io.need_out()?.line(cx, "Hello, world!")?;
    Ok(())
}

/// The invariant: a `main` that writes a line and returns `Ok` leaves exactly that line on
/// its world's standard output, says nothing, and ends `Status::OK`.
#[test]
fn a_main_writes_its_line_through_out_and_ends_ok() {
    let io = Io::sim(1);
    let mut said = String::new();
    let status = run_in(io.clone(), &mut said, hello);
    assert_eq!((status, said.as_str(), written(&io).as_str()), (Status::OK, "", "Hello, world!\n"));
}

/// A `main` that returns `Err` ends `Status::FAILED` and the entry is handed what the error
/// says: here the capability the host withheld, named.
#[test]
fn a_main_that_fails_ends_failed_and_says_its_error() {
    let io = Io::sim_with(2, Offers { out: false, ..Offers::ALL });
    let mut said = String::new();
    let status = run_in(io.clone(), &mut said, hello);
    assert_eq!(status, Status::FAILED);
    assert!(said.contains("`out`"), "the error names the missing capability: {said}");
    assert_eq!(written(&io), "", "nothing was written");
}

/// Without `Out` a program cannot write: the only way to standard output is the capability,
/// and a world that withholds it has `None` at the root. (The control for the test above: a
/// world that offers it gives one.)
#[test]
fn out_is_had_only_where_the_host_offers_it() {
    assert!(Io::sim_with(3, Offers::NONE).out().is_none());
    assert!(Io::sim_with(3, Offers { out: true, ..Offers::NONE }).out().is_some());
    assert!(crate::host::OFFERS.out && crate::host::OFFERS.err, "a native world offers both streams");
    let missing = Io::sim_with(3, Offers::NONE).need_out().expect_err("no out");
    assert_eq!(missing.capability, "out");
    assert!(missing.to_string().contains("`out`"), "{missing}");
    assert_eq!(Io::sim_with(3, Offers::NONE).need_err().expect_err("no err").capability, "err");
}

/// Each shape of `main` the entry rule allows is one closure over `run_in`'s two roots, as
/// the appended module writes it, and each `Outcome` gives its status.
#[test]
fn every_entry_shape_and_outcome_runs() {
    fn none() {}
    fn io_only(io: Io) -> Status {
        match io.out() {
            Some(_) => Status(7),
            None => Status::FAILED,
        }
    }
    fn cx_only(cx: &Cx) -> Result<(), crate::Stopped> {
        cx.check()
    }
    let mut said = String::new();
    assert_eq!(run_in(Io::sim(4), &mut said, |_cx, _io| none()), Status::OK);
    assert_eq!(run_in(Io::sim(4), &mut said, |_cx, io| io_only(io)), Status(7));
    assert_eq!(run_in(Io::sim(4), &mut said, |cx, _io| cx_only(cx)), Status::OK);
    assert_eq!(said, "");
    let nested: Result<Result<(), Error>, Error> = Ok(Err(Error::new("inner")));
    assert_eq!(nested.status(&mut said), Status::FAILED);
    assert_eq!(said, "inner");
}

/// A write under a context that stopped writes nothing and says why.
#[test]
fn a_write_under_a_stopped_context_writes_nothing() {
    let io = Io::sim(5);
    let cx = io.cx(core::time::Duration::ZERO, crate::mem::TASK_FRAME).expect("a root frame");
    let out = io.out().expect("Io::sim offers out");
    let refused: OutError = out.line(&cx, "late").expect_err("the deadline passed");
    assert!(matches!(refused.why, OutWhy::Stopped(crate::Stopped::Deadline { .. })), "{refused}");
    assert_eq!(written(&io), "");
}

/// What the simulated world's standard error holds.
fn written_err(io: &Io) -> String {
    String::from_utf8(io.script(|host| host.err.clone()).expect("a simulated world")).expect("UTF-8")
}

/// The invariant: what `line` and `write` put on a stream is exactly what `format!` of the
/// same values gives, for text, for formatted values, and for text longer than what one call
/// holds (which is handed over in pieces and loses nothing).
#[test]
fn line_and_write_say_what_format_gives() {
    let io = Io::sim(6);
    let cx = super::root(&io);
    let out = io.out().expect("Io::sim offers out");
    let (x, name) = (42u64, "Ada");
    out.line(&cx, "plain").unwrap();
    out.line(&cx, format_args!("x = {x}, name = {name:?}")).unwrap();
    out.write(&cx, format_args!("{x:>5}|")).unwrap();
    out.write(&cx, 7.5f64).unwrap();
    out.write_all(&cx, b"\n").unwrap();
    let long = "abcdefghij".repeat(HELD / 4);
    assert!(long.len() > 2 * HELD);
    out.line(&cx, format_args!("<{long}>")).unwrap();
    let expected = format!("plain\nx = {x}, name = {name:?}\n{x:>5}|7.5\n<{long}>\n");
    assert_eq!(written(&io), expected);
    assert_eq!(written_err(&io), "", "nothing went to standard error");
}

/// Standard error is a capability of its own: what is written through `io.err()` is on
/// standard error and not on standard output, and its errors name their stream.
#[test]
fn err_is_its_own_stream() {
    let io = Io::sim(7);
    let cx = super::root(&io);
    let err = io.err().expect("Io::sim offers err");
    assert_eq!(err.stream(), Stream::Err);
    err.line(&cx, format_args!("warning: {} left", 3)).unwrap();
    assert_eq!((written(&io).as_str(), written_err(&io).as_str()), ("", "warning: 3 left\n"));
    let stopped = io.cx(core::time::Duration::ZERO, crate::mem::TASK_FRAME).expect("a root frame");
    let refused = err.line(&stopped, "late").expect_err("the deadline passed");
    assert_eq!(refused.stream, Stream::Err);
    assert!(refused.to_string().contains("standard error"), "{refused}");
}

/// `write!` and `writeln!` work on `out.with(cx)`, and where a write fails the writer has
/// why, which `fmt::Error` cannot say; after a failure it writes nothing more.
#[test]
fn the_writer_takes_write_macros_and_keeps_the_first_error() {
    use core::fmt::Write as _;
    let io = Io::sim(8);
    let cx = super::root(&io);
    let out = io.out().expect("Io::sim offers out");
    let mut to = out.with(&cx);
    write!(to, "a{}", 1).unwrap();
    writeln!(to, " b{}", 2).unwrap();
    assert!(to.failed().is_none());
    to.finish().unwrap();
    assert_eq!(written(&io), "a1 b2\n");
    let stopped = io.cx(core::time::Duration::ZERO, crate::mem::TASK_FRAME).expect("a root frame");
    let mut late = out.with(&stopped);
    assert!(writeln!(late, "late {}", 1).is_err());
    let why = late.finish().expect_err("the deadline passed");
    assert!(matches!(why.why, OutWhy::Stopped(crate::Stopped::Deadline { .. })), "{why}");
    assert_eq!(written(&io), "a1 b2\n", "nothing of the late write is there");
}

/// A value whose `Display` fails is an error that says so, not a silent short line.
#[test]
fn a_display_that_fails_is_said() {
    struct Broken;
    impl core::fmt::Display for Broken {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            f.write_str("half")?;
            Err(core::fmt::Error)
        }
    }
    let io = Io::sim(9);
    let cx = super::root(&io);
    let refused = io.out().expect("out").line(&cx, Broken).expect_err("Display failed");
    assert_eq!(refused.why, OutWhy::Format);
}
