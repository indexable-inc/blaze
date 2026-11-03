//! Where a task's memory is measured (docs/v2/plan/lanes/core.md section 4, "Memory-budget
//! admission").
//!
//! [`Counting`] is the daemon's global allocator: it counts the bytes the process holds
//! ([`held`]) and, per thread, the bytes the task the thread runs now holds, so [`measure`]
//! says what a task held at most through the global allocator; what it held in its frame
//! ([`blaze_mem::frame::task`]) is [`blaze_mem::frame::measure`]'s, and the frames' cap is
//! [`blaze_mem::frame::Cap`]. [`machine_budget`] is the memory the plane may grant: the
//! machine's limit, less what others use in blaze's own cgroup and a margin.

use std::{
    cell::Cell,
    fmt, io,
    path::Path,
    sync::atomic::{AtomicI64, Ordering},
};

use crate::Budget;

/// How far a thread's count may move before it is added to [`HELD`] (Linux's
/// `percpu_counter`): an allocation then costs a few thread-local adds and no atomic, and
/// [`HELD`] is written once per this many bytes of a thread's drift.
const BATCH: i64 = 256 << 10;

#[repr(align(128))]
struct Total(AtomicI64);

/// What every thread added of its count: the bytes the process holds through [`Counting`],
/// but for less than [`BATCH`] per thread not yet added.
static HELD: Total = Total(AtomicI64::new(0));

/// The most [`HELD`] was since [`high_water_from_now`] last began a watch: what a task that ran
/// alone held at most on every thread, its own and those it handed work to.
static HIGH: Total = Total(AtomicI64::new(0));

/// A thread's counts, in one block so an allocation reaches its thread-local storage once.
struct Local {
    /// The bytes this thread allocated less those it freed, through [`Counting`].
    net: Cell<i64>,
    /// The most `net` was since the innermost [`measure`] on this thread began.
    peak: Cell<i64>,
    /// What `net` moved since it was last added to [`HELD`].
    unflushed: Cell<i64>,
    /// How far `unflushed` may go before it is added: 0 until the flush at the thread's exit is
    /// armed, and again once it ran, so no count is lost with the thread.
    batch: Cell<i64>,
    armed: Cell<bool>,
    #[cfg(test)]
    flushed: Cell<i64>,
}

// `LOCAL` is `const` and without a destructor: reading it allocates nothing and works while a
// thread ends, as an allocator must. `EXIT` has one, registered from the cold `flush` only.
thread_local! {
    static LOCAL: Local = const {
        Local {
            net: Cell::new(0),
            peak: Cell::new(0),
            unflushed: Cell::new(0),
            batch: Cell::new(0),
            armed: Cell::new(false),
            #[cfg(test)]
            flushed: Cell::new(0),
        }
    };
    static EXIT: Exit = const { Exit };
}

/// [`LOCAL`]'s one reader. Not inlined: the allocator counts through it from a job's code,
/// which may wait and resume on another thread, and an inlined read would let the job keep
/// the first thread's `Local` across the wait and count into it from the second, a data race
/// on its cells (rule 24, as pool.rs's caches were in checkpoint 36). One direct call per
/// allocation.
#[inline(never)]
fn with_local<R>(f: impl FnOnce(&Local) -> R) -> R {
    LOCAL.with(f)
}

struct Exit;

impl Drop for Exit {
    fn drop(&mut self) {
        with_local(exit_flush);
    }
}

/// Adds what is left of the thread's count to [`HELD`], and from then on every change at
/// once: what the thread's other destructors free still counts. What it added.
fn exit_flush(local: &Local) -> i64 {
    local.batch.set(0);
    let left = local.unflushed.replace(0);
    add(local, left);
    left
}

#[cfg_attr(not(test), allow(unused_variables))]
fn add(local: &Local, bytes: i64) {
    let now = HELD.0.fetch_add(bytes, Ordering::Relaxed).wrapping_add(bytes);
    if bytes > 0 {
        HIGH.0.fetch_max(now, Ordering::Relaxed);
    }
    #[cfg(test)]
    local.flushed.set(local.flushed.get().wrapping_add(bytes));
}

