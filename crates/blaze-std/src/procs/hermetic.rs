//! One way to run a program hermetically (D177): a [`HermeticSpec`] says everything the child
//! is given, a [`Hermetic`] (made from the [`Spawner`] `main` was granted, never ambient) runs
//! it, and a [`HermeticRun`] says everything that came back: its [`Exit`], the output folder's
//! listing with digests, its streams, and what it read as a read list checked against its roots
//! ([`Reads`]). Every refusal is a [`HermeticError`] naming the program and the rule.
//!
//! The levels ([`Level`]):
//! - [`Level::Inputs`]: the OS confines the child to its roots (macOS Seatbelt, Linux Landlock
//!   with a seccomp filter and no_new_privs: `seatbelt.rs`, `landlock.rs`), and its reads are
//!   reported. What a confined child can still observe is not an input it is keyed by: the
//!   host's clock (unless the program honours `SOURCE_DATE_EPOCH`), metadata timing (mtimes it
//!   stats, how long a call took), the CPU's model and features (`sysctl`, `cpuid`,
//!   `/proc/cpuinfo` where readable), `uname`, its pid, the number of cores, and randomness.
//!   A program whose output depends on one of these is not determined at this level.
//! - [`Level::Execution`]: the run itself is determined (time, randomness, scheduling and every
//!   host answer come from the spec). No backend gives it today (`execution.rs`): asking for it
//!   is [`Fault::LevelUnavailable`] naming the level, the backend and why (rule 9).
//!
//! How the pieces sit:
//! - `plan.rs`: the one statement of what the child may touch, built from the spec (sans-IO).
//!   Both OS backends render it (an SBPL profile, a Landlock ruleset), and the simulation's OS
//!   enforces it, so the proof and the shipped confinement read one list.
//! - `machine.rs`: the run as a sans-IO machine (prepared, started, running, collecting, ended)
//!   over the events a [`Port`] reports; the deadline, the budget, the stream caps, the read
//!   report and the output listing are decided there and nowhere else.
//! - [`drive`]: the thin loop that hands the machine's effects to a [`Port`] (the native one
//!   in a backend, the simulated one in `sim.rs`) and to the caller's [`RootFiles`].
//! - `report.rs`: the read report: a program's own ([`ReportParser`]; the C driver's depfile
//!   parser implements it) or, without one, every file of every package root ([`Reads`]).
//!
//! This module SUBSUMED blaze-proc-macro's native_sandbox.rs (deleted, D177): a native build
//! script is a [`HermeticSpec`] at [`Level::Inputs`] with [`ReportSpec::WholeRoots`], its read
//! folders tree roots, `OUT_DIR` the output root, its jobserver fifo and fact folder
//! [`HermeticOpts::channels`], run by [`Hermetic::run`] (blaze-proc-macro native_script.rs).

use core::time::Duration;

use blaze_core::wait::Deadline;
use blaze_own::{AVec, Alloc};
use blaze_roots::{Checked, Digest, Full, Mount, Mounts, Outside, RelPath, RootId, Roots, Seen, Unmounted};

use super::{Exit, Spawner};
use crate::cx::Cx;

mod execution;
mod machine;
mod plan;
mod report;
#[cfg(any(test, feature = "testing"))]
mod sim;

#[cfg(target_os = "linux")]
mod landlock;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod native;
#[cfg(target_os = "macos")]
mod seatbelt;

/// Source bytes of the hermetic runner, process-launch, and async file-read modules in
/// `blaze-std` (AGENTS.md rule 69).
pub const CONFINEMENT_SOURCE: &[&[u8]] = &[
    include_bytes!("../cx.rs"),
    include_bytes!("../fs.rs"),
    include_bytes!("../io.rs"),
    include_bytes!("../io/fast.rs"),
    include_bytes!("../io/native_in.rs"),
    include_bytes!("../pending.rs"),
    include_bytes!("../procs.rs"),
    include_bytes!("../rt.rs"),
    include_bytes!("linked.rs"),
    include_bytes!("linked/run.rs"),
    include_bytes!("hermetic.rs"),
    include_bytes!("hermetic/execution.rs"),
    include_bytes!("hermetic/machine.rs"),
    include_bytes!("hermetic/native.rs"),
    include_bytes!("hermetic/plan.rs"),
    include_bytes!("hermetic/report.rs"),
    include_bytes!("hermetic/landlock.rs"),
    include_bytes!("hermetic/seatbelt.rs"),
];

/// Complete transitive runner/launcher/confinement implementation source bytes across `blaze-std`,
/// `blaze-core`, and `blaze-sys` so content-addressed runner keys roll on any change to argument,
/// environment, stream, spawn, or confinement code (AGENTS.md rule 69).
pub const TRANSITIVE_CONFINEMENT_SOURCE: &[&[&[u8]]] = &[
    CONFINEMENT_SOURCE,
    blaze_core::SPAWN_SOURCE,
    blaze_sys::CONFINEMENT_SOURCE,
];

/// File metadata stamp of a verified executable inode (device, inode, size, mtime, ctime).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct VerifiedStamp {
    pub dev: u64,
    pub ino: u64,
    pub len: u64,
    pub modified_ns: i128,
    pub changed_ns: i128,
}

/// A kernel-sealed immutable executable snapshot on Linux (`memfd_create` + `F_SEAL_WRITE |
/// F_SEAL_SHRINK | F_SEAL_GROW | F_SEAL_SEAL`), populated in a single bounded streaming pass
/// through `bstd::fs` (`blaze-core`'s deadline-aware async driver) while computing the caller's
/// SHA-256 digest and launched via `execveat(..., AT_EMPTY_PATH)` so neither path replacement
/// nor in-place writes can ever alter the verified bytes before execution.
#[cfg(target_os = "linux")]
pub struct VerifiedExec {
    fd: blaze_sys::linux::OwnedFd,
    len: u64,
}

#[cfg(target_os = "linux")]
pub(super) fn remaining_for_cx(cx: &Cx) -> Result<Duration, crate::Stopped> {
    cx.check()?;
    let deadline = cx.deadline();
    if deadline.finite().is_some() {
        let now = cx.io().now();
        if deadline.passed(now) {
            return Err(crate::Stopped::Deadline { span: cx.name(), at: deadline.get() });
        }
        let left = deadline.left(now);
        if left.is_zero() {
            return Err(crate::Stopped::Deadline { span: cx.name(), at: deadline.get() });
        }
        Ok(left)
    } else {
        Ok(Duration::from_secs(30))
    }
}

/// Reusable, deadline-aware async file-descriptor reader built on `bstd::fs` (`blaze-core`'s
/// kernel-probed `Uring`/`Threads` driver + `Cx::wait` / `blaze_core::wait`) rooted at `/`
/// reading `/proc/self/fd/<fd>` so reads target the exact already-opened inode with zero TOCTOU
/// and zero bespoke blocking syscalls.
///
/// Hold one instance across probes (e.g. on `HermeticClangProbe` or `FactFiles`): when a read is
/// cancelled or expires on a stalled filesystem, `Pending::cancel` bounds the wait to 50 ms and
/// retires the stale tag via `Io::forget`. If the kernel read is still stuck when `RootReader`
/// drops (even on a stack-owned owner such as `FactFiles`), `RootReaderInner::drop` hands the
/// secondary `Io` off to a single detached background reaper thread (`rt::retire_stalled_io`) so
/// caller drop returns immediately AND the worker thread + driver are cleanly reclaimed once the
/// stalled syscall returns (zero unbounded hang, zero `mem::forget` leak).
#[cfg(target_os = "linux")]
struct RootReaderInner {
    io: Option<crate::io::Io>,
    root: Option<crate::fs::Dir<crate::fs::ReadOnly>>,
    stalled: core::sync::atomic::AtomicBool,
    holds_reaper_slot: bool,
}

#[cfg(target_os = "linux")]
impl Drop for RootReaderInner {
    fn drop(&mut self) {
        if self.stalled.load(core::sync::atomic::Ordering::Acquire) {
            if let (Some(io), Some(root)) = (self.io.take(), self.root.take()) {
                self.holds_reaper_slot = false;
                crate::rt::retire_stalled_io(io, root);
                return;
            }
        }
        if self.holds_reaper_slot {
            self.holds_reaper_slot = false;
            crate::rt::release_reaper_slot();
        }
    }
}

#[cfg(target_os = "linux")]
struct ReaperSlotGuard(bool);

#[cfg(target_os = "linux")]
impl Drop for ReaperSlotGuard {
    fn drop(&mut self) {
        if self.0 {
            crate::rt::release_reaper_slot();
        }
    }
}

#[cfg(target_os = "linux")]
#[derive(Clone)]
pub struct RootReader {
    inner: blaze_own::Arc<RootReaderInner, blaze_own::SharedAlloc<'static>>,
}

#[cfg(target_os = "linux")]
impl blaze_own::Share for RootReader {}

