//! Where a task's memory is measured (docs/v2/plan/lanes/core.md section 4, "Memory-budget
//! admission").
//!
//! [`Counting`] is the daemon's global allocator: it counts the bytes the process holds
//! ([`held`]), by the blaze-mem tag whose task allocated them ([`Holders`]:
//! `blaze_mem::tag::Counters::heap`), and, per thread, the bytes the task the thread runs now
//! holds, so [`measure`] says what a task held at most through the global allocator; what it held in its frame
//! ([`blaze_mem::frame::task`]) is [`blaze_mem::frame::measure`]'s, and the frames' cap is
//! [`blaze_mem::frame::Cap`]. [`machine_budget`] is the memory the plane may grant: the
//! machine's limit, less what others use in blaze's own cgroup and a margin.

use std::{
    cell::Cell,
    fmt, io,
    path::Path,
    sync::atomic::{AtomicI64, Ordering},
};

use blaze_mem::Tag;

use crate::Budget;

pub use global::Holders;

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
    /// What each holder's count moved since it was last folded into its blaze-mem tag line
    /// (`blaze_mem::tag::heap_moved`), by [`slot`]: each tag's, then what was allocated outside
    /// any task. Folded at `batch` each, as `unflushed` is.
    holders: [Cell<i64>; HOLDERS],
    #[cfg(test)]
    flushed: Cell<i64>,
}

/// Every tag, and outside any task.
const HOLDERS: usize = Tag::COUNT + 1;

/// A holder's index in [`Local::holders`].
#[inline(always)]
fn slot(holder: Option<Tag>) -> usize {
    holder.map_or(Tag::COUNT, Tag::index)
}

/// The holder of a [`Local::holders`] index.
fn holder_of(slot: usize) -> Option<Tag> {
    Tag::ALL.get(slot).copied()
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
            holders: [const { Cell::new(0) }; HOLDERS],
            #[cfg(test)]
            flushed: Cell::new(0),
        }
    };
    static EXIT: Exit = const { Exit };
}

/// [`LOCAL`]'s reader, but for [`count`]'s own. Not inlined: the allocator counts through it from a job's code,
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
    for (at, moved) in local.holders.iter().enumerate() {
        let moved = moved.replace(0);
        if moved != 0 {
            blaze_mem::tag::heap_moved(holder_of(at), moved);
        }
    }
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

/// Per allocation: one thread-local block, four adds and three well-predicted branches; the
/// atomic adds are in [`flush`] and [`fold`], once per [`BATCH`] bytes of drift. `holder` is
/// the tag whose task allocated the bytes ([`Holders`]).
///
/// [`LOCAL`]'s other reader, not inlined for the same reason as [`with_local`], and reading it
/// itself so that `holder` and `delta` arrive in registers: through `with_local`'s closure they
/// went by its captures on the stack, and the holder's slot was loaded only once its byte came
/// back from the store (a third of a small block's pair in a task, lane checkheap's profile).
#[inline(never)]
fn count(holder: Option<Tag>, delta: i64) {
    LOCAL.with(|local| {
        let net = local.net.get().wrapping_add(delta);
        local.net.set(net);
        if net > local.peak.get() {
            local.peak.set(net);
        }
        let batch = local.batch.get().unsigned_abs();
        let moved = &local.holders[slot(holder)];
        let held = moved.get().wrapping_add(delta);
        if held.unsigned_abs() < batch {
            moved.set(held);
        } else {
            fold(moved, holder, held);
        }
        let unflushed = local.unflushed.get().wrapping_add(delta);
        if unflushed.unsigned_abs() < batch {
            local.unflushed.set(unflushed);
        } else {
            flush(local, unflushed);
        }
    });
}