/// Per allocation: one thread-local block, three adds and two well-predicted branches; the
/// atomic add is in [`flush`], once per [`BATCH`] bytes of drift.
#[inline(always)]
fn count(delta: i64) {
    with_local(|local| {
        let net = local.net.get().wrapping_add(delta);
        local.net.set(net);
        if net > local.peak.get() {
            local.peak.set(net);
        }
        let unflushed = local.unflushed.get().wrapping_add(delta);
        if unflushed.unsigned_abs() < local.batch.get().unsigned_abs() {
            local.unflushed.set(unflushed);
        } else {
            flush(local, unflushed);
        }
    });
}

#[cold]
#[inline(never)]
fn flush(local: &Local, unflushed: i64) {
    local.unflushed.set(0);
    add(local, unflushed);
    // Registering `EXIT`'s destructor may allocate, which comes back here with `armed` set
    // and `batch` still 0: it adds its own bytes and registers nothing.
    if !local.armed.replace(true) && EXIT.try_with(|_| {}).is_ok() {
        local.batch.set(BATCH);
    }
}

/// A global allocator that counts what goes through `A`: the daemon's binary installs it over
/// the allocator it uses (`#[global_allocator] static ALLOC: Counting<Jemalloc> =
/// Counting(Jemalloc);`). It takes no lock, allocates nothing of its own and does no atomic
/// write on an allocation but once per [`BATCH`] bytes a thread's count moves.
///
/// A byte freed on another thread than the one that allocated it counts against the thread
/// that frees it: [`measure`] sees what a task's thread allocated and freed, exactly; [`held`]
/// is within [`BATCH`] per other thread of what the process holds.
pub struct Counting<A>(pub A);

fn signed(bytes: usize) -> i64 {
    // A layout's size is at most `isize::MAX`.
    bytes as i64
}

/// The allocator hook: `GlobalAlloc` is an unsafe trait, and this is the one way to see every
/// allocation (a safe wrapper type would see only the allocations made through it).
#[allow(unsafe_code)]
mod global {
    use std::alloc::{GlobalAlloc, Layout};

    use super::{Counting, count, signed};

    // SAFETY: every call goes to `A` with the caller's arguments, so `A`'s guarantees hold; the
    // counting beside it touches an atomic and `LOCAL`, a `const` thread-local with no
    // destructor, and registers `EXIT`'s destructor once per thread, whose own allocations
    // `flush` takes without registering again. An allocation that recursed into itself here
    // would deadlock or overflow the stack.
    unsafe impl<A: GlobalAlloc> GlobalAlloc for Counting<A> {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            // SAFETY: the caller upholds `alloc`'s contract for `layout`.
            let ptr = unsafe { self.0.alloc(layout) };
            if !ptr.is_null() {
                count(signed(layout.size()));
            }
            ptr
        }

        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            // SAFETY: the caller upholds `alloc_zeroed`'s contract for `layout`.
            let ptr = unsafe { self.0.alloc_zeroed(layout) };
            if !ptr.is_null() {
                count(signed(layout.size()));
            }
            ptr
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            // SAFETY: the caller says `ptr` was allocated by this allocator with `layout`.
            unsafe { self.0.dealloc(ptr, layout) };
            count(-signed(layout.size()));
        }

        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            // SAFETY: the caller upholds `realloc`'s contract for `ptr`, `layout` and `new_size`.
            let new = unsafe { self.0.realloc(ptr, layout, new_size) };
            if !new.is_null() {
                count(signed(new_size) - signed(layout.size()));
            }
            new
        }
    }
}

/// Runs `f` and says the most bytes it held at once above what its thread held when it began,
/// what it freed counted against what it allocated. Measures nest: an inner one's peak is part
/// of the outer one's. 0 when [`Counting`] is not the global allocator.
pub fn measure<R>(f: impl FnOnce() -> R) -> (R, u64) {
    struct Restore {
        outer: i64,
    }
    impl Drop for Restore {
        // On a panic too: the outer measure still sees what `f` held.
        fn drop(&mut self) {
            with_local(|local| local.peak.set(self.outer.max(local.peak.get())));
        }
    }
    let (base, outer) = with_local(|local| {
        let base = local.net.get();
        (base, local.peak.replace(base))
    });
    let restore = Restore { outer };
    let result = f();
    let peak = with_local(|local| local.peak.get());
    drop(restore);
    (result, u64::try_from(peak.saturating_sub(base)).unwrap_or(0))
}