#[cfg(target_os = "linux")]
impl RootReader {
    pub fn new(cx: &Cx) -> Result<Self, Fault> {
        match crate::rt::try_reserve_reaper_slot() {
            Ok(()) => {}
            Err(crate::rt::ReaperReserveError::BacklogFull) => {
                return Err(Fault::Host {
                    call: "bstd::fs root io (stalled-reader backlog full)",
                    code: 11,
                });
            }
            Err(crate::rt::ReaperReserveError::SpawnFailed) => {
                return Err(Fault::Host {
                    call: "bstd::fs root io (reaper worker spawn failed)",
                    code: 11,
                });
            }
        }
        let mut slot_guard = ReaperSlotGuard(true);
        let io = match cx.io().beside(crate::rt::RootPath::new("/")) {
            Some(Ok(io)) => io,
            Some(Err(_)) | None => {
                return Err(Fault::Host { call: "bstd::fs root io (open /)", code: 5 });
            }
        };
        let fs = io
            .need_fs()
            .map_err(|_| Fault::Host { call: "bstd::fs root io (missing fs)", code: 5 })?;
        let root = fs.root().read_only();
        slot_guard.0 = false;
        let inner = blaze_own::Arc::new_in(
            RootReaderInner {
                io: Some(io),
                root: Some(root),
                stalled: core::sync::atomic::AtomicBool::new(false),
                holds_reaper_slot: true,
            },
            blaze_own::SharedAlloc::global(),
        );
        Ok(Self { inner })
    }

    /// Whether a cancelled/expired read on this reader left a stalled syscall in flight (so
    /// long-lived owners should rotate in a fresh reader and let this one retire to the reaper).
    pub fn is_stalled(&self) -> bool {
        self.inner.stalled.load(core::sync::atomic::Ordering::Acquire)
    }

    fn io_ref(&self) -> Result<&crate::io::Io, Fault> {
        self.inner.io.as_ref().ok_or(Fault::Host { call: "bstd::fs root io (retired)", code: 5 })
    }

    fn root_ref(&self) -> Result<&crate::fs::Dir<crate::fs::ReadOnly>, Fault> {
        self.inner.root.as_ref().ok_or(Fault::Host { call: "bstd::fs root io (retired)", code: 5 })
    }

    fn fd_rel(fd: blaze_sys::Fd) -> Result<crate::path::Path, Fault> {
        let raw = u32::try_from(fd.raw()).map_err(|_| Fault::Host { call: "proc/self/fd path", code: 9 })?;
        let mut buf = [0u8; 32];
        const PREFIX: &[u8] = b"proc/self/fd/";
        if let Some(dst) = buf.get_mut(..PREFIX.len()) {
            dst.copy_from_slice(PREFIX);
        }
        let mut digits = [0u8; 12];
        let mut d_at = digits.len();
        let mut n = raw;
        loop {
            d_at -= 1;
            if let Some(slot) = digits.get_mut(d_at) {
                *slot = b'0' + u8::try_from(n % 10).unwrap_or(0);
            }
            n /= 10;
            if n == 0 {
                break;
            }
        }
        let d_len = digits.len() - d_at;
        let total = PREFIX.len() + d_len;
        if let (Some(dst), Some(src)) = (buf.get_mut(PREFIX.len()..total), digits.get(d_at..)) {
            dst.copy_from_slice(src);
        }
        let s = core::str::from_utf8(buf.get(..total).unwrap_or(&[]))
            .map_err(|_| Fault::Host { call: "proc/self/fd path", code: 22 })?;
        crate::path::Path::new(s).map_err(|_| Fault::Host { call: "proc/self/fd path", code: 22 })
    }

    fn sub_cx(&self, cx: &Cx) -> Result<(Cx, Deadline), Fault> {
        let io = self.io_ref()?;
        let left = remaining_for_cx(cx)
            .map_err(|_| Fault::Host { call: "nix runtime closure (deadline/stopped)", code: 110 })?;
        let deadline = Deadline::after(io.now(), left);
        Ok((cx.on_beside(blaze_own::Share::share(io), deadline), deadline))
    }

    /// Waits for `pending` on `self.io` while polling in bounded 10 ms slices so parent
    /// cancellation (`cx.cancelled()`) abandons a stalled read within a short 50 ms settle bound
    /// instead of waiting out the full remaining probe deadline. If the read still has not
    /// completed after `Pending::cancel`, marks this reader stalled so dropping it retires its
    /// worker to the detached background reaper instead of blocking caller drop or leaking.
    fn wait_read_cancellable<'buf>(
        &self,
        cx: &Cx,
        read_cx: &Cx,
        deadline: Deadline,
        mut pending: crate::pending::Pending<'buf, crate::fs::ReadInto<'buf>>,
        call: &'static str,
    ) -> Result<usize, Fault> {
        let io = self.io_ref()?;
        loop {
            match pending.try_wait() {
                Ok(res) => return res.map_err(|e| fs_err_to_fault(call, &e)),
                Err(still) => pending = still,
            }
            if cx.cancelled() {
                let give_up = Deadline::after(io.now(), Duration::from_millis(50));
                if matches!(
                    pending.cancel(read_cx, give_up),
                    Err(crate::fs::FsError { kind: crate::fs::FsKind::Stopped(_), .. })
                ) {
                    self.inner.stalled.store(true, core::sync::atomic::Ordering::Release);
                }
                return Err(Fault::Host { call: "nix runtime closure (cancelled)", code: 125 });
            }
            let now = io.now();
            if deadline.passed(now) {
                if matches!(
                    pending.cancel(read_cx, Deadline::PASSED),
                    Err(crate::fs::FsError { kind: crate::fs::FsKind::Stopped(_), .. })
                ) {
                    self.inner.stalled.store(true, core::sync::atomic::Ordering::Release);
                }
                return Err(Fault::Host { call: "nix runtime closure (deadline/stopped)", code: 110 });
            }
            let slice = deadline.min(Deadline::after(now, Duration::from_millis(10)));
            let _ = io.poll(slice);
        }
    }

    /// Reads from already-opened `fd` at `offset` through `blaze-core`'s async driver until `out`
    /// is completely filled or a zero-length read proves EOF (never truncating short FUSE/NFS
    /// reads).
    pub(super) fn read_exact_or_eof(
        &self,
        cx: &Cx,
        fd: blaze_sys::Fd,
        out: &mut [u8],
        offset: u64,
        call: &'static str,
    ) -> Result<usize, Fault> {
        if out.is_empty() {
            return Ok(0);
        }
        let rel = Self::fd_rel(fd)?;
        let (open_cx, _) = self.sub_cx(cx)?;
        let file = self.root_ref()?.open_read(&open_cx, &rel).map_err(|e| fs_err_to_fault(call, &e))?;
        let mut filled = 0usize;
        while filled < out.len() {
            let step_off = u64::try_from(filled)
                .ok()
                .and_then(|d| offset.checked_add(d))
                .ok_or(Fault::Host { call, code: 75 })?;
            let (read_cx, deadline) = self.sub_cx(cx)?;
            let Some(dst) = out.get_mut(filled..) else {
                break;
            };
            let n = self.wait_read_cancellable(cx, &read_cx, deadline, file.read_into(dst, step_off), call)?;
            if n == 0 {
                break;
            }
            filled += n;
        }
        Ok(filled)
    }

    /// Streams the entire contents of already-opened `fd` in bounded 64 KiB async `Effect::Read`
    /// chunks into `feed`, returning total bytes read.
    pub(super) fn stream_fd(
        &self,
        cx: &Cx,
        fd: blaze_sys::Fd,
        call: &'static str,
        mut feed: impl FnMut(&[u8]),
    ) -> Result<u64, Fault> {
        let rel = Self::fd_rel(fd)?;
        let (open_cx, _) = self.sub_cx(cx)?;
        let file = self.root_ref()?.open_read(&open_cx, &rel).map_err(|e| fs_err_to_fault(call, &e))?;
        let mut buf = [0u8; 64 << 10];
        let mut offset = 0u64;
        loop {
            let (read_cx, deadline) = self.sub_cx(cx)?;
            let n = self.wait_read_cancellable(cx, &read_cx, deadline, file.read_into(&mut buf, offset), call)?;
            if n == 0 {
                break;
            }
            if let Some(chunk) = buf.get(..n) {
                feed(chunk);
            }
            let step = u64::try_from(n).map_err(|_| Fault::Host { call, code: 75 })?;
            offset = offset.checked_add(step).ok_or(Fault::Host { call, code: 75 })?;
        }
        Ok(offset)
    }
}

#[cfg(target_os = "linux")]
fn fs_err_to_fault(call: &'static str, err: &crate::fs::FsError) -> Fault {
    let code = match &err.kind {
        crate::fs::FsKind::Stopped(_) => 110,
        crate::fs::FsKind::NotFound => 2,
        crate::fs::FsKind::IsADirectory => 21,
        crate::fs::FsKind::NotADirectory => 20,
        _ => 5,
    };
    Fault::Host { call, code }
}

