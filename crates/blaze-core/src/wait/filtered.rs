//! Test support: a test's case run again in a process of its own, under the part of the
//! build-script sandbox's seccomp filter a blaze inside it meets (`io_uring_setup` answered
//! `ENOSYS`: `blaze-cargo/src/sandbox/linux.rs`, `filter_program`), within a bound.
//!
//! The rule: every wait that reaches the real kernel is run once under the sandbox's filter
//! too, in a process marked as the sandbox launcher marks one, and a wait that does not end
//! there is a failed test naming its case, never a hung one. A new kernel wait that was only
//! ever tried outside the sandbox is how blaze hung inside a build script's `rustc` probe
//! (2026-11-10): the workers' wait ring was refused, each worker ended, and the main thread
//! parked with no bound.
//!
//! How a test binary uses it (this file's tests, and blaze-graph's `pool.rs`):
//!
//! 1. It lists its cases, each a name and a function from the driver the process chose to what
//!    it says when it passes.
//! 2. It calls [`take_over`] with them from an entry of its `.init_array`, so a process started
//!    with [`CASE`] set runs that case before libtest's `main`, as the process's one thread,
//!    under the filter, and ends.
//! 3. A test calls [`run`] with a case's name, whether the process is marked
//!    ([`super::SANDBOXED`]), and a bound, and matches on [`Ended`]. The control of each such
//!    test is the same case not marked: it waits on the ring, which the filter refuses, and
//!    has to fail naming `io_uring_setup`.
//!
//! macOS's sandbox is another mechanism (`sandbox_init_with_parameters`, a profile of paths and
//! Mach services, `blaze-cargo/src/sandbox/macos.rs`): it filters no system call by number and
//! macOS has one wait driver, so there is nothing of this to run there.

use std::{
    panic::{UnwindSafe, catch_unwind},
    time::Duration,
};

use super::{Clock as _, Deadline, Monotonic, Waited, Waits};

/// The variable that makes a test binary, run again, a process that runs the case it names
/// under the filter and ends.
pub const CASE: &str = "__BLAZE_TEST_CASE_UNDER_FILTER";

/// One case of a test binary: what runs in the process under the filter.
#[derive(Clone, Copy)] // clone: a name and a function pointer
pub struct Case {
    pub name: &'static str,
    /// Given the driver the process chose from its environment, as blaze's `main` chooses it;
    /// what it returns is said on the process's stdout. A panic fails the case.
    pub run: fn(Waits) -> String,
}