/// The bytes the process holds now through [`Counting`]; 0 when it is not the global
/// allocator. One load: exact for what the calling thread did, within [`BATCH`] for each other
/// thread.
pub fn held() -> u64 {
    let own = with_local(|local| local.unflushed.get());
    u64::try_from(HELD.0.load(Ordering::Relaxed).wrapping_add(own)).unwrap_or(0)
}

/// Begins a watch of the most the process holds: [`high_water`] from now on is at least what
/// it holds now. What it holds now. One watch at a time: the plane's task that runs alone
/// begins it, and nothing else does.
pub fn high_water_from_now() -> u64 {
    let now = held();
    HIGH.0.store(i64::try_from(now).unwrap_or(i64::MAX), Ordering::Relaxed);
    now
}

/// The most the process held through [`Counting`] since [`high_water_from_now`], within
/// [`BATCH`] per thread: what the threads add to [`HELD`], which a thread does once per
/// [`BATCH`] of drift, so a watch costs an allocation nothing.
pub fn high_water() -> u64 {
    u64::try_from(HIGH.0.load(Ordering::Relaxed)).unwrap_or(0).max(held())
}

/// Everything blaze itself holds: [`held`] (what goes through the global allocator) and the
/// pages `blaze_mem` committed for its regions, frames, chunks and pools, which it maps itself
/// and [`Counting`] never sees ([`blaze_mem::tag::Snapshot::committed`]).
pub fn own() -> u64 {
    held().saturating_add(blaze_mem::tag::snapshot().committed)
}

/// The memory the plane may grant: the machine's limit, less what others use within it, less
/// `margin`.
///
/// The limit is the least of what the machine has and what the process may use: on Linux
/// `/proc/meminfo`'s `MemTotal`, the `memory.max` of the process's cgroup and of each one above
/// it (cgroup v2; v1's `memory.limit_in_bytes`), and `RLIMIT_AS`; on macOS `sysctl hw.memsize`
/// and `RLIMIT_AS`.
///
/// What others use is what is in use in blaze's own cgroup (v2's `memory.current` of the group
/// `/proc/self/cgroup` names; v1's `memory.usage_in_bytes` where the memory controller is
/// v1's) less what blaze itself holds ([`own`]: [`held`], 0 when [`Counting`] is not the global
/// allocator, so the budget is then the smaller, and the pages `blaze_mem` committed). It is not the machine's `MemTotal` less
/// `MemAvailable`: that counts every process outside the cgroup against a limit they do not
/// share, and a small cgroup on a busy machine would refuse builds that fit. The root cgroup
/// keeps no count, and nothing is taken off there; nor on macOS, which has no cgroups.
///
/// The budget's `from` names the limit and what was taken off. An error where one cannot be
/// read or nothing is left, never a guess.
pub fn machine_budget(margin: u64) -> io::Result<Budget> {
    let mut limits = machine_limits()?;
    #[cfg(unix)]
    // alloc: the budget's name, once per daemon start
    limits.extend(rlimit_as()?.map(|bytes| (bytes, "the process's RLIMIT_AS".to_owned())));
    let in_use = machine_in_use(own())?;
    least(limits, in_use.as_ref(), margin)
}

#[cfg(target_os = "linux")]
fn machine_in_use(own: u64) -> io::Result<Option<(u64, String)>> {
    linux_in_use(Path::new("/"), own)
}

#[cfg(not(target_os = "linux"))]
fn machine_in_use(_own: u64) -> io::Result<Option<(u64, String)>> {
    Ok(None)
}

#[cfg(target_os = "linux")]
fn machine_limits() -> io::Result<Vec<(u64, String)>> {
    linux_limits(Path::new("/"))
}

#[cfg(target_os = "macos")]
use sys::machine_limits;
#[cfg(unix)]
use sys::rlimit_as;

/// The kernel's word on the machine's memory: two calls that write into a buffer this passes,
/// which only `libc`'s raw functions reach.
#[cfg(unix)]
#[allow(unsafe_code)]
mod sys {
    use std::io;

