//! The root heap ([`crate::root`]): its classes, every alignment the tree asks for through a
//! give and take and resize, a vector grown to 1 MiB in place and given back to nothing live,
//! eight threads on one shared handle, a leak caught at the root's drop, a full reservation
//! refused by name, and the same offsets for the same calls on two roots.
//!
//! Expected (`cargo test -p blaze-mem --lib -- root_tests`): 9 passed (8 in a release build,
//! which has no leak check).

use core::{
    alloc::{Allocator, Layout},
    fmt::Write,
};
use std::thread;

use blaze_alloc_count::measure;
use blaze_own::{Box, SharedSegs, Vec};

use crate::{
    MemError, Root, RootOpts, Tag, page_size,
    root::{CLASSES, GRAIN, LARGEST, MAX_ALIGN, class_align, class_of, class_size},
};

/// The test's root, made at its top.
fn root() -> Root {
    Root::new(RootOpts::TEST).unwrap_or_else(|e| panic!("the test's root: {e}"))
}

/// Writes `len` bytes of a pattern seeded by `seed` at `at`.
fn fill(at: core::ptr::NonNull<u8>, len: usize, seed: u8) {
    // SAFETY: the caller's block has at least `len` bytes, live, its own and reached by nothing
    // else while this runs.
    let bytes = unsafe { core::slice::from_raw_parts_mut(at.as_ptr(), len) };
    for (i, byte) in bytes.iter_mut().enumerate() {
        *byte = seed.wrapping_add(i as u8);
    }
}

/// Whether the first `len` bytes at `at` hold `fill`'s pattern for `seed`.
fn holds(at: core::ptr::NonNull<u8>, len: usize, seed: u8) -> bool {
    // SAFETY: the caller's block has at least `len` bytes, live, initialized by `fill`.
    let bytes = unsafe { core::slice::from_raw_parts(at.as_ptr(), len) };
    bytes.iter().enumerate().all(|(i, &byte)| byte == seed.wrapping_add(i as u8))
}

/// Each class is the smallest that holds its own size; sizes rise; an alignment divides its
/// size; the small classes step by the grain and the rest by under a fifth of themselves.
#[test]
fn every_class_is_the_smallest_holding_its_size() {
    let mut last = 0;
    for class in 0..CLASSES {
        let size = class_size(class);
        assert!(size > last, "class {class}: {size} after {last}");
        assert_eq!(class_of(size), Some(class), "class {class} of {size} bytes");
        assert_eq!(class_of(last + 1), Some(class), "one past class {} is class {class}", class.wrapping_sub(1));
        assert!(size % class_align(class) == 0 && class_align(class) >= GRAIN && class_align(class) <= MAX_ALIGN);
        if size > 128 {
            assert!((size - last) * 5 <= size, "class {class} of {size} wastes at most a fifth past {last}");
        } else {
            assert_eq!(size - last, GRAIN);
        }
        last = size;
    }
    assert_eq!(last, LARGEST);
    assert_eq!(class_of(LARGEST + 1), None);
    assert_eq!(class_of(0), Some(0));
}

/// A take, a growth, a shrink and a give, for each alignment the tree uses (16, 64, a page) and
/// sizes across the classes: every block aligned as asked, its bytes kept through each resize,
/// and nothing live at the end.
#[test]
fn alloc_grow_shrink_dealloc_keep_bytes_at_every_alignment() {
    let root = root();
    let alloc = root.alloc();
    for align in [16, 64, page_size()] {
        for (n, size) in [1usize, 16, 17, 100, 129, 4096, 70_000, 300_000].into_iter().enumerate() {
            let seed = n as u8;
            let small = Layout::from_size_align(size, align).expect("a layout");
            let big = Layout::from_size_align(size * 3, align).expect("a layout");
            let block = alloc.allocate(small).expect("a block");
            let at = block.cast::<u8>();
            assert_eq!(at.addr().get() % align, 0, "{size} bytes aligned to {align}");
            assert_eq!(block.len(), size);
            fill(at, size, seed);
            // A second block after it, so the growth below cannot always stay in place.
            let other = alloc.allocate(Layout::from_size_align(48, 16).expect("a layout")).expect("a block");
            // SAFETY: `at` is live, made with `small`; `big` is larger, same alignment.
            let grown = unsafe { alloc.grow(at, small, big) }.expect("grown").cast::<u8>();
            assert_eq!(grown.addr().get() % align, 0, "grown {size} aligned to {align}");
            assert!(holds(grown, size, seed), "{size} bytes aligned to {align} kept through growth");
            fill(grown, size * 3, seed);
            // SAFETY: `grown` is live, made with `big`; `small` is smaller, same alignment.
            let shrunk = unsafe { alloc.shrink(grown, big, small) }.expect("shrunk").cast::<u8>();
            assert_eq!(shrunk.addr().get() % align, 0);
            assert!(holds(shrunk, size, seed), "{size} bytes aligned to {align} kept through shrinking");
            // SAFETY: each is live, made with the layout named.
            unsafe {
                alloc.deallocate(shrunk, small);
            }
            // SAFETY: as above.
            unsafe { alloc.deallocate(other.cast(), Layout::from_size_align(48, 16).expect("a layout")) };
        }
    }
    let used = root.used();
    assert_eq!((used.live_blocks, used.live_bytes), (0, 0), "{used:?}");
}

