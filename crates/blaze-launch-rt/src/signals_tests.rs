//! The launcher's fault handler, installed and chained (signals.rs). A fault ends or redirects
//! a process, so each case runs in a child: the parent re-runs this test binary with
//! `BLAZE_SIGNALS_CASE` naming the case and the filter naming [`child`], and reads the child's
//! exit and its stderr under a deadline.
//!
//! The rule the cases rest on, LIB std/src/sys/pal/unix/stack_overflow/handler_signal.rs
//! `init` (lines 117-145): "if action.sa_sigaction == SIG_DFL { .. sigaction(signal, &action,
//! ptr::null_mut()) }": the program's std installs its handler only when its `sigaction` reports
//! SIG_DFL, so the launcher's `sigaction` must report SIG_DFL to a program that has recorded
//! nothing, and must then run what the program recorded.
//!
//! Without signals.rs's piece nothing is installed: case `retired` prints no report and ends
//! by the signal, and case `handler` never reaches the program's handler.

use core::ffi::{c_int, c_void};
use core::ptr;
use std::io::Read;
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use blaze_launch::Slot;
use blaze_sys::posix::{io, signal as os, types::sighandler_t};

use super::{Path, ProgramAction, install, own, sigaction};
use crate::retired::{Retired, Table};

/// The environment variable naming the case a child runs.
const CASE: &str = "BLAZE_SIGNALS_CASE";
/// The full name of [`child`], as the test binary's filter takes it.
const CHILD: &str = "signals::tests::child";
/// How long a child may take (rule 18): each case is one fault.
const DEADLINE: Duration = Duration::from_secs(60);

/// The program's handler of cases `handler` and `retired`: ends the process with 42.
extern "C" fn exit_42(_sig: c_int, _info: *mut os::siginfo_t, _ctx: *mut c_void) {
    os::exit_now(42)
}

fn page() -> usize {
    crate::sys::page().expect("the page size")
}

/// A fresh page mapped with no access: a read of it faults (SIGSEGV on Linux, SIGBUS on macOS).
fn no_access_page() -> usize {
    // SAFETY: a fresh private anonymous mapping that nothing else names.
    let at = unsafe { io::mmap(None, page(), io::Prot::NONE, io::MapFlags::PRIVATE | io::MapFlags::ANONYMOUS, None, 0) }
        .expect("mmap of a PROT_NONE page");
    at.as_ptr() as usize
}

/// A page that was mapped and is not any more: a read of it is SIGSEGV on both systems.
fn unmapped_page() -> usize {
    let at = no_access_page();
    let mapping = ptr::NonNull::new(at as *mut u8).expect("the mapped page");
    // SAFETY: unmaps the page mapped just above, which nothing has read.
    unsafe { io::munmap(mapping, page()) }.expect("munmap");
    at
}

/// The launcher's code area for a child: a reserved page apart from every page a case faults on.
fn area() -> core::ops::Range<usize> {
    let at = no_access_page();
    at..at + page()
}

fn read(at: usize) -> u8 {
    // SAFETY: none: the read is meant to fault. A volatile read so it is not removed.
    unsafe { ptr::read_volatile(at as *const u8) }
}

/// Records `exit_42` as the program's action on both owned signals through the launcher's
/// `sigaction`, and checks each reported SIG_DFL before.
fn program_handler() {
    for sig in [os::SIGSEGV, os::SIGBUS] {
        // SAFETY: an all-zero `sigaction` is a valid value of the C struct.
        let mut h: ProgramAction = unsafe { core::mem::zeroed() };
        h.sa_sigaction = exit_42 as extern "C" fn(c_int, *mut os::siginfo_t, *mut c_void) as sighandler_t;
        h.sa_flags = os::SA_SIGINFO | os::SA_ONSTACK;
        // SAFETY: as above.
        let mut old: ProgramAction = unsafe { core::mem::zeroed() };
        old.sa_sigaction = 7;
        // SAFETY: two live locals with the target's C action layout.
        let answered = unsafe { sigaction(sig, ptr::from_ref(&h).cast(), ptr::from_mut(&mut old).cast()) };
        assert_eq!(answered, 0, "the launcher's sigaction({sig}) answered {answered}");
        assert_eq!(old.sa_sigaction, os::SIG_DFL, "the launcher's sigaction({sig}) reported {:#x}, not SIG_DFL", old.sa_sigaction);
    }
}