    #[cfg(target_os = "macos")]
    pub(super) fn machine_limits() -> io::Result<Vec<(u64, String)>> {
        let mut bytes: u64 = 0;
        let mut len = std::mem::size_of::<u64>();
        // SAFETY: the name is a C string that outlives the call, and `bytes` and `len` say a
        // buffer of `len` bytes; a `len` larger than `bytes` would let the kernel write past it.
        let status = unsafe {
            libc::sysctlbyname(
                c"hw.memsize".as_ptr(),
                (&raw mut bytes).cast(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        if status != 0 {
            let why = io::Error::last_os_error();
            // alloc: the error's message
            return Err(io::Error::new(why.kind(), format!("sysctl hw.memsize: {why}")));
        }
        if len != std::mem::size_of::<u64>() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                // alloc: the error's message
                format!("sysctl hw.memsize gave {len} bytes, not 8"),
            ));
        }
        // alloc: the limits and their names, once per daemon start
        Ok(vec![(bytes, "sysctl hw.memsize".to_owned())])
    }

    pub(super) fn rlimit_as() -> io::Result<Option<u64>> {
        let mut limit = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
        // SAFETY: `limit` is a `rlimit` the kernel writes and nothing else borrows.
        if unsafe { libc::getrlimit(libc::RLIMIT_AS, &mut limit) } != 0 {
            let why = io::Error::last_os_error();
            // alloc: the error's message
            return Err(io::Error::new(why.kind(), format!("getrlimit(RLIMIT_AS): {why}")));
        }
        let current: u64 = limit.rlim_cur;
        Ok((limit.rlim_cur != libc::RLIM_INFINITY).then_some(current))
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn machine_limits() -> io::Result<Vec<(u64, String)>> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        // alloc: the error's message
        format!(
            "blaze reads the machine's memory only on Linux and macOS, not {}",
            std::env::consts::OS
        ),
    ))
}

fn invalid(path: &Path, why: impl fmt::Display) -> io::Error {
    // alloc: the error's message
    io::Error::new(io::ErrorKind::InvalidData, format!("{}: {why}", path.display()))
}

fn read(path: &Path) -> io::Result<String> {
    std::fs::read_to_string(path)
        // alloc: the error's message
        .map_err(|why| io::Error::new(why.kind(), format!("{}: {why}", path.display())))
}

/// The memory limits of a Linux machine whose file system is at `root`: `MemTotal`, and each
/// cgroup memory limit over the process.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn linux_limits(root: &Path) -> io::Result<Vec<(u64, String)>> {
    let meminfo = root.join("proc/meminfo");
    let text = read(&meminfo)?;
    let line = text
        .lines()
        .find_map(|it| it.strip_prefix("MemTotal:"))
        .ok_or_else(|| invalid(&meminfo, "no MemTotal line"))?;
    let kib = line
        .trim()
        .strip_suffix("kB")
        .and_then(|it| it.trim().parse::<u64>().ok())
        // alloc: the error's message
        .ok_or_else(|| invalid(&meminfo, format!("MemTotal of `{}`", line.trim())))?;
    let total = kib.checked_mul(1024).ok_or_else(|| invalid(&meminfo, "MemTotal past 2^64"))?;
    // alloc: the limits and their names, once per daemon start
    let mut limits = vec![(total, "MemTotal of /proc/meminfo".to_owned())];
    let cgroup = root.join("proc/self/cgroup");
    for line in read(&cgroup)?.lines().filter(|it| !it.is_empty()) {
        let mut parts = line.splitn(3, ':');
        let (Some(hierarchy), Some(controllers), Some(path)) =
            (parts.next(), parts.next(), parts.next())
        else {
            // alloc: the error's message
            return Err(invalid(&cgroup, format!("a line `{line}`")));
        };
        let (mount, file) = if hierarchy == "0" && controllers.is_empty() {
            ("sys/fs/cgroup", "memory.max")
        } else if controllers.split(',').any(|it| it == "memory") {
            ("sys/fs/cgroup/memory", "memory.limit_in_bytes")
        } else {
            continue;
        };
        let mut at = Some(path);
        while let Some(group) = at {
            let limit = root.join(mount).join(group.trim_start_matches('/')).join(file);
            match std::fs::read_to_string(&limit) {
                Ok(text) if text.trim() == "max" => {}
                Ok(text) => {
                    let bytes = text
                        .trim()
                        .parse::<u64>()
                        // alloc: the error's message
                        .map_err(|_| invalid(&limit, format!("a limit of `{}`", text.trim())))?;
                    // alloc: a limit's name, once per cgroup level per daemon start
                    limits.push((bytes, format!("{file} of cgroup {group}")));
                }
                // A cgroup has no limit file at the root of its hierarchy.
                Err(why) if why.kind() == io::ErrorKind::NotFound => {}
                Err(why) => {
                    // alloc: the error's message
                    return Err(io::Error::new(why.kind(), format!("{}: {why}", limit.display())));
                }
            }
            at = match group.rfind('/') {
                _ if group == "/" || group.is_empty() => None,
                Some(0) => Some("/"),
                Some(slash) => Some(&group[..slash]),
                None => None,
            };
        }
    }
    Ok(limits)
}

