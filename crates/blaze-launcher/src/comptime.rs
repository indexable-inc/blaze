//! A compile-time program's side of the launcher (D175, docs/v2/plan/design/comptime-native.md):
//! the `comptime:` entry form. The process is a compile-time child: its code records run
//! natively as any program's do (run_mode, the parent's socket on descriptor 3), and every C
//! name they bind goes through blaze-comptime-sys's table ([`blaze_launch_rt::run_mode::Table`]),
//! whose functions answer from the layer installed here, speaking blaze-comptime's protocol to
//! the compile-time host on descriptor [`FD`] (4).
//!
//! The order: [`install`] (the layer, its `Hello`, the arguments the host sends), then the
//! run with [`Handed::Comptime`](blaze_launch_rt::run_mode::Handed) of [`argv`] and
//! [`End::Return`](blaze_launch_rt::run_mode::End) (G5: the run returns the entry's status),
//! then [`exit`]: a C `main`'s return is the layer's `exit`, which sends `Exit` and ends the
//! process through [`Pipe`]'s `end`. The launcher owns the process from first to last.
//!
//! [`real`] (the real C library's functions the layer needs) is blaze-sys's
//! `posix::io::real()` on macOS. LEFT, named where it stops: on Linux it is refused, as
//! blaze-sys has no `dlsym` there and blaze-comptime-sys's `Real::dlsym` is required.

use core::ffi::{c_char, c_int};
use std::io::Write;

use blaze_comptime::wire::split_frame;
use blaze_comptime::{Nanos, WireLimits};
use blaze_comptime_sys::child::{self, Binding, ChildFault, ChildOpts, InstallError, Real, Transport, TransportError};
use blaze_comptime_sys::{Limits, Os};
use blaze_own::{AVec, Alloc, Vec};
use blaze_sys::posix::io::{ClockId, PollEvents, PollFd, clock_gettime, close_on_exec, poll, read, write};
use blaze_sys::posix::signal::exit_now;
use blaze_sys::{Errno, Fd, Timespec};

/// The compile-time host's descriptor: the parent opens it beside the run's socket (3).
pub const FD: Fd = Fd::new(4);

/// The longest the child waits for one frame of the host (rule 18): an answer to a call the
/// host serves from its own state or a file it reads. A host that has not answered in this long
/// is a hang, and the child ends with a fault naming it.
const ANSWER_WITHIN_NS: u64 = 120 * 1_000_000_000;

/// The bytes one `read` takes off the descriptor.
const CHUNK: usize = 16 << 10;

/// Why the compile-time child could not start: each names what is missing.
#[derive(Debug)]
pub enum ComptimeError {
    /// Descriptor 4 is not open: the child was not started by a compile-time host.
    NoHost,
    /// The system's page size did not read (blaze-sys facts.rs).
    Page { why: blaze_sys::facts::AuxError },
    /// The real C library's functions the layer needs are not reachable without a new
    /// `libc::` use (the launcher's port to blaze-sys): the blaze-sys items named.
    NoReal { missing: &'static str },
    Install(InstallError),
    Args(ChildFault),
    /// The table has no `exit` of the layer: a bug of blaze-comptime-sys (every row has a
    /// function, its test says).
    NoExit,
}

impl core::fmt::Display for ComptimeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ComptimeError::NoHost => {
                f.write_str("BLAZE_LAUNCH_ENTRY names a compile-time entry and descriptor 4 (the compile-time host) is not open")
            }
            ComptimeError::Page { why } => write!(f, "the system's page size did not read (blaze-sys facts.rs `Aux`): {why:?}"),
            ComptimeError::NoReal { missing } => write!(
                f,
                "a compile-time program needs the real C library's functions as values, and blaze-sys does not give them yet: {missing}"
            ),
            ComptimeError::Install(why) => write!(f, "the compile-time layer did not install (blaze-comptime-sys child::install): {why:?}"),
            ComptimeError::Args(why) => write!(f, "the compile-time host's arguments did not arrive (child::argv): {why:?}"),
            ComptimeError::NoExit => f.write_str("blaze-comptime-sys's table binds no `exit` of the layer (child::binding)"),
        }
    }
}

