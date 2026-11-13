//! The daemon's memory by area as counter rows (docs/memory-footprint.md, step 1): what
//! [`super::daemon::MemoryLine`] says in `serve.log` every few minutes, sampled with the other
//! plots each [`EVERY`], so a build's breakdown at its peak is a query of the profile
//! (`blaze explain memory`), not a read of the log.
//!
//! Each row is a named counter ([`blaze_profile::flight::plot_named`]) of the build under way
//! (the daemon's loop says which, `flight::counting_for`):
//!
//! - per tag, `tag live bytes: <tag>` (what it holds in blaze-mem's regions and frames),
//!   `tag heap bytes: <tag>` (what it holds in the global heap), and its global-heap churn,
//!   `tag heap allocated bytes: <tag>` and `tag heap freed bytes: <tag>` (in all, never
//!   netted: a rate is the difference of two rows); `outside` for the blocks made outside
//!   any task. A tag is written once it held or churned anything, and from then on each time,
//!   so a tag that went back to 0 reads 0, not its last value.
//! - for the process, `heap live bytes` (the global heap's live blocks,
//!   [`blaze_graph::arena::held`]), `heap committed bytes` (what its allocator has committed
//!   now, where it says: mimalloc's count, which a purge lowers by what it decommits but which
//!   counts arenas committed whole and never touched, so no footprint), `blaze-mem resident bytes`
//!   (blaze-mem's committed pages less the purged ones), `chunks warm bytes` and `chunks cold
//!   bytes` (free chunks resident and purged), and `blocks live bytes`, `blocks warm bytes`
//!   and `blocks cold bytes` (kept values' size classes), and the stores' buffer pools
//!   ([`blaze_cas::pool_counts`]): `pool resident bytes`, `pool pinned bytes`, `pool loaded
//!   bytes`, `pool prefetched bytes`, `pool evicted bytes` and `pool pinned bound waits`;
//!   `stack bytes` (what the live fibers' stacks hold, [`blaze_graph::sched::stack::resident`])
//!   and `threads` (the process's threads, each with a stack and heaps of its own).
//!
//! What the footprint holds past the live heap and blaze-mem's resident pages is `footprint
//! bytes` less `heap live bytes` less `blaze-mem resident bytes` (`allocator kept bytes`,
//! derived by `blaze explain memory` where no row says it): the pages the allocators keep, and
//! the stacks. Less `stack bytes` too, what is left is the allocators' alone.

use std::time::Duration;

use blaze_mem::{Tag, tag::Snapshot};
use blaze_profile::{Plot, flight::plot_named};

/// How often the breakdown is sampled, at most: 22 tags of four counters each, so not at
/// every plot (10 ms).
pub(super) const EVERY: Duration = Duration::from_secs(1);

/// A counter's name for each tag, in [`Tag::ALL`]'s order: `$prefix` and the tag's name.
macro_rules! per_tag {
    ($prefix:literal) => {
        [
            concat!($prefix, "parse"),
            concat!($prefix, "nameres"),
            concat!($prefix, "expand"),
            concat!($prefix, "infer"),
            concat!($prefix, "mir"),
            concat!($prefix, "codegen"),
            concat!($prefix, "link"),
            concat!($prefix, "store"),
            concat!($prefix, "vfs"),
            concat!($prefix, "usercode"),
            concat!($prefix, "diagnostics"),
            concat!($prefix, "query"),
            concat!($prefix, "plane"),
            concat!($prefix, "io"),
            concat!($prefix, "intern"),
            concat!($prefix, "frames"),
            concat!($prefix, "chunks"),
            concat!($prefix, "api"),
            concat!($prefix, "fuzz"),
            concat!($prefix, "profile"),
            concat!($prefix, "other"),
            concat!($prefix, "solver"),
            concat!($prefix, "pool"),
        ]
    };
}

const LIVE: [&str; Tag::COUNT] = per_tag!("tag live bytes: ");
const HEAP: [&str; Tag::COUNT] = per_tag!("tag heap bytes: ");
const ALLOCATED: [&str; Tag::COUNT] = per_tag!("tag heap allocated bytes: ");
const FREED: [&str; Tag::COUNT] = per_tag!("tag heap freed bytes: ");

/// Which tags were written so far (bit [`Tag::index`]; bit [`Tag::COUNT`] for outside any
/// task): written each time from then on.
#[derive(Default)]
pub(super) struct Shown(u32);

const _: () = assert!(Tag::COUNT < 32, "one bit a tag and one for outside any task");

impl Shown {
    fn shows(&mut self, at: usize, any: bool) -> bool {
        if any {
            self.0 |= 1 << at;
        }
        self.0 & 1 << at != 0
    }
}