/// What others use in the cgroup of the process, on a Linux machine whose file system is at
/// `root`, when blaze itself holds `own`: the bytes and what says so. `None` in the root
/// cgroup, which keeps no count.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn linux_in_use(root: &Path, own: u64) -> io::Result<Option<(u64, String)>> {
    let cgroup = root.join("proc/self/cgroup");
    let (mut v1, mut v2) = (None, None);
    let text = read(&cgroup)?;
    for line in text.lines().filter(|it| !it.is_empty()) {
        let mut parts = line.splitn(3, ':');
        let (Some(hierarchy), Some(controllers), Some(path)) =
            (parts.next(), parts.next(), parts.next())
        else {
            // alloc: the error's message
            return Err(invalid(&cgroup, format!("a line `{line}`")));
        };
        if hierarchy == "0" && controllers.is_empty() {
            v2 = Some(path);
        } else if controllers.split(',').any(|it| it == "memory") {
            v1 = Some(path);
        }
    }
    // Where the memory controller is v1's, the v2 hierarchy counts no memory.
    let (mount, file, group) = match (v1, v2) {
        (Some(group), _) => ("sys/fs/cgroup/memory", "memory.usage_in_bytes", group),
        (None, Some(group)) => ("sys/fs/cgroup", "memory.current", group),
        (None, None) => return Err(invalid(&cgroup, "no cgroup that counts memory")),
    };
    let count = root.join(mount).join(group.trim_start_matches('/')).join(file);
    let current = match std::fs::read_to_string(&count) {
        Ok(text) => text
            .trim()
            .parse::<u64>()
            // alloc: the error's message
            .map_err(|_| invalid(&count, format!("a count of `{}`", text.trim())))?,
        Err(why) if why.kind() == io::ErrorKind::NotFound && group == "/" => return Ok(None),
        Err(why) => {
            // alloc: the error's message
            return Err(io::Error::new(why.kind(), format!("{}: {why}", count.display())));
        }
    };
    let others = current.saturating_sub(own);
    // alloc: the budget's name, once per daemon start
    let from = format!(
        "{} others use in cgroup {group} ({file} {} less blaze's own {})",
        gb(others),
        gb(current),
        gb(own)
    );
    Ok(Some((others, from)))
}

fn gb(bytes: u64) -> String {
    // alloc: a part of the budget's name or an error's message, once per daemon start
    format!("{:.2} GB", bytes as f64 / f64::from(1 << 30))
}

/// The least of `limits`, less what others use (`in_use`) and `margin`, named by what each was.
fn least(
    limits: Vec<(u64, String)>,
    in_use: Option<&(u64, String)>,
    margin: u64,
) -> io::Result<Budget> {
    let (bytes, from) = limits
        .into_iter()
        .reduce(|least, it| if it.0 < least.0 { it } else { least })
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no memory limit was read"))?;
    let others = in_use.as_ref().map_or(0, |it| it.0);
    let taken = match &in_use {
        // alloc: the budget's name, once per daemon start
        Some((_, used)) => format!("{used} and a margin of {}", gb(margin)),
        // alloc: the budget's name, once per daemon start
        None => format!("a margin of {}", gb(margin)),
    };
    match bytes.checked_sub(others).and_then(|it| it.checked_sub(margin)) {
        Some(left) if left > 0 => {
            // alloc: the budget's name, once per daemon start
            Ok(Budget { bytes: left, from: format!("{from}, {}, less {taken}", gb(bytes)) })
        }
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            // alloc: the error's message
            format!("no memory is left of the {} of {from} less {taken}", gb(bytes)),
        )),
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_types, reason = "a test's own bookkeeping, off every hot path")]
mod tests {
    use blaze_ids::rng::SplitMix64;

