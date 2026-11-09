//! The heap sampler (docs/memory-footprint.md, step 1): every
//! [`blaze_graph::arena::SAMPLE_EVERY`] bytes a thread allocates through the daemon's counting
//! allocator, the allocation that crosses it is sampled with its stack, so "what churns the
//! global heap" has an answer by call site: the `alloc_samples` table, each row a sample that
//! stands for its `bytes` of allocation, the tag of the task that made it, the block's own size,
//! and the call site, the innermost frame outside the allocator and the collections' plumbing.
//!
//! A sample is taken inside the allocator, on the allocating thread: [`taken`] walks the
//! thread's frame pointers through [`crate::stack::here`] (each record vouched for by the
//! kernel, so a wild pointer ends the walk, never faults; two system calls a frame) and writes
//! the sample into a fixed ring of atomics, [`RING`], with no lock and no allocation. The
//! history's flush reads the ring ([`since`]) and names the addresses then, as it does the CPU
//! samples'. A sample written over before a flush read it, or being written as it read, is lost
//! and counted in the flush's `skipped`.

use std::{
    sync::{
        OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering, fence},
    },
    time::{Duration, Instant},
};

use blaze_graph::arena::HeapSampled;
use blaze_mem::Tag;

use crate::stack::DEPTH;

/// Samples kept: 8192 of 54 words, 3.5 MB of address space, its pages touched as it fills.
const KEPT: usize = 8192;

/// The words of a sample before its stack: when (nanoseconds since [`EPOCH`]), its thread,
/// its build, tag and depth packed, its innermost span, the block's bytes and the bytes it
/// stands for.
const HEAD: usize = 6;
const WORDS: usize = HEAD + DEPTH;

/// The tag word of a sample made outside any task.
const NO_TAG: u64 = 0xff;

/// One sample's place in [`RING`]: `seq` is `2n + 1` while sample `n` is written into it and
/// `2n + 2` once it is, so a reader takes a sample only whole, and a writer only a place no other
/// writer holds.
struct Slot {
    seq: AtomicU64,
    words: [AtomicU64; WORDS],
}

impl Slot {
    const fn new() -> Slot {
        Slot { seq: AtomicU64::new(0), words: [const { AtomicU64::new(0) }; WORDS] }
    }
}

static RING: [Slot; KEPT] = [const { Slot::new() }; KEPT];

/// How many samples were ever begun: the next sample's number.
static NEXT: AtomicU64 = AtomicU64::new(0);

/// What a sample's time counts from, set as the sampler starts.
static EPOCH: OnceLock<Instant> = OnceLock::new();

/// The sampler was started in this process ([`start`]): a history reads the ring only then, so
/// a test's history never reads what another test put.
static STARTED: AtomicBool = AtomicBool::new(false);

/// Why the process takes no heap samples.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unsampled(pub &'static str);

impl std::fmt::Display for Unsampled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "no heap samples: {}", self.0)
    }
}

impl std::error::Error for Unsampled {}

/// Starts the heap sampler, once a process: from here every
/// [`blaze_graph::arena::SAMPLE_EVERY`] bytes a thread allocates through the counting allocator
/// is a sample. Only the daemon starts it: where the counting allocator is not the global
/// allocator nothing is sampled. A sampler that cannot walk stacks is an error saying why.
pub fn start() -> Result<(), Unsampled> {
    if !crate::stack::ready() {
        return Err(Unsampled(
            "the pipe a stack walk asks the kernel to read frame records through could not be made",
        ));
    }
    _ = EPOCH.get_or_init(Instant::now);
    STARTED.store(true, Ordering::Release);
    blaze_graph::arena::sample_heap_with(taken);
    Ok(())
}

/// One sample read back from [`RING`].
#[derive(Clone, Copy, Debug)]
pub(crate) struct HeapSample {
    pub(crate) at: Instant,
    pub(crate) tid: u64,
    /// The build its thread worked for (0: none), and its innermost span (0: none).
    pub(crate) build: u32,
    pub(crate) span: u64,
    /// The tag of the task that made the block; `None` outside any task.
    pub(crate) tag: Option<Tag>,
    /// The block's own bytes, and the bytes of allocation the sample stands for.
    pub(crate) bytes: u64,
    pub(crate) weight: u64,
    pub(crate) depth: u8,
    /// Innermost first, as the CPU samples keep them.
    pub(crate) pcs: [u64; DEPTH],
}

