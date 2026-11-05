use std::cell::Cell;

use hegel::TestCase;
use hegel::generators as gs;

use super::*;

/// A class of its own, so what a test takes and gives is not shared with tests running at once.
fn own_class(size: usize, space: usize) -> &'static Class {
    Box::leak(Box::new(Class::new(size, space, None, Tag::Chunks).expect("a test's class")))
}

fn bytes_of(block: &Owned) -> &[u8] {
    // SAFETY: the block is committed for its whole length and this test's alone while it lives.
    unsafe { std::slice::from_raw_parts(block.as_ptr().as_ptr(), block.len()) }
}

fn fill(block: &Owned, byte: u8) {
    // SAFETY: as in `bytes_of`, and no shared borrow of it is live.
    unsafe { block.as_ptr().write_bytes(byte, block.len()) };
}

/// Blocks taken and given in any order: every live block is at least what was asked, aligned
/// as asked, disjoint from every other, and keeps what its owner wrote; a zeroed take is zeros
/// whether the block was carved or recycled.
#[hegel::test(test_cases = 200)]
fn live_blocks_are_disjoint_aligned_and_keep_their_bytes(tc: TestCase) {
    let mut live: Vec<(Owned, usize, u8)> = Vec::new();
    let steps: usize = tc.draw(gs::integers::<usize>().max_value(64));
    for step in 0..steps {
        if !live.is_empty() && tc.draw(gs::booleans()) {
            let at = tc.draw(gs::integers::<usize>().max_value(live.len() - 1));
            drop(live.swap_remove(at));
            continue;
        }
        let bytes = tc.draw(gs::integers::<usize>().max_value(1 << 16));
        let align = 1usize << tc.draw(gs::integers::<u32>().max_value(MAX_ALIGN.trailing_zeros()));
        let zeroed = tc.draw(gs::booleans());
        let block = take(bytes, align, if zeroed { Fill::Zeroed } else { Fill::Any }, Tag::Other)
            .expect("a block");
        assert!(block.len() >= bytes && block.len() >= SMALLEST);
        assert_eq!(block.as_ptr().as_ptr() as usize % align, 0, "aligned to {align}");
        if zeroed {
            assert!(bytes_of(&block).iter().all(|&byte| byte == 0), "a zeroed block of {bytes}");
        }
        let start = block.as_ptr().as_ptr() as usize;
        for (other, _, _) in &live {
            let other_start = other.as_ptr().as_ptr() as usize;
            assert!(
                start + block.len() <= other_start || other_start + other.len() <= start,
                "blocks overlap"
            );
        }
        let byte = step as u8 | 1;
        fill(&block, byte);
        live.push((block, bytes, byte));
        for (block, _, byte) in &live {
            assert!(bytes_of(block).iter().all(|it| it == byte), "a block lost its bytes");
        }
    }
}

/// A block given back is the next one its class hands out, carving nothing; asked zeroed, it
/// comes back zeros however its owner left it, and asked for anything, untouched.
#[hegel::test(test_cases = 100)]
fn a_recycled_block_comes_back_zeroed_when_asked(tc: TestCase) {
    let index = tc.draw(gs::integers::<usize>().max_value((16 - MIN_SHIFT) as usize * STEPS));
    let class = own_class(class_size(index), 64 << 16);
    let count = tc.draw(gs::integers::<usize>().min_value(1).max_value(32));
    let blocks: Vec<Owned> =
        (0..count).map(|_| class.take(Fill::Any, Tag::Other).expect("a block")).collect();
    for block in &blocks {
        assert!(bytes_of(block).iter().all(|&byte| byte == 0), "a carved block is zero");
        fill(block, 0xAB);
    }
    let carved = class.carved();
    drop(blocks);
    for _ in 0..count {
        let zeroed = tc.draw(gs::booleans());
        let block =
            class.take(if zeroed { Fill::Zeroed } else { Fill::Any }, Tag::Other).expect("a block");
        let want = if zeroed { 0 } else { 0xAB };
        assert!(bytes_of(&block).iter().all(|&byte| byte == want), "recycled, zeroed: {zeroed}");
        fill(&block, 0xAB);
    }
    assert_eq!(class.carved(), carved, "every take was a recycled block");
}