/// Folds what `holder`'s count on this thread moved into its tag line.
#[cold]
#[inline(never)]
fn fold(moved: &Cell<i64>, holder: Option<Tag>, held: i64) {
    moved.set(0);
    blaze_mem::tag::heap_moved(holder, held);
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
/// the allocator it uses (`#[global_allocator] static ALLOC: Counting<Heaps> =
/// Counting(Heaps::new());`). It takes no lock, allocates nothing of its own and does no atomic
/// write on an allocation but once per [`BATCH`] bytes a thread's count moves.
///
/// A byte freed on another thread than the one that allocated it counts against the thread
/// that frees it: [`measure`] sees what a task's thread allocated and freed, exactly; [`held`]
/// is within [`BATCH`] per other thread of what the process holds.
///
/// Where `A` keeps blocks by tag ([`Holders::BY_TAG`]), each allocation is made for the tag of
/// the task running on the thread (`blaze_mem::frame::running_tag`), and its free, on any
/// thread and in any task, is taken off the tag `A` says holds the block, so each tag's
/// `blaze_mem::tag::Counters::heap` is what it holds, within [`BATCH`] per thread.
pub struct Counting<A>(pub A);

fn signed(bytes: usize) -> i64 {
    // A layout's size is at most `isize::MAX`.
    bytes as i64
}

/// The allocator hook: `GlobalAlloc` is an unsafe trait, and this is the one way to see every
/// allocation (a safe wrapper type would see only the allocations made through it).
#[allow(unsafe_code)]
mod global {
    use std::alloc::{GlobalAlloc, Layout, System};

    use blaze_mem::Tag;

    use super::{Counting, count, signed};

    /// What [`Counting`] asks of the allocator under it to name the holder of a block: the
    /// blaze-mem tag whose task allocated it, kept apart so that a free, on any thread, finds
    /// it again.
    pub trait Holders: GlobalAlloc {
        /// Whether the allocator keeps blocks by tag. Where it does not, every block counts to
        /// what is held outside any task (`blaze_mem::tag::Snapshot::heap_outside`) and the
        /// tags' global-heap counts stay 0.
        const BY_TAG: bool;

        /// Allocates `layout`, zeroed when `zeroed` is set, among `tag`'s blocks.
        ///
        /// # Safety
        ///
        /// [`GlobalAlloc::alloc`]'s contract for `layout`.
        unsafe fn alloc_for(&self, tag: Tag, layout: Layout, zeroed: bool) -> *mut u8;

        /// The tag among whose blocks `ptr` is ([`Holders::alloc_for`]); `None` for a block
        /// allocated outside any task. Allocates nothing.
        ///
        /// # Safety
        ///
        /// `ptr` is a live block of this allocator.
        unsafe fn holder(&self, ptr: *mut u8) -> Option<Tag>;

        /// [`GlobalAlloc::realloc`], putting the block among `into`'s blocks (the running
        /// task's; `None`, the untagged ones) when it moves. One grown or shrunk in place stays
        /// where it was, so [`Holders::holder`] of the result says which.
        ///
        /// # Safety
        ///
        /// [`GlobalAlloc::realloc`]'s contract.
        unsafe fn realloc_for(&self, into: Option<Tag>, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8;
    }

    /// std's allocator keeps no blocks apart: its bytes are counted, not named.
    impl Holders for System {
        const BY_TAG: bool = false;

        unsafe fn alloc_for(&self, _: Tag, layout: Layout, zeroed: bool) -> *mut u8 {
            // SAFETY: the caller upholds `alloc`'s contract for `layout`.
            unsafe {
                match zeroed {
                    true => self.alloc_zeroed(layout),
                    false => self.alloc(layout),
                }
            }
        }

        unsafe fn holder(&self, _: *mut u8) -> Option<Tag> {
            None
        }

        unsafe fn realloc_for(&self, _: Option<Tag>, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            // SAFETY: the caller upholds `realloc`'s contract for `ptr`, `layout` and `new_size`.
            unsafe { self.realloc(ptr, layout, new_size) }
        }
    }

    impl<A: Holders> Counting<A> {
        /// The tag a block allocated now is for: the running task's, where `A` keeps tags.
        #[inline(always)]
        fn running() -> Option<Tag> {
            match A::BY_TAG {
                true => blaze_mem::frame::running_tag(),
                false => None,
            }
        }

        #[inline(always)]
        unsafe fn made(&self, layout: Layout, zeroed: bool) -> *mut u8 {
            let holder = Self::running();
            // SAFETY: the caller upholds `alloc`'s contract for `layout`.
            let ptr = unsafe {
                match (holder, zeroed) {
                    (Some(tag), _) => self.0.alloc_for(tag, layout, zeroed),
                    (None, true) => self.0.alloc_zeroed(layout),
                    (None, false) => self.0.alloc(layout),
                }
            };
            if !ptr.is_null() {
                count(holder, signed(layout.size()));
            }
            ptr
        }

        /// What holds `ptr`, a live block of `A`.
        #[inline(always)]
        unsafe fn holder(&self, ptr: *mut u8) -> Option<Tag> {
            match A::BY_TAG {
                // SAFETY: the caller says `ptr` is a live block of this allocator.
                true => unsafe { self.0.holder(ptr) },
                false => None,
            }
        }
    }

    // SAFETY: every call goes to `A` with the caller's arguments (or to `A`'s tagged
    // equivalents, under the same contracts), so `A`'s guarantees hold; the counting beside it
    // touches atomics, `LOCAL`, a `const` thread-local with no destructor, and blaze-mem's
    // `STACK`, another, and registers `EXIT`'s destructor once per thread, whose own
    // allocations `flush` takes without registering again. An allocation that recursed into
    // itself here would deadlock or overflow the stack.
    unsafe impl<A: Holders> GlobalAlloc for Counting<A> {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            // SAFETY: the caller upholds `alloc`'s contract for `layout`.
            unsafe { self.made(layout, false) }
        }

        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            // SAFETY: the caller upholds `alloc_zeroed`'s contract for `layout`.
            unsafe { self.made(layout, true) }
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            // SAFETY: the caller says `ptr` was allocated by this allocator with `layout`, so
            // it is live until the `dealloc` below.
            let holder = unsafe { self.holder(ptr) };
            // SAFETY: the caller says `ptr` was allocated by this allocator with `layout`.
            unsafe { self.0.dealloc(ptr, layout) };
            count(holder, -signed(layout.size()));
        }

        /// A block that moves goes to the running task's tag, as a new one would: a buffer
        /// one task made and another grew (a thread's stack of reads, a memo's vector) is the
        /// grower's once it moves, not whoever made its first few bytes.
        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            // SAFETY: the caller says `ptr` is a live block of this allocator.
            let was = unsafe { self.holder(ptr) };
            // SAFETY: the caller upholds `realloc`'s contract for `ptr`, `layout` and `new_size`.
            let new = unsafe { self.0.realloc_for(Self::running(), ptr, layout, new_size) };
            if !new.is_null() {
                // SAFETY: `new` is the live block the realloc gave.
                let now = unsafe { self.holder(new) };
                match now == was {
                    true => count(was, signed(new_size) - signed(layout.size())),
                    false => {
                        count(was, -signed(layout.size()));
                        count(now, signed(new_size));
                    }
                }
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

/// The process's footprint: as the system counts it where the daemon says how to read it
/// ([`blaze_mem::tag::resident`]), else everything blaze itself takes of it at most
/// ([`blaze_mem::tag::Snapshot::footprint`]: the global heap, as what its allocator committed
/// where it says or [`held`] where it does not, and the pages `blaze_mem` committed for its
/// regions, frames, chunks and pools, which it maps itself and [`Counting`] never sees, less the
/// free chunks it purged). The counted sum is only an upper bound: mimalloc's committed count is
/// never lowered by a purge, and committed pages never touched are not resident.
pub fn own() -> u64 {
    own_of(&blaze_mem::tag::snapshot())
}

/// [`own`], the counted sum taken from `snapshot` where the system does not say.
pub fn own_of(snapshot: &blaze_mem::tag::Snapshot) -> u64 {
    blaze_mem::tag::resident().unwrap_or_else(|| snapshot.footprint(held()))
}

/// The memory the plane may grant: the machine's limit, less what others use within it, less
/// `margin`.
///
/// The limit is the least of what the machine has and what the process may use: on Linux
/// the machine's memory ([`linux_machine`]: `MemTotal`, or the ceiling a VM that hot-plugs
/// memory grows to), the `memory.max` of the process's cgroup and of each one above it
/// (cgroup v2; v1's `memory.limit_in_bytes`), and `RLIMIT_AS`; on macOS `sysctl hw.memsize`
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
/// `told` is the limit the daemon was started with, and what told it (`blaze serve
/// --memory-gb`, or `BLAZE_DAEMON_MEMORY_GB` from whoever started it: a runner that stops the
/// daemon past a footprint passes that footprint): one more limit, the least of all winning, so
/// the budgets see a cap the machine's own numbers don't.
///
/// The budget's `from` names the limit and what was taken off. An error where one cannot be
/// read or nothing is left, never a guess.
pub fn machine_budget(margin: u64, told: Option<(u64, String)>) -> io::Result<Budget> {
    let mut limits = machine_limits()?;
    limits.extend(told);
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

/// The memory limits of a Linux machine whose file system is at `root`: the machine's memory
/// ([`linux_machine`]), and each cgroup memory limit over the process.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn linux_limits(root: &Path) -> io::Result<Vec<(u64, String)>> {
    // alloc: the limits and their names, once per daemon start
    let mut limits = vec![linux_machine(root)?];
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

/// The memory of a Linux machine whose file system is at `root`, named: `MemTotal`, or, on a
/// VM that hot-plugs memory as it is used, the ceiling it grows to, with `MemTotal` named
/// beside it. A virtio-mem VM (every ix VM) boots with a small floor and plugs more of its
/// device's region on demand, and `MemTotal` counts only what is plugged now: a 256 GiB VM
/// read 25 GB, and the budget refused a build that would have fit. The ceiling is the larger
/// of what the kernel's `/proc/iomem` says of the virtio-mem regions ([`virtio_mem_unplugged`])
/// and the `vm.max_memory_mib=` ix puts on the kernel's command line ([`cmdline_ceiling`]),
/// either where it is all there is to read.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn linux_machine(root: &Path) -> io::Result<(u64, String)> {
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
    let regions = virtio_mem_unplugged(root)?;
    let mut ceiling = match &regions {
        Regions::Sized { unplugged, devices } => Some((
            total.saturating_add(*unplugged),
            // alloc: the limit's name, once per daemon start
            format!("MemTotal plus what is unplugged of virtio-mem {devices} in /proc/iomem"),
        )),
        Regions::None | Regions::Hidden { .. } => None,
    };
    if let Some(bytes) = cmdline_ceiling(root)?
        && ceiling.as_ref().is_none_or(|it| bytes > it.0)
    {
        // alloc: the limit's name, once per daemon start
        ceiling = Some((bytes, "vm.max_memory_mib of /proc/cmdline".to_owned()));
    }
    Ok(match (ceiling, regions) {
        (Some((bytes, from)), _) if bytes > total => (
            bytes,
            // alloc: the limit's name, once per daemon start
            format!(
                "the ceiling virtio-mem grows the machine to ({from}; MemTotal of /proc/meminfo \
                 is {} now)",
                gb(total)
            ),
        ),
        (_, Regions::Hidden { devices }) => (
            total,
            // alloc: the limit's name, once per daemon start
            format!(
                "MemTotal of /proc/meminfo, what is plugged now of a machine virtio-mem {devices} \
                 can grow, whose regions /proc/iomem hides from this user and no \
                 vm.max_memory_mib on /proc/cmdline names"
            ),
        ),
        // alloc: the limit's name, once per daemon start
        _ => (total, "MemTotal of /proc/meminfo".to_owned()),
    })
}

/// What `/proc/iomem` says of a machine's virtio-mem devices.
#[derive(Debug, PartialEq, Eq)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
enum Regions {
    /// No virtio-mem device, or no `/proc/iomem` to say.
    None,
    /// The bytes of the devices' regions not plugged now, which `MemTotal` leaves out, and the
    /// devices (`virtio5`, or `virtio5, virtio6`).
    Sized { unplugged: u64, devices: String },
    /// Devices whose regions this user is shown as zeros: only `CAP_SYS_ADMIN` sees addresses.
    Hidden { devices: String },
}

/// The virtio-mem devices of `/proc/iomem` on a Linux machine whose file system is at `root`.
/// The driver claims each device's whole region at the top level, named by the device
/// (`440000000-43bfffffff : virtio5`), and adds what it plugs below it as
/// `System RAM (virtio_mem)`, which is what of the region `MemTotal` counts.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn virtio_mem_unplugged(root: &Path) -> io::Result<Regions> {
    let iomem = root.join("proc/iomem");
    let text = match std::fs::read_to_string(&iomem) {
        Ok(text) => text,
        Err(why) if why.kind() == io::ErrorKind::NotFound => return Ok(Regions::None),
        Err(why) => {
            // alloc: the error's message
            return Err(io::Error::new(why.kind(), format!("{}: {why}", iomem.display())));
        }
    };
    // alloc: the devices' names, once per daemon start
    let mut devices = String::new();
    let (mut unplugged, mut hidden, mut in_device) = (0u64, false, false);
    for line in text.lines().filter(|it| !it.trim().is_empty()) {
        let entry = line.trim_start();
        let top = entry.len() == line.len();
        let (range, name) = entry
            .split_once(" : ")
            // alloc: the error's message
            .ok_or_else(|| invalid(&iomem, format!("a line `{line}`")))?;
        let (start, end) = range
            .split_once('-')
            .and_then(|(start, end)| {
                Some((u64::from_str_radix(start, 16).ok()?, u64::from_str_radix(end, 16).ok()?))
            })
            // alloc: the error's message
            .ok_or_else(|| invalid(&iomem, format!("a range `{range}`")))?;
        let size = match end.checked_sub(start) {
            Some(it) if end > 0 => it.saturating_add(1),
            _ => 0,
        };
        if top {
            in_device = name
                .strip_prefix("virtio")
                .is_some_and(|it| !it.is_empty() && it.bytes().all(|byte| byte.is_ascii_digit()));
            if in_device {
                if !devices.is_empty() {
                    devices.push_str(", ");
                }
                devices.push_str(name);
                hidden |= size == 0;
                unplugged = unplugged.saturating_add(size);
            }
        } else if in_device && name == "System RAM (virtio_mem)" {
            unplugged = unplugged.saturating_sub(size);
        }
    }
    Ok(match (devices.is_empty(), hidden) {
        (true, _) => Regions::None,
        (false, true) => Regions::Hidden { devices },
        (false, false) => Regions::Sized { unplugged, devices },
    })
}

/// The ceiling ix puts on a VM's kernel command line, `vm.max_memory_mib=<MiB>`, in bytes, on a
/// Linux machine whose file system is at `root`; `None` where there is none.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn cmdline_ceiling(root: &Path) -> io::Result<Option<u64>> {
    let cmdline = root.join("proc/cmdline");
    let text = match std::fs::read_to_string(&cmdline) {
        Ok(text) => text,
        Err(why) if why.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(why) => {
            // alloc: the error's message
            return Err(io::Error::new(why.kind(), format!("{}: {why}", cmdline.display())));
        }
    };
    let Some(value) =
        text.split_ascii_whitespace().find_map(|it| it.strip_prefix("vm.max_memory_mib="))
    else {
        return Ok(None);
    };
    value
        .parse::<u64>()
        .ok()
        .filter(|&mib| mib > 0)
        .and_then(|mib| mib.checked_mul(1 << 20))
        .map(Some)
        // alloc: the error's message
        .ok_or_else(|| invalid(&cmdline, format!("vm.max_memory_mib of `{value}`")))
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
    use std::alloc::{GlobalAlloc, Layout};

    use blaze_ids::rng::SplitMix64;

    use super::*;

    /// `blaze_alloc_count`'s counter under this crate's own: the allocation-budget tests count
    /// calls, the arena's tests bytes held.
    #[global_allocator]
    static ALLOC: Counting<blaze_alloc_count::Counting> = Counting(blaze_alloc_count::Counting);

    /// The test binary's allocator keeps no tags: every block counts outside any task.
    #[allow(unsafe_code, reason = "forwards to the counter's own GlobalAlloc calls")]
    impl Holders for blaze_alloc_count::Counting {
        const BY_TAG: bool = false;

        unsafe fn alloc_for(&self, _: Tag, layout: Layout, zeroed: bool) -> *mut u8 {
            // SAFETY: the caller upholds `alloc`'s contract for `layout`.
            unsafe {
                match zeroed {
                    true => self.alloc_zeroed(layout),
                    false => self.alloc(layout),
                }
            }
        }

        unsafe fn holder(&self, _: *mut u8) -> Option<Tag> {
            None
        }

        unsafe fn realloc_for(&self, _: Option<Tag>, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            // SAFETY: the caller upholds `realloc`'s contract.
            unsafe { self.realloc(ptr, layout, new_size) }
        }
    }

    /// Keeps blocks by tag in a byte before each block: what `Counting` asks of the daemon's
    /// mimalloc heaps (blaze-engine's `heaps.rs`), the simplest way, for testing `Counting`.
    struct Labelled;

    /// The label's room before a block: at least the alignment the test asks for.
    const LABEL: usize = 64;
    /// The label of a block made outside any task.
    const OUTSIDE: u8 = u8::MAX;

    fn labelled(layout: Layout) -> Layout {
        assert!(layout.align() <= LABEL, "the test's layouts align to at most {LABEL}");
        Layout::from_size_align(layout.size() + LABEL, LABEL).expect("a test's layout")
    }

    #[allow(unsafe_code, reason = "a test allocator over std's")]
    mod labelled {
        use std::alloc::{GlobalAlloc, Layout, System};

        use blaze_mem::Tag;

        use super::{LABEL, Labelled, OUTSIDE, labelled};
        use crate::arena::Holders;

        impl Labelled {
            unsafe fn made(&self, label: u8, layout: Layout, zeroed: bool) -> *mut u8 {
                // SAFETY: a layout of nonzero size, `LABEL` more than the caller's.
                let block = unsafe {
                    match zeroed {
                        true => System.alloc_zeroed(labelled(layout)),
                        false => System.alloc(labelled(layout)),
                    }
                };
                assert!(!block.is_null(), "std's allocator gave a test block");
                // SAFETY: the label's byte is in the block, before what the caller gets.
                unsafe {
                    block.add(LABEL - 1).write(label);
                    block.add(LABEL)
                }
            }
        }

        // SAFETY: each block is std's, `LABEL` bytes into a block `LABEL` larger.
        unsafe impl GlobalAlloc for Labelled {
            unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
                // SAFETY: the caller's layout.
                unsafe { self.made(OUTSIDE, layout, false) }
            }

            unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
                // SAFETY: `ptr` is `LABEL` into a block std made with `labelled(layout)`.
                unsafe { System.dealloc(ptr.sub(LABEL), labelled(layout)) }
            }
        }

        impl Holders for Labelled {
            const BY_TAG: bool = true;

            unsafe fn alloc_for(&self, tag: Tag, layout: Layout, zeroed: bool) -> *mut u8 {
                // SAFETY: the caller's layout.
                unsafe { self.made(tag as u8, layout, zeroed) }
            }

            unsafe fn holder(&self, ptr: *mut u8) -> Option<Tag> {
                // SAFETY: the label's byte, just before a live block.
                Tag::ALL.get(usize::from(unsafe { ptr.sub(1).read() })).copied()
            }

            /// Every realloc here moves the block, as a large one does under mimalloc: it is
            /// labelled `into`'s.
            unsafe fn realloc_for(&self, into: Option<Tag>, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
                // SAFETY: std's block, grown whole, label and all; the new label's byte is in it.
                unsafe {
                    let block = System.realloc(ptr.sub(LABEL), labelled(layout), new_size + LABEL);
                    assert!(!block.is_null(), "std's allocator grew a test block");
                    block.add(LABEL - 1).write(into.map_or(OUTSIDE, |tag| tag as u8));
                    block.add(LABEL)
                }
            }
        }
    }

    /// A block handed to another thread.
    struct Sent(*mut u8);

    impl Sent {
        /// Taken whole, so a closure moves the `Send` wrapper, not its field.
        fn block(self) -> *mut u8 {
            self.0
        }
    }

    // SAFETY: the block is the test allocator's, which any thread may free; the sender keeps no
    // use of it.
    #[allow(unsafe_code, reason = "a test's block crossing threads")]
    unsafe impl Send for Sent {}

    /// What a tag holds in the global heap is what its tasks allocated, or moved in growing,
    /// less what anyone freed: a free on another thread, in a task of another tag, is taken off
    /// the tag that holds the block, not the one running. Fuzz and Api: tags nothing else in
    /// this binary's tests runs tasks of (the global allocator here keeps no tags anyway), and
    /// every step is past a batch, so each is folded in at once and the counts are exact.
    #[test]
    #[allow(unsafe_code, reason = "drives a GlobalAlloc by hand")]
    fn a_tags_heap_bytes_are_what_its_tasks_allocated_less_what_anyone_freed() {
        static TAGGED: Counting<Labelled> = Counting(Labelled);
        let heap = |tag: Tag| blaze_mem::tag::snapshot().get(tag).heap;
        let (fuzz, api) = (heap(Tag::Fuzz), heap(Tag::Api));
        let small = Layout::from_size_align(8 * MB, 16).expect("a layout");
        // SAFETY: a valid layout, freed below.
        let block = blaze_mem::frame::task(Tag::Fuzz, |_| unsafe { TAGGED.alloc(small) });
        assert_eq!(heap(Tag::Fuzz), fuzz + 8 * MB as u64);
        // Grown, and moved, in a task of another tag: the grower's now, all of it.
        // SAFETY: `block` is live, made with `small`.
        let block = blaze_mem::frame::task(Tag::Api, |_| unsafe { TAGGED.realloc(block, small, 12 * MB) });
        assert_eq!(heap(Tag::Fuzz), fuzz, "a block that moves leaves its maker");
        assert_eq!(heap(Tag::Api), api + 12 * MB as u64, "and is the grower's");
        let sent = Sent(block);
        std::thread::spawn(move || {
            let block = sent.block();
            let grown = Layout::from_size_align(12 * MB, 16).expect("a layout");
            // SAFETY: `block` is live, of `grown`'s size after the realloc.
            blaze_mem::frame::task(Tag::Fuzz, |_| unsafe { TAGGED.dealloc(block, grown) });
        })
        .join()
        .expect("the other thread frees the block");
        assert_eq!(heap(Tag::Api), api, "the free is taken off the tag that holds the block");
        assert_eq!(heap(Tag::Fuzz), fuzz, "not off the tag of the task that freed it");
        // Outside any task: no tag's.
        // SAFETY: a valid layout, freed at once.
        unsafe { TAGGED.dealloc(TAGGED.alloc(small), small) };
        assert_eq!((heap(Tag::Fuzz), heap(Tag::Api)), (fuzz, api));
    }

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

    /// A virtio-mem VM, as ix makes one: 2 GiB at boot and a 254 GiB region of which 23 GiB is
    /// plugged now, so `MemTotal` reads 25 GiB of its 256 GiB ceiling (blaze checking blaze, E5:
    /// the budget took the 25 GB and refused a whole-workspace check that fit).
    const GROWN: &str = "MemTotal:       26214400 kB\nMemFree:         1000 kB\n";
    const IOMEM: &str = "00001000-0009fbff : System RAM\n\
                         00100000-7fffffff : System RAM\n  \
                         01000000-01ffffff : Kernel code\n\
                         440000000-43bfffffff : virtio5\n  \
                         440000000-9ffffffff : System RAM (virtio_mem)\n";
    /// The same to a user without `CAP_SYS_ADMIN`, who is shown every address as zero.
    const IOMEM_HIDDEN: &str = "00000000-00000000 : System RAM\n\
                                00000000-00000000 : virtio5\n  \
                                00000000-00000000 : System RAM (virtio_mem)\n";
    const CMDLINE: &str = "BOOT_IMAGE=/vmlinuz console=hvc0 vm.max_memory_mib=262144\n";

    #[test]
    fn a_vm_that_hot_plugs_memory_is_limited_by_its_ceiling_not_what_is_plugged_now() {
        const GIB: u64 = 1 << 30;
        let tree = Tree::new(
            "grown-iomem",
            &[("proc/meminfo", GROWN), ("proc/iomem", IOMEM), ("proc/self/cgroup", "0::/\n")],
        );
        assert_eq!(
            linux_limits(&tree.0).unwrap(),
            [(
                256 * GIB,
                "the ceiling virtio-mem grows the machine to (MemTotal plus what is unplugged of \
                 virtio-mem virtio5 in /proc/iomem; MemTotal of /proc/meminfo is 25.00 GB now)"
                    .to_owned()
            )]
        );
        // Hidden from this user, the region is sized by what ix puts on the command line.
        let tree = Tree::new(
            "grown-cmdline",
            &[
                ("proc/meminfo", GROWN),
                ("proc/iomem", IOMEM_HIDDEN),
                ("proc/cmdline", CMDLINE),
                ("proc/self/cgroup", "0::/\n"),
            ],
        );
        let limits = linux_limits(&tree.0).unwrap();
        assert_eq!(
            limits,
            [(
                256 * GIB,
                "the ceiling virtio-mem grows the machine to (vm.max_memory_mib of /proc/cmdline; \
                 MemTotal of /proc/meminfo is 25.00 GB now)"
                    .to_owned()
            )]
        );
        // The budget, and a refusal of it, name the ceiling and what is plugged now.
        let budget = least(limits.clone(), None, GIB).unwrap();
        assert_eq!(budget.bytes, 255 * GIB);
        let named = |it: &str| it.contains("256.00 GB") && it.contains("25.00 GB now");
        assert!(named(&budget.from), "{}", budget.from);
        let refused = least(limits, Some(&(256 * GIB, "y".to_owned())), 0).unwrap_err();
        assert!(named(&refused.to_string()), "{refused}");
        // A cgroup's limit below the ceiling is still the least.
        let tree = Tree::new(
            "grown-cgroup",
            &[
                ("proc/meminfo", GROWN),
                ("proc/cmdline", CMDLINE),
                ("proc/self/cgroup", "0::/lane.scope\n"),
                ("sys/fs/cgroup/lane.scope/memory.max", "8589934592\n"),
            ],
        );
        assert_eq!(least(linux_limits(&tree.0).unwrap(), None, 0).unwrap().bytes, 8 * GIB);
    }

    #[test]
    fn a_hot_plug_ceiling_that_cannot_be_read_is_named_never_guessed() {
        // Hidden regions and no command line: `MemTotal`, saying what it leaves out.
        let tree = Tree::new(
            "grown-hidden",
            &[
                ("proc/meminfo", GROWN),
                ("proc/iomem", IOMEM_HIDDEN),
                ("proc/self/cgroup", "0::/\n"),
            ],
        );
        let limits = linux_limits(&tree.0).unwrap();
        assert_eq!(limits[0].0, 26214400 * 1024);
        assert!(limits[0].1.contains("virtio-mem virtio5 can grow"), "{}", limits[0].1);
        // A machine with no virtio-mem device is its `MemTotal`, whatever else iomem holds.
        let tree = Tree::new(
            "plain-iomem",
            &[
                ("proc/meminfo", MEMINFO),
                ("proc/iomem", "00100000-7fffffff : System RAM\n"),
                ("proc/self/cgroup", "0::/\n"),
            ],
        );
        assert_eq!(
            linux_limits(&tree.0).unwrap(),
            [(16384000 * 1024, "MemTotal of /proc/meminfo".to_owned())]
        );
        let tree = Tree::new(
            "bad-cmdline",
            &[
                ("proc/meminfo", GROWN),
                ("proc/cmdline", "vm.max_memory_mib=lots\n"),
                ("proc/self/cgroup", "0::/\n"),
            ],
        );
        let why = linux_limits(&tree.0).unwrap_err().to_string();
        assert!(why.contains("vm.max_memory_mib of `lots`"), "{why}");
        let tree = Tree::new(
            "bad-iomem",
            &[
                ("proc/meminfo", GROWN),
                ("proc/iomem", "garbage\n"),
                ("proc/self/cgroup", "0::/\n"),
            ],
        );
        assert!(linux_limits(&tree.0).unwrap_err().to_string().contains("a line `garbage`"));
    }

    /// The limit the daemon was told is one more limit of [`machine_budget`]: under the
    /// machine's it is the budget, named by what told it. A limit of 1 GiB is under any machine's;
    /// where others' use in the cgroup is taken off (Linux) nothing may be left of it, and the
    /// error names it then.
    #[test]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn a_told_limit_under_the_machines_is_the_budget_and_named() {
        let told = Some((1 << 30, "BLAZE_DAEMON_MEMORY_GB=1".to_owned()));
        match machine_budget(1 << 20, told) {
            Ok(budget) => {
                assert!(budget.from.starts_with("BLAZE_DAEMON_MEMORY_GB=1, 1.00 GB"), "{}", budget.from);
                assert!(budget.bytes < 1 << 30, "{}", budget.bytes);
            }
            Err(error) => assert!(error.to_string().contains("of BLAZE_DAEMON_MEMORY_GB=1"), "{error}"),
        }
        let over = Some((u64::MAX / 2, "blaze serve --memory-gb".to_owned()));
        let budget = machine_budget(1 << 20, over).unwrap();
        assert!(!budget.from.contains("--memory-gb"), "the machine's is the least: {}", budget.from);
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn the_machine_budget_is_read() {
        let budget = machine_budget(1 << 20, None).unwrap();
        assert!(budget.bytes > 0);
        assert!(budget.from.contains("a margin of 0.00 GB"), "{}", budget.from);
        #[cfg(target_os = "linux")]
        assert!(budget.from.contains("others use in cgroup") || budget.from.contains("less a"));
    }
}