/// The counting allocator's sample of this thread's allocation `sampled`
/// ([`blaze_graph::arena::sample_heap_with`]): its stack walked and the sample put in [`RING`].
/// Inside the allocator: no lock, no allocation, no ring claimed.
fn taken(sampled: HeapSampled) {
    let Some(epoch) = EPOCH.get() else { return };
    let mut pcs = [0u64; DEPTH];
    let depth = crate::stack::here(&mut pcs);
    let (build, span) = crate::flight::here_now();
    let t_ns = u64::try_from(epoch.elapsed().as_nanos()).unwrap_or(u64::MAX);
    let tag = sampled.holder.map_or(NO_TAG, |it| it.index() as u64);
    let packed = u64::from(build) | tag << 32 | (depth as u64) << 40;
    let head = [t_ns, crate::sampler::os_tid(), packed, span, sampled.bytes, sampled.weight];
    put(head, &pcs[..depth]);
}

/// Writes one sample into its place in [`RING`], unless another writer holds it (a lap behind,
/// still writing) or a later lap took it: then the sample is lost, as one written over is.
fn put(head: [u64; HEAD], pcs: &[u64]) {
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let slot = &RING[(n % KEPT as u64) as usize];
    let writing = 2 * n + 1;
    let mut seen = slot.seq.load(Ordering::Relaxed);
    // A few tries: a weak compare may fail with nothing in the way.
    let mut held = false;
    for _ in 0..4 {
        if seen % 2 == 1 || seen >= writing {
            return;
        }
        match slot.seq.compare_exchange_weak(seen, writing, Ordering::Acquire, Ordering::Relaxed) {
            Ok(_) => {
                held = true;
                break;
            }
            Err(now) => seen = now,
        }
    }
    if !held {
        return;
    }
    for (word, value) in slot.words.iter().zip(head.iter().chain(pcs)) {
        word.store(*value, Ordering::Relaxed);
    }
    slot.seq.store(writing + 1, Ordering::Release);
}

/// The samples begun since sample number `next`, into `into`; moves `next` past them. How
/// many were lost: written over before this read, or not whole as it read. Nothing where the
/// sampler was not started in this process.
pub(crate) fn since(next: &mut u64, into: &mut Vec<HeapSample>) -> u64 {
    match STARTED.load(Ordering::Acquire) {
        true => read(next, into),
        false => 0,
    }
}

fn read(next: &mut u64, into: &mut Vec<HeapSample>) -> u64 {
    let Some(epoch) = EPOCH.get() else { return 0 };
    let total = NEXT.load(Ordering::Acquire);
    let first = (*next).max(total.saturating_sub(KEPT as u64));
    let mut lost = first.saturating_sub(*next);
    for n in first..total {
        let slot = &RING[(n % KEPT as u64) as usize];
        let done = 2 * n + 2;
        if slot.seq.load(Ordering::Acquire) != done {
            lost += 1;
            continue;
        }
        let mut words = [0u64; WORDS];
        for (out, word) in words.iter_mut().zip(&slot.words) {
            *out = word.load(Ordering::Relaxed);
        }
        fence(Ordering::Acquire);
        if slot.seq.load(Ordering::Relaxed) != done {
            lost += 1;
            continue;
        }
        into.push(decode(epoch, &words));
    }
    *next = total;
    lost
}

fn decode(epoch: &Instant, words: &[u64; WORDS]) -> HeapSample {
    let [t_ns, tid, packed, span, bytes, weight] =
        [words[0], words[1], words[2], words[3], words[4], words[5]];
    let depth = ((packed >> 40) & 0xff).min(DEPTH as u64) as u8;
    let mut pcs = [0u64; DEPTH];
    pcs[..usize::from(depth)].copy_from_slice(&words[HEAD..HEAD + usize::from(depth)]);
    HeapSample {
        at: *epoch + Duration::from_nanos(t_ns),
        tid,
        build: packed as u32,
        span,
        tag: Tag::ALL.get(((packed >> 32) & 0xff) as usize).copied(),
        bytes,
        weight,
        depth,
        pcs,
    }
}