#[cfg(target_os = "linux")]
impl VerifiedExec {
    /// Streams `rel` beneath `dir` through `bstd::fs` (`blaze-core`'s deadline-aware, cancellable
    /// async driver — never raw blocking `pread`) into an executable `memfd` (selecting `MFD_EXEC`
    /// via `probe_memfd_exec`, or refusing explicitly under `vm.memfd_noexec=2`) while feeding
    /// those exact bytes into `on_chunk`, and permanently seals the `memfd`
    /// (`SEAL | SHRINK | GROW | WRITE`) before returning.
    pub fn open_and_seal<P: crate::fs::Rights>(
        cx: &Cx,
        dir: &crate::fs::Dir<P>,
        rel: &crate::path::Path,
        mut on_chunk: impl FnMut(&[u8]),
    ) -> Result<VerifiedExec, VerifiedExecError> {
        let _ = remaining_for_cx(cx).map_err(VerifiedExecError::Stopped)?;
        let memfd_flags = match blaze_sys::posix::io::probe_memfd_exec()
            .map_err(|errno| VerifiedExecError::Os { call: "memfd_create (probe MFD_EXEC)", errno })?
        {
            blaze_sys::posix::io::MemfdExec::Supported => blaze_sys::posix::io::MemfdFlags::CLOSE_ON_EXEC
                .with(blaze_sys::posix::io::MemfdFlags::ALLOW_SEALING)
                .with(blaze_sys::posix::io::MemfdFlags::EXEC),
            blaze_sys::posix::io::MemfdExec::LegacyKernel => blaze_sys::posix::io::MemfdFlags::CLOSE_ON_EXEC
                .with(blaze_sys::posix::io::MemfdFlags::ALLOW_SEALING),
            blaze_sys::posix::io::MemfdExec::Denied(errno) => return Err(VerifiedExecError::ExecMemfdDenied(errno)),
        };
        let file = dir.open_read(cx, rel).map_err(VerifiedExecError::from_fs)?;
        let memfd = match blaze_sys::posix::io::memfd_create(
            blaze_sys::Path::new(c"blaze-verified-exec"),
            memfd_flags,
        ) {
            Ok(fd) => fd,
            Err(errno @ (blaze_sys::Errno::Perm | blaze_sys::Errno::Acces)) => {
                return Err(VerifiedExecError::ExecMemfdDenied(errno));
            }
            Err(errno) => return Err(VerifiedExecError::Os { call: "memfd_create", errno }),
        };
        let mut chunk_buf = [0u8; 64 << 10];
        let mut offset = 0u64;
        loop {
            let _ = remaining_for_cx(cx).map_err(VerifiedExecError::Stopped)?;
            let n = file
                .read_into(&mut chunk_buf, offset)
                .wait(cx)
                .map_err(VerifiedExecError::from_fs)?;
            if n == 0 {
                break;
            }
            let Some(chunk) = chunk_buf.get(..n) else {
                break;
            };
            on_chunk(chunk);
            let mut written = 0usize;
            while written < chunk.len() {
                let _ = remaining_for_cx(cx).map_err(VerifiedExecError::Stopped)?;
                let Some(rem) = chunk.get(written..) else {
                    break;
                };
                let w = blaze_sys::linux::write(memfd.fd(), rem)
                    .map_err(|errno| VerifiedExecError::Os { call: "write (memfd)", errno })?;
                if w == 0 {
                    return Err(VerifiedExecError::Os { call: "write (memfd short)", errno: blaze_sys::Errno::Io });
                }
                written += w;
            }
            let step = u64::try_from(n).map_err(|_| VerifiedExecError::BadPath)?;
            offset = offset.checked_add(step).ok_or(VerifiedExecError::BadPath)?;
        }
        let seals = blaze_sys::posix::io::Seals::WRITE
            .with(blaze_sys::posix::io::Seals::SHRINK)
            .with(blaze_sys::posix::io::Seals::GROW)
            .with(blaze_sys::posix::io::Seals::SEAL);
        blaze_sys::posix::io::add_seals(memfd.fd(), seals)
            .map_err(|errno| VerifiedExecError::Os { call: "fcntl (F_ADD_SEALS)", errno })?;
        Ok(VerifiedExec { fd: memfd, len: offset })
    }

    /// Length in bytes of the sealed executable snapshot.
    pub fn len(&self) -> u64 {
        self.len
    }

    /// Streams the sealed snapshot in bounded 64 KiB chunks through `bstd::fs` into `on_chunk`
    /// (used by tests to verify immutability of the sealed `memfd`).
    pub fn stream_chunks(&self, cx: &Cx, mut on_chunk: impl FnMut(&[u8])) -> Result<(), VerifiedExecError> {
        let reader = RootReader::new(cx)
            .map_err(|_| VerifiedExecError::Os { call: "bstd::fs root io", errno: blaze_sys::Errno::Io })?;
        let _ = reader
            .stream_fd(cx, self.fd.fd(), "read (sealed memfd)", |chunk| on_chunk(chunk))
            .map_err(|_| VerifiedExecError::Os { call: "read (sealed memfd)", errno: blaze_sys::Errno::Io })?;
        Ok(())
    }

    /// Attempts a write directly against the sealed `memfd` (always rejected by the kernel with
    /// `EPERM` once `F_SEAL_WRITE` is installed; exposed for verification tests).
    pub fn try_write_for_test(&self, bytes: &[u8]) -> Result<usize, VerifiedExecError> {
        blaze_sys::linux::write(self.fd.fd(), bytes)
            .map_err(|errno| VerifiedExecError::Os { call: "write (sealed memfd)", errno })
    }

    pub(super) fn fd(&self) -> blaze_sys::Fd {
        self.fd.fd()
    }
}

/// Why opening or streaming a [`VerifiedExec`] failed.
#[cfg(target_os = "linux")]
#[derive(Debug)]
pub enum VerifiedExecError {
    BadPath,
    NotRegular,
    NotFound,
    ExecMemfdDenied(blaze_sys::Errno),
    Stopped(crate::Stopped),
    Os { call: &'static str, errno: blaze_sys::Errno },
}

#[cfg(target_os = "linux")]
impl VerifiedExecError {
    fn from_fs(err: crate::fs::FsError) -> Self {
        match err.kind {
            crate::fs::FsKind::Stopped(s) => VerifiedExecError::Stopped(s),
            crate::fs::FsKind::NotFound => VerifiedExecError::NotFound,
            crate::fs::FsKind::IsADirectory => VerifiedExecError::NotRegular,
            _ => VerifiedExecError::Os { call: "bstd::fs read", errno: blaze_sys::Errno::Io },
        }
    }
}

#[cfg(target_os = "linux")]
impl core::fmt::Display for VerifiedExecError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            VerifiedExecError::BadPath => f.write_str("invalid executable path"),
            VerifiedExecError::NotRegular => f.write_str("executable path is not a regular file"),
            VerifiedExecError::NotFound => f.write_str("executable file not found"),
            VerifiedExecError::ExecMemfdDenied(errno) => write!(
                f,
                "kernel/namespace forbids executable memfd (vm.memfd_noexec=2 or seccomp, errno {})",
                errno.raw()
            ),
            VerifiedExecError::Stopped(s) => write!(f, "stopped: {s}"),
            VerifiedExecError::Os { call, errno } => write!(f, "{call} failed (errno {})", errno.raw()),
        }
    }
}

#[cfg(test)]
mod tests;
#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod process_tests;

/// Sans-IO coordinator for bounded independent reaper workers (`rt::retire_stalled_io` and
/// `rt::retire_lock_fd`): guarantees that retiring an abandoned driver or file lock NEVER drops
/// synchronously on the caller (even past the `max_workers` cap or on transient `thread::spawn`
/// failure), reserves slots at admission time so `active + pending + reserved` can NEVER exceed
/// `max_workers + max_pending`, and allows up to `max_workers` independent stalled mounts to
/// drain without head-of-line blocking each other.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ReaperPlant {
    None,
    /// Control 1: drops overflowed items synchronously on the caller when at cap.
    SyncDropOnOverflow,
    /// Control 2: collapses concurrency to a single FIFO worker.
    SingleFifo,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[derive(PartialEq, Eq, Debug)]