/// The system the compile-time code was built for: this process's, as the layer answers it.
pub const OS: Os = if cfg!(target_os = "macos") { Os::Mac } else { Os::Linux };

/// The transport over [`FD`]: whole frames out, whole frames in (a u32 big-endian length and
/// the body, blaze-comptime wire.rs), each wait to a deadline.
pub struct Pipe {
    fd: Fd,
    /// Bytes read and not yet a whole frame.
    pending: AVec<'static, u8>,
    chunk: [u8; CHUNK],
}

/// Monotonic nanoseconds, or the errno of the clock.
fn mono() -> Result<u64, Errno> {
    let now = clock_gettime(ClockId::Monotonic)?;
    Ok(now.sec as u64 * 1_000_000_000 + now.nsec as u64)
}

fn io(errno: Errno) -> TransportError {
    TransportError::Io { errno: i32::from(errno.raw()) }
}

impl Transport for Pipe {
    fn send(&mut self, frame: &[u8]) -> Result<(), TransportError> {
        let mut left = frame;
        while !left.is_empty() {
            match write(self.fd, left) {
                Ok(0) => return Err(TransportError::Closed),
                Ok(wrote) => left = &left[wrote..],
                // A signal before any byte moved: the call is not done, not failed (not a retry).
                Err(Errno::Intr) => {}
                Err(errno) => return Err(io(errno)),
            }
        }
        Ok(())
    }

    fn receive(&mut self, into: &mut AVec<'static, u8>) -> Result<(), TransportError> {
        into.clear();
        let deadline = mono().map_err(io)? + ANSWER_WITHIN_NS;
        loop {
            match split_frame(&self.pending, WireLimits::SHIPPED) {
                Err(_) => return Err(TransportError::Frame),
                Ok(Some((body, took))) => {
                    into.extend_from_slice(body);
                    self.pending.drain(..took);
                    return Ok(());
                }
                Ok(None) => {}
            }
            // The wait "compile-time child: the host's next frame", to the deadline.
            let now = mono().map_err(io)?;
            let Some(left) = deadline.checked_sub(now).filter(|&left| left > 0) else {
                return Err(io(Errno::TimedOut));
            };
            let left = Timespec { sec: (left / 1_000_000_000) as i64, nsec: (left % 1_000_000_000) as i64 };
            let mut fds = [PollFd::new(self.fd, PollEvents::IN)];
            match poll(&mut fds, Some(left)) {
                // The time ran out or a signal came: the loop checks the deadline again.
                Ok(0) | Err(Errno::Intr) => continue,
                Ok(_) => {}
                Err(errno) => return Err(io(errno)),
            }
            match read(self.fd, &mut self.chunk) {
                Ok(0) => return Err(TransportError::Closed),
                Ok(got) => self.pending.extend_from_slice(&self.chunk[..got]),
                Err(Errno::Intr) => {}
                Err(errno) => return Err(io(errno)),
            }
        }
    }

    fn now(&mut self) -> Nanos {
        match mono() {
            Ok(now) => Nanos(now),
            Err(errno) => self.fault(ChildFault::Transport(io(errno))),
        }
    }

    fn end(&mut self, code: c_int) -> ! {
        // The layer's `exit` sent `Exit` and the host has it: nothing of this process is owed.
        exit_now(code)
    }

    fn fault(&mut self, fault: ChildFault) -> ! {
        // The process ends below whether or not its last words could be written.
        let _ = writeln!(std::io::stderr().lock(), "blaze: the compile-time program stopped: {fault:?}");
        exit_now(blaze_launch_rt::run_mode::STOPPED)
    }
}