/// The frames of the allocator and of the collections that call it for their caller (a vector
/// growing, a map's table, a box): a sample's call site is its innermost frame named by none
/// of these.
const PLUMBING: &[&str] = &[
    "blaze_profile::heap",
    "blaze_profile::stack",
    "blaze_graph::arena",
    "blaze_engine::heaps",
    "__rust_",
    "__rdl_",
    "__rg_",
    "alloc::",
    "<alloc::",
    "core::",
    "<core::",
    "std::",
    "<std::",
    "hashbrown::",
    "<hashbrown::",
    "smallvec::",
    "<smallvec::",
    "mi_",
    "_mi_",
];

/// The call site of a sample whose frames' names `innermost_first` gives: its innermost frame
/// that is not [`PLUMBING`]; `None` when every frame is.
pub(crate) fn site<'a>(mut innermost_first: impl Iterator<Item = &'a str>) -> Option<&'a str> {
    innermost_first.find(|name| !PLUMBING.iter().any(|it| name.starts_with(it)))
}

/// The call site of `pcs` (innermost first), each address's frames from `frames`
/// ([`crate::symbols::frames`]: outermost first, so an inlined function's come last).
pub(crate) fn site_of<'a>(
    pcs: &[u64],
    frames: &'a blaze_hash::HashMap<u64, Vec<String>, std::alloc::Global>,
) -> Option<&'a str> {
    site(pcs.iter().enumerate().flat_map(|(i, pc)| {
        frames
            .get(&crate::symbols::lookup(i, *pc))
            .into_iter()
            .flat_map(|named| named.iter().rev().map(String::as_str))
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The call site is the innermost frame past the allocator and the collections: what a
    /// vector's growth, a map's table and a box were made for.
    #[test]
    fn a_samples_site_is_its_innermost_frame_outside_the_plumbing() {
        let stack = [
            "blaze_profile::heap::taken",
            "blaze_graph::arena::sample",
            "blaze_graph::arena::count",
            "blaze_graph::arena::global::<impl core::alloc::global::GlobalAlloc for blaze_graph::arena::Counting<A>>::alloc",
            "__rust_alloc",
            "<alloc::alloc::Global as core::alloc::Allocator>::allocate",
            "alloc::raw_vec::RawVec<T,A>::grow_one",
            "hir_ty_mir::lower::body",
            "blaze::main",
        ];
        assert_eq!(site(stack.into_iter()), Some("hir_ty_mir::lower::body"));
        assert_eq!(site(["__rust_alloc", "alloc::alloc::alloc"].into_iter()), None);
    }

    /// A sample put is read back whole, once, and a ring written past a reader's place loses
    /// the oldest, counted.
    #[test]
    fn samples_put_are_read_back_whole_once_and_those_written_over_are_counted() {
        _ = EPOCH.get_or_init(Instant::now);
        let mut next = NEXT.load(Ordering::Relaxed);
        let mut read = Vec::new();
        let packed = |depth: u64| 7 | (Tag::Mir.index() as u64) << 32 | depth << 40;
        put([1, 2, packed(3), 4, 48, 4 << 20], &[0x10, 0x20, 0x30]);
        assert_eq!(super::read(&mut next, &mut read), 0);
        let [one] = read.as_slice() else { panic!("one sample: {read:?}") };
        assert_eq!((one.tid, one.build, one.span, one.tag), (2, 7, 4, Some(Tag::Mir)));
        assert_eq!((one.bytes, one.weight, one.depth), (48, 4 << 20, 3));
        assert_eq!(&one.pcs[..3], &[0x10, 0x20, 0x30]);
        read.clear();
        assert_eq!(super::read(&mut next, &mut read), 0, "read once");
        assert!(read.is_empty());
        for at in 0..KEPT as u64 + 5 {
            put([at, 0, 7 | NO_TAG << 32, 0, 1, 1], &[]);
        }
        let lost = super::read(&mut next, &mut read);
        assert_eq!(lost, 5, "the five written over");
        assert_eq!(read.len(), KEPT);
        assert!(read.iter().all(|it| it.tag.is_none() && it.depth == 0), "outside any task");
    }
}
