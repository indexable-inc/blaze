//! Growth on a frame against a model that copies, with a planted bug; what growth abandons;
//! and the table that abandons nothing.
//!
//! No reference to an old tail outlives a growth, by the types: `Allocator::grow`, `shrink` and
//! `grow_zeroed` take and return raw pointers (`NonNull<u8>`, `NonNull<[u8]>`), the frame makes
//! no `&` or `&mut` to a block's bytes on either path (`ptr::copy_nonoverlapping` and
//! `ptr::write_bytes` on raw pointers), and a collection calls them through `&mut self`, which
//! the borrow checker grants only when no slice of it is borrowed. A block grown in place comes
//! back as the SAME pointer with a longer length, and that pointer's provenance is its whole
//! chunk's or span's (it was made from the frame's cursor with `with_addr`), so writing the new
//! tail through it is in bounds. The model test below keeps only the pointer the last call
//! returned (`it.ptr = moved`) and reads and writes every byte through it.

use core::{
    alloc::{Allocator, Layout},
    ptr::NonNull,
    slice,
};

use blaze_sim::{Broke, Config, Tape};

use super::{Abandoned, abandoned_on_thread, plant_ignores_end};
use crate::{
    Frame, Tag,
    chunks::CHUNK,
    frame::table::{FrameTable, TableRefused},
};

/// What the model test runs against: the code as it is, or with the bug planted.
#[derive(Clone, Copy, Debug)]
enum Plant {
    Clean,
    /// `Frame::grow_top` extends the last block past its chunk's end or its span's committed
    /// end.
    GrowthIgnoresTheBlocksEnd,
}

/// Puts the thread's plant back as it was, however the case ends.
struct Planted(bool);

impl Drop for Planted {
    fn drop(&mut self) {
        plant_ignores_end(self.0);
    }
}

/// A live block and what its bytes should be: the model is a plain copy, grown by copying.
struct Live {
    ptr: NonNull<u8>,
    layout: Layout,
    model: Vec<u8>,
}

fn byte(seed: u64, i: usize) -> u8 {
    (seed as u8) ^ (i as u8).wrapping_mul(7)
}

fn oracle(text: String) -> Broke {
    Broke::Oracle(text)
}

/// A size: mostly small, one in three large enough that a few fill a chunk and the larger
/// ones are spans.
fn size(tape: &Tape) -> usize {
    match tape.one_in(3, "large") {
        true => (100 << 10) + tape.below(1400 << 10, "large size") as usize,
        false => 1 + tape.below(4096, "size") as usize,
    }
}

fn align(tape: &Tape) -> usize {
    1 << tape.below(13, "align")
}

/// The block is aligned and lies whole in one block of the frame: asked BEFORE its bytes are
/// read or written, so a block handed out past a chunk's end is a finding, never a wild write.
fn placed(frame: &Frame, ptr: NonNull<u8>, layout: Layout) -> Result<(), Broke> {
    if ptr.addr().get() % layout.align() != 0 {
        return Err(oracle(format!("a block of {layout:?} is misaligned at {ptr:?}")));
    }
    if !frame.holds(ptr, layout.size()) {
        return Err(oracle(format!("a block of {layout:?} at {ptr:?} is outside its block")));
    }
    Ok(())
}

/// The first `model.len()` bytes of the block are the model's.
fn same(it: &Live) -> Result<(), Broke> {
    // SAFETY: `ptr` is a live block of the frame of at least `model.len()` bytes (`placed`
    // held for its layout, and the model is never longer than the layout); only the test
    // writes it, through this pointer.
    let bytes = unsafe { slice::from_raw_parts(it.ptr.as_ptr(), it.model.len()) };
    match bytes == it.model.as_slice() {
        true => Ok(()),
        false => Err(oracle(format!("a block of {:?} differs from the model", it.layout))),
    }
}

/// Writes bytes `model.len()..layout.size()` of the block, in the frame and the model alike.
fn fill(it: &mut Live, seed: u64) {
    for i in it.model.len()..it.layout.size() {
        // SAFETY: `i < layout.size()`, inside the live block `placed` found whole in the frame.
        unsafe { it.ptr.as_ptr().wrapping_add(i).write(byte(seed, i)) };
        it.model.push(byte(seed, i));
    }
}

fn disjoint(live: &[Live]) -> Result<(), Broke> {
    let mut ranges: Vec<(usize, usize)> =
        live.iter().map(|it| (it.ptr.addr().get(), it.layout.size())).collect();
    ranges.sort_unstable();
    for pair in ranges.windows(2) {
        if pair[0].0 + pair[0].1 > pair[1].0 {
            return Err(oracle(format!("two live blocks overlap: {:?} and {:?}", pair[0], pair[1])));
        }
    }
    Ok(())
}