pub enum RetireAction<T> {
    Spawn(T),
    Queued,
    SyncDrop(T),
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub struct ReaperCore<T> {
    max_workers: usize,
    max_pending: usize,
    active: usize,
    reserved: usize,
    pending: blaze_own::VecDeque<T, blaze_own::SharedAlloc<'static>>,
    plant: ReaperPlant,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl<T> ReaperCore<T> {
    pub fn new(max_workers: usize, max_pending: usize, plant: ReaperPlant) -> Self {
        Self {
            max_workers: max_workers.max(1),
            max_pending: max_pending.max(1),
            active: 0,
            reserved: 0,
            pending: blaze_own::VecDeque::new_in(blaze_own::SharedAlloc::global()),
            plant,
        }
    }

    fn total_cap(&self) -> usize {
        self.max_workers.saturating_add(self.max_pending)
    }

    /// Whether the reaper backlog still has room to admit another live owner without allowing
    /// `active + pending + reserved` to grow past `max_workers + max_pending`.
    pub fn has_capacity(&self) -> bool {
        self.active.saturating_add(self.pending.len()).saturating_add(self.reserved) < self.total_cap()
    }

    /// Atomically reserves one slot for a newly admitted live owner (`RootReader` or `NativeLock`).
    /// Must be paired with either [`Self::release_reservation`] (normal close) or
    /// [`Self::retire_reserved`] (stalled retirement).
    pub fn try_reserve(&mut self) -> bool {
        if self.has_capacity() {
            self.reserved += 1;
            true
        } else {
            false
        }
    }

    /// Releases a reservation when a live owner closes cleanly without stalling.
    pub fn release_reservation(&mut self) {
        self.reserved = self.reserved.saturating_sub(1);
    }

    /// Consumes a previously reserved slot and retires `item` into the bounded reaper pool.
    pub fn retire_reserved(&mut self, item: T) -> RetireAction<T> {
        self.reserved = self.reserved.saturating_sub(1);
        self.retire(item)
    }

    fn effective_cap(&self) -> usize {
        match self.plant {
            ReaperPlant::SingleFifo => 1,
            ReaperPlant::None | ReaperPlant::SyncDropOnOverflow => self.max_workers,
        }
    }

    pub fn active(&self) -> usize {
        self.active
    }

    pub fn reserved(&self) -> usize {
        self.reserved
    }

    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    pub fn retire(&mut self, item: T) -> RetireAction<T> {
        let cap = self.effective_cap();
        if self.active < cap {
            self.active += 1;
            if let Some(older) = self.pending.pop_front() {
                self.pending.push_back(item);
                return RetireAction::Spawn(older);
            }
            return RetireAction::Spawn(item);
        }
        if self.plant == ReaperPlant::SyncDropOnOverflow {
            return RetireAction::SyncDrop(item);
        }
        self.pending.push_back(item);
        RetireAction::Queued
    }

    pub fn on_spawn_failed(&mut self, item: T) {
        self.active = self.active.saturating_sub(1);
        self.pending.push_back(item);
    }

    /// Pops another queued item to spawn a reaper worker for whenever `active < cap` (used to
    /// recover items queued during an earlier transient `thread::spawn` failure).
    pub fn take_spawnable(&mut self) -> Option<T> {
        let cap = self.effective_cap();
        if self.active < cap
            && let Some(item) = self.pending.pop_front()
        {
            self.active += 1;
            return Some(item);
        }
        None
    }

    pub fn next_after_drop(&mut self) -> Option<T> {
        if let Some(next) = self.pending.pop_front() {
            return Some(next);
        }
        self.active = self.active.saturating_sub(1);
        None
    }
}

/// Planted bug for deterministic interleaving DSTs of [`StandbyReaperCore`].
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StandbyReaperPlant {
    None,
    /// Control: lets an idle worker exit without checking whether live `reserved` owners still
    /// need standby coverage, stranding a subsequent retired driver with 0 workers.
    ExitWhileNeeded,
}

/// Next action for a standby reaper worker thread inside its `Condvar` loop.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[derive(Debug)]
pub enum StandbyWorkerStep<T> {
    Work(T),
    Wait,
    Exit,
}

/// Sans-IO state machine for the pre-spawned standby I/O reaper pool (`rt::try_reserve_reaper_slot`,
/// `rt::release_reaper_slot`, `rt::retire_stalled_io`):
/// - Spawns standby workers at admission time (`begin_reserve`) when a typed spawn error can still
///   be returned to the caller, so `retire_reserved` NEVER calls `thread::spawn`, NEVER fails, and
///   NEVER needs a background retry loop.
/// - Tracks in-flight spawns (`spawning`) separately from live parked workers (`idle`) so no live
///   worker ever exits prematurely counting on an unconfirmed spawn that might fail.
/// - Surplus idle workers exit immediately on `release_reservation` when `pending` is empty.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub struct StandbyReaperCore<T> {
    max_workers: usize,
    max_pending: usize,
    active: usize,
    idle: usize,
    spawning: usize,
    reserved: usize,
    pending: blaze_own::VecDeque<T, blaze_own::SharedAlloc<'static>>,
    plant: StandbyReaperPlant,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl<T> StandbyReaperCore<T> {
    pub fn new(max_workers: usize, max_pending: usize, plant: StandbyReaperPlant) -> Self {
        Self {
            max_workers: max_workers.max(1),
            max_pending: max_pending.max(1),
            active: 0,
            idle: 0,
            spawning: 0,
            reserved: 0,
            pending: blaze_own::VecDeque::new_in(blaze_own::SharedAlloc::global()),
            plant,
        }
    }

    pub fn active(&self) -> usize {
        self.active
    }

    pub fn idle(&self) -> usize {
        self.idle
    }

    pub fn spawning(&self) -> usize {
        self.spawning
    }

    pub fn reserved(&self) -> usize {
        self.reserved
    }

    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    fn total_cap(&self) -> usize {
        self.max_workers.saturating_add(self.max_pending)
    }

    fn needed_standby(&self) -> usize {
        let demand = self.pending.len().saturating_add(self.reserved);
        demand.min(self.max_workers.saturating_sub(self.active))
    }

    /// Attempts to reserve a backlog slot for a new live owner. Returns `Ok(true)` if the caller
    /// must spawn one standby worker thread (accounting for it in `spawning` until it starts or
    /// fails via [`Self::abort_spawn_failed`]), `Ok(false)` if existing standby/active workers
    /// already cover the reservation, or `Err(())` if the total backlog cap is full.
    pub fn begin_reserve(&mut self) -> Result<bool, ()> {
        if self.active.saturating_add(self.pending.len()).saturating_add(self.reserved) >= self.total_cap() {
            return Err(());
        }
        self.reserved += 1;
        let needed = self.needed_standby();
        if self.idle.saturating_add(self.spawning) < needed {
            self.spawning += 1;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Rolls back a `begin_reserve()` call whose standby worker spawn failed.
    pub fn abort_spawn_failed(&mut self) {
        self.spawning = self.spawning.saturating_sub(1);
        self.reserved = self.reserved.saturating_sub(1);
    }

    /// Called as the first step of a newly spawned standby worker thread before polling for work.
    pub fn on_worker_started(&mut self) {
        self.spawning = self.spawning.saturating_sub(1);
        self.idle += 1;
    }

    /// Releases a reservation on clean close; returns `true` if a surplus idle worker should be
    /// woken to exit.
    pub fn release_reservation(&mut self) -> bool {
        self.reserved = self.reserved.saturating_sub(1);
        self.idle > self.needed_standby()
    }

    /// Consumes a reservation and enqueues a stalled item for an already-spawned standby worker
    /// (or an active worker finishing its current drop). Never spawns and never fails.
    pub fn retire_reserved(&mut self, item: T) {
        self.reserved = self.reserved.saturating_sub(1);
        self.pending.push_back(item);
    }

    /// Polls for the next action for a parked `idle` worker under the pool lock.
    pub fn worker_poll(&mut self) -> StandbyWorkerStep<T> {
        if self.active < self.max_workers
            && let Some(item) = self.pending.pop_front()
        {
            self.idle = self.idle.saturating_sub(1);
            self.active += 1;
            return StandbyWorkerStep::Work(item);
        }
        let exit_now = match self.plant {
            StandbyReaperPlant::None => self.pending.is_empty() && self.idle > self.needed_standby(),
            StandbyReaperPlant::ExitWhileNeeded => self.pending.is_empty(),
        };
        if exit_now {
            self.idle = self.idle.saturating_sub(1);
            return StandbyWorkerStep::Exit;
        }
        StandbyWorkerStep::Wait
    }

    /// Transitions a worker from `active` back to `idle` after finishing a retired drop.
    pub fn worker_finished_drop(&mut self) {
        self.active = self.active.saturating_sub(1);
        self.idle += 1;
    }
}

#[cfg(target_os = "linux")]
pub use landlock::{
    HostLdCaps, PreloadScanner, dl_dst_lib_token, dl_dst_platform_token, expand_rpath_tokens,
    feed_nix_runtime_closure, feed_nix_runtime_closure_with, parse_ld_so_cache_match,
    validate_rpath_entry, x86_64_fhs_hwcap_subdirs,
};
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use crate::rt::{AttemptSignal, LockBackend, LockPlant, LockRuntime, LockedIoHandshake, LockedIoPlant, NativeLockErr};
pub use execution::{Absent, BlazeLauncher, EXECUTION_BACKENDS, ExecutionBackend, HypervisorSandbox};
pub use machine::{Effect, Event, HermeticMachine, MachineOpts, State, Stream, Usage};
pub use plan::{Access, Plan, PlanOpts, Rule as PlanRule, Why};
pub use report::{ReportError, ReportParser};
#[cfg(any(test, feature = "testing"))]
pub use sim::{Act, LinesReport, STEP, SimFiles, SimOs, SimPort, Source};

/// What runs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Program<'a> {
    /// An executable in the shelf or the store, named by its content's digest; `path` is the
    /// absolute host path the store keeps it at (read-only), which the plan lets the child run
    /// and read, and nothing else of its folder.
    Stored { name: &'a str, digest: Digest, path: &'a str },
    /// A program blaze built, as its code records (D175): only [`Level::Execution`] runs one
    /// (blaze's launcher against a host-call system layer), never an OS process here.
    Records { name: &'a str, code: Digest },
}

impl<'a> Program<'a> {
    /// The name errors give it.
    pub fn name(&self) -> &'a str {
        match *self {
            Program::Stored { name, .. } | Program::Records { name, .. } => name,
        }
    }

    /// Its identity: the executable's digest, or its code records'.
    pub fn digest(&self) -> Digest {
        match *self {
            Program::Stored { digest, .. } => digest,
            Program::Records { code, .. } => code,
        }
    }
}

/// What the child's clock says.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ClockSpec {
    /// A fixed time, seconds since the Unix epoch. At [`Level::Inputs`] it is given to the
    /// program as `SOURCE_DATE_EPOCH`, which the spec's `env` must hold with this value (the
    /// environment is exactly the pairs given: nothing is added); a call to the host's clock
    /// is not intercepted, so a program that ignores the variable is not determined. At
    /// [`Level::Execution`] the system layer answers every clock call with it.
    Fixed { unix_secs: u64 },
    /// The program never asks. At [`Level::Inputs`] that is the caller's promise, checked by
    /// nothing; the env must then hold no `SOURCE_DATE_EPOCH`. At [`Level::Execution`] a clock
    /// call is refused and ends the run.
    NeverAsks,
    /// The caller explicitly supplied a malformed or out-of-range `SOURCE_DATE_EPOCH` at
    /// [`Level::Inputs`] and wants the child process (e.g., Clang) to see that exact environment
    /// value and produce its normal diagnostic rather than rejecting the spec pre-launch.
    /// Accepted ONLY at [`Level::Inputs`] when `SOURCE_DATE_EPOCH` is present and fails `u64`
    /// parsing.
    InvalidEnv,
}

impl ClockSpec {
    /// Derives the `ClockSpec` for a caller-supplied `SOURCE_DATE_EPOCH` value using the exact
    /// same parser that [`HermeticSpec::new`] validates (`parse_u64`), so leading-zero (`01`),
    /// non-numeric, or out-of-range values resolve to [`ClockSpec::InvalidEnv`] and never fail
    /// pre-launch with [`Fault::ClockNotInEnv`].
    pub fn from_source_date_epoch(value: &str) -> Self {
        match parse_u64(value) {
            Some(unix_secs) => ClockSpec::Fixed { unix_secs },
            None => ClockSpec::InvalidEnv,
        }
    }
}

/// What the child may use. Every cap is required (there is no default budget).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Budget {
    /// CPU time, user and system, every thread and helper of the child's group. Linux: held by
    /// the kernel (`RLIMIT_CPU`, rounded up to whole seconds) and metered. macOS: metered
    /// (`proc_pid_rusage` every [`machine::METER_EVERY`]), the group killed when it is passed.
    pub cpu: Duration,
    /// Bytes of memory. Linux: address space (`RLIMIT_AS`), held by the kernel, and the
    /// footprint metered. macOS: the physical footprint, metered (xnu enforces no `RLIMIT_AS`).
    pub memory: u64,
    /// Steps of the program, where they can be metered: only at [`Level::Execution`] (the
    /// launcher counts them). `Some` at [`Level::Inputs`] is refused ([`Fault::StepsNotMetered`]).
    pub steps: Option<u64>,
}

/// What becomes of one of the child's output streams. Its input is always empty (`/dev/null`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Capture {
    /// Written to `/dev/null`.
    Null,
    /// Kept, at most `most` bytes; more stops the run ([`Stop::Stream`]): never cut short.
    Keep { most: u32 },
}