#[test]
fn a_class_used_up_is_an_error_not_another_place() {
    let class = own_class(SMALLEST, 2 * SMALLEST);
    let first = class.take(Fill::Any, Tag::Other).expect("the first block");
    let second = class.take(Fill::Any, Tag::Other).expect("the second block");
    assert!(matches!(class.take(Fill::Any, Tag::Other), Err(MemError::Exhausted { .. })));
    drop(second);
    assert!(class.take(Fill::Any, Tag::Other).is_ok());
    drop(first);
}

#[test]
fn sizes_and_alignments_past_the_limits_are_errors() {
    assert!(matches!(take(LARGEST + 1, 8, Fill::Any, Tag::Other), Err(MemError::TooLarge { .. })));
    assert!(matches!(
        take(64, MAX_ALIGN * 2, Fill::Any, Tag::Other),
        Err(MemError::TooLarge { .. })
    ));
    assert!(matches!(take(64, 3, Fill::Any, Tag::Other), Err(MemError::TooLarge { .. })));
    let size = |bytes, align| class_of(bytes, align, Tag::Other).map(class_size).ok();
    assert_eq!(size(0, 1), Some(64));
    assert_eq!(size(4096, 4096), Some(4096));
    assert_eq!(size(8, 4096), Some(4096));
    #[cfg(target_pointer_width = "64")]
    {
        assert_eq!(size(65, 1), Some(80));
        assert_eq!(size(129, 1), Some(160));
        assert_eq!(size(1000, 8), Some(1024));
        assert_eq!(size(1025, 8), Some(1280));
        assert_eq!(size(3000, 8), Some(3072));
        // 5 KiB is aligned to 1 KiB only: a 4 KiB alignment takes 8 KiB.
        assert_eq!(size(4097, 1024), Some(5120));
        assert_eq!(size(4097, 4096), Some(8192));
        assert_eq!(size(LARGEST - 1, 1), Some(LARGEST));
    }
    #[cfg(not(target_pointer_width = "64"))]
    assert_eq!(size(65, 1), Some(128));
}

/// Taking and giving a warm class's blocks, and making and dropping slices in them, never
/// calls the global allocator.
#[test]
fn warm_blocks_and_slices_allocate_nothing() {
    drop(take(1000, 8, Fill::Zeroed, Tag::Other).expect("a block"));
    drop(Slice::new_with(300, Tag::Other, |at| at as u32).expect("a slice"));
    blaze_alloc_count::assert_at_most(0, "warm blocks and slices", || {
        for round in 0..100 {
            drop(take(1000, 8, Fill::Zeroed, Tag::Other).expect("a block"));
            let slice =
                Slice::new_with(300, Tag::Other, |at| (at + round) as u32).expect("a slice");
            assert_eq!(slice[299], (299 + round) as u32);
        }
    });
}

/// A slice's values are each dropped once: all of them with the slice, and the ones made so
/// far when a `make` panics.
#[test]
fn a_slice_drops_each_value_once() {
    struct Counted<'a>(&'a Cell<usize>);
    impl Drop for Counted<'_> {
        fn drop(&mut self) {
            self.0.set(self.0.get() + 1);
        }
    }

    let drops = Cell::new(0);
    let slice = Slice::new_with(100, Tag::Other, |_| Counted(&drops)).expect("a slice");
    assert_eq!(slice.len(), 100);
    drop(slice);
    assert_eq!(drops.get(), 100);

    drops.set(0);
    let made = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        Slice::new_with(100, Tag::Other, |at| {
            assert!(at < 40, "make fails at 40");
            Counted(&drops)
        })
    }));
    assert!(made.is_err());
    assert_eq!(drops.get(), 40);
}