    use super::*;

    /// `blaze_alloc_count`'s counter under this crate's own: the allocation-budget tests count
    /// calls, the arena's tests bytes held.
    #[global_allocator]
    static ALLOC: Counting<blaze_alloc_count::Counting> = Counting(blaze_alloc_count::Counting);

    const MB: usize = 1 << 20;

    #[test]
    fn a_measure_says_what_its_work_held_at_most() {
        let (held, peak) = measure(|| {
            let big = std::hint::black_box(vec![1u8; 4 * MB]);
            drop(big);
            std::hint::black_box(vec![2u8; MB])
        });
        assert!(peak >= 4 * MB as u64, "{peak}");
        assert!(peak < 5 * MB as u64, "{peak}");
        assert_eq!(held.len(), MB);
        // What was held before it and freed in it lowers what it holds.
        let before = std::hint::black_box(vec![3u8; 4 * MB]);
        let ((), peak) = measure(|| {
            drop(before);
            let again = vec![4u8; MB];
            std::hint::black_box(&again);
        });
        assert!(peak < MB as u64 / 2, "{peak}");
    }

    #[test]
    fn measures_nest() {
        let ((inner, after), outer) = measure(|| {
            let (_, inner) = measure(|| std::hint::black_box(vec![1u8; 8 * MB]).len());
            let (_, after) = measure(|| std::hint::black_box(vec![1u8; MB]).len());
            (inner, after)
        });
        assert!(inner >= 8 * MB as u64, "{inner}");
        assert!(after >= MB as u64 && after < 2 * MB as u64, "{after}");
        assert!(outer >= inner, "{outer} < {inner}");
        // A panic inside a measure leaves the outer one whole.
        let ((), outer) = measure(|| {
            let caught = std::panic::catch_unwind(|| {
                measure(|| {
                    let big = std::hint::black_box(vec![1u8; 6 * MB]);
                    if !big.is_empty() {
                        panic!("on purpose");
                    }
                })
            });
            assert!(caught.is_err());
        });
        assert!(outer >= 6 * MB as u64, "{outer}");
    }

    #[test]
    fn held_counts_what_the_process_holds() {
        let big = vec![1u8; 32 * MB];
        assert!(held() >= 32 * MB as u64, "{}", held());
        std::hint::black_box(&big);
    }

    /// A watch sees the most every thread held at once, the threads a task hands its work to
    /// as well as its own, though they freed it before the watch is read: what [`measure`] on
    /// the task's thread misses. Other tests allocate and free meanwhile, so it asks for a
    /// quarter of what the threads held together.
    #[test]
    fn a_watch_sees_what_every_thread_held_at_most() {
        let start = high_water_from_now();
        let together = std::sync::Barrier::new(4);
        let ((), own) = measure(|| {
            std::thread::scope(|scope| {
                for _ in 0..4 {
                    scope.spawn(|| {
                        let held = std::hint::black_box(vec![1u8; 32 * MB]);
                        together.wait();
                        drop(held);
                    });
                }
            });
        });
        let most = high_water().saturating_sub(start);
        assert!(own < 32 * MB as u64, "the task's own thread held {own}");
        assert!(most >= 32 * MB as u64, "the watch saw {most}");
    }

    /// What blaze holds counts the pages `blaze_mem` maps itself, which the global allocator
    /// never sees, beside what goes through it.
    #[test]
    fn blaze_holds_what_blaze_mem_committed_too() {
        let mut column = blaze_mem::RegionVec::<u64>::new(4 * MB, blaze_mem::Tag::Plane).unwrap();
        column.extend_from_slice(&vec![7u64; MB]);
        let committed = blaze_mem::tag::snapshot().committed;
        assert!(committed >= 8 * MB as u64, "{committed}");
        assert!(own() >= 8 * MB as u64, "{}", own());
        std::hint::black_box(&column);
    }