/// The child's two output streams.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct HermeticStreams {
    pub stdout: Capture,
    pub stderr: Capture,
}

/// How far a run is determined (module docs).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Level {
    /// OS-confined to its roots, with its reads reported.
    Inputs,
    /// The run itself determined.
    Execution,
}

/// Where the read list comes from.
#[derive(Clone, Copy)]
pub enum ReportSpec<'a> {
    /// The program's own report, the file `file` it writes under the output root, read by
    /// `parser` (a C compiler's depfile: the C driver's parser). Each path it names must be
    /// under a mount; the list is checked by `ReadList::under` against the roots.
    Program { file: RelPath<'a>, parser: &'a dyn ReportParser },
    /// No report: each package root's every admitted file is the read ([`Reads::WholeRoots`]);
    /// a tree root is covered by its digest, which is in the roots' identity.
    WholeRoots,
}

/// Everything a hermetic run is given. Made only by [`HermeticSpec::new`], which checks it.
#[derive(Clone, Copy)]
pub struct HermeticSpec<'a> {
    program: Program<'a>,
    args: &'a [&'a str],
    env: &'a [(&'a str, &'a str)],
    roots: Roots<'a>,
    mounts: &'a [Mount<'a>],
    cwd: &'a str,
    helpers: &'a [&'a str],
    helper_folders: &'a [&'a str],
    channels: &'a [&'a str],
    clock: ClockSpec,
    seed: u64,
    budget: Budget,
    deadline: Deadline,
    streams: HermeticStreams,
    level: Level,
    report: ReportSpec<'a>,
    #[cfg(target_os = "linux")]
    arg0: Option<&'a str>,
    #[cfg(target_os = "linux")]
    verified_exec: Option<&'a VerifiedExec>,
}

/// The fields of a [`HermeticSpec`], every one required.
pub struct HermeticOpts<'a> {
    pub program: Program<'a>,
    /// `argv[1..]`; `argv[0]` is the program's path.
    pub args: &'a [&'a str],
    /// The child's whole environment: exactly these pairs, keys in strictly ascending byte
    /// order (so one spec has one spelling), nothing inherited. Empty, the child gets an empty
    /// environment: no `PATH` (a helper is run by its absolute path or not at all), no `HOME`
    /// (a program looking for a dotfile finds none), no `TMPDIR` (a program that makes a
    /// temporary file then tries `/tmp`, which the plan refuses: give it a folder under the
    /// output root), no `LANG` or `LC_*` (the POSIX "C" locale), no `TZ` (UTC: the zone
    /// files are outside every root and refused).
    pub env: &'a [(&'a str, &'a str)],
    /// What the child may read, and the one folder it writes (the output root).
    pub roots: Roots<'a>,
    /// Where each root is on this host: an absolute spelling a root (blaze-roots `Mounts`).
    pub mounts: &'a [Mount<'a>],
    /// The child's directory: an absolute host spelling under a mount.
    pub cwd: &'a str,
    /// Absolute paths of the programs the child may start (none: it starts none). A C driver
    /// passes `-fintegrated-cc1` (clang then runs its `-cc1` in its own process) and `-c` (no
    /// linker), and names no helper.
    pub helpers: &'a [&'a str],
    /// Absolute folders beneath which any program may be started (and read): only a folder
    /// under the output root, whose programs the run built itself (a configure script's test
    /// programs, D112), so what runs there is the run's own output, never a host program.
    /// Empty for nearly every run. Anything else is [`Fault::HelperFolderOutside`].
    pub helper_folders: &'a [&'a str],
    /// Absolute paths outside every root the child may open to read and write, which carry
    /// neither an input nor an output of the run: a jobserver's fifo (tokens, never data), a log
    /// folder the caller reads back itself. Each is the caller's promise that nothing read
    /// there decides what the run makes; a path under a mount is
    /// [`Fault::ChannelUnderRoot`] (a root's rules already say what may be done there).
    pub channels: &'a [&'a str],
    pub clock: ClockSpec,
    /// The seed of every random answer. At [`Level::Inputs`] it is part of the run's identity
    /// and given to nothing (the child's own randomness is not intercepted); at
    /// [`Level::Execution`] it seeds the system layer's random answers.
    pub seed: u64,
    pub budget: Budget,
    /// When the run is stopped, on the driver's clock (a value: nothing reads a clock here).
    pub deadline: Deadline,
    pub streams: HermeticStreams,
    pub level: Level,
    pub report: ReportSpec<'a>,
}

impl<'a> HermeticSpec<'a> {
    /// The spec, checked: the environment's spelling, the clock against it, the steps against
    /// the level, an output root and its mount, and the directory under a mount.
    pub fn new(opts: HermeticOpts<'a>) -> Result<HermeticSpec<'a>, HermeticError<'a>> {
        let refuse = |fault| Err(HermeticError::new(opts.program.name(), fault));
        let mut last: Option<&str> = None;
        let mut epoch: Option<&str> = None;
        for (at, &(key, value)) in opts.env.iter().enumerate() {
            let at = at as u32;
            if key.is_empty() || key.bytes().any(|b| b == b'=' || b == 0) || value.bytes().any(|b| b == 0) {
                return refuse(Fault::EnvSpelling { at });
            }
            if last.is_some_and(|last| last.as_bytes() >= key.as_bytes()) {
                return refuse(Fault::EnvNotSorted { at });
            }
            last = Some(key);
            if key == "SOURCE_DATE_EPOCH" {
                epoch = Some(value);
            }
        }
        match (opts.clock, epoch) {
            (ClockSpec::Fixed { unix_secs }, Some(value)) if parse_u64(value) == Some(unix_secs) => {}
            (ClockSpec::NeverAsks, None) => {}
            (ClockSpec::InvalidEnv, Some(value)) if opts.level == Level::Inputs && parse_u64(value).is_none() => {}
            _ => return refuse(Fault::ClockNotInEnv),
        }
        if opts.level == Level::Inputs && opts.budget.steps.is_some() {
            return refuse(Fault::StepsNotMetered);
        }
        let Some(output) = opts.roots.output() else {
            return refuse(Fault::NoOutputRoot);
        };
        if !opts.mounts.iter().any(|mount| mount.root == output) {
            return refuse(Fault::Unmounted { root: output });
        }
        if Mounts::new(opts.mounts, None, &opts.roots).is_err() {
            return refuse(Fault::MountsRefused);
        }
        if mount_under(opts.mounts, opts.cwd.as_bytes()).is_none() {
            return refuse(Fault::CwdUnmounted);
        }
        for (at, folder) in opts.helper_folders.iter().enumerate() {
            if mount_under(opts.mounts, folder.as_bytes()).is_none_or(|(root, _)| root != output) {
                return refuse(Fault::HelperFolderOutside { at: at as u32 });
            }
        }
        for (at, channel) in opts.channels.iter().enumerate() {
            if !channel.starts_with('/') || mount_under(opts.mounts, channel.as_bytes()).is_some() {
                return refuse(Fault::ChannelUnderRoot { at: at as u32 });
            }
        }
        Ok(HermeticSpec {
            program: opts.program,
            args: opts.args,
            env: opts.env,
            roots: opts.roots,
            mounts: opts.mounts,
            cwd: opts.cwd,
            helpers: opts.helpers,
            helper_folders: opts.helper_folders,
            channels: opts.channels,
            clock: opts.clock,
            seed: opts.seed,
            budget: opts.budget,
            deadline: opts.deadline,
            streams: opts.streams,
            level: opts.level,
            report: opts.report,
            #[cfg(target_os = "linux")]
            arg0: None,
            #[cfg(target_os = "linux")]
            verified_exec: None,
        })
    }