#[test]
fn a_slice_of_over_aligned_values_is_aligned() {
    #[repr(align(128))]
    struct Line(u8);
    let slice = Slice::new_with(5, Tag::Other, |at| Line(at as u8)).expect("a slice");
    assert_eq!(slice.as_ptr() as usize % 128, 0);
    assert_eq!(slice[4].0, 4);
    let empty = Slice::new_with(0, Tag::Other, |at| Line(at as u8)).expect("an empty slice");
    assert!(empty.is_empty());
}

/// Every class is at least the size that picks it and at most a step past it, aligned as asked;
/// the classes rise with the size.
#[hegel::test(test_cases = 500)]
fn a_size_picks_the_smallest_class_aligned_as_asked(tc: TestCase) {
    let bytes = tc.draw(gs::integers::<usize>().max_value(LARGEST));
    let align = 1usize << tc.draw(gs::integers::<u32>().max_value(MAX_ALIGN.trailing_zeros()));
    let index = class_of(bytes, align, Tag::Other).expect("a class");
    let wanted = bytes.max(align).max(SMALLEST);
    assert!(class_size(index) >= wanted, "class {index} holds {bytes}");
    assert!(class_align(index) >= align, "class {index} is aligned to {align}");
    if index > 0 && class_align(index - 1) >= align {
        assert!(class_size(index - 1) < wanted, "class {index} is the smallest that holds {bytes}");
    }
}

/// `Kept` values: a `Vec` grown and shrunk across classes keeps its bytes, its tag counts
/// what its blocks hold while they live and nothing after, and a warm `Kept` allocates nothing
/// from the global heap.
#[test]
fn kept_values_keep_their_bytes_count_to_their_tag_and_skip_the_global_heap() {
    use std::alloc::Allocator as _;
    // `Store`: no other test of the crate counts to it, and the tests run at once.
    let kept = Kept(Tag::Store);
    let live = || crate::tag::snapshot().get(Tag::Store).live;
    std::thread::spawn(move || {
        let before = live();
        let mut grown: Vec<u8, Kept> = Vec::new_in(kept);
        for at in 0..10_000u32 {
            grown.push(at as u8);
        }
        assert!(
            grown.iter().enumerate().all(|(at, &byte)| byte == at as u8),
            "kept across every grow"
        );
        assert!(live() >= before + 10_000, "the block's bytes, counted to the tag");
        grown.truncate(100);
        grown.shrink_to_fit();
        assert!(
            grown.iter().enumerate().all(|(at, &byte)| byte == at as u8),
            "kept across the shrink"
        );
        let zeroed =
            kept.allocate_zeroed(std::alloc::Layout::from_size_align(3000, 8).unwrap()).unwrap();
        // SAFETY: a block of at least 3000 bytes, just made.
        assert!(unsafe { zeroed.as_ref() }.iter().all(|&byte| byte == 0));
        // SAFETY: the block of that layout, given back once.
        unsafe {
            kept.deallocate(zeroed.cast(), std::alloc::Layout::from_size_align(3000, 8).unwrap())
        };
        drop(grown);
        let empty: Vec<u64, Kept> = Vec::with_capacity_in(0, kept);
        drop(empty);
        drop(Vec::<u8, Kept>::with_capacity_in(5000, kept));
        blaze_alloc_count::assert_at_most(0, "warm kept values", || {
            for round in 0..100 {
                let mut value: Vec<u32, Kept> = Vec::with_capacity_in(1000, kept);
                value.extend(0..1000u32);
                assert_eq!(value[999] + round, 999 + round);
            }
        });
        before
    })
    .join()
    .map(|before| {
        assert_eq!(live(), before, "every block freed, the thread's frees folded as it ended")
    })
    .expect("the thread ran");
}

/// Which of the pages of `[at, at + len)` (whole pages) are resident, as the kernel says.
#[cfg(target_os = "linux")]
fn resident_pages(at: NonNull<u8>, len: usize) -> usize {
    let page = crate::os::page_size();
    assert!(at.as_ptr() as usize % page == 0 && len % page == 0, "whole pages");
    let mut flags = vec![0u8; len / page];
    // SAFETY: the range is mapped (a committed part of a class's region) and `flags` has a
    // byte per page.
    let rc = unsafe { libc::mincore(at.as_ptr().cast(), len, flags.as_mut_ptr()) };
    assert_eq!(rc, 0, "mincore: {}", std::io::Error::last_os_error());
    flags.iter().filter(|&&flag| flag & 1 != 0).count()
}

