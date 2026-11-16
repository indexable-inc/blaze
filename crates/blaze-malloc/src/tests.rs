//! The allocator's tests. On blaze's engine (`blaze_sim::check`): a tape of allocate, free,
//! reallocate and zeroed requests of mixed sizes and alignments against a shadow record. Between
//! real threads: blocks passed from thread to thread and freed there, under a deadline, and an
//! ended thread's heap adopted with frees still arriving. Sizes shrink under Miri, which the
//! coordinator runs (the suite stays under five minutes there). The interleavings of the two
//! cross-thread protocols are the model checker's (`model.rs`).

use core::{alloc::Layout, ptr::NonNull};
use std::{
    alloc::Allocator,
    format,
    string::String,
    sync::{Arc, Barrier, mpsc},
    thread,
    time::Duration,
    vec::Vec,
};

use blaze_sim::{Broke, Config, Tape};

use crate::{Malloc, Stats, class, segment, thread_stats};

/// Cases a property test runs under Miri, whose interpreter is some thousand times slower.
const MIRI_CASES: u32 = 8;

fn cases(n: u32) -> Config {
    Config::cases(if cfg!(miri) { n.min(MIRI_CASES) } else { n })
}

/// `normal`, or `miri` under Miri: a size the interpreter runs quickly that still reaches the
/// same code (each use says what it must reach).
fn sized<T>(normal: T, miri: T) -> T {
    if cfg!(miri) { miri } else { normal }
}

// ---------------------------------------------------------------------------------------------
// Size classes.

/// Every class is larger than the one before, past 128 bytes at most 12.5% larger, and the
/// lookup gives the least class that holds a size, for every size to the largest class.
#[test]
fn classes_are_ordered_tight_and_least() {
    for c in 1..class::CLASSES {
        let (before, now) = (class::size(c - 1), class::size(c));
        assert!(now > before, "class {c} ({now}) is not past class {} ({before})", c - 1);
        if before >= 128 {
            assert!(now - before <= before / 8, "class {c} ({now}) is over 12.5% past {before}");
        }
        assert_eq!(class::SIZES[c] as usize, now);
    }
    assert_eq!(class::size(class::CLASSES - 1), class::MAX);
    let step = sized(1, 61);
    let mut size = 0;
    while size <= class::MAX {
        let c = class::of(size);
        assert!(class::size(c) >= size.max(1), "class {c} does not hold {size} bytes");
        assert!(c == 0 || class::size(c - 1) < size, "class {c} is not the least for {size}");
        size += step;
    }
}