/// One case: a random sequence of allocate, grow, grow zeroed, shrink, deallocate and reset on
/// a frame, each block's bytes kept beside it in a model that copies.
fn run(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let _planted =
        Planted(plant_ignores_end(matches!(plant, Plant::GrowthIgnoresTheBlocksEnd)));
    let mut frame = Frame::with_reserve(1 << 30, Tag::Mir)
        .map_err(|error| Broke::Harness(format!("the frame's reserve: {error}")))?;
    let mut live: Vec<Live> = Vec::new();
    // What the model says growth left behind since the last reset: every block that moved.
    let mut left = Abandoned::default();
    let steps = u64::from(tape.int_in(1..=96u32, "steps"));
    for step in 0..steps {
        match tape.below(8, "op") {
            0..=2 => {
                let layout = Layout::from_size_align(size(tape), align(tape))
                    .map_err(|_| Broke::Harness("a layout".into()))?;
                let ptr = frame
                    .allocate(layout)
                    .map_err(|_| Broke::Harness(format!("{layout:?} refused within the reserve")))?
                    .cast::<u8>();
                placed(&frame, ptr, layout)?;
                let mut it = Live { ptr, layout, model: Vec::new() };
                fill(&mut it, step);
                live.push(it);
            }
            op @ 3..=5 if !live.is_empty() => {
                let which = tape.index(live.len(), "which");
                let old = live[which].layout;
                let ptr = live[which].ptr;
                let grows = op != 5;
                // Never zeroed with the bug planted: `grow_zeroed` itself writes the new tail,
                // which the plant puts past the block's end, before the oracle can look.
                let zeroed = op == 4 && matches!(plant, Plant::Clean);
                let new_size = match grows {
                    true => old.size() + size(tape),
                    false => 1 + tape.below(old.size() as u64, "shrunk size") as usize,
                };
                let new_align = match tape.one_in(4, "another alignment") {
                    true => align(tape),
                    false => old.align(),
                };
                let new = Layout::from_size_align(new_size, new_align)
                    .map_err(|_| Broke::Harness("a layout".into()))?;
                // Whether the frame must grow it where it is: the last block, aligned already,
                // with room up to the bump's end.
                let top = ptr.addr().get() + old.size() == frame.cursor.get().addr();
                let aligned = ptr.addr().get() % new.align() == 0;
                let room = ptr.addr().get() + new.size() <= frame.end.get();
                let used = frame.used();
                let moved = match (grows, zeroed) {
                    // SAFETY: `ptr` is a live block of the frame of layout `old`, and
                    // `new.size() >= old.size()`.
                    (true, false) => unsafe { frame.grow(ptr, old, new) },
                    // SAFETY: as above.
                    (true, true) => unsafe { frame.grow_zeroed(ptr, old, new) },
                    // SAFETY: a live block of layout `old`, and `new.size() <= old.size()`.
                    (false, _) => unsafe { frame.shrink(ptr, old, new) },
                }
                .map_err(|_| Broke::Harness(format!("{old:?} to {new:?} refused")))?
                .cast::<u8>();
                placed(&frame, moved, new)?;
                if moved != ptr {
                    left = left.plus(Abandoned { growths: 1, bytes: old.size() as u64 });
                    if top && aligned && room {
                        return Err(oracle(format!(
                            "the last block moved though it had room: {old:?} to {new:?}"
                        )));
                    }
                } else if top && frame.used() != used - old.size() + new.size() {
                    return Err(oracle(format!(
                        "the last block changed in place from {old:?} to {new:?} and the frame's \
                         use went from {used} to {}",
                        frame.used()
                    )));
                }
                let it = &mut live[which];
                it.ptr = moved;
                it.layout = new;
                it.model.truncate(new.size());
                same(it)?;
                if zeroed {
                    // SAFETY: `[old.size(), new.size())` is inside the block `placed` found.
                    let tail = unsafe {
                        slice::from_raw_parts(
                            moved.as_ptr().wrapping_add(old.size()),
                            new.size() - old.size(),
                        )
                    };
                    if tail.iter().any(|&it| it != 0) {
                        return Err(oracle(format!("{old:?} to {new:?}: the new tail not zeroed")));
                    }
                }
                fill(it, step);
            }
            6 if !live.is_empty() => {
                let which = tape.index(live.len(), "which");
                let it = live.swap_remove(which);
                // SAFETY: a live block of the frame of this layout, given back once.
                unsafe { frame.deallocate(it.ptr, it.layout) };
            }
            7 if tape.one_in(4, "reset") => {
                for it in &live {
                    same(it)?;
                }
                live.clear();
                frame.reset();
                left = Abandoned::default();
                if frame.used() != 0 {
                    return Err(oracle(format!("a reset frame uses {} bytes", frame.used())));
                }
            }
            _ => {}
        }
        disjoint(&live)?;
        if frame.abandoned() != left {
            return Err(oracle(format!(
                "the frame says `{}`, the model `{left}`",
                frame.abandoned()
            )));
        }
    }
    for it in &live {
        same(it)?;
    }
    Ok(())
}