/// A block of [`AT_ONCE`] or more gives its pages back as it is given: the class counts it
/// cold and its pages given back, the kernel holds none of them (Linux), and the next take is
/// that block, made the process's again, keeping no byte of its last owner's when asked zeroed.
#[test]
fn a_large_block_gives_its_pages_back_as_it_is_given() {
    let class = own_class(AT_ONCE, 4 * AT_ONCE);
    let block = class.take(Fill::Any, Tag::Other).expect("a block");
    fill(&block, 0x5A);
    let at = block.as_ptr();
    #[cfg(target_os = "linux")]
    assert_eq!(
        resident_pages(at, AT_ONCE),
        AT_ONCE / crate::os::page_size(),
        "written, so resident"
    );
    drop(block);
    let counters = class.counters();
    assert_eq!((counters.warm, counters.cold), (0, AT_ONCE as u64), "{counters:?}");
    assert_eq!(counters.given_back, AT_ONCE as u64, "a page-aligned block gives every page back");
    assert_eq!(counters.live(), 0);
    #[cfg(target_os = "linux")]
    assert_eq!(resident_pages(at, AT_ONCE), 0, "the kernel took its pages");
    let again = class.take(Fill::Zeroed, Tag::Other).expect("the block again");
    assert_eq!(again.as_ptr(), at, "the cold block, not a new one");
    assert!(bytes_of(&again).iter().all(|&byte| byte == 0));
    assert_eq!(class.carved(), 1);
    assert_eq!(class.counters().cold, 0);
}

/// Small blocks stay warm until the class's warm bytes pass [`KEEP`]; then every page that
/// freed neighbors cover whole goes back (counted, and gone from the kernel's count on Linux),
/// the class keeps about half of [`KEEP`] warm, and taking every block again gives each one
/// back whole: distinct, zeroed when asked, keeping what is written.
#[test]
fn a_small_class_past_its_keep_gives_whole_pages_back() {
    let size = 96;
    let count = 3 * KEEP / size;
    let class = own_class(size, 2 * count * size);
    let blocks: Vec<Owned> =
        (0..count).map(|_| class.take(Fill::Any, Tag::Other).expect("a block")).collect();
    for block in &blocks {
        fill(block, 0xC3);
    }
    let first = blocks[0].as_ptr();
    let below = KEEP / size;
    let mut blocks = blocks.into_iter();
    for block in blocks.by_ref().take(below) {
        drop(block);
    }
    let kept = class.counters();
    assert_eq!(
        (kept.warm, kept.cold, kept.given_back, kept.trims),
        ((below * size) as u64, 0, 0, 0),
        "{kept:?}"
    );
    drop(blocks);
    let trimmed = class.counters();
    assert!(trimmed.trims >= 1, "{trimmed:?}");
    assert!(trimmed.warm <= KEEP as u64, "the class keeps at most its keep warm: {trimmed:?}");
    assert_eq!(
        trimmed.warm + trimmed.cold,
        (count * size) as u64,
        "every block is free: {trimmed:?}"
    );
    let page = crate::os::page_size() as u64;
    assert_eq!(trimmed.given_back % page, 0, "whole pages");
    // A run's blocks go cold whole but only the pages it covers whole go back: at most two
    // pages short per run. Added, not subtracted from `cold`: with 16 KiB pages (macOS arm64)
    // the slack is 2 MiB a trim, more than `cold` holds.
    let slack = 2 * page * (trimmed.trims * TRIM_FUEL as u64);
    assert!(trimmed.given_back + slack >= trimmed.cold, "{trimmed:?}");
    #[cfg(target_os = "linux")]
    {
        let len = count * size / page as usize * page as usize;
        let resident = resident_pages(first, len) as u64 * page;
        assert!(
            resident <= len as u64 - trimmed.given_back + 2 * page,
            "resident {resident} of {trimmed:?}"
        );
    }
    let _ = first;
    let again: Vec<Owned> = (0..count)
        .map(|at| {
            class
                .take(if at % 2 == 0 { Fill::Zeroed } else { Fill::Any }, Tag::Other)
                .expect("a block")
        })
        .collect();
    assert_eq!(class.carved() as usize, count, "every take was a block given back");
    let mut starts: Vec<usize> =
        again.iter().map(|block| block.as_ptr().as_ptr() as usize).collect();
    starts.sort_unstable();
    starts.dedup();
    assert_eq!(starts.len(), count, "each block once");
    for (at, block) in again.iter().enumerate() {
        if at % 2 == 0 {
            assert!(bytes_of(block).iter().all(|&byte| byte == 0), "zeroed when asked");
        }
        fill(block, at as u8);
    }
    for (at, block) in again.iter().enumerate() {
        assert!(bytes_of(block).iter().all(|&byte| byte == at as u8), "a block lost its bytes");
    }
    let taken = class.counters();
    assert_eq!((taken.warm, taken.cold, taken.live()), (0, 0, (count * size) as u64), "{taken:?}");
}

