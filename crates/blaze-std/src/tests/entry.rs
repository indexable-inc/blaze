//! The entry on the simulation ([`crate::entry::run_in`]): a program's `main` run in a
//! simulated world with no process, which is how a program of the blaze edition is tested
//! (docs/v2/plan/design/std.md, "The blaze edition: opting in"). The process's side
//! ([`crate::entry::run`]: the native world, standard error, the exit status) needs a real
//! process and is crates/blaze-engine/tests/repro.rs's.

use crate::{
    cx::Cx,
    entry::{Error, Outcome, Status, run_in},
    io::{Io, Offers},
    out::{OutError, OutWhy},
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
    assert!(crate::host::OFFERS.out, "a native world offers standard output");
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