/// How a case run under the filter ended.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: a test's outcome
pub enum Ended {
    /// The case returned: what it and the process wrote.
    Passed { said: String },
    /// The case panicked, or the process ended otherwise than by its case's end.
    Failed { code: Option<i32>, said: String },
    /// The process had not ended within the bound and was killed: a wait of the case has no
    /// end under the filter.
    Hung { case: &'static str, within: Duration, said: String },
}

/// Installs, for this thread and those it starts, a seccomp filter that answers
/// `io_uring_setup` with `ENOSYS` and allows every other call.
///
/// # Panics
///
/// When the kernel refuses the filter.
pub fn refuse_io_uring() {
    let insn =
        |code: u32, jt: u8, jf: u8, k: u32| libc::sock_filter { code: code as u16, jt, jf, k };
    let (load, jump, ret) = (
        libc::BPF_LD | libc::BPF_W | libc::BPF_ABS,
        libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K,
        libc::BPF_RET | libc::BPF_K,
    );
    let mut filter = [
        // The call's number, which is `io_uring_setup`'s or not.
        insn(load, 0, 0, 0),
        insn(jump, 0, 1, libc::SYS_io_uring_setup as u32),
        insn(ret, 0, 0, libc::SECCOMP_RET_ERRNO | libc::ENOSYS as u32),
        insn(ret, 0, 0, libc::SECCOMP_RET_ALLOW),
    ];
    let program = libc::sock_fprog { len: filter.len() as u16, filter: filter.as_mut_ptr() };
    let (on, no): (libc::c_ulong, libc::c_ulong) = (1, 0);
    // SAFETY: `prctl` with integer arguments, each the `unsigned long` it reads; `seccomp`
    // reads `program` and the filter it points at, both live for the call, and keeps a copy
    // of its own.
    unsafe {
        assert_eq!(libc::prctl(libc::PR_SET_NO_NEW_PRIVS, on, no, no, no), 0, "no_new_privs");
        let installed = libc::syscall(
            libc::SYS_seccomp,
            libc::SECCOMP_SET_MODE_FILTER,
            0,
            std::ptr::from_ref(&program),
        );
        assert_eq!(installed, 0, "seccomp: {}", std::io::Error::last_os_error());
    }
}

/// What `io_uring_setup` answers this thread: the error's number.
///
/// # Panics
///
/// When the call makes a ring.
pub fn io_uring_setup_errno() -> Option<i32> {
    // SAFETY: a null `io_uring_params`, which a kernel that takes the call refuses (`EFAULT`)
    // before it makes anything.
    let made = unsafe {
        libc::syscall(libc::SYS_io_uring_setup, 8u32, std::ptr::null_mut::<libc::c_void>())
    };
    assert!(made < 0, "io_uring_setup with no parameters made a ring");
    std::io::Error::last_os_error().raw_os_error()
}

fn message(panic: &(dyn std::any::Any + Send)) -> &str {
    match (panic.downcast_ref::<&str>(), panic.downcast_ref::<String>()) {
        (Some(it), _) => *it,
        (_, Some(it)) => it.as_str(),
        _ => "a panic with no message",
    }
}

/// In a process started with [`CASE`] set: runs the case of `cases` it names under the filter,
/// with the driver chosen as blaze's `main` chooses it, and ends the process: 0 having said
/// what the case returned, 3 having said its panic, 4 for a name that is no case of `cases`.
/// In any other process it returns at once. Called from an entry of the test binary's
/// `.init_array`, before any other thread exists.
#[allow(
    clippy::disallowed_methods,
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "a test binary run again as another process: told so by a variable, speaking on its own output, and \
              ended here, before libtest's `main`"
)]
pub fn take_over(cases: &[Case]) {
    let Some(name) = std::env::var_os(CASE) else { return };
    let Some(case) = cases.iter().find(|it| *it.name == *name.to_string_lossy()) else {
        eprintln!(
            "failed: `{CASE}={}` names no case of this test binary (it has {})",
            name.to_string_lossy(),
            cases.iter().map(|it| it.name).collect::<Vec<_>>().join(", ") // alloc: an error, once
        );
        std::process::exit(4)
    };
    let code = match run_case(move || {
        refuse_io_uring();
        assert_eq!(io_uring_setup_errno(), Some(libc::ENOSYS), "the filter is not in force");
        let waits = super::choose_from_env().unwrap_or_else(|refused| panic!("{refused}"));
        (case.run)(waits)
    }) {
        Ok(said) => {
            println!("{said}");
            0
        }
        Err(why) => {
            eprintln!("failed: {why}");
            3
        }
    };
    std::process::exit(code)
}

fn run_case(case: impl FnOnce() -> String + UnwindSafe) -> Result<String, String> {
    // alloc: a failed case's message, once a process
    catch_unwind(case).map_err(|panic| message(&*panic).to_owned())
}