/// Free blocks that cover no whole page stay warm, and a class whose warm blocks are all such
/// trims again only once per half [`KEEP`] of frees, not on every fold.
#[test]
fn blocks_that_cover_no_whole_page_stay_warm_and_trim_rarely() {
    let size = 64;
    let count = 4 * KEEP / size;
    let class = own_class(size, 2 * count * size);
    let blocks: Vec<Owned> =
        (0..count).map(|_| class.take(Fill::Any, Tag::Other).expect("a block")).collect();
    let (odd, even): (Vec<(usize, Owned)>, Vec<(usize, Owned)>) =
        blocks.into_iter().enumerate().partition(|(at, _)| at % 2 == 1);
    drop(odd);
    let counters = class.counters();
    assert_eq!((counters.cold, counters.given_back), (0, 0), "no page is free whole: {counters:?}");
    assert_eq!(counters.warm, (count / 2 * size) as u64);
    assert!((1..=2).contains(&counters.trims), "one trim per half keep of frees: {counters:?}");
    drop(even);
}

/// Threads taking and giving blocks of one class at once, through its trims: every live block
/// is its holder's alone and keeps its bytes. Blocks of 3 KiB, so any two neighbors cover a
/// page whole and trims give pages back while the threads take and give.
#[test]
fn threads_taking_and_giving_through_trims_keep_their_blocks() {
    let size = 3072;
    let class = own_class(size, 64 << 20);
    let threads: Vec<_> = (0..4u8)
        .map(|thread| {
            std::thread::spawn(move || {
                let mut held: Vec<(Owned, u8)> = Vec::new();
                let mut seed = (u64::from(thread) + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15);
                for step in 0..200_000u32 {
                    seed ^= seed << 13;
                    seed ^= seed >> 7;
                    seed ^= seed << 17;
                    let grow = held.len() < 100 || (seed % 3 != 0 && held.len() < 2000);
                    if grow {
                        let byte = (step as u8) ^ thread;
                        let block = class.take(Fill::Any, Tag::Other).expect("a block");
                        fill(&block, byte);
                        held.push((block, byte));
                    } else {
                        let at = (seed >> 8) as usize % held.len();
                        let (block, byte) = held.swap_remove(at);
                        assert!(
                            bytes_of(&block).iter().all(|&it| it == byte),
                            "a block lost its bytes"
                        );
                        drop(block);
                    }
                    if step % 50_000 == 0 {
                        // Give back nearly everything, so the class trims.
                        while held.len() > 10 {
                            let (block, byte) = held.pop().expect("held");
                            assert!(
                                bytes_of(&block).iter().all(|&it| it == byte),
                                "a block lost its bytes"
                            );
                        }
                    }
                }
                for (block, byte) in &held {
                    assert!(bytes_of(block).iter().all(|it| it == byte), "a block lost its bytes");
                }
            })
        })
        .collect();
    for thread in threads {
        thread.join().expect("a thread");
    }
    let counters = class.counters();
    assert!(counters.trims > 0 && counters.given_back > 0, "the class trimmed: {counters:?}");
    assert_eq!(counters.live(), 0, "{counters:?}");
}