/// (i) Random allocate, grow, grow zeroed, shrink, deallocate and reset against a model that
/// copies: every live block is aligned, whole inside one block of the frame and disjoint from
/// the others; its bytes are the model's; a zeroed tail is zero; the last block with room
/// never moves; the abandoned count is the blocks that moved.
///
/// Prints: `test frame::grow::tests::a_frame_matches_a_copying_model_on_allocate_grow_shrink_and_reset ... ok`
#[test]
fn a_frame_matches_a_copying_model_on_allocate_grow_shrink_and_reset() {
    blaze_sim::check(Config::cases(200), |tape| run(tape, Plant::Clean));
}

/// (iii) The planted bug, growth in place that ignores the chunk's end (and a span's committed
/// end), is caught by the model test's oracle before any byte is written past the end.
/// Removing the cause: with `&& !grow::ignores_end()` gone from `Frame::grow_top` the plant
/// does nothing and this test fails with "the oracle missed GrowthIgnoresTheBlocksEnd".
///
/// Prints: `test frame::grow::tests::growth_in_place_that_ignores_the_blocks_end_is_caught ... ok`
#[test]
fn growth_in_place_that_ignores_the_blocks_end_is_caught() {
    blaze_sim::control(
        Config::cases(200),
        Plant::Clean,
        Plant::GrowthIgnoresTheBlocksEnd,
        "is outside its block",
        run,
    );
}

/// (ii) One vector growing from 1 to 1,000,000 elements on an otherwise idle frame grows in
/// place: its address changes at most once (where it leaves its first chunk for a span of its
/// own), so at most one block of at most a chunk is abandoned, and the frame uses at most the
/// vector's final capacity plus one chunk.
///
/// Prints (with `--nocapture`): `alone: abandoned by growth: 1048576 bytes in 1 growths; used 9437184 of capacity 8388608`
#[test]
fn a_vector_alone_grows_a_million_elements_within_its_size_and_a_chunk() {
    let frame = Frame::with_reserve(1 << 30, Tag::Mir).expect("reserve");
    let mut values: Vec<u64, &Frame> = Vec::new_in(&frame);
    let mut moves = 0;
    let mut at = values.as_ptr();
    for i in 0..1_000_000u64 {
        values.push(i);
        if values.as_ptr() != at && i != 0 {
            moves += 1;
        }
        at = values.as_ptr();
    }
    assert!(values.iter().enumerate().all(|(i, &it)| it == i as u64));
    let bytes = values.capacity() * size_of::<u64>();
    let abandoned = frame.abandoned();
    println!("alone: {abandoned}; used {} of capacity {bytes}", frame.used());
    assert!(moves <= 1, "the vector moved {moves} times");
    assert!(abandoned.growths <= 1 && abandoned.bytes <= CHUNK as u64, "{abandoned}");
    assert!(frame.used() <= bytes + CHUNK, "used {} for {bytes}", frame.used());
    assert!(frame.peak() <= bytes + CHUNK, "peak {} for {bytes}", frame.peak());
}

