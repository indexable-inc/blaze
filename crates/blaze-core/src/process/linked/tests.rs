//! A linked child with real processes: a socket on its descriptor 3, a group of its own, a
//! deadline that passes, and the whole group killed; and the planted bug that kills only the
//! leader, caught by the same oracle.

use std::{
    os::fd::{AsFd as _, AsRawFd as _, FromRawFd as _, OwnedFd},
    path::PathBuf,
    time::Duration,
};

use super::{Exit, Link, LinkPlant, LinkedChild, Waited, start_linked};
use crate::{
    Command, Sandbox,
    wait::{Clock as _, Deadline, Monotonic},
};

/// The child: ignores TERM (so only a KILL ends it), starts a `sleep 60` in its own group (no
/// job control in a non-interactive sh: a background job stays in the shell's group), reads a
/// line from descriptor 3, writes it back there, then waits for the sleep.
const SCRIPT: &str = "trap '' TERM; sleep 60 & read line <&3; echo \"$line\" >&3; wait";

fn command() -> Command {
    Command {
        program: PathBuf::from("/bin/sh"),
        args: vec!["-c".into(), SCRIPT.into()],
        env: vec![("PATH".into(), "/usr/bin:/bin".into())],
        dir: PathBuf::from("/"),
        stdin: Vec::new(),
        sandbox: Sandbox::Off,
        memory: None,
    }
}

/// A connected pair of Unix stream sockets, both closed on exec.
fn socket_pair() -> (OwnedFd, OwnedFd) {
    let mut ends = [0; 2];
    // SAFETY: two descriptors written by the call.
    let made = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, ends.as_mut_ptr()) };
    assert_eq!(made, 0, "socketpair: {}", std::io::Error::last_os_error());
    for end in ends {
        crate::net::set_cloexec(end).expect("FD_CLOEXEC on a socket end");
    }
    // SAFETY: the call made both; each owned once from here.
    unsafe { (OwnedFd::from_raw_fd(ends[0]), OwnedFd::from_raw_fd(ends[1])) }
}

/// `bytes` written whole to `fd`.
fn write_all(fd: &OwnedFd, bytes: &[u8]) {
    let mut at = 0;
    while at < bytes.len() {
        // SAFETY: a live slice and an open descriptor.
        let wrote = unsafe { libc::write(fd.as_raw_fd(), bytes[at..].as_ptr().cast(), bytes.len() - at) };
        assert!(wrote > 0, "write: {}", std::io::Error::last_os_error());
        at += wrote as usize;
    }
}

/// One line from `fd`, each read waited for by a 5 s deadline.
fn read_line(fd: &OwnedFd) -> Vec<u8> {
    let clock = Monotonic::new();
    let deadline = Deadline::after(clock.now(), Duration::from_secs(5));
    let mut line = Vec::new();
    while !line.ends_with(b"\n") {
        let mut fds = [libc::pollfd { fd: fd.as_raw_fd(), events: libc::POLLIN, revents: 0 }];
        match crate::wait::readable("the child's echo on its socket", &mut fds, Some(deadline), &clock) {
            Ok(crate::wait::Waited::Ready(_)) => {}
            other => panic!("no echo from the child by 5 s: {other:?}"),
        }
        let mut chunk = [0u8; 64];
        // SAFETY: a live buffer and an open descriptor.
        let got = unsafe { libc::read(fd.as_raw_fd(), chunk.as_mut_ptr().cast(), chunk.len()) };
        assert!(got > 0, "read: the socket ended or failed ({})", std::io::Error::last_os_error());
        line.extend_from_slice(&chunk[..got as usize]);
    }
    line
}

/// Whether no member of group `pgid` is left by `within`: `kill(-pgid, 0)` failing with
/// ESRCH. A grandchild killed with the group is reaped by init (launchd), not by this
/// process, so its zombie may stay a member a moment: looked at every 10 ms (TO VERIFY BY A
/// TEST: how long that takes on each system; 2 s is far above it).
fn group_gone(pgid: u32, within: Duration) -> bool {
    let clock = Monotonic::new();
    let deadline = Deadline::after(clock.now(), within);
    loop {
        // SAFETY: no pointers; signal 0 only asks whether the group has a member.
        let sent = unsafe { libc::kill(-(pgid as libc::pid_t), 0) };
        if sent != 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
            return true;
        }
        if deadline.passed(clock.now()) {
            return false;
        }
        let step = Deadline::after(clock.now(), Duration::from_millis(10));
        crate::wait::sleep_until("a killed group's members to be reaped", step, &clock)
            .expect("a 10 ms sleep");
    }
}

/// The child started with `ours`' peer on its descriptor 3, "ping" echoed, and the 300 ms
/// deadline passed with it still running (it waits for its `sleep 60`).
fn started_and_echoed() -> (LinkedChild, OwnedFd) {
    let (ours, theirs) = socket_pair();
    let inherit = [(theirs.as_fd(), 3u32)];
    let child = start_linked(&command(), &Link { inherit: &inherit, own_group: true }).expect("a linked start").child;
    drop(theirs);
    write_all(&ours, b"ping\n");
    assert_eq!(read_line(&ours), b"ping\n", "the child's echo on descriptor 3");
    let deadline = Deadline::after(child.clock().now(), Duration::from_millis(300));
    let Waited::Running(child) = child.wait_until(deadline).expect("a wait by its deadline") else {
        panic!("the child ended before its 300 ms deadline; it waits for its sleep 60");
    };
    (child, ours)
}

/// Prints "linked: fd 3 echoed; group of 2 killed at the deadline; plant KillLeaderOnly caught".
#[test]
fn linked_child_echoes_on_fd_3_and_its_group_is_killed() {
    // The real kill: the shell and its sleep both gone.
    let (child, _ours) = started_and_echoed();
    let pgid = child.pid();
    assert_eq!(child.kill_group().expect("the group killed"), Exit::Signaled(libc::SIGKILL), "the leader ended by SIGKILL");
    assert!(group_gone(pgid, Duration::from_secs(2)), "a member of group {pgid} is left after kill_group");

    // The plant: only the leader killed, so its sleep is left, and the oracle sees it.
    let (child, _ours) = started_and_echoed();
    let pgid = child.pid();
    let _leader = child.kill_with(LinkPlant::KillLeaderOnly).expect("the leader killed");
    let caught = !group_gone(pgid, Duration::from_millis(500));
    // What the plant left: killed here, so no `sleep 60` outlives the test.
    // SAFETY: no pointers; the group still has its sleep as a member, so the id is its.
    unsafe { libc::kill(-(pgid as libc::pid_t), libc::SIGKILL) };
    assert!(caught, "plant KillLeaderOnly was not caught: group {pgid} had no member left");
    println!("linked: fd 3 echoed; group of 2 killed at the deadline; plant KillLeaderOnly caught");
}

/// A sandboxed command is refused by name, never started outside its sandbox.
#[test]
fn a_sandbox_is_refused_by_name() {
    let mut command = command();
    command.sandbox = Sandbox::On { read: Vec::new(), write: Vec::new(), network: false, trace: false, services: Vec::new() };
    let refused = start_linked(&command, &Link { inherit: &[], own_group: true }).err().expect("a refusal");
    assert_eq!(refused.op, super::LinkOp::SandboxNotJoined);
}