/// The class counts a snapshot reports: a real class's blocks, held and given back, show in
/// its entry once the thread's gathered counts are folded in.
#[test]
fn the_snapshot_counts_a_class_by_its_blocks() {
    // A size no other test of the crate takes, as the tests run at once.
    let bytes = 200_000;
    let index = class_of(bytes, 8, Tag::Other).expect("a class");
    std::thread::spawn(move || {
        let blocks: Vec<Owned> =
            (0..64).map(|_| take(bytes, 8, Fill::Any, Tag::Other).expect("a block")).collect();
        let size = blocks[0].len() as u64;
        let held = snapshot().classes[index];
        assert_eq!(held.size, size);
        assert!(held.live() >= 64 * size, "{held:?}");
        drop(blocks);
        let freed = snapshot().classes[index];
        assert!(freed.free() >= 64 * size, "{freed:?}");
        assert!(freed.carved >= 64 * size && freed.committed >= freed.carved, "{freed:?}");
        let total = snapshot().total();
        assert!(total.carved >= freed.carved && total.size == 0, "{total:?}");
    })
    .join()
    .expect("the thread ran");
}

/// coord-102 (B5): a block a class handed out counted twice, to [`Tag::Chunks`] (its region's
/// commit) and to its owner's tag (the take), so the tags summed past what was committed (28.38
/// GiB of tags against 21.36 committed, the 7 GiB of the classes' live blocks) and "chunks",
/// 11.04 GiB, was mostly live values. A block counts once: to its owner while it lives, to its
/// class's free tag (here [`Tag::Usercode`], which no other test of this crate counts to) once
/// given back, warm or cold, and the free tag plus the owners' is what the class committed.
#[test]
fn a_block_counts_to_its_owner_while_it_lives_and_to_the_free_tag_after_never_both() {
    let size = 4096;
    let (free, owner) = (Tag::Usercode, Tag::Profile);
    let live = |tag: Tag| crate::tag::snapshot().get(tag).live as i64;
    let class: &'static Class =
        Box::leak(Box::new(Class::new(size, 1 << 20, None, free).expect("a test's class")));
    // What the class committed for its blocks and their links, both counted to `free`.
    let committed = || (class.region.committed() + class.links.committed()) as i64;
    let (free_at, owner_at, committed_at) = (live(free), live(owner), committed());
    let owned = 8 * size as i64;
    let taken: Vec<Owned> =
        (0..8).map(|_| class.take(Fill::Any, owner).expect("a block")).collect();
    let made = committed() - committed_at;
    assert!(made >= owned, "the class committed {made} bytes for {owned} carved");
    assert_eq!(live(owner) - owner_at, owned, "the owner counts what it took");
    assert_eq!(
        live(free) - free_at,
        made - owned,
        "the free tag counts what was committed past the blocks taken"
    );
    drop(taken);
    assert_eq!(
        live(owner) - owner_at,
        0,
        "the owner counts nothing once its blocks are given back"
    );
    assert_eq!(live(free) - free_at, made, "the free tag counts every block given back");
    let again: Vec<Owned> =
        (0..8).map(|_| class.take(Fill::Any, owner).expect("a block")).collect();
    assert_eq!(
        committed() - committed_at,
        made,
        "the blocks given back are taken again, nothing more committed"
    );
    assert_eq!(live(owner) - owner_at, owned);
    assert_eq!(live(free) - free_at, made - owned, "a block taken again counts to its owner alone");
    drop(again);
    assert_eq!((live(owner) - owner_at, live(free) - free_at), (0, made));
}
