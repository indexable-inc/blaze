use std::cell::Cell;

use hegel::TestCase;
use hegel::generators as gs;

use super::*;

/// A class of its own, so what a test takes and gives is not shared with tests running at once.
fn own_class(shift: u32, space: usize) -> &'static Class {
    Box::leak(Box::new(Class::new(shift, space).expect("a test's class")))
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
        assert!(block.len() >= bytes && block.len() >= SMALLEST && block.len().is_power_of_two());
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
    let shift = tc.draw(gs::integers::<u32>().min_value(MIN_SHIFT).max_value(16));
    let class = own_class(shift, 64 << 16);
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
        let block = class.take(if zeroed { Fill::Zeroed } else { Fill::Any }, Tag::Other).expect("a block");
        let want = if zeroed { 0 } else { 0xAB };
        assert!(bytes_of(&block).iter().all(|&byte| byte == want), "recycled, zeroed: {zeroed}");
        fill(&block, 0xAB);
    }
    assert_eq!(class.carved(), carved, "every take was a recycled block");
}

#[test]
fn a_class_used_up_is_an_error_not_another_place() {
    let class = own_class(MIN_SHIFT, 2 * SMALLEST);
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
    assert!(matches!(take(64, MAX_ALIGN * 2, Fill::Any, Tag::Other), Err(MemError::TooLarge { .. })));
    assert!(matches!(take(64, 3, Fill::Any, Tag::Other), Err(MemError::TooLarge { .. })));
    assert_eq!(shift_of(0, 1, Tag::Other).ok(), Some(MIN_SHIFT));
    assert_eq!(shift_of(65, 1, Tag::Other).ok(), Some(MIN_SHIFT + 1));
    assert_eq!(shift_of(8, 4096, Tag::Other).ok(), Some(12));
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
            let slice = Slice::new_with(300, Tag::Other, |at| (at + round) as u32).expect("a slice");
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