    /// Overrides the child's `argv[0]` on Linux (where `Landlock` launches the target directly via
    /// `execve` / `execveat` without a wrapper binary) while keeping the executed binary bound to
    /// `program` (or `verified_exec`). Used so driver aliases (`cc`, `c++`, `gcc`) emit diagnostics
    /// under the requested driver spelling without rewriting captured output streams.
    #[cfg(target_os = "linux")]
    pub fn with_arg0(mut self, arg0: &'a str) -> Self {
        self.arg0 = Some(arg0);
        self
    }

    #[cfg(target_os = "linux")]
    pub fn arg0(&self) -> Option<&'a str> {
        self.arg0
    }

    /// Binds this spec to a pre-opened, pre-verified executable snapshot on Linux so Landlock and
    /// `execveat(..., AT_EMPTY_PATH)` act on that exact descriptor without reopening `program`
    /// by path. The borrow lifetime `'a` statically guarantees `exec` outlives this spec.
    #[cfg(target_os = "linux")]
    pub fn with_verified_exec(mut self, exec: &'a VerifiedExec) -> Self {
        self.verified_exec = Some(exec);
        self
    }

    #[cfg(target_os = "linux")]
    pub(super) fn verified_exec_fd(&self) -> Option<blaze_sys::Fd> {
        self.verified_exec.map(VerifiedExec::fd)
    }

    pub fn program(&self) -> Program<'a> {
        self.program
    }
    pub fn args(&self) -> &'a [&'a str] {
        self.args
    }
    pub fn env(&self) -> &'a [(&'a str, &'a str)] {
        self.env
    }
    pub fn roots(&self) -> Roots<'a> {
        self.roots
    }
    pub fn mounts(&self) -> &'a [Mount<'a>] {
        self.mounts
    }
    pub fn cwd(&self) -> &'a str {
        self.cwd
    }
    pub fn helpers(&self) -> &'a [&'a str] {
        self.helpers
    }
    pub fn helper_folders(&self) -> &'a [&'a str] {
        self.helper_folders
    }
    pub fn channels(&self) -> &'a [&'a str] {
        self.channels
    }
    /// Whether the child may start any program (a helper, or one under a helper folder): the
    /// seccomp filter and the profile let it make processes only then.
    pub fn starts_programs(&self) -> bool {
        !self.helpers.is_empty() || !self.helper_folders.is_empty()
    }
    pub fn clock(&self) -> ClockSpec {
        self.clock
    }
    pub fn seed(&self) -> u64 {
        self.seed
    }
    pub fn budget(&self) -> Budget {
        self.budget
    }
    pub fn deadline(&self) -> Deadline {
        self.deadline
    }
    pub fn streams(&self) -> HermeticStreams {
        self.streams
    }
    pub fn level(&self) -> Level {
        self.level
    }
    pub fn report(&self) -> ReportSpec<'a> {
        self.report
    }

    /// The spelling of `root`'s mount, if it has one.
    pub fn mount_of(&self, root: RootId) -> Option<&'a [u8]> {
        self.mounts.iter().find(|mount| mount.root == root).map(|mount| mount.spelling)
    }
}

/// The root whose mount holds the absolute spelling `path` (the longest such mount), and the
/// rest of `path` beneath it (no leading `/`).
pub(crate) fn mount_under<'p>(mounts: &[Mount<'_>], path: &'p [u8]) -> Option<(RootId, &'p [u8])> {
    let mut best: Option<(usize, RootId)> = None;
    for mount in mounts {
        let spelling = mount.spelling;
        let under = path.starts_with(spelling) && (path.len() == spelling.len() || path[spelling.len()] == b'/' || spelling == b"/");
        if under && best.is_none_or(|(len, _)| spelling.len() > len) {
            best = Some((spelling.len(), mount.root));
        }
    }
    best.map(|(len, root)| {
        let rest = &path[len..];
        (root, rest.strip_prefix(b"/").unwrap_or(rest))
    })
}

/// A decimal `u64`, or `None` (the clock's check: no allocation, no `parse` of std's).
fn parse_u64(text: &str) -> Option<u64> {
    if text.is_empty() || (text.len() > 1 && text.starts_with('0')) {
        return None;
    }
    let mut value: u64 = 0;
    for b in text.bytes() {
        let digit = b.checked_sub(b'0').filter(|d| *d < 10)?;
        value = value.checked_mul(10)?.checked_add(u64::from(digit))?;
    }
    Some(value)
}

/// What a run read, and how that was learned.
pub enum Reads<'a> {
    /// The program's own report, every path in it under its roots.
    Exact(Checked<'a>),
    /// No report (or a crash, whose report is not whole): every admitted file of every package
    /// root, each with its digest.
    WholeRoots(Checked<'a>),
}

impl<'a> Reads<'a> {
    /// The checked list, however it was learned.
    pub fn checked(&self) -> &Checked<'a> {
        match self {
            Reads::Exact(checked) | Reads::WholeRoots(checked) => checked,
        }
    }
}

/// One file the run left in its output root.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Output<'o> {
    pub path: RelPath<'o>,
    pub digest: Digest,
    pub len: u64,
}

/// The output root's files, in byte order of their paths.
pub struct Outputs<'a> {
    text: AVec<'a, u8>,
    rows: AVec<'a, (u32, u32, Digest, u64)>,
}

impl<'a> Outputs<'a> {
    pub(crate) fn new(alloc: Alloc<'a>) -> Outputs<'a> {
        Outputs { text: AVec::new_in(alloc), rows: AVec::new_in(alloc) }
    }

    pub(crate) fn push(&mut self, path: RelPath<'_>, digest: Digest, len: u64, most: u32) -> Result<(), Fault> {
        if self.rows.len() >= most as usize {
            return Err(Fault::OutputsOver { most });
        }
        let at = self.text.len() as u32;
        let bytes = path.as_str().as_bytes();
        self.text.try_extend_from_slice(bytes).map_err(|_| Fault::OutOfMemory)?;
        self.rows.try_push((at, bytes.len() as u32, digest, len)).map_err(|_| Fault::OutOfMemory)?;
        Ok(())
    }

    pub(crate) fn sort(&mut self) {
        let text = &self.text;
        self.rows.sort_unstable_by(|a, b| {
            text[a.0 as usize..(a.0 + a.1) as usize].cmp(&text[b.0 as usize..(b.0 + b.1) as usize])
        });
    }

    pub(crate) fn clear(&mut self) {
        self.text.clear();
        self.rows.clear();
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Each file, in byte order of its path.
    pub fn entries(&self) -> impl Iterator<Item = Output<'_>> + '_ {
        self.rows.iter().map(|&(at, len, digest, size)| {
            let bytes = &self.text[at as usize..(at + len) as usize];
            // The text was a `RelPath`'s when pushed: valid by construction.
            let path = RelPath::from_bytes(bytes, u32::MAX).expect("blaze-std hermetic outputs: a row's text was a `RelPath` when pushed");
            Output { path, digest, len: size }
        })
    }

    /// The listing's identity: BLAKE3 over each (path, NUL, digest, length), in order.
    pub fn digest(&self) -> Digest {
        let mut hasher = crate::digest::Blake3::new();
        for output in self.entries() {
            hasher.update(output.path.as_str().as_bytes());
            hasher.update(&[0]);
            hasher.update(&output.digest.0);
            hasher.update(&output.len.to_le_bytes());
        }
        Digest(hasher.finalize())
    }
}

/// What came back of a run that ended.
pub struct HermeticRun<'a> {
    pub exit: Exit,
    /// The output root's files (empty when the child was ended by a signal: what it left is
    /// partial and removed).
    pub outputs: Outputs<'a>,
    pub stdout: AVec<'a, u8>,
    pub stderr: AVec<'a, u8>,
    pub reads: Reads<'a>,
}

/// The rule a refusal is by.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Rule {
    /// 9 sandbox-everything: where no sandbox can be made, blaze stops.
    SandboxEverything,
    /// 18 bounded-progress: a deadline, a budget, a cap.
    BoundedProgress,
    /// 21 keys-cover-every-input: a read outside the roots leaves the run with no whole key.
    KeysCoverEveryInput,
    /// 25 deterministic-output: one spec has one spelling.
    DeterministicOutput,
    /// 75 nothing-ambient: the clock and the environment are values of the spec.
    NothingAmbient,
    /// 3 no-fallback: a host call that failed, named.
    NoFallback,
}

