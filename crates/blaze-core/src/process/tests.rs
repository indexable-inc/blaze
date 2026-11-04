//! The list of children started and not reaped ([`super::unreaped`]): what a stall report names
//! when a driver's loop stops polling, as the daemon's did inside a maintain pass (lane
//! reaphang, 2026-11-03), and the one place a child leaves it, its reap.

use std::path::{Path, PathBuf};

use super::{State, Unreaped, Waiter, reap, start, unreaped, wait_exited};
use crate::{Command, Sandbox, Tag};

/// `/bin/sh -c script`, unsandboxed, with no input.
fn sh(script: &str) -> Box<Command> {
    Box::new(Command {
        program: PathBuf::from("/bin/sh"),
        args: vec!["-c".into(), script.into()],
        env: Vec::new(),
        dir: PathBuf::from("/"),
        stdin: Vec::new(),
        sandbox: Sandbox::Off,
        memory: None,
    })
}

/// `pid` as the list says it now. Other tests' children are listed too: only `pid` is looked at.
fn listed(pid: libc::pid_t) -> Option<Unreaped> {
    unreaped().into_iter().find(|it| it.pid == pid)
}

/// A child is listed from its start; once it exits and nothing has reaped it (the daemon's
/// build scripts while its loop was stuck in a turn), it is said exited, with the effect that
/// waits for it; its one reap takes it off the list. Remove the reap's `unlist` and it stays
/// listed after its reap.
#[test]
fn a_child_is_listed_until_its_one_reap_and_said_exited_while_unreaped() {
    let started = start(Path::new("/"), sh("exit 3"), None).unwrap();
    let pid = started.pid;
    started.finish.waited_by(Tag(0x5ca1_ab1e));
    // Exited, and left unreaped: a zombie, as a stuck loop leaves one.
    wait_exited(pid).unwrap();
    let it = listed(pid).expect("a child is listed until it is reaped");
    assert_eq!(
        (it.state, it.waiter, it.program.as_path()),
        (State::Exited, Waiter::Effect(Tag(0x5ca1_ab1e)), Path::new("/bin/sh")),
        "{it}"
    );
    let said = it.to_string();
    for named in [
        format!("pid {pid}"),
        "exited and not reaped".to_owned(),
        "0x5ca1ab1e".to_owned(),
        "/bin/sh".to_owned(),
    ] {
        assert!(said.contains(&named), "it says {named:?}: {said}");
    }
    let exit = started.finish.exited(pid, Vec::new(), Vec::new()).unwrap();
    assert_eq!((exit.code, exit.signal), (Some(3), None), "{exit:?}");
    assert_eq!(listed(pid), None, "reaped, it is no longer listed");
}

/// A child whose `Finish` is dropped without reaping it stays listed, said abandoned: a zombie
/// no one will reap is named, not forgotten. Reaping it takes it off.
#[test]
fn a_child_dropped_unreaped_stays_listed_as_abandoned() {
    let started = start(Path::new("/"), sh("exit 0"), None).unwrap();
    let pid = started.pid;
    drop(started.finish);
    wait_exited(pid).unwrap();
    let it = listed(pid).expect("still listed: nothing reaped it");
    assert_eq!((it.state, it.waiter), (State::Exited, Waiter::Abandoned), "{it}");
    assert!(it.to_string().contains("no one"), "{it}");
    reap(pid).unwrap();
    assert_eq!(listed(pid), None, "reaped, it is no longer listed");
}