    /// A thread's count reaches the total in batches: what it added plus what it has not yet
    /// is exactly what it allocated less what it freed, what it has not stays under a batch,
    /// and at its exit the rest is added, after which every change is added at once.
    #[test]
    fn a_thread_adds_its_count_in_batches_and_the_rest_at_exit() {
        std::thread::spawn(|| {
            std::hint::black_box(vec![0u8; 16]);
            assert_eq!(LOCAL.with(|it| it.batch.get()), BATCH);
            let mut rng = SplitMix64::new(7);
            let mut kept: Vec<Vec<u8>> = Vec::with_capacity(4000);
            for _ in 0..4000 {
                if !kept.is_empty() && rng.one_in(3) {
                    let at = rng.index(kept.len());
                    drop(kept.swap_remove(at));
                } else {
                    kept.push(std::hint::black_box(vec![0u8; 1 + rng.index(9000)]));
                }
                LOCAL.with(|it| {
                    assert!(it.unflushed.get().unsigned_abs() < BATCH as u64);
                    assert_eq!(it.net.get(), it.flushed.get().wrapping_add(it.unflushed.get()));
                });
            }
            let left = LOCAL.with(|it| it.unflushed.get());
            assert_eq!(LOCAL.with(exit_flush), left);
            drop(kept);
            LOCAL.with(|it| {
                assert_eq!((it.unflushed.get(), it.batch.get()), (0, 0));
                assert_eq!(it.net.get(), it.flushed.get());
            });
        })
        .join()
        .unwrap();
    }

    struct Tree(std::path::PathBuf);

    impl Tree {
        fn new(name: &str, files: &[(&str, &str)]) -> Tree {
            let root = std::env::temp_dir()
                .join(format!("blaze-graph-arena-{}-{name}", std::process::id()));
            _ = std::fs::remove_dir_all(&root);
            for (path, text) in files {
                let path = root.join(path);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, text).unwrap();
            }
            Tree(root)
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const MEMINFO: &str = "MemTotal:       16384000 kB\nMemFree:         1000 kB\n";

    #[test]
    fn linux_limits_are_the_machine_and_every_cgroup_above_the_process() {
        let tree = Tree::new(
            "v2",
            &[
                ("proc/meminfo", MEMINFO),
                ("proc/self/cgroup", "0::/user.slice/lane.scope\n"),
                ("sys/fs/cgroup/user.slice/memory.max", "8589934592\n"),
                ("sys/fs/cgroup/user.slice/lane.scope/memory.max", "max\n"),
            ],
        );
        let limits = linux_limits(&tree.0).unwrap();
        assert_eq!(
            limits,
            [
                (16384000 * 1024, "MemTotal of /proc/meminfo".to_owned()),
                (8589934592, "memory.max of cgroup /user.slice".to_owned()),
            ]
        );
        let budget = least(limits, None, 1 << 30).unwrap();
        assert_eq!(budget.bytes, 8589934592 - (1 << 30));
        assert_eq!(
            budget.from,
            "memory.max of cgroup /user.slice, 8.00 GB, less a margin of 1.00 GB"
        );
    }

    /// What others use is what blaze's own cgroup counts less what blaze holds, not what the
    /// machine has in use: a busy machine does not shrink a small cgroup's budget.
    #[test]
    fn others_are_what_blazes_own_cgroup_uses_less_blaze() {
        const GB: u64 = 1 << 30;
        let busy = "MemTotal:       16384000 kB\nMemAvailable:     100000 kB\n";
        let tree = Tree::new(
            "current",
            &[
                ("proc/meminfo", busy),
                ("proc/self/cgroup", "0::/lane.scope\n"),
                ("sys/fs/cgroup/lane.scope/memory.max", "4294967296\n"),
                ("sys/fs/cgroup/lane.scope/memory.current", "3221225472\n"),
            ],
        );
        let in_use = linux_in_use(&tree.0, 2 * GB).unwrap();
        assert_eq!(
            in_use,
            Some((
                GB,
                "1.00 GB others use in cgroup /lane.scope (memory.current 3.00 GB less blaze's \
                 own 2.00 GB)"
                    .to_owned()
            ))
        );
        let budget = least(linux_limits(&tree.0).unwrap(), in_use.as_ref(), GB / 2).unwrap();
        assert_eq!(budget.bytes, 4 * GB - GB - GB / 2);
        assert_eq!(
            budget.from,
            "memory.max of cgroup /lane.scope, 4.00 GB, less 1.00 GB others use in cgroup \
             /lane.scope (memory.current 3.00 GB less blaze's own 2.00 GB) and a margin of 0.50 \
             GB"
        );
        // Blaze holding more than the count says (it counts late) leaves others none.
        assert_eq!(linux_in_use(&tree.0, 4 * GB).unwrap().map(|it| it.0), Some(0));
        // Others that leave nothing are an error naming them.
        let full = least(vec![(4 * GB, "x".to_owned())], Some(&(4 * GB, "y".to_owned())), 0);
        let why = full.unwrap_err();
        assert_eq!(why.kind(), io::ErrorKind::InvalidInput);
        assert!(why.to_string().contains("less y and a margin"), "{why}");
    }