/// (ii, the other half) With a second vector interleaved neither is the frame's last block
/// when it grows: each growth falls back to a new block and a copy, the contents are kept, and
/// the counter says what was left behind. A reset starts the count again and adds it to the
/// thread's.
///
/// Prints (with `--nocapture`): `interleaved: abandoned by growth: N bytes in M growths` with
/// `M >= 2` and `N` at least half the smaller vector's final bytes.
#[test]
fn two_vectors_interleaved_fall_back_and_the_counter_says_so() {
    let mut frame = Frame::with_reserve(1 << 30, Tag::Mir).expect("reserve");
    let smaller = {
        let mut a: Vec<u64, &Frame> = Vec::new_in(&frame);
        let mut b: Vec<u64, &Frame> = Vec::new_in(&frame);
        for i in 0..100_000u64 {
            a.push(i);
            b.push(!i);
        }
        assert!(a.iter().zip(&b).enumerate().all(|(i, (&a, &b))| a == i as u64 && b == !(i as u64)));
        a.capacity().min(b.capacity()) * size_of::<u64>()
    };
    let abandoned = frame.abandoned();
    println!("interleaved: {abandoned}");
    assert!(abandoned.growths >= 2, "{abandoned}");
    assert!(abandoned.bytes >= (smaller / 2) as u64, "{abandoned}");
    assert_eq!(abandoned.bytes, frame.copied().bytes, "every moved block was copied whole");
    let before = abandoned_on_thread();
    frame.reset();
    assert_eq!(frame.abandoned(), Abandoned::default(), "a reset starts the count again");
    assert_eq!(abandoned_on_thread(), before.plus(abandoned), "a reset reports the frame's count");
}

/// The table for growth under interleaved allocations: 1,000,000 pushes with one small
/// allocation on the same frame after each. Nothing is copied or abandoned, no row moves, and
/// the table holds at most a quarter over its rows. The same pushes into a `Vec<u64, &Frame>`
/// (the cause, put back) abandon at least the vector's final bytes.
///
/// Prints (with `--nocapture`): `table: abandoned by growth: 0 bytes in 0 growths; committed C for 8000000`
/// then `vector: abandoned by growth: N bytes in M growths` with `N >= 8000000`.
#[test]
fn a_frame_table_under_interleaved_allocations_copies_and_abandons_nothing() {
    const ROWS: usize = 1_000_000;
    let frame = Frame::with_reserve(1 << 30, Tag::Mir).expect("reserve");
    {
        let mut table: FrameTable<'_, u64> = frame.table(ROWS).expect("reserve");
        let first = table.as_slice().as_ptr();
        for i in 0..ROWS as u64 {
            let index = table.try_push(i).expect("within its rows");
            assert_eq!(u64::from(index), i);
            frame.alloc(i as u8);
        }
        assert_eq!(table.as_slice().as_ptr(), first, "no row moved");
        assert!(table.iter().enumerate().all(|(i, &it)| it == i as u64));
        let rows = ROWS * size_of::<u64>();
        println!("table: {}; committed {} for {rows}", frame.abandoned(), table.committed());
        assert_eq!(frame.abandoned(), Abandoned::default());
        assert_eq!(frame.copied().copies, 0);
        assert!(table.committed() <= rows + rows / 4 + (64 << 10), "{}", table.committed());
        assert!(frame.used() <= ROWS + CHUNK, "the frame holds only the small allocations");
        assert_eq!(table.peak(), rows);
    }
    let frame = Frame::with_reserve(1 << 30, Tag::Mir).expect("reserve");
    let mut vector: Vec<u64, &Frame> = Vec::new_in(&frame);
    for i in 0..ROWS as u64 {
        vector.push(i);
        frame.alloc(i as u8);
    }
    println!("vector: {}", frame.abandoned());
    assert!(frame.abandoned().bytes >= (ROWS * size_of::<u64>()) as u64, "{}", frame.abandoned());
}

/// A full table refuses by type, naming the frame's tag, the bytes asked, its peak and its
/// reserve; nothing panics and the rows it holds stay.
///
/// Prints: `test frame::grow::tests::a_full_frame_table_refuses_by_type ... ok`
#[test]
fn a_full_frame_table_refuses_by_type() {
    let frame = Frame::with_reserve(1 << 20, Tag::Mir).expect("reserve");
    let mut table: FrameTable<'_, u32> = frame.table(3).expect("reserve");
    for i in 0..3 {
        assert_eq!(table.try_push(i).expect("room"), i);
    }
    match table.try_push(3) {
        Err(TableRefused::Full { tag, asked, peak, reserve, .. }) => {
            assert_eq!((tag, asked, peak, reserve), (Tag::Mir, 16, 12, 12));
        }
        other => panic!("a fourth row of three: {other:?}"),
    }
    assert!(matches!(table.try_extend_from_slice(&[7, 8]), Err(TableRefused::Full { asked: 20, .. })));
    assert!(matches!(table.try_reserve(1), Err(TableRefused::Full { .. })));
    assert_eq!(table.as_slice(), &[0, 1, 2]);
    assert!(matches!(
        frame.table::<u32>(usize::MAX),
        Err(crate::MemError::TooLarge { .. })
    ));
}