/// Writes the breakdown of `memory` now, the global heap holding `heap` live
/// ([`blaze_graph::arena::held`]).
pub(super) fn plot(memory: &Snapshot, heap: u64, shown: &mut Shown) {
    for tag in Tag::ALL {
        let it = memory.get(tag);
        let at = tag.index();
        if !shown.shows(at, (it.live, it.heap, it.heap_allocated, it.heap_freed) != (0, 0, 0, 0)) {
            continue;
        }
        plot_named(LIVE[at], it.live);
        plot_named(HEAP[at], it.heap);
        plot_named(ALLOCATED[at], it.heap_allocated);
        plot_named(FREED[at], it.heap_freed);
    }
    let outside = (memory.heap_outside, memory.heap_outside_allocated, memory.heap_outside_freed);
    if shown.shows(Tag::COUNT, outside != (0, 0, 0)) {
        plot_named("tag heap bytes: outside", outside.0);
        plot_named("tag heap allocated bytes: outside", outside.1);
        plot_named("tag heap freed bytes: outside", outside.2);
    }
    plot_named("heap live bytes", heap);
    if let Some(committed) = memory.heap_committed {
        plot_named("heap committed bytes", committed);
    }
    plot_named("blaze-mem resident bytes", memory.resident_committed());
    let chunks = blaze_mem::chunks::cached();
    plot_named("chunks warm bytes", chunks.warm);
    plot_named("chunks cold bytes", chunks.cold);
    let blocks = blaze_mem::blocks::snapshot().total();
    plot_named("blocks live bytes", blocks.live());
    plot_named("blocks warm bytes", blocks.warm);
    plot_named("blocks cold bytes", blocks.cold);
    // The stores' buffer pools: what they hold resident is `Tag::Pool`'s live bytes above;
    // these say how much of it readers pin (not the sweep's to take), the bound on that, the
    // budget the sweep holds the pools to, and what moved through them.
    let pool = blaze_cas::pool_counts();
    plot_named("pool resident bytes", pool.resident);
    // Every store's global-heap bytes, as each last counted itself (blaze_cas::PoolCounts).
    plot_named("store heap bytes", pool.store_heap);
    plot_named("pool pinned bytes", pool.pinned);
    plot_named("pool loaded bytes", pool.loaded);
    plot_named("pool prefetched bytes", pool.prefetched);
    plot_named("pool evicted bytes", pool.evicted);
    plot_named("pool pinned bound waits", pool.bound_waits);
    // The part of the footprint past the heaps that is not an allocator's: the fibers' stacks,
    // and the threads that each hold a stack and heaps of their own.
    blaze_profile::plot(Plot::StackBytes, blaze_graph::sched::stack::resident());
    if let Some(threads) = threads() {
        blaze_profile::plot(Plot::Threads, threads);
    }
}

/// The threads this process runs now: `num_threads`, field 20 of `/proc/self/stat`. `None`
/// where it cannot be read, and no row is written then.
#[cfg(target_os = "linux")]
fn threads() -> Option<u64> {
    use std::io::Read as _;
    let mut buf = [0u8; 1024];
    let mut file = std::fs::File::open("/proc/self/stat").ok()?;
    let mut len = 0;
    // Bounded by the buffer: each turn reads at least a byte or ends.
    while len < buf.len() {
        match file.read(&mut buf[len..]).ok()? {
            0 => break,
            read => len += read,
        }
    }
    let text = std::str::from_utf8(&buf[..len]).ok()?;
    // The name, between the first `(` and the last `)`, may hold spaces; `num_threads` is the
    // 18th field after it (state, then 17 more).
    text.get(text.rfind(')')? + 1..)?.split_whitespace().nth(17)?.parse().ok()
}

/// The threads this process runs now: `proc_pidinfo`'s `PROC_PIDTASKINFO`, `pti_threadnum`.
#[cfg(target_os = "macos")]
#[allow(clippy::cast_possible_wrap, reason = "the size of a small C struct")]
fn threads() -> Option<u64> {
    let size = std::mem::size_of::<libc::proc_taskinfo>() as libc::c_int;
    // SAFETY: `proc_taskinfo` is a C struct of integers, for which all-zero bytes is valid.
    let mut info: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
    // SAFETY: `info` is a live local of `size` bytes that the call writes and does not keep.
    let got = unsafe {
        libc::proc_pidinfo(
            libc::getpid(),
            libc::PROC_PIDTASKINFO,
            0,
            (&raw mut info).cast(),
            size,
        )
    };
    (got == size).then(|| u64::try_from(info.pti_threadnum).ok()).flatten()
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn threads() -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each tag's counters are named for it, in [`Tag::ALL`]'s order: a tag added to blaze-mem
    /// without its names here fails, never names another tag's bytes.
    #[test]
    fn each_tags_counters_are_named_for_it() {
        for tag in Tag::ALL {
            for names in [&LIVE, &HEAP, &ALLOCATED, &FREED] {
                let name = names[tag.index()];
                assert_eq!(name.rsplit(": ").next(), Some(tag.name()), "{name}");
            }
        }
    }
}