/// A block given back that is not the last carved is the next of its class taken (its list);
/// the last carved one is uncarved, so the end moves back.
#[test]
fn a_given_block_is_reused_and_the_last_is_uncarved() {
    let root = root();
    let alloc = root.alloc();
    let layout = Layout::from_size_align(64, 16).expect("a layout");
    let first = alloc.allocate(layout).expect("a block").cast::<u8>();
    let second = alloc.allocate(layout).expect("a block").cast::<u8>();
    assert_eq!((root.offset_of(first), root.offset_of(second)), (Some(0), Some(64)));
    // SAFETY: live, made with `layout`.
    unsafe { alloc.deallocate(first, layout) };
    let again = alloc.allocate(layout).expect("a block").cast::<u8>();
    assert_eq!(again, first, "the free block of its class is taken again");
    // SAFETY: both live, made with `layout`; `second` ends the carved part.
    unsafe { alloc.deallocate(second, layout) };
    assert_eq!(root.used().carved, 64, "the last block is uncarved");
    // SAFETY: live, made with `layout`; now the last.
    unsafe { alloc.deallocate(again, layout) };
    assert_eq!(root.used().carved, 0);
}

/// A vector grown to 1 MiB alone in the root grows in place (one take in all, nothing copied
/// to a second block) and leaves nothing live when dropped; the same again, on pages already
/// committed, asks the global allocator for nothing.
#[test]
fn a_vec_grown_to_a_mebibyte_and_dropped_leaves_nothing_live() {
    let root = root();
    let push_mib = || {
        let mut bytes: Vec<u8, _> = Vec::new_in(root.alloc());
        for i in 0..(1usize << 20) {
            bytes.push(i as u8);
        }
        assert!(bytes.as_slice().iter().enumerate().all(|(i, &b)| b == i as u8), "every byte kept through each growth");
        let used = root.used();
        assert_eq!(used.live_blocks, 1);
        assert!(used.live_bytes >= 1 << 20, "{used:?}");
        assert!(used.carved < 2 << 20, "grown in place, not left behind: {used:?}");
    };
    let before = root.used().allocations;
    push_mib();
    let used = root.used();
    assert_eq!(used.allocations - before, 1, "the last block grows in place: {used:?}");
    assert_eq!((used.live_blocks, used.live_bytes, used.carved), (0, 0, 0), "{used:?}");
    let ((), global) = measure(push_mib);
    assert_eq!(global.allocations, 0, "a warm root asks the global allocator for nothing");
    assert_eq!(root.used().live_blocks, 0);
}

/// Eight threads on one `SharedAlloc`: each grows a vector of 10,000 values and churns boxes of
/// sizes across the classes, then appends its values to one shared append-only vector. Every
/// value is where it was put, the shared vector holds all 80,000 once, and nothing is live after.
#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "eight plain threads on one root are the test (blaze_graph's pool sits above this crate); the appender wait is bounded by its count"
)]
fn eight_threads_share_one_root() {
    const THREADS: u64 = 8;
    const EACH: u64 = 10_000;
    let root = root();
    let shared = root.shared();
    let all: SharedSegs<u64, _> = SharedSegs::new_in(shared);
    thread::scope(|s| {
        for t in 0..THREADS {
            let all = &all;
            s.spawn(move || {
                let mut mine: Vec<u64, _> = Vec::new_in(shared);
                for i in 0..EACH {
                    mine.push(t * EACH + i);
                    if i % 7 == 0 {
                        let sized = (i as usize % 5 + 1) * 40;
                        let mut churn: Vec<u8, _> = Vec::with_capacity_in(sized, shared);
                        churn.resize(sized, t as u8);
                        let boxed = Box::new_in([t; 4], shared);
                        assert!(churn.as_slice().iter().all(|&b| b == t as u8) && *boxed == [t; 4]);
                    }
                }
                assert!(mine.as_slice().iter().enumerate().all(|(i, &v)| v == t * EACH + i as u64), "thread {t}'s values");
                // One appender at a time: bounded, the others each hold it for 10,000 pushes.
                let mut tries = 0u32;
                loop {
                    if let Some(mut appender) = all.appender() {
                        for &v in mine.as_slice() {
                            appender.push(v);
                        }
                        break;
                    }
                    tries += 1;
                    assert!(tries < 50_000_000, "thread {t} never got the appender");
                    thread::yield_now();
                }
            });
        }
    });
    assert_eq!(all.len() as u64, THREADS * EACH);
    let mut seen: Vec<bool, _> = Vec::new_in(root.alloc());
    seen.resize((THREADS * EACH) as usize, false);
    for i in 0..all.len() {
        let v = *all.get(i).expect("published") as usize;
        assert!(!seen[v], "value {v} appended once");
        seen[v] = true;
    }
    drop(seen);
    drop(all);
    let used = root.used();
    assert_eq!((used.live_blocks, used.live_bytes), (0, 0), "{used:?}");
}