/// The case a child runs; in the parent's own run (no `BLAZE_SIGNALS_CASE`) it does nothing.
#[test]
fn child() {
    let Some(case) = std::env::var_os(CASE) else {
        return;
    };
    static TABLE: Table<4> = Table::new();
    match case.to_str() {
        Some("handler") => {
            // SAFETY: the child's first install; no other thread installs an action.
            unsafe { install(&TABLE, area()) }.expect("install");
            program_handler();
            read(no_access_page());
            panic!("the read of a PROT_NONE page returned");
        }
        Some("retired") => {
            let at = no_access_page();
            let mut appender = TABLE.appender().expect("the table's appender");
            appender.append(Retired { start: at, len: page(), function: Slot(7), version: 3 }).expect("append");
            // SAFETY: as above.
            unsafe { install(&TABLE, area()) }.expect("install");
            program_handler();
            read(at);
            panic!("the read of a retired page returned");
        }
        Some("default") => {
            // SAFETY: as above.
            unsafe { install(&TABLE, area()) }.expect("install");
            read(unmapped_page());
            panic!("the read of an unmapped page returned");
        }
        _ => panic!("{CASE} names no case: {case:?}"),
    }
}

/// Runs `case` in a child; its exit and its stderr.
fn run(case: &str) -> (ExitStatus, String) {
    let exe = std::env::current_exe().expect("this test binary's path");
    let mut child = Command::new(exe)
        .args([CHILD, "--exact", "--nocapture", "--test-threads=1"])
        .env(CASE, case)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the child starts");
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().expect("the child's status") {
            break status;
        }
        if started.elapsed() > DEADLINE {
            let _ = child.kill();
            panic!("case {case}: the child did not end within {DEADLINE:?} (rule 18)");
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let mut err = String::new();
    child.stderr.take().expect("the child's stderr").read_to_string(&mut err).expect("the child's stderr reads");
    (status, err)
}

/// The program's SIGSEGV handler runs behind the launcher's: after `install`, the launcher's
/// `sigaction` reports SIG_DFL (so the program's std would install), and a fault then runs the
/// program's handler, which ends the child with 42.
/// Prints `test signals::tests::the_programs_handler_runs_behind_the_launchers ... ok`.
#[test]
fn the_programs_handler_runs_behind_the_launchers() {
    let (status, err) = run("handler");
    assert_eq!(status.code(), Some(42), "the child ended {status:?}; its stderr:\n{err}");
}

/// A fault on an address in the retired table ends the child with fault.rs's report and status
/// 134, and the program's handler (exit 42) does NOT run.
/// Prints `test signals::tests::a_retired_address_reports_and_skips_the_program ... ok`.
#[test]
fn a_retired_address_reports_and_skips_the_program() {
    let (status, err) = run("retired");
    assert!(
        err.starts_with("stopped: a call reached retired code of function 7 version 3 at 0x"),
        "the child's stderr does not start with the retired-code report ({status:?}):\n{err}"
    );
    assert_eq!(status.code(), Some(134), "the child ended {status:?}; its stderr:\n{err}");
}

/// With no program handler, a fault on a program thread ends the child by SIGSEGV, as the
/// default would.
/// Prints `test signals::tests::with_no_program_handler_a_fault_ends_by_sigsegv ... ok`.
#[test]
fn with_no_program_handler_a_fault_ends_by_sigsegv() {
    let (status, err) = run("default");
    assert_eq!(status.signal(), Some(os::SIGSEGV), "the child ended {status:?}; its stderr:\n{err}");
}

/// The one own-symbol table: `pthread_create` is the launcher's only on its own path; the
/// signal functions are the launcher's on both.
/// Prints `test signals::tests::own_binds_pthread_create_only_on_the_launcher_path ... ok`.
#[test]
fn own_binds_pthread_create_only_on_the_launcher_path() {
    assert!(own("pthread_create", Path::Launcher).is_some());
    assert_eq!(own("pthread_create", Path::InProcess), None);
    let ours = sigaction as unsafe extern "C" fn(c_int, *const libc::sigaction, *mut libc::sigaction) -> c_int;
    for path in [Path::Launcher, Path::InProcess] {
        assert_eq!(own("sigaction", path), Some(ours as *const () as u64));
        assert!(own("signal", path).is_some());
        assert!(own("__dso_handle", path).is_some());
        assert_eq!(own("malloc", path), None);
    }
}