/// Runs this test binary again as a process that runs `case` under the filter
/// ([`take_over`]), marked as the sandbox marks what it launches or not, and says how it ended
/// within `within`. A process still running then is killed and is [`Ended::Hung`].
///
/// # Panics
///
/// When the process cannot be started or waited for.
#[allow(clippy::disallowed_methods, reason = "a test's child: this test binary, run again")]
pub fn run(case: &'static str, marked: bool, within: Duration) -> Ended {
    let exe = std::env::current_exe().unwrap_or_else(|why| panic!("this test binary's path: {why}"));
    let mut command = std::process::Command::new(exe);
    command
        .env(CASE, case)
        .env_remove(super::SANDBOXED)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    if marked {
        command.env(super::SANDBOXED, "1");
    }
    let mut child = command.spawn().unwrap_or_else(|why| panic!("starting the case `{case}`: {why}"));
    let clock = Monotonic::new();
    let deadline = Some(Deadline::after(clock.now(), within));
    let pid = libc::pid_t::try_from(child.id()).unwrap_or_else(|_| panic!("a pid past pid_t"));
    let what = "a test's case run again under a filter that refuses io_uring";
    let waited =
        super::exited(what, pid, deadline, &clock).unwrap_or_else(|why| panic!("`{case}`: {why}"));
    let hung = waited == Waited::Deadline;
    if hung {
        child.kill().unwrap_or_else(|why| panic!("killing the hung case `{case}`: {why}"));
    }
    let out = child.wait_with_output().unwrap_or_else(|why| panic!("reaping `{case}`: {why}"));
    // alloc: a test's outcome
    let said =
        format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    match (hung, out.status.code()) {
        (true, _) => Ended::Hung { case, within, said },
        (false, Some(0)) => Ended::Passed { said },
        (false, code) => Ended::Failed { code, said },
    }
}