/// Why a run stopped before its end.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Stop {
    /// The spec's deadline passed.
    Deadline,
    /// A cap of the budget was passed.
    Budget(Over),
    /// A kept stream passed its cap.
    Stream { stream: Stream, most: u32 },
}

/// Which cap of the [`Budget`] was passed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Over {
    Cpu { used: Duration, most: Duration },
    Memory { used: u64, most: u64 },
    Steps { used: u64, most: u64 },
}

/// What failed.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Fault {
    /// No backend gives `level` on this host: `backend` is absent, for `why`.
    LevelUnavailable { level: Level, backend: &'static str, why: Absent },
    /// This OS has no [`Level::Inputs`] backend.
    NoBackend { os: &'static str },
    /// A start was asked with no plan to confine the child by: refused, never run unconfined.
    StartUnconfined,
    /// Linux: the kernel's Landlock ABI is older than the least that confines a child.
    LandlockAbi { abi: u32, least: u32 },
    /// A [`Program::Records`] at [`Level::Inputs`]: only blaze's launcher runs code records.
    RecordsNeedExecution,
    EnvSpelling { at: u32 },
    EnvNotSorted { at: u32 },
    /// The clock and the env's `SOURCE_DATE_EPOCH` disagree ([`ClockSpec`]).
    ClockNotInEnv,
    StepsNotMetered,
    NoOutputRoot,
    Unmounted { root: RootId },
    MountsRefused,
    CwdUnmounted,
    /// A helper folder (`at`) not under the output root: what runs there would be a host
    /// program no digest names.
    HelperFolderOutside { at: u32 },
    /// A channel (`at`) under a mount, or not absolute ([`HermeticOpts::channels`]).
    ChannelUnderRoot { at: u32 },
    /// A host path in the plan is not one spelling of one absolute path, or holds a byte the
    /// profile cannot quote (a quote, a backslash, a control byte): `at` its rule.
    PathRefused { at: u32 },
    PlanOver { most: u32 },
    /// The run was stopped; nothing it wrote is kept.
    Stopped(Stop),
    /// A read the report names is under no mount: outside every root.
    ReadUnmounted(Unmounted),
    /// A read is under a mount and outside what its root admits.
    ReadOutside(Outside),
    ReadListFull(Full),
    Report(ReportError),
    OutputsOver { most: u32 },
    /// The caller's files ([`RootFiles`]) failed.
    Files(FilesError),
    /// A host call failed (rule 3: named, never retried).
    Host { call: &'static str, code: u16 },
    /// A start or a wait of blaze's linked child failed.
    Run(super::RunError),
    OutOfMemory,
    /// An event the machine's state does not take: a driver's bug, named.
    Machine { state: State, event: &'static str },
}

impl Fault {
    /// The rule this fault is a refusal by.
    pub fn rule(&self) -> Rule {
        match self {
            Fault::LevelUnavailable { .. }
            | Fault::NoBackend { .. }
            | Fault::StartUnconfined
            | Fault::LandlockAbi { .. }
            | Fault::RecordsNeedExecution => {
                Rule::SandboxEverything
            }
            Fault::PathRefused { .. }
            | Fault::Unmounted { .. }
            | Fault::MountsRefused
            | Fault::CwdUnmounted
            | Fault::NoOutputRoot
            | Fault::HelperFolderOutside { .. }
            | Fault::ChannelUnderRoot { .. } => {
                Rule::SandboxEverything
            }
            Fault::EnvSpelling { .. } | Fault::EnvNotSorted { .. } => Rule::DeterministicOutput,
            Fault::ClockNotInEnv => Rule::NothingAmbient,
            Fault::StepsNotMetered | Fault::Stopped(_) | Fault::PlanOver { .. } | Fault::OutputsOver { .. } | Fault::ReadListFull(_) => {
                Rule::BoundedProgress
            }
            Fault::ReadUnmounted(_) | Fault::ReadOutside(_) | Fault::Report(_) => Rule::KeysCoverEveryInput,
            Fault::Files(_) | Fault::Host { .. } | Fault::Run(_) | Fault::OutOfMemory | Fault::Machine { .. } => Rule::NoFallback,
        }
    }
}

/// A hermetic run refused or failed: the program, the rule and what.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct HermeticError<'a> {
    pub program: &'a str,
    pub rule: Rule,
    pub fault: Fault,
}

impl<'a> HermeticError<'a> {
    pub fn new(program: &'a str, fault: Fault) -> HermeticError<'a> {
        HermeticError { program, rule: fault.rule(), fault }
    }
}

impl core::fmt::Display for HermeticError<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}: a hermetic run was refused by {:?}: {:?}", self.program, self.rule, self.fault)
    }
}

/// The planted bugs of the proof (tests.rs): shipped code passes `NoPlant`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HermeticPlant {
    None,
    /// The plan lets the child read the whole host (`/`): a read outside the roots succeeds.
    HoleInPlan,
    /// The read report is not checked against the roots: an undeclared read is accepted.
    ReportUnchecked,
    /// The child is given the host's `PATH` beside the spec's pairs.
    EnvInherited,
    /// The child's `SOURCE_DATE_EPOCH` is the driver's clock, not the spec's.
    ClockLeaks,
    /// A stopped run's output folder is not cleared.
    PartialKept,
    /// The read list loses the report's last read.
    ReadDropped,
    /// The child is started without its confinement (the start's confine step dropped).
    ConfineDropped,
}

impl blaze_own::planted::Plants for HermeticPlant {}

/// Which side of the caller's files failed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FilesOp {
    PackageFiles,
    Seen,
    ReadOutput,
    ListOutput,
    ClearOutput,
}

/// The caller's files failed ([`RootFiles`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FilesError {
    pub op: FilesOp,
    pub root: RootId,
    /// The OS's error number where there was one, else 0.
    pub code: u16,
}

/// The roots' files, as their owner (the store, a workspace's record view) knows them: the run
/// names files by root and relative path and never opens a host path of the caller's itself.
pub trait RootFiles {
    /// Every file of the package root `root` its globs admit, in byte order of path.
    fn package_files(&self, root: RootId, each: &mut dyn FnMut(RelPath<'_>, Digest, u64) -> Result<(), Fault>) -> Result<(), Fault>;
    /// What `path` under `root` is now.
    fn seen(&self, root: RootId, path: RelPath<'_>) -> Result<Seen, Fault>;
    /// The bytes of `path` under the output root (a read report), at most `most`.
    fn read_output(&self, path: RelPath<'_>, most: u32, into: &mut AVec<'_, u8>) -> Result<(), Fault>;
    /// Every file under the output root, in any order, with its digest and length.
    fn list_output(&self, each: &mut dyn FnMut(RelPath<'_>, Digest, u64) -> Result<(), Fault>) -> Result<(), Fault>;
    /// Everything under the output root removed (a stopped or crashed run's partial output).
    fn clear_output(&self) -> Result<(), Fault>;
}

/// What a backend starts: the spec, the plan the child is confined by and the child's
/// environment. `confine` is `None` only under the planted bug [`HermeticPlant::ConfineDropped`]:
/// a native port refuses it ([`Fault::StartUnconfined`]); the simulation runs the child
/// unconfined and records it, which its oracle catches.
pub struct StartView<'v> {
    pub spec: &'v HermeticSpec<'v>,
    pub confine: Option<&'v Plan<'v>>,
    pub env: &'v [(&'v str, &'v str)],
}

impl<'v> StartView<'v> {
    /// The plan to confine by, or [`Fault::StartUnconfined`]: no port starts a child without one.
    pub fn plan(&self) -> Result<&'v Plan<'v>, Fault> {
        self.confine.ok_or(Fault::StartUnconfined)
    }
}

/// One OS's side of a run: it starts the child confined by the plan, and reports what the
/// child does as [`Event`]s. A native backend's or the simulation's.
pub trait Port {
    /// The driver's clock.
    fn now(&self) -> Duration;
    fn start(&mut self, start: &StartView<'_>) -> Result<(), Fault>;
    /// Until `until` or the child's next happening (bytes, a closed stream, its end).
    fn wait(&mut self, until: Duration) -> Result<(), Fault>;
    /// The child's use so far, as an [`Event::Usage`].
    fn meter(&mut self) -> Result<(), Fault>;
    /// The child's whole group killed; its end follows as [`Event::Exited`].
    fn kill(&mut self) -> Result<(), Fault>;
    /// The next event the port has, or `None`. Every [`Event::Bytes`] of the child comes
    /// before its [`Event::Exited`].
    fn next_event(&mut self) -> Option<Event<'_>>;
}

/// The most bytes of a read report.
pub const MOST_REPORT: u32 = 16 << 20;

/// The loop: the machine's effects to `port` and `files`, their events back, until it ends.
pub fn drive<'a, Pt: Port, P: blaze_own::planted::Planted<HermeticPlant>>(
    machine: &mut HermeticMachine<'a, P>,
    port: &mut Pt,
    plan: &Plan<'_>,
    files: &dyn RootFiles,
    alloc: Alloc<'a>,
) -> Result<(), Fault> {
    let spec = machine.spec();
    machine.start(port.now())?;
    loop {
        let now = port.now();
        while let Some(event) = port.next_event() {
            machine.step(now, event)?;
        }
        if machine.state() == State::Ended {
            return Ok(());
        }
        match machine.next_effect() {
            Some(Effect::Start) => {
                {
                    let mut env = AVec::new_in(alloc);
                    for pair in machine.env() {
                        env.try_push(pair).map_err(|_| Fault::OutOfMemory)?;
                    }
                    let confine = if machine.drops_confinement() { None } else { Some(plan) };
                    port.start(&StartView { spec, confine, env: &env })?;
                }
                machine.step(port.now(), Event::Started)?;
            }
            Some(Effect::Meter) => port.meter()?,
            Some(Effect::Kill) => port.kill()?,
            Some(Effect::ReadReport) => report::read(machine, spec, files, now, alloc)?,
            Some(Effect::ReadWholeRoots) => {
                for at in 0..spec.roots().len() {
                    let root = RootId(at as u8);
                    if matches!(spec.roots().get(root).map(|r| r.kind), Some(blaze_roots::RootKind::Package { .. })) {
                        files.package_files(root, &mut |path, digest, len| {
                            machine.step(now, Event::Read { root, path, seen: Seen::File { digest, len } })
                        })?;
                    }
                }
                machine.step(now, Event::ReportEnd)?;
            }
            Some(Effect::ListOutputs) => {
                files.list_output(&mut |path, digest, len| machine.step(now, Event::Output { path, digest, len }))?;
                machine.step(now, Event::OutputsEnd)?;
            }
            Some(Effect::DiscardOutputs) => {
                if !machine.keeps_partial() {
                    files.clear_output()?;
                }
                machine.step(now, Event::Discarded)?;
            }
            None => {
                let until = machine.next_deadline().ok_or(Fault::Machine { state: machine.state(), event: "no effect and no deadline" })?;
                port.wait(until)?;
                machine.step(port.now(), Event::Tick)?;
            }
        }
    }
}

/// The capability to run programs hermetically: made from the [`Spawner`] the program's
/// `main` was granted (`host.spawner()`), and handed down; nothing starts a child without it.
#[derive(Clone)]
pub struct Hermetic {
    #[cfg_attr(
        not(any(target_os = "linux", target_os = "macos")),
        expect(dead_code, reason = "only the Linux and macOS backends start a child")
    )]
    spawner: Spawner,
    /// The driver's clock, the deadline's and the meter's base (a grant, never read ambiently).
    #[cfg_attr(
        not(any(target_os = "linux", target_os = "macos")),
        expect(dead_code, reason = "only the Linux and macOS backends start a child")
    )]
    clock: crate::clock::Clock,
}