    #[test]
    fn others_are_counted_by_v1_where_memory_is_v1s_and_not_in_the_root_cgroup() {
        let tree = Tree::new(
            "current-v1",
            &[
                ("proc/self/cgroup", "4:memory:/a/b\n0::/\n"),
                ("sys/fs/cgroup/memory/a/b/memory.usage_in_bytes", "1073741824\n"),
            ],
        );
        assert_eq!(linux_in_use(&tree.0, 0).unwrap().map(|it| it.0), Some(1 << 30));
        let tree = Tree::new("current-root", &[("proc/self/cgroup", "0::/\n")]);
        assert_eq!(linux_in_use(&tree.0, 0).unwrap(), None);
        // A group below the root has a count; one missing is an error, never a guess.
        let tree = Tree::new("current-none", &[("proc/self/cgroup", "0::/x\n")]);
        assert!(linux_in_use(&tree.0, 0).unwrap_err().to_string().contains("memory.current"));
        let tree = Tree::new(
            "current-bad",
            &[("proc/self/cgroup", "0::/x\n"), ("sys/fs/cgroup/x/memory.current", "lots\n")],
        );
        assert!(linux_in_use(&tree.0, 0).unwrap_err().to_string().contains("a count of `lots`"));
        let tree = Tree::new("current-nogroup", &[("proc/self/cgroup", "3:cpu:/a\n")]);
        assert!(linux_in_use(&tree.0, 0).unwrap_err().to_string().contains("counts memory"));
    }

    #[test]
    fn linux_limits_read_cgroup_v1_and_a_namespace_root() {
        let tree = Tree::new(
            "v1",
            &[
                ("proc/meminfo", MEMINFO),
                ("proc/self/cgroup", "12:cpu,cpuacct:/a\n4:memory:/a/b\n0::/\n"),
                ("sys/fs/cgroup/memory/a/b/memory.limit_in_bytes", "2147483648\n"),
                ("sys/fs/cgroup/memory/a/memory.limit_in_bytes", "9223372036854771712\n"),
                ("sys/fs/cgroup/memory.max", "4294967296\n"),
            ],
        );
        let limits = linux_limits(&tree.0).unwrap();
        let mut bytes: Vec<u64> = limits.iter().map(|it| it.0).collect();
        bytes.sort_unstable();
        assert_eq!(bytes, [2147483648, 4294967296, 16384000 * 1024, 9223372036854771712]);
        assert_eq!(least(limits, None, 0).unwrap().bytes, 2147483648);
    }

    #[test]
    fn what_cannot_be_read_is_an_error_never_a_guess() {
        let tree = Tree::new("none", &[("proc/meminfo", MEMINFO)]);
        assert!(linux_limits(&tree.0).unwrap_err().to_string().contains("proc/self/cgroup"));
        let tree = Tree::new(
            "bad",
            &[
                ("proc/meminfo", MEMINFO),
                ("proc/self/cgroup", "0::/x\n"),
                ("sys/fs/cgroup/x/memory.max", "lots\n"),
            ],
        );
        assert!(linux_limits(&tree.0).unwrap_err().to_string().contains("a limit of `lots`"));
        let tree = Tree::new("nototal", &[("proc/meminfo", "MemFree: 1 kB\n")]);
        assert!(linux_limits(&tree.0).unwrap_err().to_string().contains("no MemTotal"));
        assert_eq!(
            least(vec![(1 << 30, "x".to_owned())], None, 1 << 30).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn the_machine_budget_is_read() {
        let budget = machine_budget(1 << 20).unwrap();
        assert!(budget.bytes > 0);
        assert!(budget.from.contains("a margin of 0.00 GB"), "{}", budget.from);
        #[cfg(target_os = "linux")]
        assert!(budget.from.contains("others use in cgroup") || budget.from.contains("less a"));
    }
}