/// The real process's functions the layer itself needs ([`Real`]): `errno`'s cell, `mmap`,
/// `mprotect`, `dlsym` and `RTLD_DEFAULT`. macOS: libSystem's, as values (blaze-sys
/// posix/io/real.rs).
#[cfg(target_os = "macos")]
fn real() -> Result<Real, ComptimeError> {
    let it = blaze_sys::posix::io::real();
    Ok(Real { errno: it.errno, mmap: it.mmap, mprotect: it.mprotect, dlsym: it.dlsym, default_handle: it.default_handle })
}

/// Linux: refused by name. blaze-sys references no C-library symbol there (posix/io/real.rs),
/// so there is no `dlsym` to hand, and blaze-comptime-sys's [`Real`] requires one (for a row the
/// table answers `Own`). The errno cell and `mmap`/`mprotect` could be the layer's own over
/// `blaze_sys::linux::{mmap, mprotect}`, but a `Real` without `dlsym` cannot be made, so building
/// them here would be a half that runs nothing.
#[cfg(target_os = "linux")]
fn real() -> Result<Real, ComptimeError> {
    Err(ComptimeError::NoReal {
        missing: "on Linux blaze-sys offers no `dlsym` (no C-library symbol, posix/io/real.rs), and \
                  blaze-comptime-sys child.rs `Real::dlsym` is required: the layer must own its errno cell and \
                  `mmap`/`mprotect` wrappers over blaze_sys::linux and take `Own` rows' addresses from the launcher's binder",
    })
}

/// The system's page size, which the layer's `sysconf(_SC_PAGESIZE)` answers: macOS's
/// `sysconf`, Linux's auxiliary vector (blaze-sys facts.rs).
fn page() -> Result<u64, ComptimeError> {
    #[cfg(target_os = "macos")]
    let aux = blaze_sys::facts::Aux::from_system();
    #[cfg(target_os = "linux")]
    let aux = blaze_sys::facts::Aux::from_proc();
    aux.map(|aux| aux.page_size() as u64).map_err(|why| ComptimeError::Page { why })
}

/// Installs the layer on [`FD`]: the host's `Hello`, the working directory and the arguments
/// cross now. `alloc` is the process's, for the layer's own state (never the code's `malloc`,
/// never `Global`).
pub fn install(alloc: Alloc<'static>) -> Result<(), ComptimeError> {
    // Whether descriptor 4 is open (`fcntl(F_GETFD)`); changes nothing.
    if close_on_exec(FD).is_err() {
        return Err(ComptimeError::NoHost);
    }
    let page = page()?;
    let real = real()?;
    let pipe = Pipe { fd: FD, pending: Vec::new_in(alloc), chunk: [0; CHUNK] };
    // The layer keeps its transport for the process's life: a box in `alloc`, never freed.
    let transport: &'static mut dyn Transport = blaze_own::Box::leak_static(blaze_own::Box::new_in(pipe, alloc));
    let opts = ChildOpts { os: OS, limits: Limits { page, .. }, alloc, transport, real };
    child::install(opts).map_err(ComptimeError::Install)
}

/// The arguments the host sent, for the entry.
pub fn argv() -> Result<(c_int, *const *const c_char), ComptimeError> {
    child::argv().map_err(ComptimeError::Args)
}

/// Ends the process as a C `main`'s return does: the layer's `exit` with `status`, which sends
/// `Exit` and ends through [`Pipe`]'s `end`. Returns only when the table has no such function.
pub fn exit(status: c_int) -> ComptimeError {
    let Ok(Binding::Here(at)) = child::binding("exit", OS) else {
        return ComptimeError::NoExit;
    };
    // SAFETY: `at` is the layer's `exit` (blaze-comptime-sys child.rs `c_exit`), an
    // `unsafe extern "C" fn(c_int) -> !` by the table's row, installed above; it ends the
    // process on every path.
    let exit: unsafe extern "C" fn(c_int) -> ! = unsafe { core::mem::transmute(at) };
    // SAFETY: called on the program's one thread after its entry returned, as C's `exit` is.
    unsafe { exit(status) }
}