/// The fields of a [`Hermetic`]: the grants it is made from.
pub struct HermeticCapOpts {
    pub spawner: Spawner,
    pub clock: crate::clock::Clock,
}

impl Hermetic {
    pub fn new(opts: HermeticCapOpts) -> Hermetic {
        Hermetic { spawner: opts.spawner, clock: opts.clock }
    }

    /// `spec` run at its level, with `files` its roots' owner, everything that comes back in
    /// `alloc`. [`Level::Execution`] is refused today, naming the absent backends.
    pub fn run<'a>(&self, cx: &Cx, spec: &'a HermeticSpec<'a>, files: &dyn RootFiles, alloc: Alloc<'a>) -> Result<HermeticRun<'a>, HermeticError<'a>> {
        let name = spec.program().name();
        match spec.level() {
            Level::Execution => {
                for backend in EXECUTION_BACKENDS {
                    if let Some(why) = backend.absent() {
                        return Err(HermeticError::new(name, Fault::LevelUnavailable { level: Level::Execution, backend: backend.name(), why }));
                    }
                }
                Err(HermeticError::new(name, Fault::NoBackend { os: "any" }))
            }
            Level::Inputs => {
                if let Program::Records { .. } = spec.program() {
                    return Err(HermeticError::new(name, Fault::RecordsNeedExecution));
                }
                self.run_inputs(cx, spec, files, alloc)
            }
        }
    }

    #[cfg(target_os = "macos")]
    fn run_inputs<'a>(&self, cx: &Cx, spec: &'a HermeticSpec<'a>, files: &dyn RootFiles, alloc: Alloc<'a>) -> Result<HermeticRun<'a>, HermeticError<'a>> {
        seatbelt::run(&self.spawner, &self.clock, cx, spec, files, alloc)
    }

    #[cfg(target_os = "linux")]
    fn run_inputs<'a>(&self, cx: &Cx, spec: &'a HermeticSpec<'a>, files: &dyn RootFiles, alloc: Alloc<'a>) -> Result<HermeticRun<'a>, HermeticError<'a>> {
        landlock::run(&self.spawner, &self.clock, cx, spec, files, alloc)
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    fn run_inputs<'a>(&self, _cx: &Cx, spec: &'a HermeticSpec<'a>, _files: &dyn RootFiles, _alloc: Alloc<'a>) -> Result<HermeticRun<'a>, HermeticError<'a>> {
        Err(HermeticError::new(spec.program().name(), Fault::NoBackend { os: "neither Linux nor macOS" }))
    }
}

/// The run of `spec` through `port`: plan, machine, loop, result. Every backend and the
/// simulation call this, so a run has one path from spec to result.
pub fn run_through<'a, Pt: Port, P: blaze_own::planted::Planted<HermeticPlant>>(
    spec: &'a HermeticSpec<'a>,
    port: &mut Pt,
    files: &dyn RootFiles,
    system: &[&str],
    alloc: Alloc<'a>,
    plant: P,
) -> Result<HermeticRun<'a>, HermeticError<'a>> {
    let name = spec.program().name();
    let plan = Plan::new(PlanOpts { spec, system, files, alloc }, plant).map_err(|fault| HermeticError::new(name, fault))?;
    let mut machine = HermeticMachine::new(MachineOpts { spec, alloc, plant });
    let driven = drive(&mut machine, port, &plan, files, alloc);
    if let Err(fault) = driven {
        // The child, if it runs, is killed by the port's drop (blaze-core kills a linked
        // child's group when it is dropped unreaped).
        return Err(HermeticError::new(name, fault));
    }
    machine.finish().map_err(|fault| HermeticError::new(name, fault))
}

/// What this host's backend confines `spec`'s child by, as text, rebuilt from the spec the way
/// the run builds it (one plan, one rendering): on macOS the SBPL profile `sandbox-exec -p` is
/// given; on Linux one line a Landlock rule, then the limits and whether processes may start. A
/// failed run's error carries its [`confinement_digest`], and a test or a driver prints this
/// text with it (rule 19: what confined a child that said nothing is part of what happened).
pub fn confinement<'a, 's>(spec: &'s HermeticSpec<'s>, files: &'s dyn RootFiles, alloc: Alloc<'a>) -> Result<AVec<'a, u8>, Fault> {
    #[cfg(target_os = "macos")]
    {
        let plan = Plan::new(PlanOpts { spec, system: &seatbelt::SYSTEM, files, alloc }, blaze_own::planted::NoPlant)?;
        let mut text = seatbelt::profile(&plan, alloc)?;
        // The NUL that ends the C string (always the last byte); the text is what is before it.
        text.truncate(text.len() - 1);
        Ok(text)
    }
    #[cfg(target_os = "linux")]
    {
        let plan = Plan::new(PlanOpts { spec, system: &landlock::SYSTEM, files, alloc }, blaze_own::planted::NoPlant)?;
        let mut text = AVec::new_in(alloc);
        let mut put = |bytes: &[u8]| text.try_extend_from_slice(bytes).map_err(|_| Fault::OutOfMemory);
        for rule in plan.rules() {
            let access: &[u8] = match rule.access {
                Access::Exec => b"exec ",
                Access::Read => b"read ",
                Access::Write => b"write",
            };
            put(access)?;
            let extent: &[u8] = if rule.subtree { b" beneath " } else { b" file    " };
            put(extent)?;
            put(rule.path.as_bytes())?;
            put(b"\n")?;
        }
        let budget = spec.budget();
        let mut digits = [0u8; 20];
        put(b"RLIMIT_CPU ")?;
        // As landlock.rs's `prepare` sets it: whole seconds, rounded up, at least one.
        let seconds = budget.cpu.as_secs() + u64::from(budget.cpu.subsec_nanos() > 0);
        put(machine::decimal(seconds.max(1), &mut digits).as_bytes())?;
        put(b" s each process; RLIMIT_AS ")?;
        put(machine::decimal(budget.memory, &mut digits).as_bytes())?;
        put(b" bytes each process; RLIMIT_CORE 0\n")?;
        let processes: &[u8] = if spec.starts_programs() { b"processes: may start the helpers\n" } else { b"processes: none (fork and clone refused)\n" };
        put(processes)?;
        Ok(text)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (spec, files, alloc);
        Err(Fault::NoBackend { os: "neither Linux nor macOS" })
    }
}

/// The identity of a [`confinement`] text: BLAKE3 of its bytes.
pub fn confinement_digest(text: &[u8]) -> Digest {
    let mut hasher = crate::digest::Blake3::new();
    hasher.update(text);
    Digest(hasher.finalize())
}
