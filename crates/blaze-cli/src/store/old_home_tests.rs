//! The old home (rule 26) and the no-home refusal, on bstd's simulated world with the store on
//! the world's lent driver, as the command opens it: the stand-in's folders are reported by
//! name, once in the home's life and once in each checkout's, and never read, migrated or
//! deleted; a command with no `HOME` stops with the typed refusal naming it.
//!
//! A CHANGE OF BEHAVIOUR (D114, 2026-11-16): before the one store, a command with no `HOME` ran
//! with the machine store off and kept every record in the checkout's own store
//! (`.blaze/v2/packs`). Now there is no checkout store: the one store is under the home, so no
//! `HOME` is `StoreFailure::NoHome`, and the command stops.

use blaze_cas::plain::Settings;
use bstd::fs::{Content, Durable};
use bstd::io::{Io, Limits, SimWorld};
use bstd::mem::TASK_FRAME;
use bstd::path::Path;
use bstd::test::BUDGET;

use super::plain_place::after_answer;
use super::{Facts, Home, StoreFailure, old_home};
use crate::CliError;

const HOME: &str = "/h";

fn write(io: &Io, path: &str) {
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    io.need_fs().expect("files").root().publish(&cx, &Path::new(path).expect("a path"), Content::bytes(b"an old pack"), Durable::No).expect("write");
}

fn stderr(io: &Io) -> std::string::String {
    let err = io.script(|host| host.err.clone()).expect("a simulated host");
    std::string::String::from_utf8(err).expect("UTF-8")
}

/// One command over the checkout `one`: what the old-home look found.
fn command(io: &Io, nonce: u64) -> u32 {
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let fs = io.need_fs().expect("files");
    let err = io.need_err().expect("err");
    let home = Home::of(&cx, &fs, Some(HOME)).expect("the home");
    let root = fs.open_dir(&cx, &Path::new("one").expect("a path")).expect("the checkout");
    let lent = home.open_store(&Settings::default(), Facts { boot: Some(1), nonce }, blaze_own::testing::heap().alloc()).expect("the store opens");
    let found = old_home(&cx, &home, &root, "/one", &lent, &err).expect("the look");
    after_answer(&cx, &lent, 0).expect("the commit after the answer");
    found
}

/// The home's `v2/own`, the checkout's `.blaze/v2/packs` and its `.blaze/v2/at` pointer file are
/// each said once by name; the next command says nothing (the look is a root); all three are
/// still there, untouched.
#[test]
fn the_old_home_is_reported_by_name_once_and_never_touched() {
    let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
    let old = ["h/.blaze/v2/own/0123456789abcdef", "one/.blaze/v2/packs/fedcba9876543210", "one/.blaze/v2/at"];
    for path in old {
        write(&io, path);
    }
    write(&io, "one/Cargo.toml");
    assert_eq!(command(&io, 1), 3, "the first command's look");
    let said = stderr(&io);
    for name in ["~/.blaze/v2/own", ".blaze/v2/packs", ".blaze/v2/at"] {
        assert!(said.contains(name), "{name} not said by name: {said}");
    }
    assert!(!said.contains("v2/shared"), "a folder not there was said: {said}");
    assert_eq!(command(&io, 2), 0, "the second command looked again");
    assert_eq!(stderr(&io), said, "the second command said more");
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let fs = io.need_fs().expect("files");
    for path in old {
        assert!(fs.root().stat(&cx, &Path::new(path).expect("a path")).expect("a stat").is_some(), "{path} was touched");
    }
    println!("old home: 3 old forms said by name once, the next command said nothing, all kept");
}

/// No `HOME`: the typed refusal, naming it.
#[test]
fn no_home_is_a_typed_refusal() {
    let io = Io::simulated(SimWorld::new(12, Limits::USUAL));
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let fs = io.need_fs().expect("files");
    match Home::of(&cx, &fs, None) {
        Err(CliError::Store(StoreFailure::NoHome)) => {}
        Err(other) => panic!("no home gave another error: {other}"),
        Ok(_) => panic!("no home opened a store"),
    }
    println!("no home: refused by name");
}