#[cfg(test)]
mod tests {
    use std::{
        os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd},
        sync::Arc,
    };

    use super::*;
    use crate::wait::{Parker, exited, pollable, sleep_until};

    const LONG: Duration = Duration::from_secs(60);

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// Every wait `blaze_core::wait` gives its callers, made once against the real kernel on
    /// the driver the process chose: a sleep, a poll that times out and one that is ready, a
    /// child's exit, and a park another thread ends.
    #[allow(clippy::disallowed_methods, reason = "a test's child and a test's thread")]
    fn every_public_wait(waits: Waits) -> String {
        let clock = Monotonic::new();
        let soon = |clock: &Monotonic| Deadline::after(clock.now(), ms(20));
        let long = |clock: &Monotonic| Some(Deadline::after(clock.now(), LONG));

        sleep_until("a filtered test's sleep", soon(&clock), &clock).unwrap();

        let mut ends = [0; 2];
        // SAFETY: two descriptors for the kernel to write.
        assert_eq!(unsafe { libc::pipe2(ends.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
        // SAFETY: the pipe's two ends, which no one else holds.
        let (read, write) =
            unsafe { (OwnedFd::from_raw_fd(ends[0]), OwnedFd::from_raw_fd(ends[1])) };
        let what = "a filtered test's pipe";
        let polled = pollable(what, read.as_raw_fd(), libc::POLLIN, Some(soon(&clock)), &clock);
        assert_eq!(polled.unwrap(), Waited::Deadline, "an empty pipe polled readable");
        // SAFETY: one byte of a live local written to the pipe's own end.
        assert_eq!(unsafe { libc::write(write.as_raw_fd(), [1u8].as_ptr().cast(), 1) }, 1);
        match pollable(what, read.as_raw_fd(), libc::POLLIN, long(&clock), &clock).unwrap() {
            Waited::Ready(revents) => assert_ne!(revents & libc::POLLIN, 0),
            Waited::Deadline => panic!("a written pipe did not poll readable within 60 s"),
        }

        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "exit 7"])
            .stdin(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let pid = i32::try_from(child.id()).unwrap();
        let ended = exited("a filtered test's child", pid, long(&clock), &clock).unwrap();
        assert_eq!(ended, Waited::Ready(()), "a child that exits at once had not within 60 s");
        assert_eq!(child.wait().unwrap().code(), Some(7));

        let parker = Arc::new(Parker::new());
        let waker = {
            let parker = Arc::clone(&parker); // clone: a reference count, for the waking thread
            std::thread::Builder::new()
                .name("filtered-unpark".to_owned())
                .spawn(move || {
                    let clock = Monotonic::new();
                    let until = Deadline::after(clock.now(), ms(20));
                    sleep_until("a filtered test's pause before its unpark", until, &clock).unwrap();
                    parker.unpark();
                })
                .unwrap()
        };
        let parked = parker.park("a filtered test's park", long(&clock), &clock).unwrap();
        assert_eq!(parked, Waited::Ready(()), "a park was not ended by its unpark within 60 s");
        waker.join().unwrap();
        format!("every public wait ended on {waits:?}")
    }

    /// A wait with no end: a park nothing unparks, with no deadline.
    fn a_park_nothing_ends(_: Waits) -> String {
        let clock = Monotonic::new();
        Parker::new().park("a filtered test's park that nothing ends", None, &clock).unwrap();
        "a park with no unpark and no deadline ended".to_owned()
    }

    const CASES: &[Case] = &[
        Case { name: "every public wait", run: every_public_wait },
        Case { name: "a park nothing ends", run: a_park_nothing_ends },
    ];

    extern "C" fn take_over_this_binary() {
        take_over(CASES);
    }

    // SAFETY: the loader calls each entry of this section as a function as the binary loads,
    // before `main`, when the process is one thread. The entry is an `extern "C" fn()`: the
    // loader passes it `argc`, `argv` and `envp`, which the C calling convention lets it
    // ignore. Anything but a function pointer there would be called as code.
    #[used]
    #[unsafe(link_section = ".init_array")]
    static TAKE_OVER: extern "C" fn() = take_over_this_binary;

    /// The invariant (tested): in a process the sandbox launcher marked, under a filter that
    /// refuses io_uring as the sandbox's does, every wait `blaze_core::wait` gives ends, on
    /// the direct driver, and no ring is asked for.
    #[test]
    fn every_public_wait_ends_in_a_sandboxed_process() {
        match run("every public wait", true, LONG) {
            Ended::Passed { said } => {
                assert!(said.contains("every public wait ended on Direct"), "{said}");
                assert!(!said.contains("io_uring"), "it asked for a ring: {said}");
            }
            other => panic!("{other:?}"),
        }
    }

    /// The control (the cause, put back): the same process not marked waits on its ring,
    /// which the filter refuses, and fails naming `io_uring_setup`. It ends; a ring that
    /// worked under the filter would mean the test above proves nothing.
    #[test]
    fn the_oracle_catches_a_sandboxed_process_waiting_on_the_ring() {
        match run("every public wait", false, LONG) {
            Ended::Failed { code, said } => {
                assert_eq!(code, Some(3), "{said}");
                assert!(said.contains("io_uring_setup"), "the failure does not name the call: {said}");
            }
            other => panic!("{other:?}"),
        }
    }

    /// A wait with no end under the filter is an outcome naming its case, within the bound:
    /// the helper never hangs the test that uses it.
    #[test]
    fn a_wait_with_no_end_is_said_hung_within_the_bound() {
        let clock = Monotonic::new();
        let ended = run("a park nothing ends", true, Duration::from_secs(2));
        assert!(clock.now() < LONG, "the helper waited {:?} past its 2 s", clock.now());
        match ended {
            Ended::Hung { case, within, .. } => {
                assert_eq!((case, within), ("a park nothing ends", Duration::from_secs(2)));
            }
            other => panic!("{other:?}"),
        }
    }

    /// A name that is no case is refused naming the cases, not run as libtest.
    #[test]
    fn a_name_that_is_no_case_is_refused() {
        match run("no such case", true, LONG) {
            Ended::Failed { code, said } => {
                assert_eq!(code, Some(4), "{said}");
                assert!(said.contains("every public wait"), "{said}");
            }
            other => panic!("{other:?}"),
        }
    }
}