/// An aligned class is aligned and holds the size.
#[test]
fn aligned_classes_meet_the_alignment() {
    for shift in 3..=16 {
        let align = 1usize << shift;
        for size in [1, 7, 8, 9, 24, 48, 100, 1000, 3000, 5000, 40000, 65536] {
            match class::of_aligned(size, align) {
                Some(c) => {
                    let bytes = class::size(c);
                    assert!(bytes >= size && bytes % align == 0, "class {c} for {size}/{align}");
                }
                None => assert!(size.max(align) > class::MAX),
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The thread's word.

/// The direct read of the key's slot (macOS on arm64) gives what `pthread_getspecific` gives, and
/// both name this thread's heap once it has one (rule 45: the private TSD layout, tested).
#[test]
fn the_thread_word_matches_pthread_getspecific() {
    let block = Malloc.allocate(Layout::new::<u64>()).expect("a block");
    let chosen = crate::probe::chosen();
    assert!(!chosen.is_null(), "the thread has a heap after an allocation");
    assert_eq!(chosen.addr(), crate::probe::pthread().addr(), "the direct read and the call agree");
    // SAFETY: allocated just above with this layout.
    unsafe { Malloc.deallocate(block.cast(), Layout::new::<u64>()) };
}

// ---------------------------------------------------------------------------------------------
// The tape.

/// A block the case holds, and the byte its contents were filled from.
struct Live {
    at: NonNull<u8>,
    layout: Layout,
    seed: u8,
}

/// The byte at offset `i` of a block filled from `seed`.
fn byte(seed: u8, i: usize) -> u8 {
    seed.wrapping_add((i as u8).wrapping_mul(31)).wrapping_add((i >> 8) as u8)
}

/// The offsets a case fills and checks of a block of `len` bytes: all of a small one; the edges
/// and every 4 KiB of a larger one, visited directly, so a megabyte costs no megabyte per step
/// (an earlier version filtered every index, which under Miri was most of the suite's time).
fn offsets(len: usize) -> impl Iterator<Item = usize> {
    let (whole, edge) = sized((4096, 512), (256, 64));
    let (head, middle, tail) = if len <= whole {
        (0..len, (0..0).step_by(4096), 0..0)
    } else {
        (0..edge, (4096..len - edge).step_by(4096), len - edge..len)
    };
    head.chain(middle).chain(tail)
}

fn fill(live: &Live) {
    for i in offsets(live.layout.size()) {
        // SAFETY: `i` is inside the live block.
        unsafe { live.at.as_ptr().add(i).write(byte(live.seed, i)) };
    }
}

/// Its first `len` bytes hold what `fill` wrote when the block was `filled` bytes long.
fn holds(live: &Live, filled: usize, len: usize, what: &str) -> Result<(), Broke> {
    for i in offsets(filled).filter(|&i| i < len) {
        // SAFETY: `i` is inside the live block.
        let got = unsafe { live.at.as_ptr().add(i).read() };
        if got != byte(live.seed, i) {
            return Err(Broke::Oracle(format!(
                "{what}: byte {i} of a block of {:?} is {got}, not {}",
                live.layout,
                byte(live.seed, i)
            )));
        }
    }
    Ok(())
}

/// A layout drawn from the tape: mostly small, some medium, a few large; alignments from 1 to
/// 4 KiB, and now and then 2 MiB (not under Miri: a 2 MiB-aligned mapping is 6 MiB there).
fn layout(tape: &Tape) -> Layout {
    let size = match tape.below(10, "size kind") {
        0..=5 => tape.below(129, "tiny size") as usize,
        6 | 7 => tape.below(4097, "small size") as usize,
        8 => tape.below(class::MAX as u64 + 1, "medium size") as usize,
        _ => tape.below(sized(1 << 20, 160 << 10) + 1, "large size") as usize,
    };
    let align = match tape.below(8, "align kind") {
        0..=4 => 1 << tape.below(4, "small align"),
        5 | 6 => 1 << tape.below(13, "align"),
        _ if !cfg!(miri) && tape.one_in(8, "huge align") => segment::SEG,
        _ => 1 << tape.below(17, "big align"),
    };
    Layout::from_size_align(size, align).expect("a layout")
}

/// One case: requests against the shadow `live`, each checked: a new block is aligned and
/// overlaps no live one, a zeroed block reads zero, contents survive a reallocation, and at the
/// end every byte is back (the thread's heap's counters return to where they began, less what
/// other threads freed into it meanwhile).
fn requests(tape: &Tape) -> Result<(), Broke> {
    let start = thread_stats();
    let mut live: Vec<Live> = Vec::new();
    let mut seed = 0u8;
    for _ in 0..tape.index(sized(120, 30), "steps") {
        match tape.below(10, "op") {
            0..=4 => {
                let layout = layout(tape);
                let zero = tape.one_in(3, "zeroed");
                let got = if zero { Malloc.allocate_zeroed(layout) } else { Malloc.allocate(layout) };
                let at = got.map_err(|_| Broke::Oracle(format!("{layout:?} refused")))?.cast::<u8>();
                if at.as_ptr().addr() % layout.align() != 0 {
                    return Err(Broke::Oracle(format!("{at:?} for {layout:?} is not aligned")));
                }
                let (from, to) = (at.as_ptr().addr(), at.as_ptr().addr() + layout.size());
                for other in &live {
                    let (a, b) = (other.at.as_ptr().addr(), other.at.as_ptr().addr() + other.layout.size());
                    if from < b && a < to {
                        return Err(Broke::Oracle(format!(
                            "{at:?} for {layout:?} overlaps a live {:?} at {:?}",
                            other.layout, other.at
                        )));
                    }
                }
                if zero {
                    for i in offsets(layout.size()) {
                        // SAFETY: inside the new block.
                        let got = unsafe { at.as_ptr().add(i).read() };
                        if got != 0 {
                            return Err(Broke::Oracle(format!("zeroed {layout:?}: byte {i} is {got}")));
                        }
                    }
                }
                seed = seed.wrapping_add(1);
                let block = Live { at, layout, seed };
                fill(&block);
                live.push(block);
            }
            5..=7 if !live.is_empty() => {
                let block = live.swap_remove(tape.index(live.len(), "free which"));
                holds(&block, block.layout.size(), block.layout.size(), "before its free")?;
                // SAFETY: a live block of ours with its layout.
                unsafe { Malloc.deallocate(block.at, block.layout) };
            }
            8 | 9 if !live.is_empty() => {
                let at = tape.index(live.len(), "realloc which");
                let new = Layout::from_size_align(layout(tape).size(), live[at].layout.align())
                    .expect("a layout");
                let old = live[at].layout;
                // SAFETY: a live block of ours with its layout.
                let moved = unsafe {
                    if new.size() >= old.size() {
                        Malloc.grow(live[at].at, old, new)
                    } else {
                        Malloc.shrink(live[at].at, old, new)
                    }
                };
                let moved = moved.map_err(|_| Broke::Oracle(format!("{old:?} to {new:?} refused")))?;
                live[at].at = moved.cast();
                live[at].layout = new;
                if moved.cast::<u8>().as_ptr().addr() % new.align() != 0 {
                    return Err(Broke::Oracle(format!("{old:?} moved to {new:?} unaligned")));
                }
                holds(&live[at], old.size(), old.size().min(new.size()), "after a reallocation")?;
                fill(&live[at]);
            }
            _ => {}
        }
    }
    for block in live.drain(..) {
        holds(&block, block.layout.size(), block.layout.size(), "at the end")?;
        // SAFETY: a live block of ours with its layout.
        unsafe { Malloc.deallocate(block.at, block.layout) };
    }
    let end = thread_stats();
    let back = |s: Stats| s.small_bytes.wrapping_add(s.cross_thread_bytes);
    if back(end) != back(start) || end.large_bytes != start.large_bytes {
        return Err(Broke::Oracle(format!("bytes not all returned: from {start:?} to {end:?}")));
    }
    Ok(())
}

/// Gives the calling thread its heap, so the counters a test reads before and after are one
/// heap's: a thread's first allocation may adopt an ended thread's heap, counters and all.
fn own_a_heap() {
    let layout = Layout::new::<u64>();
    let block = Malloc.allocate(layout).expect("a block");
    // SAFETY: allocated just above with this layout.
    unsafe { Malloc.deallocate(block.cast(), layout) };
}

#[test]
fn requests_hold_their_contents_and_return_every_byte() {
    own_a_heap();
    blaze_sim::check(cases(300), requests);
}

/// The fast path's case, many small blocks: the spans fill, run full, come back and are given
/// back; every byte returns.
#[test]
fn many_small_blocks_cycle_through_their_spans() {
    own_a_heap();
    let before = thread_stats();
    let n = sized(100_000, 1_500);
    let layout = Layout::from_size_align(24, 8).expect("a layout");
    let mut blocks = Vec::with_capacity(n);
    for round in 0..3 {
        for i in 0..n {
            let at = Malloc.allocate(layout).expect("a block").cast::<u8>();
            // SAFETY: a fresh block of 24 bytes.
            unsafe { at.as_ptr().write((i + round) as u8) };
            blocks.push(at);
        }
        for (i, at) in blocks.drain(..).enumerate().rev() {
            // SAFETY: written above.
            assert_eq!(unsafe { at.as_ptr().read() }, (i + round) as u8);
            // SAFETY: a live block of ours.
            unsafe { Malloc.deallocate(at, layout) };
        }
    }
    let after = thread_stats();
    assert_eq!(after.small_bytes.wrapping_add(after.cross_thread_bytes), before.small_bytes.wrapping_add(before.cross_thread_bytes));
}

// ---------------------------------------------------------------------------------------------
// Real threads.

/// Waits for `handles` at most `long`, then joins them: a hang is a failure naming what hung.
fn join_within<R>(what: &str, long: Duration, handles: Vec<thread::JoinHandle<R>>) -> Vec<R> {
    blaze_spin::SpinWait::within(what, long, || handles.iter().all(|it| it.is_finished()));
    handles.into_iter().map(|it| it.join().expect("a stress thread ends")).collect()
}

/// A block sent between threads, by address.
struct Sent(NonNull<u8>, Layout);

// SAFETY: a block is plain memory; the sender gives it up.
unsafe impl Send for Sent {}

/// Threads in a ring each allocate blocks of mixed sizes, keep some, and send the rest to the next
/// thread, which checks and frees them: blocks freed by a thread other than their heap's reach
/// their heap, and every block is handed out once.
#[test]
fn threads_pass_blocks_around_a_ring() {
    let threads = sized(6, 3);
    let each = sized(20_000, 150);
    let (senders, receivers): (Vec<_>, Vec<_>) = (0..threads).map(|_| mpsc::channel::<Sent>()).unzip();
    let start = Arc::new(Barrier::new(threads));
    let handles: Vec<_> = receivers
        .into_iter()
        .enumerate()
        .map(|(me, inbox)| {
            let next = senders[(me + 1) % threads].clone();
            let start = Arc::clone(&start);
            thread::Builder::new()
                .name(format!("malloc stress {me}"))
                .spawn(move || {
                    let mut rng = blaze_rng::SplitMix64::new(me as u64 + 1);
                    let mut kept: Vec<Sent> = Vec::new();
                    start.wait();
                    for i in 0..each {
                        let size = match rng.below(10) {
                            // At least 2: byte 0 names the sender, the last byte the step.
                            0..=6 => rng.range(2..129) as usize,
                            7 | 8 => rng.range(129..8193) as usize,
                            _ => rng.range(8193..65537) as usize,
                        };
                        let layout = Layout::from_size_align(size, 8).expect("a layout");
                        let at = Malloc.allocate(layout).expect("a block").cast::<u8>();
                        // SAFETY: a fresh block of `size` bytes.
                        unsafe {
                            at.as_ptr().write(me as u8);
                            at.as_ptr().add(size - 1).write(i as u8);
                        }
                        if rng.coin() {
                            next.send(Sent(at, layout)).expect("the next thread listens");
                        } else {
                            kept.push(Sent(at, layout));
                        }
                        while let Ok(Sent(at, layout)) = inbox.try_recv() {
                            // SAFETY: a block the previous thread wrote and gave up.
                            let from = unsafe { at.as_ptr().read() } as usize;
                            assert_eq!(from, (me + threads - 1) % threads, "a block from the wrong thread");
                            // SAFETY: as above.
                            unsafe { Malloc.deallocate(at, layout) };
                        }
                        if kept.len() > 256 {
                            let Sent(at, layout) = kept.swap_remove(rng.index(kept.len()));
                            // SAFETY: a block of this thread's.
                            unsafe { Malloc.deallocate(at, layout) };
                        }
                    }
                    drop(next);
                    for Sent(at, layout) in kept {
                        // SAFETY: as above.
                        unsafe { Malloc.deallocate(at, layout) };
                    }
                    // The rest of the inbox, until the previous thread hung up.
                    for Sent(at, layout) in inbox {
                        // SAFETY: as above.
                        unsafe { Malloc.deallocate(at, layout) };
                    }
                })
                .expect("a stress thread")
        })
        .collect();
    drop(senders);
    join_within("the ring's threads", Duration::from_secs(sized(60, 240)), handles);
}

/// A large block grows in place while its reservation holds it, and its contents survive the
/// moves past it.
#[test]
fn large_blocks_grow_in_place() {
    let small = Layout::from_size_align(100 << 10, 8).expect("a layout");
    let at = Malloc.allocate(small).expect("a block").cast::<u8>();
    // SAFETY: 100 KiB just allocated.
    unsafe { at.as_ptr().add(1000).write(9) };
    let bigger = Layout::from_size_align(sized(1 << 20, 300 << 10), 8).expect("a layout");
    // SAFETY: a live block with its layout.
    let grown = unsafe { Malloc.grow(at, small, bigger) }.expect("grown").cast::<u8>();
    assert_eq!(grown, at, "a reservation of 2 MiB holds 1 MiB in place");
    // SAFETY: as above.
    assert_eq!(unsafe { grown.as_ptr().add(1000).read() }, 9);
    let huge = Layout::from_size_align(sized(8 << 20, 1 << 20), 8).expect("a layout");
    // SAFETY: as above.
    let moved = unsafe { Malloc.grow(grown, bigger, huge) }.expect("grown").cast::<u8>();
    // SAFETY: as above.
    assert_eq!(unsafe { moved.as_ptr().add(1000).read() }, 9);
    // SAFETY: as above.
    unsafe { Malloc.deallocate(moved, huge) };
}

/// The allocator's errors name what was asked.
#[test]
fn errors_name_the_request() {
    let text: String = format!("{}", crate::MallocError::TooLarge { size: 7, align: 8 });
    assert!(text.contains("7 bytes aligned to 8"), "{text}");
}