/// A root dropped with a block still live is a test's failure, naming the count.
#[test]
#[cfg(debug_assertions)]
#[should_panic(expected = "dropped with 1 blocks (")]
fn a_root_dropped_with_a_live_block_names_it() {
    let root = root();
    let mut kept: Vec<u64, _> = Vec::with_capacity_in(3, root.alloc());
    kept.push(7);
    core::mem::forget(kept);
    drop(root);
}

/// A take past the reservation is refused to its caller (never an abort), and the root names
/// the reservation and what was wanted; it goes on giving what fits.
#[test]
fn a_full_reservation_is_a_typed_refusal_naming_it() {
    let root = Root::new(RootOpts { reserve: 1 << 20, tag: Tag::Other, live_at_drop: crate::LiveAtDrop::Refused }).unwrap_or_else(|e| panic!("{e}"));
    assert!(root.refusal().is_none());
    let alloc = root.alloc();
    let too_big = Layout::from_size_align(2 << 20, 16).expect("a layout");
    assert!(alloc.allocate(too_big).is_err());
    match root.refusal() {
        Some(MemError::Exhausted { tag: Tag::Other, reserved, wanted }) => {
            assert_eq!((reserved, wanted), (1 << 20, 2 << 20));
        }
        other => panic!("refused as {other:?}"),
    }
    let refusal = root.refusal().expect("refused");
    let mut said = blaze_own::String::new_in(alloc);
    write!(said, "{refusal}").expect("written");
    assert!(said.as_str().contains("1048576 reserved bytes"), "{}", said.as_str());
    // Through a collection: its typed error, nothing aborted.
    let mut bytes: Vec<u8, _> = Vec::new_in(alloc);
    assert!(bytes.try_reserve(4 << 20).is_err());
    // An alignment past a root's is refused by name too.
    assert!(alloc.allocate(Layout::from_size_align(16, MAX_ALIGN * 2).expect("a layout")).is_err());
    assert!(matches!(root.refusal(), Some(MemError::TooLarge { wanted, .. }) if wanted == MAX_ALIGN * 2));
    // What fits is still given.
    bytes.push(1);
    assert_eq!(bytes.as_slice(), [1]);
}

/// The same calls on two roots give the same offsets within each (rule 25: nothing keyed by
/// address order can differ between two runs), on one thread.
#[test]
fn the_same_calls_give_the_same_offsets() {
    fn run(root: &Root) -> Vec<usize, blaze_own::Alloc<'_>> {
        let alloc = root.alloc();
        let mut offsets: Vec<usize, _> = Vec::with_capacity_in(64, alloc);
        let mut live: Vec<(core::ptr::NonNull<u8>, Layout), _> = Vec::with_capacity_in(64, alloc);
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        for _ in 0..48 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            if state % 3 == 0 && !live.is_empty() {
                let (at, layout) = live.swap_remove(state as usize / 3 % live.len());
                // SAFETY: live, made with `layout`.
                unsafe { alloc.deallocate(at, layout) };
                continue;
            }
            let size = (state >> 8) as usize % 5000 + 1;
            let align = [16, 64, 4096][(state >> 40) as usize % 3];
            let layout = Layout::from_size_align(size, align).expect("a layout");
            let at = alloc.allocate(layout).expect("a block").cast::<u8>();
            offsets.push(root.offset_of(at).expect("in the root"));
            live.push((at, layout));
        }
        for (at, layout) in live.drain(..) {
            // SAFETY: live, made with `layout`.
            unsafe { alloc.deallocate(at, layout) };
        }
        drop(live);
        offsets
    }
    let (one, two) = (root(), root());
    let (first, second) = (run(&one), run(&two));
    assert_eq!(first.as_slice(), second.as_slice());
    assert!(!first.is_empty(), "the sequence took blocks");
}

/// The one named form for an entry point that demands `'static` handles: the root moved into
/// its own pages for good, its handles `'static`, its counts still kept.
#[test]
fn a_leaked_root_gives_static_handles() {
    let root = Root::new(RootOpts { reserve: 1 << 20, tag: Tag::Other, live_at_drop: crate::LiveAtDrop::Refused }).unwrap_or_else(|e| panic!("{e}"));
    let leaked: &'static Root = Root::leak(root).unwrap_or_else(|e| panic!("{e}"));
    let alloc: blaze_own::Alloc<'static> = leaked.alloc();
    let mut values: Vec<u32, _> = Vec::new_in(alloc);
    values.extend_from_slice(&[1, 2, 3]);
    assert_eq!(leaked.used().live_blocks, 1);
    drop(values);
    assert_eq!(leaked.used().live_blocks, 0);
    assert!(leaked.used().carved >= size_of::<Root>(), "the root's own block is carved");
}
