//! Each collection against std's as the oracle, over random operation sequences
//! (`blaze_sim::check`), under a counting allocator that proves every block is freed exactly
//! once with the layout it was made with; panics in the middle of `extend`, `drain`, `retain`,
//! `truncate` and `clone`; values sized 0; alignment over 16; `Arc` under threads; unsizing; and
//! a planted-bug control per oracle (rule 53).
//!
//! The counting allocator writes a pattern into every block it hands out, so a planted bug that
//! reads a slot never written reads defined bytes (the oracle sees the wrong value) rather than
//! uninitialized memory. The tests use no `unsafe` but the allocator's and the raw-pointer round
//! trips', whose contracts they keep.
//!
//! The tests named for one hard spot (`make_mut` with only a `Weak`, `get_mut` against another
//! thread's reads, growth from every wrap, the columns' places) are small and wait on nothing
//! timed, so they mean the same under Miri, whose race detector checks the orderings.
//!
//! # Under Miri
//!
//! `cargo miri test -p blaze-own` runs the whole suite at Miri sizes: every property test
//! [`MIRI_CASES`] cases (`cases`), and the sizes inside them small (`sized`), each still
//! reaching its code: the B-tree tests insert at least [`MIRI_TALL`] keys where they need
//! height two, which `miri_sizes_reach_every_shape` holds in the normal suite. Every
//! `every_failure` test refuses at most `testing::MIRI_FAILURE_POINTS` (24) allocations, spread
//! evenly over its run, and the B-tree's builds its smallest splitting tree
//! (`MIRI_BTREE_TRY`, which `btree_try_forms_at_miri_sizes` runs in the normal suite). The planted-bug
//! controls are ignored under Miri (each says why): they test the oracle, not the crate's
//! unsafe code, and their plants leak or read stale slots on purpose; each one's clean half is
//! a test of its own. The one test that leaks on purpose (the `every_failure` control) names
//! its one block to Miri (`miri_leaks_on_purpose`). Which tests reach which unsafe code:
//!
//! | unsafe code | reached by |
//! |---|---|
//! | `RawBuf`: allocate, grow, shrink, free; values sized 0; alignment over 16 | `vec_matches_std`, `values_sized_zero_never_allocate`, `over_aligned_values_are_aligned`, `vec_try_forms_*` |
//! | `Vec`: the initialized prefix, `drain`, `retain`, `truncate`, `extend`, `clone`, and their unwinding | `vec_matches_std`, `vec_frees_everything_when_*`, `vec_sorts_and_dedups_a_tail`, `vec_in_makes_what_it_says` |
//! | the block copies and the `TrustedLen` extend (`spec`), `insert_many` | `specialized_paths_match_the_generic_ones`, `trusted_extend_frees_everything_when_the_iterator_panics`, `insert_many_frees_everything_when_a_clone_panics`, `collect_in_from_an_exact_iterator_allocates_once` |
//! | `VecDeque`: the wrapped slots, growth from a wrap, `append` in runs, drain and retain unwinding | `deque_matches_std`, `deque_grows_from_every_wrap`, `deque_grown_while_wrapped_matches_std`, `deque_frees_everything_when_a_drop_panics_across_the_wrap`, `deque_drain_and_retain_keep_order_when_a_drop_panics` |
//! | `Box`: unsizing, `dyn` drop, `CallOnce`, pinned unsizing | `box_unsizes_and_drops_once`, `boxed_closures_are_called_once_and_freed`, `pinned_box_unsizes_and_drops_its_future_once` |
//! | `Arc`, `Rc`, their `Weak`s: counts, `make_mut`, `get_mut`, `new_cyclic`, raw round trips, slices | `arc_under_threads_drops_once_and_frees`, `arc_unique_paths`, `arc_make_mut_with_only_a_weak`, `arc_get_mut_waits_for_the_others_reads`, `new_cyclic_refers_to_itself_and_frees`, `raw_pointers_round_trip`, `arc_slice_frees_everything_when_clone_panics`, `arc_clones_count`, `rc_counts_and_frees` |
//! | `Soa`: one block of columns, growth, the column copies, permute, compaction | `soa_matches_a_vec_of_rows`, `soa_grows_every_column`, `soa_columns_sit_where_the_layout_says`, `soa_frees_everything_when_a_drop_panics`, `soa_retain_keeps_the_unseen_when_a_drop_panics`, `soa_of_fields_sized_zero_never_allocates`, `soa_debug_and_insert_remove` |
//! | `SortedMap`, `SortedSet` | `sorted_map_and_set_match_std`, `sorted_map_entry_and_index`, `from_sorted_refuses_what_is_not` |
//! | `BTreeMap`, `BTreeSet`: nodes, split, merge, borrow from either side, delete from an internal node, the dying iterators, `#[may_dangle]` drop, unwinding from user code | `btree_matches_std`, `btree_shrinks_to_empty_like_std`, `btree_structural_cases`, `btree_stays_whole_when_user_code_panics`, `btree_try_forms_fail_cleanly_at_every_allocation` |
//! | `Packed` | `packed_matches_a_vector_and_seeks`, `packed_try_push_fails_cleanly_at_every_allocation`, `packed_stops_on_a_pack_that_reads_another_length` |
//! | `FixedVec`, `FixedString`, `Inline` | `fixed_vec_refuses_past_its_capacity_and_drops_once`, `fixed_string_refuses_a_piece_whole_and_cuts_at_no_boundary`, `inline_holds_collections_on_the_stack_and_refuses_when_full`, `inline_under_every_failure` |
//! | `String`, `Cow`, `make` (`CloneIn`, `Share`) | `string_stays_utf8_and_frees`, `cow_owns_only_when_it_must`, `make_try_forms_fail_cleanly_at_every_allocation`, `share_adds_a_handle_and_allocates_nothing`, `clone_in_leaves_the_first_allocator_alone` |
//! | `Alloc`, `SharedAlloc`, `Erased` (the handle) | `handle`'s tests: `*_through_a_handle`, `a_handle_is_one_word_and_copies_are_the_same_allocator`, `a_handle_of_an_inline_buffer_grows_in_place_and_refuses_when_full`, `a_vector_over_a_shared_handle_crosses_threads`, `the_global_handle_allocates` |
//! | the testing allocators (`testing`) | every `*_fail_cleanly_at_every_allocation`, `every_failure_catches_a_swallowed_refusal_and_a_leak` |

// The cases still call the old names (`insert`, `remove`, `swap_remove`), which forward to the
// ones that say the cost: they test both until the callers move.
#![allow(deprecated, reason = "the old names forward to the new ones and are tested through them")]

use core::{
    alloc::{AllocError, Allocator, Layout},
    ptr::NonNull,
    sync::atomic::{AtomicUsize, Ordering::SeqCst},
};
use std::{
    alloc::{GlobalAlloc, System},
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    format,
    panic::{AssertUnwindSafe, catch_unwind},
    string::String,
    sync::{Mutex, MutexGuard, PoisonError},
    vec::Vec as StdVec,
};

use blaze_sim::{Broke, Config, Tape};

use crate::{
    Arc, Box, Cow, Rc, SortedMap, SortedSet, Vec, VecDeque,
    plant::{self, Plant},
};

// The cases below again, through an `Alloc` handle; `every_failure`, `Inline` and a planted bug
// through one.
mod handle;
// The fast paths of `spec` against the generic ones.
mod spec;
// Lists, Texts and `get_or_insert_with` against their std forms.
mod shapes;
// `Heap4` against a sorted vector, and its plant's typed control.
mod heap4;

// ---------------------------------------------------------------------------------------------
// The counting allocator.

/// An allocator over `System` that keeps every live block and its layout: a free of a block
/// not live, a free with another layout and a request for zero bytes are each recorded as
/// wrong; [`Counting::check`] says whether anything is live or wrong.
struct Counting {
    ledger: Mutex<Ledger>,
}

#[derive(Default)]
struct Ledger {
    live: BTreeMap<usize, Layout>,
    allocs: usize,
    frees: usize,
    /// Allocations to grant before refusing one, when set.
    refuse_in: Option<usize>,
    wrong: StdVec<String>,
}

/// The pattern a fresh block is filled with.
const POISON: u8 = 0xA5;

impl Counting {
    fn new() -> Counting {
        Counting { ledger: Mutex::new(Ledger::default()) }
    }

    fn lock(&self) -> MutexGuard<'_, Ledger> {
        self.ledger.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn live(&self) -> usize {
        self.lock().live.len()
    }

    fn allocs(&self) -> usize {
        self.lock().allocs
    }

    /// Every live block: its address and the layout it was made with.
    fn live_layouts(&self) -> StdVec<(usize, Layout)> {
        self.lock().live.iter().map(|(&addr, &layout)| (addr, layout)).collect()
    }

    /// The allocation after `granted` more is refused (once).
    fn refuse_after(&self, granted: usize) {
        self.lock().refuse_in = Some(granted);
    }

    /// Nothing live, nothing wrong.
    fn check(&self) -> Result<(), Broke> {
        let ledger = self.lock();
        if let Some(wrong) = ledger.wrong.first() {
            return Err(Broke::Oracle(format!("the allocator saw: {wrong}")));
        }
        if !ledger.live.is_empty() {
            let blocks = ledger.live.len();
            return Err(Broke::Oracle(format!("{blocks} blocks never freed")));
        }
        if ledger.allocs != ledger.frees {
            return Err(Broke::Oracle(format!("{} allocations, {} frees", ledger.allocs, ledger.frees)));
        }
        Ok(())
    }
}

// SAFETY: blocks come from `System` for the layout asked, are not handed out twice while live,
// and are freed with the layout they were made with (a free of anything else is recorded and
// not passed on).
unsafe impl Allocator for Counting {
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        let mut ledger = self.lock();
        if layout.size() == 0 {
            ledger.wrong.push(format!("a request for zero bytes ({layout:?})"));
            return Err(AllocError);
        }
        match ledger.refuse_in {
            Some(0) => {
                ledger.refuse_in = None;
                return Err(AllocError);
            }
            Some(n) => ledger.refuse_in = Some(n - 1),
            None => {}
        }
        // SAFETY: the layout is not sized 0 (checked above).
        let raw = unsafe { System.alloc(layout) };
        let Some(block) = NonNull::new(raw) else { return Err(AllocError) };
        // SAFETY: the block has `layout.size()` bytes, just allocated and unused.
        unsafe { block.as_ptr().write_bytes(POISON, layout.size()) };
        ledger.live.insert(block.addr().get(), layout);
        ledger.allocs += 1;
        Ok(NonNull::slice_from_raw_parts(block, layout.size()))
    }

    unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
        let mut ledger = self.lock();
        match ledger.live.remove(&ptr.addr().get()) {
            None => ledger.wrong.push(format!("a free of {ptr:?}, not live: freed twice, or never ours")),
            Some(made) => {
                if made != layout {
                    ledger.wrong.push(format!("a free with {layout:?} of a block made with {made:?}"));
                }
                ledger.frees += 1;
                // SAFETY: the block is live, from `System` with `made`.
                unsafe { System.dealloc(ptr.as_ptr(), made) };
            }
        }
    }
}

// SAFETY: a block is invalidated only by `deallocate` (which passes it to `System`): dropping the
// ledger frees nothing it handed out, no method invalidates a block, and every `Counting` is its
// own `System` blocks, so a pinned box over one keeps its block until it frees it.
unsafe impl core::alloc::StaticAllocator for Counting {}

// ---------------------------------------------------------------------------------------------
// Tokens: values whose drops are counted, on this thread.

/// A value whose every copy is registered, so a drop of one not live (dropped twice) and a copy
/// never dropped are seen; it panics on drop or clone when told to.
#[derive(Debug)]
struct Token {
    value: u64,
    serial: u64,
}

#[derive(Default)]
struct Book {
    live: BTreeSet<u64>,
    next: u64,
    wrong: StdVec<String>,
    panic_on_drop: Option<u64>,
    panic_on_clone: Option<u64>,
}

std::thread_local! {
    static BOOK: RefCell<Book> = RefCell::new(Book::default());
}

impl Token {
    fn new(value: u64) -> Token {
        BOOK.with_borrow_mut(|book| {
            let serial = book.next;
            book.next += 1;
            book.live.insert(serial);
            Token { value, serial }
        })
    }
}

impl Clone for Token {
    fn clone(&self) -> Token {
        let panics = BOOK.with_borrow_mut(|book| {
            let panics = book.panic_on_clone == Some(self.value);
            if panics {
                book.panic_on_clone = None;
            }
            panics
        });
        if panics {
            panic!("a planned panic in the clone of {}", self.value);
        }
        Token::new(self.value)
    }
}

impl Drop for Token {
    fn drop(&mut self) {
        let panics = BOOK.with_borrow_mut(|book| {
            if !book.live.remove(&self.serial) {
                book.wrong.push(format!("token {} ({}) dropped twice", self.serial, self.value));
            }
            let panics = book.panic_on_drop == Some(self.value);
            if panics {
                book.panic_on_drop = None;
            }
            panics
        });
        if panics {
            panic!("a planned panic in the drop of {}", self.value);
        }
    }
}

impl PartialEq for Token {
    fn eq(&self, other: &Token) -> bool {
        self.value == other.value
    }
}

mod tokens {
    use super::*;

    /// A fresh book for a case.
    pub(super) fn fresh() {
        BOOK.with_borrow_mut(|book| *book = Book::default());
    }

    pub(super) fn live() -> usize {
        BOOK.with_borrow(|book| book.live.len())
    }

    pub(super) fn panic_on_drop(value: u64) {
        BOOK.with_borrow_mut(|book| book.panic_on_drop = Some(value));
    }

    pub(super) fn panic_on_clone(value: u64) {
        BOOK.with_borrow_mut(|book| book.panic_on_clone = Some(value));
    }

    /// `expected` tokens live, none dropped twice.
    pub(super) fn check(expected: usize) -> Result<(), Broke> {
        BOOK.with_borrow(|book| {
            if let Some(wrong) = book.wrong.first() {
                return Err(Broke::Oracle(wrong.clone()));
            }
            let live = book.live.len();
            if live > expected {
                return Err(Broke::Oracle(format!("{} values never dropped", live - expected)));
            }
            if live < expected {
                return Err(Broke::Oracle(format!("{} values dropped too early", expected - live)));
            }
            Ok(())
        })
    }
}

/// `ours` and `model` hold the same values in the same order.
fn same<T: PartialEq + core::fmt::Debug>(
    what: &str,
    ours: impl Iterator<Item = T>,
    model: impl Iterator<Item = T>,
) -> Result<(), Broke> {
    let (ours, model): (StdVec<T>, StdVec<T>) = (ours.collect(), model.collect());
    if ours == model {
        Ok(())
    } else {
        Err(Broke::Oracle(format!("{what} differs from the model: {ours:?}, model {model:?}")))
    }
}

/// Cases a property test runs under Miri, whose interpreter is some thousand times slower.
const MIRI_CASES: u32 = 16;

/// A property test's cases: `n`, or [`MIRI_CASES`] under Miri.
fn cases(n: u32) -> Config {
    Config::cases(if cfg!(miri) { n.min(MIRI_CASES) } else { n })
}

/// `normal`, or `miri` under Miri: a size the interpreter runs quickly that still reaches the
/// same code; each use says what it must reach, and `miri_sizes_reach_every_shape` holds the
/// B-tree's ones in the normal suite.
fn sized<T>(normal: T, miri: T) -> T {
    if cfg!(miri) { miri } else { normal }
}

/// Keys that make a B-tree of height two (a root over internal nodes over leaves) when inserted
/// in order: the least that reaches merges and borrows of internal nodes and deletes from them.
const MIRI_TALL: u64 = 200;

/// Tells Miri's leak check that the block starting at `block` is leaked on purpose
/// (`miri_static_root`): one block, named by the test that leaks it.
#[cfg(miri)]
fn miri_leaks_on_purpose(block: *const u8) {
    unsafe extern "Rust" {
        safe fn miri_static_root(ptr: *const u8);
    }
    miri_static_root(block);
}

fn bounds(tape: &Tape, len: usize) -> (usize, usize) {
    let a = tape.index(len + 1, "from");
    let b = tape.index(len + 1, "to");
    (a.min(b), a.max(b))
}

/// Runs `body`, expecting it to panic.
fn panics(body: impl FnOnce()) -> Result<(), Broke> {
    match catch_unwind(AssertUnwindSafe(body)) {
        Err(_) => Ok(()),
        Ok(()) => Err(Broke::Harness(String::from("a planned panic did not happen"))),
    }
}

/// An over-aligned value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(align(64))]
struct Wide(u32);

/// A value sized 0 whose drops are counted, in [`UNIT_DROPS`] (one test uses it).
struct Unit;

static UNIT_DROPS: AtomicUsize = AtomicUsize::new(0);

impl Drop for Unit {
    fn drop(&mut self) {
        UNIT_DROPS.fetch_add(1, SeqCst);
    }
}

// ---------------------------------------------------------------------------------------------
// Vec.

fn vec_case(tape: &Tape) -> Result<(), Broke> {
    let alloc = Counting::new();
    vec_case_in(tape, &alloc)?;
    alloc.check()
}

/// [`vec_case`] over any allocator, so `handle` runs it through an `Alloc` too.
fn vec_case_in<A: Allocator + Copy>(tape: &Tape, alloc: A) -> Result<(), Broke> {
    tokens::fresh();
    {
        let mut ours: Vec<Token, A> = Vec::new_in(alloc);
        let mut model: StdVec<u64> = StdVec::new();
        for _ in 0..tape.index(48, "steps") {
            let len = model.len();
            let v = tape.below(6, "value");
            match tape.below(17, "op") {
                0 | 1 => {
                    ours.push(Token::new(v));
                    model.push(v);
                }
                2 => same("pop", ours.pop().map(|t| t.value).into_iter(), model.pop().into_iter())?,
                3 => {
                    let at = tape.index(len + 1, "at");
                    ours.insert(at, Token::new(v));
                    model.insert(at, v);
                }
                4 if len > 0 => {
                    let at = tape.index(len, "at");
                    same("remove", [ours.remove(at).value].into_iter(), [model.remove(at)].into_iter())?;
                }
                5 if len > 0 => {
                    let at = tape.index(len, "at");
                    let (o, m) = (ours.swap_remove(at).value, model.swap_remove(at));
                    same("swap_remove", [o].into_iter(), [m].into_iter())?;
                }
                6 => {
                    let to = tape.index(len + 1, "len");
                    ours.truncate(to);
                    model.truncate(to);
                }
                7 => {
                    let k = tape.index(5, "k");
                    let new: StdVec<Token> = (0..k).map(|i| Token::new(v + i as u64)).collect();
                    ours.extend_from_slice(&new);
                    model.extend(new.iter().map(|t| t.value));
                }
                8 => {
                    let (a, b) = bounds(tape, len);
                    let drained: StdVec<u64> = ours.drain(a..b).map(|t| t.value).collect();
                    same("drain", drained.into_iter(), model.drain(a..b))?;
                }
                9 => {
                    let (a, b) = bounds(tape, len);
                    let k = tape.index(b - a + 1, "taken");
                    let mut drain = ours.drain(a..b);
                    let taken: StdVec<u64> = drain.by_ref().take(k).map(|t| t.value).collect();
                    drop(drain);
                    let model_taken: StdVec<u64> = model.drain(a..b).take(k).collect();
                    same("a partly taken drain", taken.into_iter(), model_taken.into_iter())?;
                }
                10 => {
                    let m = tape.below(3, "m") + 2;
                    ours.retain(|t| t.value % m != 0);
                    model.retain(|x| x % m != 0);
                }
                11 => {
                    ours.dedup();
                    model.dedup();
                }
                12 => {
                    let at = tape.index(len + 1, "at");
                    let mut tail = ours.split_off(at);
                    let mut model_tail = model.split_off(at);
                    same("split_off", tail.iter().map(|t| t.value), model_tail.iter().copied())?;
                    if tape.coin("append") {
                        ours.append(&mut tail);
                        model.append(&mut model_tail);
                    }
                }
                13 => {
                    ours.clear();
                    model.clear();
                }
                14 => {
                    ours.reserve(tape.index(20, "additional"));
                    if tape.coin("shrink") {
                        ours.shrink_to_fit();
                    }
                }
                15 => {
                    let to = tape.index(len + 4, "len");
                    ours.resize(to, Token::new(v));
                    model.resize(to, v);
                }
                _ => {
                    let k = tape.index(6, "k");
                    ours.extend((0..k).map(|i| Token::new(v * i as u64)));
                    model.extend((0..k).map(|i| v * i as u64));
                }
            }
            same("the vector", ours.iter().map(|t| t.value), model.iter().copied())?;
            if ours.len() > ours.capacity() {
                return Err(Broke::Oracle(String::from("the length passed the capacity")));
            }
            tokens::check(model.len())?;
        }
        if tape.coin("into_iter") {
            let mut iter = ours.into_iter();
            let k = tape.index(model.len() + 1, "taken");
            same("into_iter", iter.by_ref().take(k).map(|t| t.value), model.iter().copied().take(k))?;
            drop(iter);
        }
    }
    tokens::check(0)
}

/// The refinement: blaze-own collections match std collections under random operation sequences, free every allocation with its matching layout, and run every drop on truncate, retain, clone, growth, and merge.
#[test]
fn vec_matches_std() {
    blaze_sim::check(cases(300), vec_case);
}

#[test]
#[cfg_attr(miri, ignore = "a planted bug: tests the oracle, not unsafe code; its clean half is `vec_matches_std`")]
fn the_oracle_catches_truncate_forgetting_drops() {
    let run = |tape: &Tape, p: Plant| plant::with(p, || vec_case(tape));
    blaze_sim::control(cases(300), Plant::Clean, Plant::TruncateForgetsDrop, "never dropped", run);
}

#[test]
#[cfg_attr(miri, ignore = "a planted bug: tests the oracle, not unsafe code; its clean half is `vec_matches_std`")]
fn the_oracle_catches_retain_forgetting_drops() {
    let run = |tape: &Tape, p: Plant| plant::with(p, || vec_case(tape));
    blaze_sim::control(cases(300), Plant::Clean, Plant::RetainForgetsDrop, "never dropped", run);
}

fn ten(alloc: &Counting) -> Vec<Token, &Counting> {
    let mut v = Vec::with_capacity_in(10, alloc);
    v.extend((0..10).map(Token::new));
    v
}

#[test]
fn vec_frees_everything_when_extend_panics() -> Result<(), Broke> {
    tokens::fresh();
    let alloc = Counting::new();
    let mut v: Vec<Token, &Counting> = Vec::new_in(&alloc);
    panics(|| v.extend((0..10).map(|i| if i == 5 { panic!("in the iterator") } else { Token::new(i) })))?;
    same("after a panicking extend", v.iter().map(|t| t.value), 0..5)?;
    tokens::check(5)?;
    drop(v);
    tokens::check(0)?;
    alloc.check()
}

#[test]
fn vec_frees_everything_when_retain_panics() -> Result<(), Broke> {
    tokens::fresh();
    let alloc = Counting::new();
    let mut v = ten(&alloc);
    panics(|| {
        v.retain(|t| {
            assert!(t.value != 6, "in the predicate");
            t.value % 2 == 0
        })
    })?;
    // Seen and kept: 0, 2, 4; not yet seen: 6..10.
    same("after a panicking retain", v.iter().map(|t| t.value), [0, 2, 4, 6, 7, 8, 9].into_iter())?;
    tokens::check(7)?;
    drop(v);
    tokens::check(0)?;
    alloc.check()
}

#[test]
fn vec_frees_everything_when_a_drop_panics_in_retain() -> Result<(), Broke> {
    tokens::fresh();
    let alloc = Counting::new();
    let mut v = ten(&alloc);
    tokens::panic_on_drop(3);
    panics(|| v.retain(|t| t.value % 3 != 0))?;
    // 0 dropped; 3's drop panicked (it is gone); 4..10 not yet seen are kept.
    same("after a drop panicked in retain", v.iter().map(|t| t.value), [1, 2, 4, 5, 6, 7, 8, 9].into_iter())?;
    tokens::check(8)?;
    drop(v);
    tokens::check(0)?;
    alloc.check()
}

#[test]
fn vec_frees_everything_when_a_drop_panics_in_drain() -> Result<(), Broke> {
    tokens::fresh();
    let alloc = Counting::new();
    let mut v = ten(&alloc);
    tokens::panic_on_drop(4);
    panics(|| drop(v.drain(2..8)))?;
    same("after a drop panicked in a drain", v.iter().map(|t| t.value), [0, 1, 8, 9].into_iter())?;
    tokens::check(4)?;
    drop(v);
    tokens::check(0)?;
    alloc.check()
}

#[test]
fn vec_frees_everything_when_a_drop_panics_in_truncate() -> Result<(), Broke> {
    tokens::fresh();
    let alloc = Counting::new();
    let mut v = ten(&alloc);
    tokens::panic_on_drop(3);
    panics(|| v.truncate(1))?;
    same("after a drop panicked in truncate", v.iter().map(|t| t.value), [0].into_iter())?;
    tokens::check(1)?;
    drop(v);
    tokens::check(0)?;
    alloc.check()
}

#[test]
fn vec_frees_everything_when_clone_panics() -> Result<(), Broke> {
    tokens::fresh();
    let alloc = Counting::new();
    let v = ten(&alloc);
    tokens::panic_on_clone(5);
    panics(|| drop(v.clone()))?;
    tokens::check(10)?;
    if alloc.live() != 1 {
        return Err(Broke::Oracle(format!("{} blocks live after a failed clone", alloc.live())));
    }
    let copy = v.clone();
    same("a clone", copy.iter().map(|t| t.value), v.iter().map(|t| t.value))?;
    drop((v, copy));
    tokens::check(0)?;
    alloc.check()
}

#[test]
fn vec_try_forms_leave_the_vector_as_it_was() -> Result<(), Broke> {
    tokens::fresh();
    let alloc = Counting::new();
    let mut v = ten(&alloc);
    alloc.refuse_after(0);
    let Err(refused) = v.try_push(Token::new(10)) else {
        return Err(Broke::Oracle(String::from("a refused allocation still pushed")));
    };
    same("the refused value", [refused.value.value].into_iter(), [10].into_iter())?;
    drop(refused);
    same("after a refused push", v.iter().map(|t| t.value), 0..10)?;
    if v.try_reserve(usize::MAX).is_ok() {
        return Err(Broke::Oracle(String::from("an overflowing reserve succeeded")));
    }
    alloc.refuse_after(0);
    panics(|| v.push(Token::new(11)))?;
    same("after a refused push that panicked", v.iter().map(|t| t.value), 0..10)?;
    drop(v);
    tokens::check(0)?;
    alloc.check()
}

#[test]
fn values_sized_zero_never_allocate() -> Result<(), Broke> {
    let alloc = Counting::new();
    {
        let mut v: Vec<Unit, &Counting> = Vec::new_in(&alloc);
        for _ in 0..1000 {
            v.push(Unit);
        }
        if v.capacity() != usize::MAX {
            return Err(Broke::Oracle(String::from("a vector of values sized 0 has a bounded capacity")));
        }
        drop(v.drain(10..20));
        v.truncate(500);
        let mut ring: VecDeque<Unit, &Counting> = VecDeque::new_in(&alloc);
        for _ in 0..100 {
            ring.push_front(Unit);
            ring.push_back(Unit);
        }
        drop(ring.pop_front());
        ring.make_contiguous();
        let b = Box::new_in(Unit, &alloc);
        drop(b);
        let units: Vec<(), &Counting> = vec_in![&alloc; (); 1000];
        if units.len() != 1000 {
            return Err(Broke::Oracle(String::from("a vector of units lost length")));
        }
    }
    if alloc.allocs() != 0 {
        return Err(Broke::Oracle(format!("{} allocations for values sized 0", alloc.allocs())));
    }
    // 1000 in the vector, 200 in the ring, 1 boxed.
    if UNIT_DROPS.load(SeqCst) != 1201 {
        return Err(Broke::Oracle(format!("{} drops of 1201 values sized 0", UNIT_DROPS.load(SeqCst))));
    }
    alloc.check()
}

#[test]
fn over_aligned_values_are_aligned() -> Result<(), Broke> {
    let alloc = Counting::new();
    {
        let mut v: Vec<Wide, &Counting> = Vec::new_in(&alloc);
        for i in 0..100 {
            v.push(Wide(i));
            if v.as_ptr().addr() % 64 != 0 {
                return Err(Broke::Oracle(String::from("a vector of `Wide` misaligned")));
            }
        }
        let b = Box::new_in(Wide(1), &alloc);
        let a = Arc::new_in(Wide(2), &alloc);
        let r = Rc::new_in(Wide(3), &alloc);
        let mut ring: VecDeque<Wide, &Counting> = VecDeque::new_in(&alloc);
        ring.push_front(Wide(4));
        let addrs = [
            Box::as_ptr(&b).addr(),
            Arc::as_ptr(&a).addr(),
            Rc::as_ptr(&r).addr(),
            core::ptr::from_ref(&ring[0]).addr(),
        ];
        if addrs.iter().any(|addr| addr % 64 != 0) {
            return Err(Broke::Oracle(format!("misaligned `Wide`s at {addrs:x?}")));
        }
        if (*b, *a, *r, ring[0]) != (Wide(1), Wide(2), Wide(3), Wide(4)) {
            return Err(Broke::Oracle(String::from("an over-aligned value changed")));
        }
    }
    alloc.check()
}

#[test]
fn vec_in_makes_what_it_says() -> Result<(), Broke> {
    let alloc = Counting::new();
    {
        let empty: Vec<u8, &Counting> = vec_in![&alloc];
        let listed = vec_in![&alloc; 1u8, 2, 3];
        let repeated = vec_in![&alloc; 7u8; 4];
        same("vec_in![a]", empty.iter(), [].iter())?;
        same("vec_in![a; x, y]", listed.iter(), [1, 2, 3].iter())?;
        same("vec_in![a; x; n]", repeated.iter(), [7, 7, 7, 7].iter())?;
        let collected = (0..5u8).collect_in(&alloc);
        same("collect_in", collected.iter(), [0, 1, 2, 3, 4].iter())?;
    }
    alloc.check()
}

use crate::CollectIn;

// ---------------------------------------------------------------------------------------------
// Box.

trait Speak {
    fn word(&self) -> u64;
}

impl Speak for Token {
    fn word(&self) -> u64 {
        self.value
    }
}

#[test]
fn box_unsizes_and_drops_once() -> Result<(), Broke> {
    tokens::fresh();
    let alloc = Counting::new();
    {
        let speaker: Box<dyn Speak, &Counting> = Box::new_in(Token::new(7), &alloc);
        if speaker.word() != 7 {
            return Err(Broke::Oracle(String::from("a boxed trait object lost its value")));
        }
        let values: Box<[Token], &Counting> = Box::new_in([Token::new(1), Token::new(2)], &alloc);
        same("an unsized box", values.iter().map(|t| t.value), [1, 2].into_iter())?;
        let back = Box::into_vec(values);
        same("into_vec", back.iter().map(|t| t.value), [1, 2].into_iter())?;
        let again = back.into_boxed_slice();
        let cloned = again.clone();
        same("a cloned slice box", cloned.iter().map(|t| t.value), [1, 2].into_iter())?;
        tokens::check(5)?;
        let inner = Box::into_inner(Box::new_in(Token::new(3), &alloc));
        drop(inner);
    }
    tokens::check(0)?;
    alloc.check()
}

/// `Pin<Box<{async block}, A>>` unsizes to `Pin<Box<dyn Future, A>>` (`PinSafePointer` for
/// `Box` in a `StaticAllocator`), an unstarted one drops its captured token once through the
/// vtable, a polled one gives its value, and both blocks are freed. A box without the impl does
/// not build; one whose drop skips the value leaves a token live. Passes with the harness's `ok`
/// line.
#[test]
fn pinned_box_unsizes_and_drops_its_future_once() -> Result<(), Broke> {
    use core::{
        future::Future,
        pin::Pin,
        task::{Context, Poll, Waker},
    };
    tokens::fresh();
    let alloc = Counting::new();
    {
        let held = Token::new(3);
        let unstarted: Pin<Box<dyn Future<Output = u64>, &Counting>> = Box::pin_in(
            async move {
                let held = held;
                held.value + 1
            },
            &alloc,
        );
        let mut ready: Pin<Box<dyn Future<Output = u64>, &Counting>> = Box::pin_in(async { 7u64 }, &alloc);
        tokens::check(1)?;
        drop(unstarted);
        tokens::check(0)?;
        let mut cx = Context::from_waker(Waker::noop());
        if ready.as_mut().poll(&mut cx) != Poll::Ready(7) {
            return Err(Broke::Oracle(String::from("a pinned boxed future did not finish with its value")));
        }
    }
    tokens::check(0)?;
    alloc.check()
}

// ---------------------------------------------------------------------------------------------
// Arc and Rc.

/// Counts its drops.
struct Counted<'a>(&'a AtomicUsize);

impl Drop for Counted<'_> {
    fn drop(&mut self) {
        self.0.fetch_add(1, SeqCst);
    }
}

#[test]
fn arc_under_threads_drops_once_and_frees() -> Result<(), Broke> {
    let alloc = Counting::new();
    let drops = AtomicUsize::new(0);
    {
        let shared = Arc::new_in(Counted(&drops), &alloc);
        std::thread::scope(|s| {
            for _ in 0..sized(8, 4) {
                let mine = shared.clone();
                s.spawn(move || {
                    for _ in 0..sized(500, 25) {
                        let another = mine.clone();
                        let weak = Arc::downgrade(&another);
                        assert!(weak.upgrade().is_some(), "an upgrade failed while owners live");
                        drop(another);
                    }
                });
            }
        });
        if Arc::strong_count(&shared) != 1 || Arc::weak_count(&shared) != 0 {
            return Err(Broke::Oracle(String::from("counts are off after the threads")));
        }
        // The last owners race to drop.
        let weak = Arc::downgrade(&shared);
        std::thread::scope(|s| {
            for _ in 0..8 {
                let mine = shared.clone();
                s.spawn(move || drop(mine));
            }
            drop(shared);
        });
        if weak.upgrade().is_some() {
            return Err(Broke::Oracle(String::from("a weak upgraded after the last owner")));
        }
        drop(weak);
    }
    if drops.load(SeqCst) != 1 {
        return Err(Broke::Oracle(format!("the value was dropped {} times", drops.load(SeqCst))));
    }
    alloc.check()
}

#[test]
fn arc_unique_paths() -> Result<(), Broke> {
    tokens::fresh();
    let alloc = Counting::new();
    {
        let mut a = Arc::new_in(Token::new(1), &alloc);
        if Arc::get_mut(&mut a).is_none() {
            return Err(Broke::Oracle(String::from("a lone Arc is not unique")));
        }
        let weak = Arc::downgrade(&a);
        if Arc::get_mut(&mut a).is_some() {
            return Err(Broke::Oracle(String::from("an Arc with a Weak is unique")));
        }
        // Only a Weak shares: the value moves to a new block, and the Weak upgrades to nothing.
        Arc::make_mut(&mut a).value = 2;
        if weak.upgrade().is_some() || a.value != 2 {
            return Err(Broke::Oracle(String::from("make_mut with a Weak did not move")));
        }
        drop(weak);
        let b = a.clone();
        Arc::make_mut(&mut a).value = 3;
        if (a.value, b.value) != (3, 2) || Arc::ptr_eq(&a, &b) {
            return Err(Broke::Oracle(String::from("make_mut with another owner did not copy")));
        }
        let Err(again) = Arc::try_unwrap(b.clone()) else {
            return Err(Broke::Oracle(String::from("try_unwrap of a shared Arc succeeded")));
        };
        drop(again);
        let Ok(token) = Arc::try_unwrap(b) else {
            return Err(Broke::Oracle(String::from("try_unwrap of the last Arc failed")));
        };
        drop(token);
        let c = a.clone();
        let (first, second) = (Arc::into_inner(a), Arc::into_inner(c));
        if first.is_some() || second.is_none() {
            return Err(Broke::Oracle(String::from("into_inner gave the value to the wrong owner")));
        }
        drop(second);
        let speaker: Arc<dyn Speak, &Counting> = Arc::new_in(Token::new(9), &alloc);
        let other = speaker.clone();
        if other.word() != 9 {
            return Err(Broke::Oracle(String::from("an unsized Arc lost its value")));
        }
        drop((speaker, other));
        let slice = Arc::from_slice_in(&[Token::new(4), Token::new(5)], &alloc);
        same("Arc::from_slice_in", slice.iter().map(|t| t.value), [4, 5].into_iter())?;
        let text = Arc::from_str_in("blaze", &alloc);
        if &*text != "blaze" {
            return Err(Broke::Oracle(String::from("Arc::from_str_in changed the text")));
        }
    }
    tokens::check(0)?;
    alloc.check()
}

/// `make_mut` when only a `Weak` shares: a refused block leaves owner and `Weak` as they were;
/// a granted one moves the value (no clone), the `Weak` upgrades to nothing, and the old block
/// lives exactly as long as the `Weak`.
#[test]
fn arc_make_mut_with_only_a_weak() -> Result<(), Broke> {
    tokens::fresh();
    let alloc = Counting::new();
    {
        let mut a = Arc::new_in(Token::new(1), &alloc);
        let weak = Arc::downgrade(&a);
        let before = Arc::as_ptr(&a);
        alloc.refuse_after(0);
        panics(|| {
            Arc::make_mut(&mut a);
        })?;
        if Arc::strong_count(&a) != 1 || Arc::as_ptr(&a) != before || a.value != 1 {
            return Err(Broke::Oracle(String::from("a refused make_mut changed the owner")));
        }
        let Some(again) = weak.upgrade() else {
            return Err(Broke::Oracle(String::from("a refused make_mut left the Weak dangling")));
        };
        drop(again);
        Arc::make_mut(&mut a).value = 2;
        if Arc::as_ptr(&a) == before || a.value != 2 || weak.upgrade().is_some() {
            return Err(Broke::Oracle(String::from("make_mut with only a Weak did not move the value")));
        }
        // Moved, not cloned: one token.
        tokens::check(1)?;
        if alloc.live() != 2 {
            return Err(Broke::Oracle(format!("{} blocks live; the old one is the Weak's", alloc.live())));
        }
        drop(weak);
        if alloc.live() != 1 {
            return Err(Broke::Oracle(String::from("the old block outlived its last Weak")));
        }
        if Arc::get_mut(&mut a).is_none() {
            return Err(Broke::Oracle(String::from("the moved Arc is not unique")));
        }
    }
    tokens::check(0)?;
    alloc.check()
}

/// `get_mut` writes only after another thread's reads through an owner and through a `Weak`
/// are over: under Miri, a missing acquire in `is_unique` or a missing release in a drop is a
/// data race on the value.
#[test]
fn arc_get_mut_waits_for_the_others_reads() -> Result<(), Broke> {
    let alloc = Counting::new();
    {
        let mut a = Arc::new_in(7u64, &alloc);
        let other = a.clone();
        let weak = Arc::downgrade(&a);
        std::thread::scope(|s| {
            s.spawn(move || {
                let through_owner = *other;
                let through_weak = weak.upgrade().map(|b| *b);
                assert_eq!((through_owner, through_weak), (7, Some(7)), "the other thread read a changed value");
                drop(other);
                drop(weak);
            });
            // Bounded by count, not time: the other thread finishes in a few steps.
            for _ in 0..1_000_000 {
                if let Some(value) = Arc::get_mut(&mut a) {
                    *value = 8;
                    return Ok(());
                }
                std::thread::yield_now();
            }
            Err(Broke::Harness(String::from("get_mut never saw the other thread's owners go")))
        })?;
        if *a != 8 || Arc::strong_count(&a) != 1 || Arc::weak_count(&a) != 0 {
            return Err(Broke::Oracle(String::from("the counts are off after get_mut")));
        }
    }
    alloc.check()
}

/// A value that refers to itself through a `Weak`.
struct Node<'a> {
    me: crate::arc::Weak<Node<'a>, &'a Counting>,
    token: Token,
}

struct RcNode<'a> {
    me: crate::rc::Weak<RcNode<'a>, &'a Counting>,
    token: Token,
}

#[test]
fn new_cyclic_refers_to_itself_and_frees() -> Result<(), Broke> {
    tokens::fresh();
    let alloc = Counting::new();
    {
        let node = Arc::new_cyclic_in(
            |me| {
                assert!(me.upgrade().is_none(), "the Weak upgraded before the value was made");
                Node { me: me.clone(), token: Token::new(1) }
            },
            &alloc,
        );
        let Some(itself) = node.me.upgrade() else {
            return Err(Broke::Oracle(String::from("the cyclic Weak does not upgrade")));
        };
        if !Arc::ptr_eq(&itself, &node) || itself.token.value != 1 || Arc::weak_count(&node) != 1 {
            return Err(Broke::Oracle(String::from("the cyclic Weak points elsewhere")));
        }
        drop((itself, node));
        let rc = Rc::new_cyclic_in(|me| RcNode { me: me.clone(), token: Token::new(2) }, &alloc);
        match rc.me.upgrade() {
            Some(itself) if Rc::ptr_eq(&itself, &rc) && itself.token.value == 2 => {}
            _ => return Err(Broke::Oracle(String::from("the cyclic rc::Weak points elsewhere"))),
        }
        drop(rc);
        tokens::check(0)?;
        // A panic in the maker frees the block, which holds no value.
        panics(|| drop(Arc::<Node<'_>, &Counting>::new_cyclic_in(|_| panic!("a planned panic in the maker"), &alloc)))?;
        panics(|| drop(Rc::<RcNode<'_>, &Counting>::new_cyclic_in(|_| panic!("a planned panic in the maker"), &alloc)))?;
    }
    tokens::check(0)?;
    alloc.check()
}

#[test]
fn raw_pointers_round_trip() -> Result<(), Broke> {
    tokens::fresh();
    let alloc = Counting::new();
    {
        // Over-aligned (the value sits 64 bytes in), byte-aligned, and unsized values.
        let (wide, a) = Arc::into_raw_with_allocator(Arc::new_in(Wide(5), &alloc));
        if wide.addr() % 64 != 0 {
            return Err(Broke::Oracle(String::from("the raw pointer is misaligned")));
        }
        // SAFETY: from `into_raw_with_allocator` of this type and allocator, taken back once.
        let wide = unsafe { Arc::from_raw_in(wide, a) };
        let (byte, a) = Arc::into_raw_with_allocator(Arc::new_in(3u8, &alloc));
        // SAFETY: as above.
        let byte = unsafe { Arc::from_raw_in(byte, a) };
        let slice = Arc::from_slice_in(&[Token::new(1), Token::new(2)], &alloc);
        let weak = Arc::downgrade(&slice);
        let (raw, a) = Arc::into_raw_with_allocator(slice);
        // SAFETY: as above.
        let slice = unsafe { Arc::from_raw_in(raw, a) };
        let (raw_weak, a) = weak.into_raw_with_allocator();
        if !core::ptr::addr_eq(raw_weak, Arc::as_ptr(&slice)) {
            return Err(Broke::Oracle(String::from("a Weak's pointer is not its value's")));
        }
        // SAFETY: from a `Weak`'s `into_raw_with_allocator`, taken back once.
        let weak = unsafe { crate::arc::Weak::from_raw_in(raw_weak, a) };
        let speaker: Arc<dyn Speak, &Counting> = Arc::new_in(Token::new(9), &alloc);
        let (raw, a) = Arc::into_raw_with_allocator(speaker);
        // SAFETY: as above.
        let speaker = unsafe { Arc::from_raw_in(raw, a) };
        if wide.0 != 5 || *byte != 3 || speaker.word() != 9 || Arc::strong_count(&slice) != 1 {
            return Err(Broke::Oracle(String::from("a value changed in a round trip")));
        }
        let upgraded = weak.upgrade();
        same("a slice through a round trip", upgraded.iter().flat_map(|s| s.iter()).map(|t| t.value), [1, 2].into_iter())?;
        drop(upgraded);
        drop((wide, byte, slice, weak, speaker));
        let (raw, a) = Rc::into_raw_with_allocator(Rc::new_in(Wide(6), &alloc));
        // SAFETY: as above, for an `Rc`.
        let rc = unsafe { Rc::from_raw_in(raw, a) };
        let (raw, a) = Rc::downgrade(&rc).into_raw_with_allocator();
        // SAFETY: as above, for an `rc::Weak`.
        let rc_weak = unsafe { crate::rc::Weak::from_raw_in(raw, a) };
        if rc.0 != 6 || rc_weak.upgrade().map(|r| r.0) != Some(6) {
            return Err(Broke::Oracle(String::from("an Rc changed in a round trip")));
        }
    }
    tokens::check(0)?;
    alloc.check()
}

/// `f`, its closure kind fixed to `FnMut` by the bound.
fn fn_mut<F: FnMut()>(f: F) -> F {
    f
}

/// `f`, its closure kind fixed to `FnOnce` by the bound.
fn fn_once<F: FnOnce() -> u64>(f: F) -> F {
    f
}

#[test]
fn boxed_closures_are_called_once_and_freed() -> Result<(), Broke> {
    tokens::fresh();
    let alloc = Counting::new();
    {
        // Each closure takes its token whole (`let held = token`): a closure that only reads
        // `token.value` captures that `u64` alone (precise capture copies a `Copy` field even
        // out of a type with `Drop`), and the token would stay in this block.
        //
        // Path 1: the unsized form, called. The call consumes the closure: its token is gone
        // and its block freed.
        let token = Token::new(3);
        let once: Box<dyn crate::CallOnce<(u64,), Output = u64>, &Counting> = Box::new_in(
            move |x: u64| {
                let held = token;
                held.value + x
            },
            &alloc,
        );
        if once(4) != 7 {
            return Err(Broke::Oracle(String::from("a boxed one-shot closure returned the wrong value")));
        }
        tokens::check(0)?;
        if alloc.live() != 0 {
            return Err(Broke::Oracle(String::from("a called box was not freed")));
        }
        // Path 2: the unsized form, dropped uncalled: the box's drop drops the closure.
        let token = Token::new(5);
        let never: Box<dyn crate::CallOnce<(), Output = u64>, &Counting> = Box::new_in(
            move || {
                let held = token;
                held.value
            },
            &alloc,
        );
        tokens::check(1)?;
        drop(never);
        tokens::check(0)?;
        if alloc.live() != 0 {
            return Err(Broke::Oracle(String::from("a box dropped uncalled was not freed")));
        }
        // Path 3: the sized form, called by value.
        let token = Token::new(4);
        let sized = Box::new_in(
            fn_once(move || {
                let held = token;
                held.value
            }),
            &alloc,
        );
        if sized() != 4 {
            return Err(Broke::Oracle(String::from("a boxed sized closure returned the wrong value")));
        }
        tokens::check(0)?;
        if alloc.live() != 0 {
            return Err(Broke::Oracle(String::from("a called sized box was not freed")));
        }
        // Path 4: the unsized form, panicking in the call: the token is dropped by the unwind
        // and the block freed by the call's guard.
        let token = Token::new(6);
        let panicking: Box<dyn crate::CallOnce<(), Output = ()>, &Counting> = Box::new_in(
            move || {
                let _held = token;
                panic!("a planned panic in a boxed closure");
            },
            &alloc,
        );
        panics(|| panicking())?;
        tokens::check(0)?;
        // A sized closure in a box is called by every closure trait.
        let mut count = 0;
        // A call on a box whose closure's kind is not yet inferred tries `Fn` first and fixes
        // the closure to it (as with std's `Box`); `fn_mut` gives the closure its kind first.
        let mut counter = Box::new_in(fn_mut(|| count += 1), &alloc);
        counter();
        counter();
        drop(counter);
        if count != 2 {
            return Err(Broke::Oracle(String::from("a boxed FnMut was not called twice")));
        }
        // A `dyn Fn` is no `CallOnce` (that is for sized closures): it is called through `Deref`.
        let speak: Box<dyn Fn(u64) -> u64, &Counting> = Box::new_in(|x: u64| x * 2, &alloc);
        if speak(21) != 42 {
            return Err(Broke::Oracle(String::from("a boxed dyn Fn returned the wrong value")));
        }
    }
    tokens::check(0)?;
    alloc.check()
}

#[test]
fn arc_slice_frees_everything_when_clone_panics() -> Result<(), Broke> {
    tokens::fresh();
    let alloc = Counting::new();
    let values = [Token::new(0), Token::new(1), Token::new(2)];
    tokens::panic_on_clone(1);
    panics(|| drop(Arc::from_slice_in(&values, &alloc)))?;
    tokens::check(3)?;
    drop(values);
    tokens::check(0)?;
    alloc.check()
}

fn arc_clone_case(tape: &Tape) -> Result<(), Broke> {
    let alloc = Counting::new();
    let drops = AtomicUsize::new(0);
    {
        let first = Arc::new_in(Counted(&drops), &alloc);
        let mut owners = StdVec::new();
        for _ in 0..tape.index(5, "clones") {
            owners.push(first.clone());
            let expected = owners.len() + 1;
            if Arc::strong_count(&first) != expected {
                // The planted clone counted nothing: forget the extra owners, so the run does
                // not free the block under them.
                owners.into_iter().for_each(core::mem::forget);
                core::mem::forget(first);
                return Err(Broke::Oracle(format!("the strong count missed a clone: {expected} owners")));
            }
        }
    }
    if drops.load(SeqCst) != 1 {
        return Err(Broke::Oracle(String::from("the value was not dropped once")));
    }
    alloc.check()
}

/// The control's clean half on its own.
#[test]
fn arc_clones_count() {
    blaze_sim::check(cases(64), arc_clone_case);
}

#[test]
#[cfg_attr(miri, ignore = "a planted bug: tests the oracle, not unsafe code, and forgets the shared block on purpose; its clean half is `arc_clones_count`")]
fn the_oracle_catches_a_clone_that_skips_the_count() {
    let run = |tape: &Tape, p: Plant| plant::with(p, || arc_clone_case(tape));
    blaze_sim::control(cases(64), Plant::Clean, Plant::ArcCloneSkipsCount, "missed a clone", run);
}

#[test]
fn rc_counts_and_frees() -> Result<(), Broke> {
    tokens::fresh();
    let alloc = Counting::new();
    {
        let a = Rc::new_in(Token::new(1), &alloc);
        let b = a.clone();
        let weak = Rc::downgrade(&a);
        if Rc::strong_count(&a) != 2 || Rc::weak_count(&a) != 1 {
            return Err(Broke::Oracle(String::from("Rc counts are off")));
        }
        drop((a, b));
        if weak.upgrade().is_some() {
            return Err(Broke::Oracle(String::from("an rc::Weak upgraded after the last owner")));
        }
        tokens::check(0)?;
        drop(weak);
        let mut c = Rc::new_in(Token::new(2), &alloc);
        let d = c.clone();
        Rc::make_mut(&mut c).value = 3;
        if (c.value, d.value) != (3, 2) {
            return Err(Broke::Oracle(String::from("Rc::make_mut did not copy")));
        }
        let speaker: Rc<dyn Speak, &Counting> = Rc::new_in(Token::new(5), &alloc);
        if speaker.word() != 5 {
            return Err(Broke::Oracle(String::from("an unsized Rc lost its value")));
        }
        let text = Rc::from_str_in("own", &alloc);
        if &*text != "own" {
            return Err(Broke::Oracle(String::from("Rc::from_str_in changed the text")));
        }
    }
    tokens::check(0)?;
    alloc.check()
}

// ---------------------------------------------------------------------------------------------
// VecDeque.

fn deque_case(tape: &Tape) -> Result<(), Broke> {
    let alloc = Counting::new();
    deque_case_in(tape, &alloc)?;
    alloc.check()
}

/// [`deque_case`] over any allocator, so `handle` runs it through an `Alloc` too.
fn deque_case_in<A: Allocator + Copy>(tape: &Tape, alloc: A) -> Result<(), Broke> {
    tokens::fresh();
    {
        let mut ours: VecDeque<Token, A> = VecDeque::new_in(alloc);
        let mut model: std::collections::VecDeque<u64> = std::collections::VecDeque::new();
        for _ in 0..tape.index(64, "steps") {
            let len = model.len();
            let v = tape.below(100, "value");
            match tape.below(15, "op") {
                0 | 1 => {
                    ours.push_back(Token::new(v));
                    model.push_back(v);
                }
                12 => {
                    let (from, to) = bounds(tape, len);
                    let taken = tape.index(to - from + 1, "taken");
                    let back = tape.below(2, "from the back") == 1;
                    let mut drain = ours.drain(from..to);
                    let mut model_drain = model.drain(from..to);
                    for _ in 0..taken {
                        let (o, m) = if back {
                            (drain.next_back(), model_drain.next_back())
                        } else {
                            (drain.next(), model_drain.next())
                        };
                        same("drain", o.map(|t| t.value).into_iter(), m.into_iter())?;
                    }
                }
                13 => {
                    let keep = tape.below(3, "keep") + 2;
                    ours.retain(|t| t.value % keep != 0);
                    model.retain(|v| v % keep != 0);
                }
                14 => {
                    let mut other: VecDeque<Token, A> = VecDeque::new_in(alloc);
                    let mut model_other = std::collections::VecDeque::new();
                    for i in 0..tape.index(6, "appended") {
                        // Wrapped sometimes, so the copy crosses the other ring's end.
                        if i % 2 == 0 {
                            other.push_back(Token::new(v + i as u64));
                            model_other.push_back(v + i as u64);
                        } else {
                            other.push_front(Token::new(v + i as u64));
                            model_other.push_front(v + i as u64);
                        }
                    }
                    ours.append(&mut other);
                    model.append(&mut model_other);
                    if !other.is_empty() {
                        return Err(Broke::Oracle(String::from("append left values in the other ring")));
                    }
                }
                2 | 3 => {
                    ours.push_front(Token::new(v));
                    model.push_front(v);
                }
                4 => same("pop_back", ours.pop_back().map(|t| t.value).into_iter(), model.pop_back().into_iter())?,
                5 => same("pop_front", ours.pop_front().map(|t| t.value).into_iter(), model.pop_front().into_iter())?,
                6 => {
                    let at = tape.index(len + 1, "at");
                    ours.insert(at, Token::new(v));
                    model.insert(at, v);
                }
                7 => {
                    let at = tape.index(len + 1, "at");
                    same("remove", ours.remove(at).map(|t| t.value).into_iter(), model.remove(at).into_iter())?;
                }
                8 => {
                    let to = tape.index(len + 1, "len");
                    ours.truncate(to);
                    model.truncate(to);
                }
                9 => {
                    let contiguous: StdVec<u64> = ours.make_contiguous().iter().map(|t| t.value).collect();
                    same("make_contiguous", contiguous.into_iter(), model.make_contiguous().iter().copied())?;
                }
                10 => ours.reserve(tape.index(10, "additional")),
                11 => {
                    ours.clear();
                    model.clear();
                }
                _ => {}
            }
            same("the ring", ours.iter().map(|t| t.value), model.iter().copied())?;
            same("the ring backwards", ours.iter().rev().map(|t| t.value), model.iter().rev().copied())?;
            let (front, back) = ours.as_slices();
            same("the ring's slices", front.iter().chain(back).map(|t| t.value), model.iter().copied())?;
            if let Some(i) = len.checked_sub(1) {
                same("get", ours.get(i).map(|t| t.value).into_iter(), model.get(i).copied().into_iter())?;
            }
            tokens::check(model.len())?;
        }
    }
    tokens::check(0)
}

#[test]
fn deque_matches_std() {
    blaze_sim::check(cases(300), deque_case);
}

/// Wraps the ring around its end before it grows, with `u64`s (a planted bug reads poisoned
/// bytes, never uninitialized ones).
fn deque_wrap_case(tape: &Tape) -> Result<(), Broke> {
    let alloc = Counting::new();
    {
        let mut ours: VecDeque<u64, &Counting> = VecDeque::with_capacity_in(8, &alloc);
        let mut model = std::collections::VecDeque::new();
        for i in 0..tape.int_in(1..=7u64, "front") {
            ours.push_front(i);
            model.push_front(i);
        }
        for i in 0..tape.int_in(1..=40u64, "back") {
            ours.push_back(100 + i);
            model.push_back(100 + i);
        }
        same("a ring grown while wrapped", ours.iter().copied(), model.iter().copied())?;
    }
    alloc.check()
}

/// The control's clean half on its own, so Miri runs the growth from every wrap.
#[test]
fn deque_grown_while_wrapped_matches_std() {
    blaze_sim::check(cases(64), deque_wrap_case);
}

#[test]
#[cfg_attr(miri, ignore = "a planted bug: tests the oracle, not unsafe code, and reads stale slots on purpose; its clean half is `deque_grown_while_wrapped_matches_std`")]
fn the_oracle_catches_growth_that_keeps_the_wrap() {
    let run = |tape: &Tape, p: Plant| plant::with(p, || deque_wrap_case(tape));
    blaze_sim::control(cases(64), Plant::Clean, Plant::DequeGrowKeepsWrap, "differs from the model", run);
}

#[test]
fn deque_frees_everything_when_a_drop_panics_across_the_wrap() -> Result<(), Broke> {
    tokens::fresh();
    let alloc = Counting::new();
    let mut ring: VecDeque<Token, &Counting> = VecDeque::with_capacity_in(8, &alloc);
    for i in 0..4 {
        ring.push_front(Token::new(i));
        ring.push_back(Token::new(10 + i));
    }
    // Front to back: 3 2 1 0 10 11 12 13, wrapped around the end of the buffer.
    tokens::panic_on_drop(1);
    panics(|| ring.truncate(1))?;
    same("after a drop panicked in truncate", ring.iter().map(|t| t.value), [3].into_iter())?;
    tokens::check(1)?;
    drop(ring);
    tokens::check(0)?;
    alloc.check()
}

/// Growth from every place the head can be, at every length, by a little and by a lot: the
/// ring's state when it grows is the whole input of `unwrap_after_grow`, and this covers it for
/// small capacities.
#[test]
fn deque_grows_from_every_wrap() -> Result<(), Broke> {
    tokens::fresh();
    let alloc = Counting::new();
    for asked in [1usize, 2, 3, 4, 7, 8] {
        let cap = VecDeque::<Token, &Counting>::with_capacity_in(asked, &alloc).capacity();
        for head in 0..cap {
            for len in 0..=cap {
                for more in [1, 2, cap + 1] {
                    let mut ours: VecDeque<Token, &Counting> = VecDeque::with_capacity_in(cap, &alloc);
                    let mut model = std::collections::VecDeque::new();
                    // `head` fillers go in first and out as the values come, so the head ends
                    // at slot `head` without the ring growing.
                    for i in 0..head {
                        ours.push_back(Token::new(1000 + i as u64));
                    }
                    let mut fillers = head;
                    for i in 0..len as u64 {
                        if ours.len() == cap {
                            drop(ours.pop_front());
                            fillers -= 1;
                        }
                        ours.push_back(Token::new(i));
                        model.push_back(i);
                    }
                    for _ in 0..fillers {
                        drop(ours.pop_front());
                    }
                    if ours.capacity() != cap {
                        return Err(Broke::Harness(String::from("the ring grew while it was set up")));
                    }
                    ours.reserve(cap - len + more);
                    if ours.capacity() < cap + more {
                        return Err(Broke::Oracle(String::from("reserve left too little room")));
                    }
                    same("a ring just grown", ours.iter().map(|t| t.value), model.iter().copied())?;
                    for i in 0..(cap + more) as u64 {
                        if i % 2 == 0 {
                            ours.push_back(Token::new(100 + i));
                            model.push_back(100 + i);
                        } else {
                            ours.push_front(Token::new(100 + i));
                            model.push_front(100 + i);
                        }
                    }
                    same("a grown ring, pushed at both ends", ours.iter().map(|t| t.value), model.iter().copied())?;
                    tokens::check(model.len())?;
                }
            }
        }
    }
    tokens::check(0)?;
    alloc.check()
}

#[test]
fn deque_drain_and_retain_keep_order_when_a_drop_panics() -> Result<(), Broke> {
    tokens::fresh();
    let alloc = Counting::new();
    let wrapped = || {
        let mut ring: VecDeque<Token, &Counting> = VecDeque::with_capacity_in(8, &alloc);
        for i in 0..4 {
            ring.push_front(Token::new(i));
            ring.push_back(Token::new(10 + i));
        }
        // Front to back: 3 2 1 0 10 11 12 13, wrapped around the end of the buffer.
        ring
    };
    let mut ring = wrapped();
    tokens::panic_on_drop(0);
    // 2 1 0 10 11 are dropped by the drain, 0 panicking; the gap still closes.
    panics(|| drop(ring.drain(1..6)))?;
    same("after a drop panicked in a drain", ring.iter().map(|t| t.value), [3, 12, 13].into_iter())?;
    tokens::check(3)?;
    drop(ring);
    let mut ring = wrapped();
    // Odd values go; 1's drop panics after 3's: the rest are kept.
    tokens::panic_on_drop(1);
    panics(|| ring.retain(|t| t.value % 2 == 0))?;
    same("after a drop panicked in retain", ring.iter().map(|t| t.value), [2, 0, 10, 11, 12, 13].into_iter())?;
    tokens::check(6)?;
    // A drain from each side of the middle moves the shorter side.
    drop(ring.drain(4..5));
    drop(ring.drain(0..1));
    same("after two drains", ring.iter().map(|t| t.value), [0, 10, 11, 13].into_iter())?;
    drop(ring);
    tokens::check(0)?;
    alloc.check()
}

// ---------------------------------------------------------------------------------------------
// SortedMap and SortedSet.

fn sorted_case(tape: &Tape) -> Result<(), Broke> {
    let alloc = Counting::new();
    sorted_case_in(tape, &alloc)?;
    alloc.check()
}

/// [`sorted_case`] over any allocator, so `handle` runs it through an `Alloc` too.
fn sorted_case_in<A: Allocator + Copy>(tape: &Tape, alloc: A) -> Result<(), Broke> {
    tokens::fresh();
    {
        let mut ours: SortedMap<u64, Token, A> = SortedMap::new_in(alloc);
        let mut model: BTreeMap<u64, u64> = BTreeMap::new();
        let mut set: SortedSet<u64, A> = SortedSet::new_in(alloc);
        let mut model_set: BTreeSet<u64> = BTreeSet::new();
        for _ in 0..tape.index(64, "steps") {
            let k = tape.below(24, "key");
            let v = tape.below(100, "value");
            match tape.below(8, "op") {
                0..=2 => {
                    let old = ours.insert(k, Token::new(v)).map(|t| t.value);
                    same("insert", old.into_iter(), model.insert(k, v).into_iter())?;
                    same("set insert", [set.insert(k)].into_iter(), [model_set.insert(k)].into_iter())?;
                }
                3 => {
                    same("remove", ours.remove(&k).map(|t| t.value).into_iter(), model.remove(&k).into_iter())?;
                    same("set remove", [set.remove(&k)].into_iter(), [model_set.remove(&k)].into_iter())?;
                }
                4 => {
                    let ends = (k, tape.below(24, "end"));
                    let (a, b) = (ends.0.min(ends.1), ends.0.max(ends.1));
                    same(
                        "range",
                        ours.range(a..b).map(|(k, t)| (*k, t.value)),
                        model.range(a..b).map(|(k, v)| (*k, *v)),
                    )?;
                    same("set range", set.range(a..=b).copied(), model_set.range(a..=b).copied())?;
                }
                5 => {
                    let first = ours.pop_first().map(|(k, t)| (k, t.value));
                    same("pop_first", first.into_iter(), model.pop_first().into_iter())?;
                }
                6 => {
                    let m = tape.below(3, "m") + 2;
                    ours.retain(|_, t| t.value % m != 0);
                    model.retain(|_, v| *v % m != 0);
                    set.retain(|k| k % m != 0);
                    model_set.retain(|k| k % m != 0);
                }
                _ => same("get", ours.get(&k).map(|t| t.value).into_iter(), model.get(&k).copied().into_iter())?,
            }
            same(
                "the map",
                ours.iter().map(|(k, t)| (*k, t.value)),
                model.iter().map(|(k, v)| (*k, *v)),
            )?;
            same("the set", set.iter().copied(), model_set.iter().copied())?;
            tokens::check(model.len())?;
        }
    }
    tokens::check(0)
}

#[test]
fn sorted_map_and_set_match_std() {
    blaze_sim::check(cases(300), sorted_case);
}

#[test]
fn sorted_map_entry_and_index() -> Result<(), Broke> {
    tokens::fresh();
    let alloc = Counting::new();
    {
        let mut counts: SortedMap<u64, u64, &Counting> = SortedMap::new_in(&alloc);
        for key in [5, 1, 5, 3, 1, 5] {
            counts.entry(key).and_modify(|n| *n += 1).or_insert(1);
        }
        same("counts by entry", counts.iter().map(|(k, v)| (*k, *v)), [(1, 2), (3, 1), (5, 3)].into_iter())?;
        let missing = panics(|| {
            let _missing = counts[&4];
        });
        if counts[&5] != 3 || missing.is_err() {
            return Err(Broke::Oracle(String::from("Index gave the wrong value, or none for a missing key")));
        }
        let mut tokens_by_key: SortedMap<u64, Token, &Counting> = SortedMap::new_in(&alloc);
        let made = tokens_by_key.entry(7).or_insert_with_key(|k| Token::new(*k)).value;
        // An occupied entry makes nothing.
        let again = tokens_by_key.entry(7).or_insert_with(|| Token::new(99)).value;
        tokens::check(1)?;
        let crate::sorted::Entry::Occupied(entry) = tokens_by_key.entry(7) else {
            return Err(Broke::Oracle(String::from("a held key's entry is vacant")));
        };
        if (made, again, entry.remove().value) != (7, 7, 7) || !tokens_by_key.is_empty() {
            return Err(Broke::Oracle(String::from("entry made, kept or removed the wrong value")));
        }
    }
    tokens::check(0)?;
    alloc.check()
}

#[test]
fn vec_sorts_and_dedups_a_tail() -> Result<(), Broke> {
    tokens::fresh();
    let alloc = Counting::new();
    {
        let mut v = vec_in![&alloc; 9, 9, 4, 2, 4, 1, 2, 2];
        // The run crate's `sort_tail`: a sorted set in the vector from index 2 on.
        v[2..].sort_unstable();
        v.dedup_range(2..);
        same("a sorted, deduplicated tail", v.iter().copied(), [9, 9, 1, 2, 4].into_iter())?;
        // A range in the middle keeps the values after it, which move down.
        let mut held: Vec<Token, &Counting> = Vec::new_in(&alloc);
        for value in [1, 1, 2, 2, 2, 3, 3] {
            held.push(Token::new(value));
        }
        held.dedup_range_by(1..5, |a, b| a.value == b.value);
        same("a range deduplicated", held.iter().map(|t| t.value), [1, 1, 2, 3, 3].into_iter())?;
        tokens::check(5)?;
    }
    tokens::check(0)?;
    alloc.check()
}

#[test]
fn from_sorted_refuses_what_is_not() -> Result<(), Broke> {
    let alloc = Counting::new();
    {
        let unsorted = vec_in![&alloc; (1u8, 'a'), (3, 'b'), (2, 'c')];
        let Err(not) = SortedMap::from_sorted(unsorted) else {
            return Err(Broke::Oracle(String::from("unsorted entries were taken")));
        };
        same("where the order breaks", [not.at].into_iter(), [2].into_iter())?;
        let sorted = SortedMap::from_sorted(vec_in![&alloc; (1u8, 'a'), (2, 'b')]);
        if sorted.is_err() {
            return Err(Broke::Oracle(String::from("sorted entries were refused")));
        }
    }
    alloc.check()
}

// ---------------------------------------------------------------------------------------------
// Cow.

#[test]
fn cow_owns_only_when_it_must() -> Result<(), Broke> {
    let alloc = Counting::new();
    {
        let values = [1u8, 2, 3];
        let mut cow: Cow<'_, [u8], &Counting> = Cow::Borrowed(&values);
        if alloc.allocs() != 0 || &*cow != [1, 2, 3].as_slice() {
            return Err(Broke::Oracle(String::from("a borrowed Cow allocated or changed")));
        }
        // The first `to_mut` copies into one block (exactly the three values, so a push would
        // grow it: the count is read before any push).
        let first = cow.to_mut(&alloc).as_ptr();
        if !cow.is_owned() || &*cow != [1, 2, 3].as_slice() || alloc.allocs() != 1 {
            return Err(Broke::Oracle(String::from("to_mut did not own once")));
        }
        // A second `to_mut` of an owned value copies nothing.
        if cow.to_mut(&alloc).as_ptr() != first || alloc.allocs() != 1 {
            return Err(Broke::Oracle(String::from("to_mut owned a second time")));
        }
        cow.to_mut(&alloc).push(4);
        cow.to_mut(&alloc).push(5);
        let owned = cow.into_owned(&alloc);
        same("into_owned", owned.iter(), [1, 2, 3, 4, 5].iter())?;
    }
    alloc.check()
}

// ---------------------------------------------------------------------------------------------
// The struct-of-arrays vector.

soa! {
    /// A row with a field with a destructor, a field sized 0 and an over-aligned field.
    struct Row, Rows, RowRef {
        id: u64 => ids / ids_mut,
        token: Token => tokens / tokens_mut,
        tag: u8 => tags / tags_mut,
        unit: () => units / units_mut,
        wide: Wide => wides / wides_mut,
    }
}

type Model = (u64, u64, u8, Wide);

fn row(id: u64, tag: u8) -> Row {
    Row { id, token: Token::new(id), tag, unit: (), wide: Wide(id as u32) }
}

fn model_row(id: u64, tag: u8) -> Model {
    (id, id, tag, Wide(id as u32))
}

fn unrow(r: RowRef<'_>) -> Model {
    let () = *r.unit;
    (*r.id, r.token.value, *r.tag, *r.wide)
}

fn soa_case(tape: &Tape) -> Result<(), Broke> {
    let alloc = Counting::new();
    soa_case_in(tape, &alloc, &alloc)?;
    alloc.check()
}

/// [`soa_case`] over any allocator that allocates from `counted`, so `handle` runs it through
/// an `Alloc` too; `counted` says how many blocks it made and holds.
fn soa_case_in<A: Allocator + Copy>(tape: &Tape, counted: &Counting, alloc: A) -> Result<(), Broke> {
    tokens::fresh();
    {
        let mut ours = Rows::new_in(alloc);
        let mut model: StdVec<Model> = StdVec::new();
        for step in 0..tape.index(64, "steps") {
            let len = model.len();
            let id = 1000 + step as u64;
            let tag = tape.below(5, "tag") as u8;
            let capacity = ours.capacity();
            let allocs = counted.allocs();
            match tape.below(12, "op") {
                8 => {
                    let at = tape.index(len + 1, "at");
                    ours.insert(at, row(id, tag));
                    model.insert(at, model_row(id, tag));
                }
                9 if len > 0 => {
                    let at = tape.index(len, "at");
                    let r = ours.remove(at);
                    same("remove", [(r.id, r.token.value, r.tag, r.wide)].into_iter(), [model.remove(at)].into_iter())?;
                }
                10 => {
                    let tag = tape.below(5, "kept tag") as u8;
                    ours.retain(|r| *r.tag != tag);
                    model.retain(|m| m.2 != tag);
                }
                0..=2 => {
                    ours.push(row(id, tag));
                    model.push(model_row(id, tag));
                    if ours.capacity() != capacity && counted.allocs() != allocs + 1 {
                        return Err(Broke::Oracle(String::from("growth made more than one block")));
                    }
                }
                3 => {
                    let popped = ours.pop().map(|r| (r.id, r.token.value, r.tag, r.wide));
                    same("pop", popped.into_iter(), model.pop().into_iter())?;
                }
                4 if len > 0 => {
                    let at = tape.index(len, "at");
                    let r = ours.swap_remove(at);
                    same("swap_remove", [(r.id, r.token.value, r.tag, r.wide)].into_iter(), [model.swap_remove(at)].into_iter())?;
                }
                5 if len > 0 => {
                    let at = tape.index(len, "at");
                    ours.set(at, row(id, tag));
                    model[at] = model_row(id, tag);
                }
                6 => {
                    let to = tape.index(len + 1, "len");
                    ours.truncate(to);
                    model.truncate(to);
                }
                7 => {
                    ours.sort_by_key(|r| *r.tag);
                    model.sort_by_key(|m| m.2);
                }
                _ => ours.reserve(tape.index(20, "additional")),
            }
            same("the rows", ours.iter().map(unrow), model.iter().copied())?;
            same("the ids column", ours.ids().iter().copied(), model.iter().map(|m| m.0))?;
            same("the tags column", ours.tags().iter().copied(), model.iter().map(|m| m.2))?;
            same("the wides column", ours.wides().iter().copied(), model.iter().map(|m| m.3))?;
            if ours.units().len() != model.len() || ours.tokens().len() != model.len() {
                return Err(Broke::Oracle(String::from("a column's length is off")));
            }
            if ours.wides().as_ptr().addr() % 64 != 0 {
                return Err(Broke::Oracle(String::from("the over-aligned column is misaligned")));
            }
            if counted.live() > 1 {
                return Err(Broke::Oracle(format!("{} blocks live for one vector", counted.live())));
            }
            tokens::check(model.len())?;
        }
        if let Some(first) = ours.get(0) {
            same("get", [unrow(first)].into_iter(), model.first().copied().into_iter())?;
        }
        let copy = ours.clone();
        same("a clone", copy.iter().map(unrow), model.iter().copied())?;
        tokens::check(2 * model.len())?;
    }
    tokens::check(0)
}

#[test]
fn soa_matches_a_vec_of_rows() {
    blaze_sim::check(cases(300), soa_case);
}

soa! {
    /// Attributes on the row and on its fields go on the row struct.
    #[derive(Clone, Copy, Debug, PartialEq)]
    struct Pair, Pairs, PairRef {
        /// A doc comment on a field.
        big: u64 => bigs / bigs_mut,
        #[allow(unused)]
        small: u32 => smalls / smalls_mut,
    }
}

/// `u64` and `u32` columns, grown several times (a planted bug reads poisoned bytes).
fn soa_grow_case(tape: &Tape) -> Result<(), Broke> {
    let alloc = Counting::new();
    {
        let mut ours = Pairs::new_in(&alloc);
        let n = tape.int_in(1..=100u64, "rows");
        for i in 0..n {
            ours.push(Pair { big: i, small: i as u32 * 3 });
        }
        same("the bigs", ours.bigs().iter().copied(), 0..n)?;
        same("the smalls", ours.smalls().iter().copied(), (0..n).map(|i| i as u32 * 3))?;
    }
    alloc.check()
}

/// The control's clean half on its own, so Miri runs the growth of every column.
#[test]
fn soa_grows_every_column() {
    blaze_sim::check(cases(64), soa_grow_case);
}

#[test]
#[cfg_attr(miri, ignore = "a planted bug: tests the oracle, not unsafe code, and reads a column never written on purpose; its clean half is `soa_grows_every_column`")]
fn the_oracle_catches_growth_that_skips_a_column() {
    let run = |tape: &Tape, p: Plant| plant::with(p, || soa_grow_case(tape));
    blaze_sim::control(cases(64), Plant::Clean, Plant::SoaGrowSkipsColumn, "differs from the model", run);
}

#[test]
fn soa_frees_everything_when_a_drop_panics() -> Result<(), Broke> {
    tokens::fresh();
    let alloc = Counting::new();
    let mut rows = Rows::with_capacity_in(10, &alloc);
    for id in 0..10 {
        rows.push(row(id, 0));
    }
    tokens::panic_on_drop(4);
    panics(|| rows.truncate(2))?;
    same("after a drop panicked in truncate", rows.ids().iter().copied(), [0, 1].into_iter())?;
    tokens::check(2)?;
    drop(rows);
    tokens::check(0)?;
    alloc.check()
}

soa! {
    struct Nothing, Nothings, NothingRef {
        a: () => as_ / as_mut,
        b: [u64; 0] => bs / bs_mut,
    }
}

#[test]
fn soa_of_fields_sized_zero_never_allocates() -> Result<(), Broke> {
    let alloc = Counting::new();
    {
        let mut rows = Nothings::new_in(&alloc);
        for _ in 0..1000 {
            rows.push(Nothing { a: (), b: [] });
        }
        rows.sort_by_key(|_| 0u8);
        if rows.len() != 1000 || rows.bs().len() != 1000 || rows.as_().len() != 1000 {
            return Err(Broke::Oracle(String::from("rows of fields sized 0 lost length")));
        }
    }
    if alloc.allocs() != 0 {
        return Err(Broke::Oracle(format!("{} allocations for rows sized 0", alloc.allocs())));
    }
    alloc.check()
}

soa! {
    /// Fields of every alignment, declared out of order, and one sized 0.
    struct Mixed, Mixeds, MixedRef {
        byte: u8 => bytes / bytes_mut,
        word: u64 => words / words_mut,
        half: u16 => halves / halves_mut,
        wide: Wide => wides / wides_mut,
        unit: () => units / units_mut,
    }
}

/// The block is one allocation of `75 * capacity` bytes at 64; its columns sit in descending
/// alignment (wide at the start, then word, half, byte), each at its prefix times the capacity.
/// A row of only fields sized 0 starts at a pointer aligned to its most aligned field, and
/// every operation on it touches no memory.
#[test]
fn soa_columns_sit_where_the_layout_says() -> Result<(), Broke> {
    let alloc = Counting::new();
    {
        let mut rows = Mixeds::new_in(&alloc);
        for i in 0..9u16 {
            rows.push(Mixed { byte: i as u8, word: u64::from(i) << 40, half: i * 3, wide: Wide(u32::from(i)), unit: () });
            let cap = rows.capacity();
            let blocks = alloc.live_layouts();
            let [(base, layout)] = blocks.as_slice() else {
                return Err(Broke::Oracle(format!("{} blocks for one vector", blocks.len())));
            };
            let want = (75 * cap, 64);
            if (layout.size(), layout.align()) != want {
                return Err(Broke::Oracle(format!("the block is {layout:?}, not {want:?}, at capacity {cap}")));
            }
            let places = [
                ("wide", rows.wides().as_ptr().addr(), 0),
                ("word", rows.words().as_ptr().addr(), 64 * cap),
                ("half", rows.halves().as_ptr().addr(), 72 * cap),
                ("byte", rows.bytes().as_ptr().addr(), 74 * cap),
            ];
            for (name, at, offset) in places {
                if at != base + offset {
                    return Err(Broke::Oracle(format!("the {name} column is at {}, not {offset}", at.wrapping_sub(*base))));
                }
            }
            same("the halves", rows.halves().iter().copied(), (0..=i).map(|j| j * 3))?;
            same("the words", rows.words().iter().copied(), (0..=i).map(|j| u64::from(j) << 40))?;
        }
        same("the capacities grown through", [rows.capacity()].into_iter(), [16].into_iter())?;
    }
    {
        let mut rows = Nothings::new_in(&alloc);
        if rows.bs().as_ptr().addr() % 8 != 0 || rows.as_().as_ptr().addr() % 8 != 0 {
            return Err(Broke::Oracle(String::from("the rows sized 0 start misaligned")));
        }
        for _ in 0..5 {
            rows.push(Nothing { a: (), b: [] });
        }
        rows.insert(2, Nothing { a: (), b: [] });
        let _ = rows.remove(0);
        let _ = rows.swap_remove(1);
        rows.retain(|_| true);
        let _ = rows.pop();
        let copy = rows.clone();
        if rows.len() != 3 || copy.len() != 3 || rows.iter().count() != 3 || rows.bs().as_ptr().addr() % 8 != 0 {
            return Err(Broke::Oracle(String::from("rows sized 0 lost count or alignment")));
        }
    }
    // Three for growth to 16; one for `retain`'s flags, the only memory rows sized 0 take.
    if alloc.allocs() != 4 {
        return Err(Broke::Oracle(format!("{} allocations, not 3 for growth to 16 and 1 for retain", alloc.allocs())));
    }
    alloc.check()
}

#[test]
fn soa_debug_and_insert_remove() -> Result<(), Broke> {
    let alloc = Counting::new();
    {
        let mut pairs = Pairs::new_in(&alloc);
        pairs.push(Pair { big: 1, small: 10 });
        pairs.push(Pair { big: 3, small: 30 });
        pairs.insert(1, Pair { big: 2, small: 20 });
        pairs.insert(3, Pair { big: 4, small: 40 });
        let text = format!("{pairs:?}");
        if text != "[(1, 10), (2, 20), (3, 30), (4, 40)]" {
            return Err(Broke::Oracle(format!("Debug of a vector of pairs: {text}")));
        }
        let gone = pairs.remove(0);
        same("after remove", pairs.bigs().iter().copied(), [2, 3, 4].into_iter())?;
        // The row derives what its attributes say.
        same("the removed row", [gone].into_iter(), [Pair { big: 1, small: 10 }].into_iter())?;
        pairs.retain(|p| *p.big != 3);
        same("after retain", pairs.smalls().iter().copied(), [20, 40].into_iter())?;
    }
    alloc.check()
}

#[test]
fn soa_retain_keeps_the_unseen_when_a_drop_panics() -> Result<(), Broke> {
    tokens::fresh();
    let alloc = Counting::new();
    let mut rows = Rows::with_capacity_in(10, &alloc);
    for id in 0..10 {
        rows.push(row(id, 0));
    }
    // Odd rows go; 3's drop panics after 1's: 4 to 9 are not yet seen and stay.
    tokens::panic_on_drop(3);
    panics(|| rows.retain(|r| *r.id % 2 == 0))?;
    same("after a drop panicked in retain", rows.ids().iter().copied(), [0, 2, 4, 5, 6, 7, 8, 9].into_iter())?;
    same("the tokens moved with their rows", rows.tokens().iter().map(|t| t.value), [0, 2, 4, 5, 6, 7, 8, 9].into_iter())?;
    tokens::check(8)?;
    // A panic in the predicate changes nothing.
    panics(|| rows.retain(|r| if *r.id == 5 { panic!("a planned panic in the predicate") } else { true }))?;
    same("after a predicate panicked", rows.ids().iter().copied(), [0, 2, 4, 5, 6, 7, 8, 9].into_iter())?;
    drop(rows);
    tokens::check(0)?;
    alloc.check()
}

// ---------------------------------------------------------------------------------------------
// String.

#[test]
fn string_stays_utf8_and_frees() -> Result<(), Broke> {
    let alloc = Counting::new();
    {
        let mut text = crate::String::new_in(&alloc);
        for ch in ['a', 'é', '日', '🦀'] {
            text.push(ch);
        }
        text.push_str("本");
        if text != "aé日🦀本" || text.pop() != Some('本') || text.pop() != Some('🦀') {
            return Err(Broke::Oracle(format!("pushes and pops gave {text:?}")));
        }
        panics(|| text.truncate(2))?;
        text.truncate(3);
        if text != "aé" {
            return Err(Broke::Oracle(format!("truncate gave {text:?}")));
        }
        if crate::String::from_utf8(text.clone().into_bytes()).is_err() {
            return Err(Broke::Oracle(String::from("from_utf8 refused a string's own bytes")));
        }
        let refused = crate::String::from_utf8(vec_in![&alloc; b'a', 0xff]);
        if refused.map_err(|e| e.utf8_error().valid_up_to()).err() != Some(1) {
            return Err(Broke::Oracle(String::from("from_utf8 took bytes that are not UTF-8")));
        }
        let boxed = text.into_boxed_str();
        let formatted = format_in!(&alloc, "{}-{:03}", &*boxed, 7);
        let mut buf = crate::String::with_capacity_in(16, &alloc);
        format_into!(&mut buf, "{}!", formatted);
        if &*buf != "aé-007!" || buf.capacity() != 16 {
            return Err(Broke::Oracle(format!("format_in and format_into gave {buf:?}")));
        }
    }
    alloc.check()
}

/// `format_in!` of a text that starts with a literal reserves once, by `format!`'s estimate
/// (twice the literal bytes, 54 here, for 41 written), and so allocates exactly one block. A
/// text that starts with a placeholder and has under 16 literal bytes (`"{} {}"`) is estimated
/// at 0 by that same rule, so it grows as it is written and is not asserted here. Prints
/// nothing, as no test in this file does; it passes as `test format_in_reserves_once ... ok`.
#[test]
fn format_in_reserves_once() -> Result<(), Broke> {
    let alloc = Counting::new();
    {
        let text = format_in!(&alloc, "after the count {}, the text {}", 12345u32, "some text");
        if &*text != "after the count 12345, the text some text" {
            return Err(Broke::Oracle(format!("format_in gave {text:?}")));
        }
        if alloc.allocs() != 1 {
            return Err(Broke::Oracle(format!("{} allocations for one format_in!, not 1", alloc.allocs())));
        }
    }
    alloc.check()
}

// ---------------------------------------------------------------------------------------------
// Every allocation failure (`testing::every_failure`): each `try_` method, run once per
// allocation or growth with that one refused, returns its typed error, leaves its value as it
// was and frees everything. The values held are boxes, so a refused value dropped twice or not
// at all is seen by the counting allocator.

use crate::{
    AllocFailure, FixedVec, Full, Inline, Refused, Soa,
    testing::{self, EveryFailure, FailAt, every_failure},
};

/// The allocator every run builds with.
type Failing = FailAt<testing::Counting<&'static System>>;

/// What a run came back with: a refusal (what is wanted), or a value a refused call changed.
#[derive(Debug)]
enum Why {
    Refused(AllocFailure),
    Changed(String),
}

impl From<AllocFailure> for Why {
    fn from(failure: AllocFailure) -> Why {
        Why::Refused(failure)
    }
}

impl<T> From<Refused<T>> for Why {
    fn from(refused: Refused<T>) -> Why {
        Why::Refused(refused.why)
    }
}

/// One `try_` call on `value`: on a refusal, `look` must read the same before and after.
fn step<V, S: PartialEq + core::fmt::Debug, R, E: Into<Why>>(
    value: &mut V,
    look: impl Fn(&V) -> S,
    call: impl FnOnce(&mut V) -> Result<R, E>,
) -> Result<R, Why> {
    let before = look(value);
    match call(value) {
        Ok(done) => Ok(done),
        Err(refused) => {
            let after = look(value);
            if after != before {
                return Err(Why::Changed(format!("{before:?} became {after:?}")));
            }
            Err(refused.into())
        }
    }
}

/// `run` under [`every_failure`] over `System`; the number of allocations and growths it makes.
fn under_every_failure(what: &str, mut run: impl FnMut(&Failing) -> Result<(), Why>) -> Result<usize, Broke> {
    let mut changed = None;
    let found = every_failure(&System, |alloc| match run(alloc) {
        Ok(()) => Ok(()),
        Err(Why::Refused(failure)) => Err(failure),
        Err(Why::Changed(how)) => {
            if changed.is_none() {
                changed = Some(how);
            }
            Ok(())
        }
    });
    if let Some(how) = changed {
        return Err(Broke::Oracle(format!("{what}: a refused call changed its value: {how}")));
    }
    let events = found.map_err(|e| Broke::Oracle(format!("{what}: {e}")))?;
    if events == 0 {
        return Err(Broke::Harness(format!("{what}: the run allocated nothing, so nothing was refused")));
    }
    Ok(events)
}

fn boxed_values<A: Allocator, B: Allocator>(v: &Vec<Box<u64, A>, B>) -> StdVec<u64> {
    v.iter().map(|b| **b).collect()
}

fn words<A: Allocator>(v: &Vec<u64, A>) -> StdVec<u64> {
    v.iter().copied().collect()
}

#[test]
fn vec_try_forms_fail_cleanly_at_every_allocation() -> Result<(), Broke> {
    under_every_failure("Vec", |alloc| {
        let mut v = Vec::try_with_capacity_in(2, alloc)?;
        for n in 0..9 {
            let value = Box::try_new_in(n, alloc)?;
            step(&mut v, boxed_values, |v| {
                v.try_push(value).map_err(|refused| {
                    if *refused.value == n { Why::from(refused) } else { Why::Changed(String::from("another value came back")) }
                })
            })?;
        }
        step(&mut v, boxed_values, |v| v.try_reserve(40))?;
        step(&mut v, boxed_values, |v| v.try_reserve_exact(100))?;
        step(&mut v, boxed_values, |v| v.try_shrink_to_fit())?;
        let mut w = Vec::try_with_capacity_in(1, alloc)?;
        step(&mut w, words, |w| w.try_extend_from_slice(&[1, 2, 3, 4, 5]))?;
        step(&mut w, words, |w| w.try_extend_from_slice(&[6; 20]))?;
        if boxed_values(&v) != (0..9).collect::<StdVec<u64>>() || w.len() != 25 {
            return Err(Why::Changed(format!("the pushes gave {:?} and {:?}", boxed_values(&v), words(&w))));
        }
        Ok(())
    })?;
    Ok(())
}

fn deque_values<A: Allocator, B: Allocator>(d: &VecDeque<Box<u64, A>, B>) -> StdVec<u64> {
    d.iter().map(|b| **b).collect()
}

#[test]
fn deque_try_forms_fail_cleanly_at_every_allocation() -> Result<(), Broke> {
    under_every_failure("VecDeque", |alloc| {
        let mut d = VecDeque::try_with_capacity_in(3, alloc)?;
        for n in 0..14 {
            let value = Box::try_new_in(n, alloc)?;
            // Pushes at both ends and pops at the front, so growth meets every wrap.
            if n % 3 == 0 {
                step(&mut d, deque_values, |d| d.try_push_front(value))?;
            } else {
                step(&mut d, deque_values, |d| d.try_push_back(value))?;
            }
            if n % 4 == 3 {
                let _front = d.pop_front();
            }
        }
        step(&mut d, deque_values, |d| d.try_reserve(50))?;
        Ok(())
    })?;
    Ok(())
}

fn soa_rows<A: Allocator, B: Allocator>(s: &Soa<(u32, Box<u64, A>), B>) -> StdVec<(u32, u64)> {
    s.iter().map(|(a, b)| (*a, **b)).collect()
}

#[test]
fn soa_try_forms_fail_cleanly_at_every_allocation() -> Result<(), Broke> {
    under_every_failure("Soa", |alloc| {
        let mut s = Soa::try_with_capacity_in(1, alloc)?;
        for n in 0..10u32 {
            let row = (n, Box::try_new_in(u64::from(n) * 3, alloc)?);
            step(&mut s, soa_rows, |s| s.try_push(row))?;
        }
        step(&mut s, soa_rows, |s| s.try_reserve(30))?;
        Ok(())
    })?;
    Ok(())
}

fn map_entries<A: Allocator, B: Allocator>(m: &SortedMap<u64, Box<u64, A>, B>) -> StdVec<(u64, u64)> {
    m.iter().map(|(k, v)| (*k, **v)).collect()
}

#[test]
fn sorted_map_try_insert_fails_cleanly_at_every_allocation() -> Result<(), Broke> {
    under_every_failure("SortedMap", |alloc| {
        let mut m = SortedMap::new_in(alloc);
        for n in [5, 1, 9, 3, 7, 2, 8, 0, 6, 4] {
            let value = Box::try_new_in(n * 10, alloc)?;
            step(&mut m, map_entries, |m| {
                m.try_insert(n, value).map_err(|refused| {
                    if refused.value.0 == n && *refused.value.1 == n * 10 {
                        Why::from(refused)
                    } else {
                        Why::Changed(String::from("another entry came back"))
                    }
                })
            })?;
        }
        // A key already there is replaced in place, with no allocation to refuse.
        let value = Box::try_new_in(55, alloc)?;
        step(&mut m, map_entries, |m| m.try_insert(5, value))?;
        Ok(())
    })?;
    Ok(())
}

fn text_of<A: Allocator>(s: &crate::String<A>) -> String {
    String::from(s.as_str())
}

#[test]
fn string_try_forms_fail_cleanly_at_every_allocation() -> Result<(), Broke> {
    under_every_failure("String", |alloc| {
        let mut s = crate::String::try_from_str_in("ab", alloc)?;
        for ch in ['c', 'é', '日', '🦀', 'd', 'e', 'f'] {
            step(&mut s, text_of, |s| s.try_push(ch))?;
        }
        step(&mut s, text_of, |s| s.try_push_str("-0123456789"))?;
        step(&mut s, text_of, |s| s.try_reserve(100))?;
        let mut t = crate::String::try_with_capacity_in(0, alloc)?;
        step(&mut t, text_of, |t| t.try_push_str(s.as_str()))?;
        if t != "abcé日🦀def-0123456789" {
            return Err(Why::Changed(format!("the pushes gave {t:?}")));
        }
        Ok(())
    })?;
    Ok(())
}

#[test]
fn box_arc_rc_try_forms_fail_cleanly_at_every_allocation() -> Result<(), Broke> {
    under_every_failure("Box, Arc and Rc", |alloc| {
        // Each refusal must hand back the value, here a box of its own, whole.
        let back = |refused: Refused<Box<u64, &Failing>>, n: u64| {
            if *refused.value == n { Why::from(refused) } else { Why::Changed(String::from("another value came back")) }
        };
        let b = Box::try_new_in(Box::try_new_in(1, alloc)?, alloc).map_err(|r| back(r, 1))?;
        let a = Arc::try_new_in(Box::try_new_in(2, alloc)?, alloc).map_err(|r| back(r, 2))?;
        let r = Rc::try_new_in(Box::try_new_in(3, alloc)?, alloc).map_err(|r| back(r, 3))?;
        let a_slice = Arc::try_from_slice_in(&[4, 5, 6], alloc)?;
        let r_slice = Rc::try_from_slice_in(&[7, 8], alloc)?;
        if **b != 1 || **a != 2 || **r != 3 || *a_slice != [4, 5, 6] || *r_slice != [7, 8] {
            return Err(Why::Changed(String::from("a counted or boxed value reads wrong")));
        }
        Ok(())
    })?;
    Ok(())
}

#[test]
fn every_failure_catches_a_swallowed_refusal_and_a_leak() -> Result<(), Broke> {
    // Controls (rule 53): a run that ignores a refusal, and one that forgets a block.
    let swallowed = every_failure(&System, |alloc| {
        let mut v = Vec::new_in(alloc);
        let _ignored = v.try_push(1u8);
        Ok::<(), AllocFailure>(())
    });
    if !matches!(swallowed, Err(EveryFailure::Swallowed { at: 0 })) {
        return Err(Broke::Oracle(format!("a swallowed refusal gave {swallowed:?}")));
    }
    let leaked = every_failure(&System, |alloc| {
        let v = Vec::<u8, _>::try_with_capacity_in(4, alloc)?;
        // The one block leaked on purpose: Miri's leak check is told of exactly it (the failing
        // allocator hands out `System`'s block as it is, so the buffer is the block's start).
        #[cfg(miri)]
        miri_leaks_on_purpose(v.as_ptr());
        core::mem::forget(v);
        Ok::<(), AllocFailure>(())
    });
    match leaked {
        Err(EveryFailure::Leaked { at: None, leaked }) if leaked.live == 1 && leaked.first[0].map(|b| b.seq) == Some(0) => {
            Ok(())
        }
        other => Err(Broke::Oracle(format!("a forgotten block gave {other:?}"))),
    }
}

// ---------------------------------------------------------------------------------------------
// Fixed capacity.

#[test]
fn fixed_vec_refuses_past_its_capacity_and_drops_once() -> Result<(), Broke> {
    tokens::fresh();
    {
        let mut v = FixedVec::<Token, 4>::new();
        for n in 0..4 {
            if v.push(Token::new(n)).is_err() {
                return Err(Broke::Oracle(format!("push {n} of 4 was refused")));
            }
        }
        match v.push(Token::new(9)) {
            Err(Full { value, cap: 4 }) if value.value == 9 => {}
            other => return Err(Broke::Oracle(format!("a push past 4 gave {other:?}"))),
        }
        if v.insert(0, Token::new(8)).map_err(|full| full.value.value) != Err(8) {
            return Err(Broke::Oracle(String::from("an insert into a full vector was taken")));
        }
        let removed = v.remove(1);
        let swapped = v.swap_remove(0);
        if removed.value != 1 || swapped.value != 0 || v.iter().map(|t| t.value).collect::<StdVec<_>>() != [3, 2] {
            return Err(Broke::Oracle(String::from("remove and swap_remove moved the wrong values")));
        }
        drop((removed, swapped));
        if v.insert(1, Token::new(7)).is_err() || v.insert(3, Token::new(6)).is_err() {
            return Err(Broke::Oracle(String::from("an insert with room was refused")));
        }
        v.retain(|t| t.value % 2 == 0);
        let copy = v.clone();
        let kept: StdVec<u64> = copy.into_iter().map(|t| t.value).collect();
        if kept != [2, 6] {
            return Err(Broke::Oracle(format!("retain kept {kept:?}")));
        }
        tokens::check(2)?;
        // A drop that panics mid-retain: the values not yet seen stay, after the ones kept.
        let mut w = FixedVec::<Token, 6>::new();
        for n in 0..6 {
            let _ = w.push(Token::new(n));
        }
        tokens::panic_on_drop(1);
        panics(|| w.retain(|t| t.value != 1))?;
        if w.iter().map(|t| t.value).collect::<StdVec<_>>() != [0, 2, 3, 4, 5] {
            return Err(Broke::Oracle(String::from("a panic in retain lost or kept the wrong values")));
        }
        let mut iter = w.into_iter();
        let _front = iter.next();
        let _back = iter.next_back();
    }
    tokens::check(0)
}

#[test]
fn fixed_string_refuses_a_piece_whole_and_cuts_at_no_boundary() -> Result<(), Broke> {
    use core::fmt::Write as _;
    let mut s = crate::FixedString::<5>::new();
    s.try_push_str("ab").map_err(|_| Broke::Oracle(String::from("two bytes refused")))?;
    // A three-byte char past the cap is refused whole: the string stays UTF-8.
    if s.try_push_str("c€").is_ok() || s.as_str() != "ab" {
        return Err(Broke::Oracle(format!("a refused piece was cut in: {s:?}")));
    }
    // The refusal is sticky: a piece that would fit is refused too, and the count goes on.
    let overflow = crate::Overflow { capacity: 5, needed: 2 + 4 + 3 };
    if s.try_push('€').is_ok() || s.as_str() != "ab" || s.text() != Err(overflow) {
        return Err(Broke::Oracle(format!("a write after a refusal gave {s:?}, {:?}", s.text())));
    }
    s.clear();
    if s.try_push('€').is_err() || s.as_str() != "€" || s.try_push_str("xyz").is_ok() || s.needed() != 6 {
        return Err(Broke::Oracle(format!("after a clear the pushes gave {s:?}, {} bytes needed", s.needed())));
    }
    s.clear();
    if write!(s, "{}", 12345).is_err() || s.text() != Ok("12345") || write!(s, "{}", 6).is_ok() || &*s != "12345" {
        return Err(Broke::Oracle(format!("the writes gave {s:?}")));
    }
    // The exact size of the next buffer: what all the writes wanted.
    if !s.is_refused() || s.text() != Err(crate::Overflow { capacity: 5, needed: 6 }) {
        return Err(Broke::Oracle(format!("the overflow read {:?}", s.text())));
    }
    Ok(())
}

#[test]
fn inline_holds_collections_on_the_stack_and_refuses_when_full() -> Result<(), Broke> {
    let inline = Inline::<256>::new();
    {
        let mut v = Vec::new_in(&inline);
        let mut pushed = 0u64;
        // The newest block grows in place (4, 8, 16, 32 values), so the vector fills all 256
        // bytes; growing to 64 is refused.
        let refused = loop {
            match v.try_push(pushed) {
                Ok(()) => pushed += 1,
                Err(refused) => break refused,
            }
        };
        if refused.value != pushed || pushed != 32 {
            return Err(Broke::Oracle(format!("{pushed} pushes then {refused:?}")));
        }
        if words(&v) != (0..pushed).collect::<StdVec<_>>() {
            return Err(Broke::Oracle(String::from("growth in place lost values")));
        }
    }
    if inline.used() != 0 {
        return Err(Broke::Oracle(format!("{} bytes still used after the vector was freed", inline.used())));
    }
    // Every collection over it, freed newest first. The text is 16 bytes, so the next block
    // needs no padding and the frees bring the top back to 0 (a free rewinds to the block's
    // start, not over padding before it).
    {
        let text = crate::String::try_from_str_in("sixteen bytes!!!", &inline).map_err(|e| Broke::Oracle(format!("{e}")))?;
        let shared = Rc::try_new_in(7u32, &inline).map_err(|e| Broke::Oracle(format!("{}", e.why)))?;
        let mut rows = Soa::<(u8, u32), _>::new_in(&inline);
        rows.try_push((1, 2)).map_err(|e| Broke::Oracle(format!("{}", e.why)))?;
        if &*text != "sixteen bytes!!!" || *shared != 7 || rows.len() != 1 {
            return Err(Broke::Oracle(String::from("values over the inline buffer read wrong")));
        }
        drop((rows, shared, text));
    }
    if inline.used() != 0 {
        return Err(Broke::Oracle(format!("{} bytes still used after newest-first frees", inline.used())));
    }
    // Nothing is taken from anywhere else: a request past the buffer is refused at once.
    if Vec::<u8, _>::try_with_capacity_in(257, &inline).is_ok() {
        return Err(Broke::Oracle(String::from("a 257-byte block came from a 256-byte buffer")));
    }
    Ok(())
}

#[test]
fn inline_under_every_failure() -> Result<(), Broke> {
    // The fixed buffer as the base under the failing allocator: refusals stay typed, and with
    // newest-first frees the buffer comes back whole each run.
    let inline = Inline::<512>::new();
    let events = every_failure(&&inline, |alloc| {
        let mut s = crate::String::try_with_capacity_in(2, alloc)?;
        // 16 bytes, so the vector after it needs no padding (see the test above).
        s.try_push_str("fixed buffers!!!")?;
        let mut v = Vec::try_with_capacity_in(1, alloc)?;
        v.try_extend_from_slice(&[1u32, 2, 3, 4, 5, 6])?;
        drop(v);
        drop(s);
        Ok::<(), AllocFailure>(())
    })
    .map_err(|e| Broke::Oracle(format!("{e}")))?;
    if events < 4 || inline.used() != 0 {
        return Err(Broke::Oracle(format!("{events} events; {} bytes left used", inline.used())));
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Packed.

use crate::{Pack, Packed};

/// An instruction whose variants pack to 1, 3 and 9 bytes: a tag, then the operand.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    Nop,
    Push(u16),
    Jump(u64),
}

impl Pack for Op {
    fn packed_len(&self) -> usize {
        match self {
            Op::Nop => 1,
            Op::Push(_) => 3,
            Op::Jump(_) => 9,
        }
    }

    fn pack(&self, out: &mut [u8]) {
        match *self {
            Op::Nop => out[0] = 0,
            Op::Push(n) => {
                out[0] = 1;
                n.pack(&mut out[1..]);
            }
            Op::Jump(to) => {
                out[0] = 2;
                to.pack(&mut out[1..]);
            }
        }
    }

    fn unpack(bytes: &[u8]) -> (Op, usize) {
        match bytes[0] {
            0 => (Op::Nop, 1),
            1 => (Op::Push(u16::unpack(&bytes[1..]).0), 3),
            2 => (Op::Jump(u64::unpack(&bytes[1..]).0), 9),
            tag => panic!("an Op tag {tag} that `pack` never writes"),
        }
    }
}

fn op(tape: &Tape) -> Op {
    match tape.below(3, "variant") {
        0 => Op::Nop,
        1 => Op::Push(tape.below(1 << 16, "push") as u16),
        _ => Op::Jump(tape.next_u64("jump")),
    }
}

/// `packed` holds `model`, read whole and from every place a seek can start.
fn packed_matches<const K: usize>(packed: &Packed<Op, &Counting, K>, model: &[Op]) -> Result<(), Broke> {
    let read: StdVec<Op> = packed.iter().collect();
    if read != model || packed.len() != model.len() || packed.iter().len() != model.len() {
        return Err(Broke::Oracle(format!("K = {K}: read {read:?}, pushed {model:?}")));
    }
    let bytes: usize = model.iter().map(Pack::packed_len).sum();
    if packed.byte_len() != bytes {
        return Err(Broke::Oracle(format!("K = {K}: {} bytes for {bytes} bytes of values", packed.byte_len())));
    }
    for at in 0..=model.len() {
        let Some(from) = packed.seek(at) else {
            return Err(Broke::Oracle(format!("K = {K}: seek({at}) of {} found nothing", model.len())));
        };
        let from: StdVec<Op> = from.collect();
        if from != model[at..] {
            return Err(Broke::Oracle(format!("K = {K}: seek({at}) read {from:?}")));
        }
    }
    if packed.seek(model.len() + 1).is_some() {
        return Err(Broke::Oracle(format!("K = {K}: a seek past the end found values")));
    }
    Ok(())
}

fn packed_case(tape: &Tape) -> Result<(), Broke> {
    let alloc = Counting::new();
    {
        let mut model = StdVec::new();
        let mut plain = Packed::<Op, &Counting>::new_in(&alloc);
        let mut indexed = Packed::<Op, &Counting, 4>::new_in(&alloc);
        for _ in 0..tape.index(40, "steps") {
            if tape.one_in(16, "clear") {
                model.clear();
                plain.clear();
                indexed.clear();
                continue;
            }
            let value = op(tape);
            model.push(value);
            plain.push(&value);
            indexed.push(&value);
        }
        packed_matches(&plain, &model)?;
        packed_matches(&indexed, &model)?;
    }
    alloc.check()
}

#[test]
fn packed_matches_a_vector_and_seeks() {
    blaze_sim::check(cases(300), packed_case);
}

#[test]
fn packed_try_push_fails_cleanly_at_every_allocation() -> Result<(), Broke> {
    under_every_failure("Packed", |alloc| {
        let mut packed = Packed::<Op, &Failing, 3>::new_in(alloc);
        let ops = [Op::Jump(1), Op::Nop, Op::Push(7), Op::Jump(u64::MAX), Op::Nop, Op::Push(9), Op::Nop];
        for value in ops.iter().cycle().take(40) {
            step(&mut packed, |p: &Packed<Op, &Failing, 3>| p.iter().collect::<StdVec<Op>>(), |p| p.try_push(value))?;
        }
        Ok(())
    })?;
    Ok(())
}

/// A `Pack` that reads back one byte more than it wrote: the control for the length check.
#[derive(Debug)]
struct Overreads;

impl Pack for Overreads {
    fn packed_len(&self) -> usize {
        1
    }
    fn pack(&self, out: &mut [u8]) {
        out[0] = 0;
    }
    fn unpack(_bytes: &[u8]) -> (Overreads, usize) {
        (Overreads, 2)
    }
}

#[test]
fn packed_stops_on_a_pack_that_reads_another_length() -> Result<(), Broke> {
    let alloc = Counting::new();
    {
        let mut packed = Packed::<Overreads, &Counting>::new_in(&alloc);
        packed.push(&Overreads);
        panics(|| {
            let _read = packed.iter().count();
        })?;
    }
    alloc.check()
}

// ---------------------------------------------------------------------------------------------
// The B-tree, against std's as the model.

use std::{collections::btree_map::Entry as StdEntry, ops::Bound};

use crate::{
    String as OwnString,
    btree::{BTreeMap as OwnMap, BTreeSet as OwnSet, Entry},
    make::{CloneIn, DefaultIn, FromIn, FromStrIn, IntoIn, ParseIn, Share},
};

/// An invariant check's finding, as the oracle's.
fn oracle(found: Result<(), String>) -> Result<(), Broke> {
    found.map_err(|why| Broke::Oracle(format!("the tree is malformed: {why}")))
}

fn entries<A: Allocator>(map: &OwnMap<u64, Token, A>) -> StdVec<(u64, u64)> {
    map.iter().map(|(k, t)| (*k, t.value)).collect()
}

fn model_entries(model: &BTreeMap<u64, u64>) -> StdVec<(u64, u64)> {
    model.iter().map(|(k, v)| (*k, *v)).collect()
}

/// A random range's bounds over keys below `span`; never both excluded at one key, which both
/// trees refuse with a panic.
fn tree_bounds(tape: &Tape, span: u64) -> (Bound<u64>, Bound<u64>) {
    let (x, y) = (tape.below(span, "start"), tape.below(span, "end"));
    let (a, b) = (x.min(y), x.max(y));
    match tape.below(6, "bounds") {
        0 => (Bound::Included(a), Bound::Included(b)),
        1 => (Bound::Included(a), Bound::Excluded(b)),
        2 => (Bound::Excluded(a), Bound::Included(b)),
        3 => (Bound::Unbounded, Bound::Excluded(b)),
        4 => (Bound::Excluded(a), Bound::Unbounded),
        _ => (Bound::Unbounded, Bound::Unbounded),
    }
}

fn btree_case(tape: &Tape) -> Result<(), Broke> {
    let alloc = Counting::new();
    btree_case_in(tape, &alloc)?;
    alloc.check()
}

/// [`btree_case`] over any allocator, so `handle` runs it through an `Alloc` too.
fn btree_case_in<A: core::alloc::AllocatorClone + Copy>(tape: &Tape, alloc: A) -> Result<(), Broke> {
    tokens::fresh();
    {
        // Small spans meet the same keys again; large ones and the runs below grow tall trees.
        let span = *tape.pick(&[24, 300, 3000], "span").unwrap_or(&24);
        let mut ours: OwnMap<u64, Token, A> = OwnMap::new_in(alloc);
        let mut model: BTreeMap<u64, u64> = BTreeMap::new();
        let mut set: OwnSet<u64, A> = OwnSet::new_in(alloc);
        let mut model_set: BTreeSet<u64> = BTreeSet::new();
        // Under Miri fewer steps; a run of keys (op 13) still makes height two in most cases.
        for _ in 0..tape.index(sized(200, 60), "steps") {
            let k = tape.below(span, "key");
            let v = tape.below(1000, "value");
            match tape.below(16, "op") {
                0..=3 => {
                    let old = ours.insert(k, Token::new(v)).map(|t| t.value);
                    same("insert", old.into_iter(), model.insert(k, v).into_iter())?;
                    same("set insert", [set.insert(k)].into_iter(), [model_set.insert(k)].into_iter())?;
                }
                4 | 5 => {
                    same("remove", ours.remove(&k).map(|t| t.value).into_iter(), model.remove(&k).into_iter())?;
                    same("set remove", [set.remove(&k)].into_iter(), [model_set.remove(&k)].into_iter())?;
                }
                6 => {
                    let range = tree_bounds(tape, span);
                    same(
                        "range",
                        ours.range(range).map(|(k, t)| (*k, t.value)),
                        model.range(range).map(|(k, v)| (*k, *v)),
                    )?;
                    same(
                        "range from the back",
                        ours.range(range).rev().map(|(k, t)| (*k, t.value)),
                        model.range(range).rev().map(|(k, v)| (*k, *v)),
                    )?;
                    same("set range", set.range(range).rev().copied(), model_set.range(range).rev().copied())?;
                    for (_, t) in ours.range_mut(range) {
                        t.value += 1;
                    }
                    for (_, v) in model.range_mut(range) {
                        *v += 1;
                    }
                }
                7 => {
                    if tape.coin("first") {
                        let first = ours.pop_first().map(|(k, t)| (k, t.value));
                        same("pop_first", first.into_iter(), model.pop_first().into_iter())?;
                        same("set pop_first", set.pop_first().into_iter(), model_set.pop_first().into_iter())?;
                    } else {
                        let last = ours.pop_last().map(|(k, t)| (k, t.value));
                        same("pop_last", last.into_iter(), model.pop_last().into_iter())?;
                        same("set pop_last", set.pop_last().into_iter(), model_set.pop_last().into_iter())?;
                    }
                }
                8 => {
                    let m = tape.below(4, "m") + 2;
                    ours.retain(|k, t| (k + t.value) % m != 0);
                    model.retain(|k, v| (k + *v) % m != 0);
                    set.retain(|k| k % m != 0);
                    model_set.retain(|k| k % m != 0);
                }
                9 => match tape.below(3, "entry") {
                    0 => {
                        ours.entry(k).or_insert_with(|| Token::new(v));
                        model.entry(k).or_insert(v);
                    }
                    1 => {
                        ours.entry(k).and_modify(|t| t.value += 1).or_insert_with(|| Token::new(v));
                        model.entry(k).and_modify(|x| *x += 1).or_insert(v);
                    }
                    _ => {
                        let removed = match ours.entry(k) {
                            Entry::Occupied(there) => Some(there.remove().value),
                            Entry::Vacant(_) => None,
                        };
                        let model_removed = match model.entry(k) {
                            StdEntry::Occupied(there) => Some(there.remove()),
                            StdEntry::Vacant(_) => None,
                        };
                        same("entry remove", removed.into_iter(), model_removed.into_iter())?;
                    }
                },
                10 => {
                    if let Some(t) = ours.get_mut(&k) {
                        t.value += 7;
                    }
                    if let Some(x) = model.get_mut(&k) {
                        *x += 7;
                    }
                }
                11 => {
                    let mut right = ours.split_off(&k);
                    let model_right = model.split_off(&k);
                    oracle(right.invariants())?;
                    oracle(ours.invariants())?;
                    same("split_off's right part", entries(&right).into_iter(), model_entries(&model_right).into_iter())?;
                    same("split_off's left part", entries(&ours).into_iter(), model_entries(&model).into_iter())?;
                    let mut set_right = set.split_off(&k);
                    let model_set_right = model_set.split_off(&k);
                    oracle(set_right.invariants())?;
                    same("set split_off", set_right.iter().copied(), model_set_right.iter().copied())?;
                    if tape.coin("append") {
                        ours.append(&mut right);
                        model.extend(model_right);
                        set.append(&mut set_right);
                        model_set.extend(model_set_right);
                        if !right.is_empty() || !set_right.is_empty() {
                            return Err(Broke::Oracle(String::from("append left entries in the map appended")));
                        }
                    }
                }
                12 => {
                    // Both ends at once, as the tape says, until either runs out.
                    let (mut it, mut model_it) = (ours.iter(), model.iter());
                    loop {
                        let (got, want) = if tape.coin("back") {
                            (it.next_back(), model_it.next_back())
                        } else {
                            (it.next(), model_it.next())
                        };
                        let done = got.is_none() || want.is_none();
                        same(
                            "iteration from both ends",
                            got.map(|(k, t)| (*k, t.value)).into_iter(),
                            want.map(|(k, v)| (*k, *v)).into_iter(),
                        )?;
                        if it.len() != model_it.len() {
                            return Err(Broke::Oracle(format!("iter says {} left, the model {}", it.len(), model_it.len())));
                        }
                        if done {
                            break;
                        }
                    }
                }
                13 => {
                    // A run of increasing keys: the splits of an append-heavy load, and height.
                    for i in 0..tape.below(60, "run") + 20 {
                        let key = k * 7 + i;
                        drop(ours.insert(key, Token::new(i)));
                        model.insert(key, i);
                        set.insert(key);
                        model_set.insert(key);
                    }
                }
                14 => {
                    let copy = ours.clone();
                    oracle(copy.invariants())?;
                    if copy != ours {
                        return Err(Broke::Oracle(String::from("the clone differs from the map")));
                    }
                    let (mut it, mut model_it) = (copy.into_iter(), model.clone().into_iter());
                    for _ in 0..tape.index(model.len() + 1, "take") {
                        let (got, want) = if tape.coin("back") {
                            (it.next_back(), model_it.next_back())
                        } else {
                            (it.next(), model_it.next())
                        };
                        same("into_iter", got.map(|(k, t)| (k, t.value)).into_iter(), want.into_iter())?;
                    }
                    // The rest is dropped with the iterator.
                }
                _ => {
                    same("get", ours.get(&k).map(|t| t.value).into_iter(), model.get(&k).copied().into_iter())?;
                    same(
                        "contains",
                        [ours.contains_key(&k), set.contains(&k)].into_iter(),
                        [model.contains_key(&k), model_set.contains(&k)].into_iter(),
                    )?;
                    same(
                        "first and last",
                        [ours.first_key_value(), ours.last_key_value()].into_iter().map(|e| e.map(|(k, t)| (*k, t.value))),
                        [model.first_key_value(), model.last_key_value()].into_iter().map(|e| e.map(|(k, v)| (*k, *v))),
                    )?;
                    same(
                        "set first and last",
                        [set.first().copied(), set.last().copied()].into_iter(),
                        [model_set.first().copied(), model_set.last().copied()].into_iter(),
                    )?;
                }
            }
            same("the map", entries(&ours).into_iter(), model_entries(&model).into_iter())?;
            same(
                "the map from the back",
                ours.iter().rev().map(|(k, t)| (*k, t.value)),
                model.iter().rev().map(|(k, v)| (*k, *v)),
            )?;
            same("keys", ours.keys().copied(), model.keys().copied())?;
            same("the set", set.iter().copied(), model_set.iter().copied())?;
            if ours.len() != model.len() || set.len() != model_set.len() {
                return Err(Broke::Oracle(format!("lengths {} and {} differ from the model", ours.len(), set.len())));
            }
            oracle(ours.invariants())?;
            oracle(set.invariants())?;
            tokens::check(model.len())?;
        }
    }
    tokens::check(0)
}

#[test]
fn btree_matches_std() {
    blaze_sim::check(cases(300), btree_case);
}

/// A tree of height two or more, emptied one tape-chosen key at a time: every merge, of leaves
/// and of internal nodes, and every collapse of the root.
fn btree_shrink_case(tape: &Tape) -> Result<(), Broke> {
    let alloc = Counting::new();
    {
        let n = sized(300 + tape.below(300, "size"), MIRI_TALL + tape.below(40, "size"));
        let mut ours: OwnMap<u64, u64, &Counting> = OwnMap::new_in(&alloc);
        let mut model = BTreeMap::new();
        for k in 0..n {
            ours.insert(k * 10, k);
            model.insert(k * 10, k);
        }
        if ours.height() < 2 {
            return Err(Broke::Harness(format!("{n} keys made a tree of height {}", ours.height())));
        }
        let mut left: StdVec<u64> = (0..n).map(|k| k * 10).collect();
        while !left.is_empty() {
            let k = left.swap_remove(tape.index(left.len(), "which"));
            same("remove", ours.remove(&k).into_iter(), model.remove(&k).into_iter())?;
            same("the map", ours.iter().map(|(k, v)| (*k, *v)), model.iter().map(|(k, v)| (*k, *v)))?;
            oracle(ours.invariants())?;
        }
    }
    alloc.check()
}

/// The Miri sizes of the B-tree tests still reach every shape: the fewest keys they insert in
/// order make a tree of height two, so its internal nodes split, merge, borrow from each side
/// and lose keys. Runs in the normal suite, where the Miri sizes are otherwise unused.
#[test]
fn miri_sizes_reach_every_shape() -> Result<(), Broke> {
    let alloc = Counting::new();
    {
        let map = spaced(MIRI_TALL, &alloc);
        if map.height() < 2 {
            return Err(Broke::Harness(format!("{MIRI_TALL} keys in order made height {}, not two", map.height())));
        }
    }
    alloc.check()
}

#[test]
fn btree_shrinks_to_empty_like_std() {
    blaze_sim::check(cases(60), btree_shrink_case);
}

#[test]
#[cfg_attr(miri, ignore = "a planted bug: tests the oracle, not unsafe code, and leaks a node on purpose; its clean half is `btree_shrinks_to_empty_like_std`")]
fn the_oracle_catches_a_merge_that_forgets_a_child() {
    let run = |tape: &Tape, p: Plant| plant::with(p, || btree_shrink_case(tape));
    blaze_sim::control(cases(60), Plant::Clean, Plant::BTreeMergeForgetsChild, "differs from the model", run);
}

/// Keys `0, 10, .., 10 (n - 1)`, each with its index.
fn spaced(n: u64, alloc: &Counting) -> OwnMap<u64, u64, &Counting> {
    let mut map = OwnMap::new_in(alloc);
    for k in 0..n {
        map.insert(k * 10, k);
    }
    map
}

/// The tree is well formed, `height` high, in `blocks` nodes, and holds `keys`.
fn shape(
    what: &str,
    map: &OwnMap<u64, u64, &Counting>,
    alloc: &Counting,
    height: usize,
    blocks: usize,
    keys: impl Iterator<Item = u64>,
) -> Result<(), Broke> {
    oracle(map.invariants())?;
    if map.height() != height || alloc.live() != blocks {
        return Err(Broke::Oracle(format!(
            "{what}: height {} in {} nodes, not {height} in {blocks}",
            map.height(),
            alloc.live()
        )));
    }
    same(what, map.keys().copied(), keys)
}

#[test]
fn btree_structural_cases() -> Result<(), Broke> {
    let alloc = Counting::new();
    {
        // Empty to one to empty: no node, one leaf, no node.
        let mut map = OwnMap::new_in(&alloc);
        shape("empty", &map, &alloc, 0, 0, [].into_iter())?;
        map.insert(5, 5);
        shape("one", &map, &alloc, 0, 1, [5].into_iter())?;
        map.remove(&5);
        shape("empty again", &map, &alloc, 0, 0, [].into_iter())?;
        drop(map);

        // A split at the root: 15 fill the root leaf; the 16th splits it into 0..=60 and
        // 80..=150 under a new root holding 70.
        let mut map = spaced(15, &alloc);
        shape("fifteen", &map, &alloc, 0, 1, (0..15).map(|k| k * 10))?;
        map.insert(150, 15);
        shape("sixteen", &map, &alloc, 1, 3, (0..16).map(|k| k * 10))?;
        if map.iter().nth(7) != Some((&70, &7)) || map.first_key_value() != Some((&0, &0)) {
            return Err(Broke::Oracle(String::from("the split's median is not 70")));
        }

        // Borrow from the right sibling: the left leaf drops to 6, the right has 8.
        map.remove(&0);
        shape("borrowed from the right", &map, &alloc, 1, 3, (1..16).map(|k| k * 10))?;
        drop(map);

        // Borrow from the left sibling: the left leaf has 8, the right drops to 6.
        let mut map = spaced(16, &alloc);
        map.insert(5, 99);
        map.remove(&150);
        map.remove(&140);
        let keys = [0, 5].into_iter().chain((1..14).map(|k| k * 10));
        shape("borrowed from the left", &map, &alloc, 1, 3, keys)?;
        drop(map);

        // A merge: the right leaf drops to 6 beside a left one with only 7; the two and the root's
        // key make one leaf of 14, and the root, left empty, goes.
        let mut map = spaced(16, &alloc);
        map.remove(&150);
        map.remove(&140);
        shape("merged", &map, &alloc, 0, 1, (0..14).map(|k| k * 10))?;
        drop(map);

        // A delete from an internal node: the root's 70 swaps with its predecessor 60, which then
        // leaves the left leaf, which borrows from the right.
        let mut map = spaced(16, &alloc);
        if map.remove(&70) != Some(7) {
            return Err(Broke::Oracle(String::from("the root's key was not removed")));
        }
        shape("deleted from the root", &map, &alloc, 1, 3, (0..16).filter(|&k| k != 7).map(|k| k * 10))?;
        drop(map);

        // Deep: 2000 keys (under Miri the fewest that make height two), removed in a scattered
        // order down to nothing.
        let deep = sized(2000, MIRI_TALL);
        let mut map = spaced(deep, &alloc);
        if map.height() < 2 {
            return Err(Broke::Oracle(format!("{deep} keys made a tree of height {}", map.height())));
        }
        let mut model: BTreeMap<u64, u64> = (0..deep).map(|k| (k * 10, k)).collect();
        for i in 0..deep {
            // 7919 is prime, so coprime to both sizes: every key once.
            let k = (i * 7919 % deep) * 10;
            same("deep remove", map.remove(&k).into_iter(), model.remove(&k).into_iter())?;
            oracle(map.invariants())?;
        }
        shape("emptied", &map, &alloc, 0, 0, [].into_iter())?;
    }
    alloc.check()
}

// Panics in user code: a comparison, a drop, a clone.

std::thread_local! {
    /// Comparisons left before one panics; `None` for never.
    static TOUCHES: core::cell::Cell<Option<u64>> = const { core::cell::Cell::new(None) };
}

/// A key whose comparison panics when [`TOUCHES`] runs out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Touchy(u64);

impl Ord for Touchy {
    fn cmp(&self, other: &Touchy) -> core::cmp::Ordering {
        let panics = TOUCHES.with(|left| match left.get() {
            Some(0) => {
                left.set(None);
                true
            }
            Some(n) => {
                left.set(Some(n - 1));
                false
            }
            None => false,
        });
        if panics {
            panic!("a planned panic in a comparison of {}", self.0);
        }
        self.0.cmp(&other.0)
    }
}

impl PartialOrd for Touchy {
    fn partial_cmp(&self, other: &Touchy) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

fn btree_panic_case(tape: &Tape) -> Result<(), Broke> {
    tokens::fresh();
    let alloc = Counting::new();
    {
        let mut map: OwnMap<Touchy, Token, &Counting> = OwnMap::new_in(&alloc);
        for n in 0..tape.below(sized(400, MIRI_TALL), "size") {
            map.insert(Touchy(n * 2), Token::new(n));
        }
        for _ in 0..tape.index(12, "steps") {
            let k = Touchy(tape.below(900, "key"));
            let op = tape.below(6, "op");
            TOUCHES.with(|left| left.set(Some(tape.below(16, "touches"))));
            let _caught = catch_unwind(AssertUnwindSafe(|| match op {
                0 => drop(map.insert(k, Token::new(k.0))),
                1 => drop(map.remove(&k)),
                2 => {
                    let _seen = map.range(k..).count();
                }
                3 => {
                    let mut right = map.split_off(&k);
                    map.append(&mut right);
                }
                4 => {
                    let _value = map.entry(k).or_insert_with(|| Token::new(1));
                }
                _ => map.retain(|key, _| key.0 % 3 != 0),
            }));
            TOUCHES.with(|left| left.set(None));
            oracle(map.invariants())?;
            tokens::check(map.len())?;
        }
        // A clone whose value's clone panics frees the part copied.
        let first = map.values().next().map(|t| t.value);
        if let Some(value) = first {
            tokens::panic_on_clone(value);
            panics(|| drop(map.clone()))?;
            tokens::check(map.len())?;
        }
        // A map dropped while one value's drop panics drops the rest and frees every node.
        let copy = map.clone();
        let victim = copy.values().nth(tape.index(copy.len().max(1), "victim")).map(|t| t.value);
        match victim {
            Some(value) => {
                tokens::panic_on_drop(value);
                panics(move || drop(copy))?;
            }
            None => drop(copy),
        }
        tokens::check(map.len())?;
        // A `retain` whose dropped value's drop panics leaves a whole tree of the rest.
        let victim = map.values().nth(tape.index(map.len().max(1), "victim")).map(|t| t.value);
        if let Some(value) = victim {
            tokens::panic_on_drop(value);
            panics(|| map.retain(|_, _| false))?;
            oracle(map.invariants())?;
            tokens::check(map.len())?;
        }
    }
    tokens::check(0)?;
    alloc.check()
}

#[test]
fn btree_stays_whole_when_user_code_panics() {
    blaze_sim::check(cases(200), btree_panic_case);
}

fn tree_values<A: Allocator, B: Allocator>(m: &OwnMap<u64, Box<u64, A>, B>) -> StdVec<(u64, u64)> {
    m.iter().map(|(k, b)| (*k, **b)).collect()
}

#[test]
fn btree_try_forms_fail_cleanly_at_every_allocation() -> Result<(), Broke> {
    // Under Miri the smallest that still splits (16 keys fill a 15-entry leaf and split it; the
    // splits off cut through a node and fix its edges): each refusal reruns everything, and the
    // interpreter cannot pay the square of the full size.
    let (up_n, down_n, set_n, tall) = sized((40, 200, 60, 2), MIRI_BTREE_TRY);
    btree_try_forms(up_n, down_n, set_n, tall)
}

/// The sizes `btree_try_forms_fail_cleanly_at_every_allocation` runs under Miri.
const MIRI_BTREE_TRY: (u64, u64, u64, usize) = (16, 20, 20, 1);

/// The Miri sizes of the B-tree's try forms, run in the normal suite so they are known to pass.
#[test]
#[cfg_attr(miri, ignore = "under Miri the test above runs these sizes")]
fn btree_try_forms_at_miri_sizes() -> Result<(), Broke> {
    let (up_n, down_n, set_n, tall) = MIRI_BTREE_TRY;
    btree_try_forms(up_n, down_n, set_n, tall)
}

/// `up_n` boxed keys ascending and `down_n` plain ones descending, a tree at least `tall` high,
/// split off in the middle, and a set of `set_n` scattered keys, every allocation refused in turn.
fn btree_try_forms(up_n: u64, down_n: u64, set_n: u64, tall: usize) -> Result<(), Broke> {
    under_every_failure("BTreeMap", |alloc| {
        // Ascending and descending: splits on the right edge and on the left. `Failing` keeps
        // at most `testing::SLOTS` (256) blocks live, so the boxed map is small and the tall one
        // (height 2) holds plain values.
        let (mut up, mut down) = (OwnMap::new_in(alloc), OwnMap::new_in(alloc));
        let down_values = |m: &OwnMap<u64, u64, &Failing>| m.iter().map(|(k, v)| (*k, *v)).collect::<StdVec<_>>();
        for n in 0..down_n {
            if n < up_n {
                let value = Box::try_new_in(n, alloc)?;
                step(&mut up, tree_values, |m| m.try_insert(n, value).map(|_| ()))?;
            }
            step(&mut down, down_values, |m| m.try_insert(1000 - n, n).map(|_| ()))?;
        }
        if down.height() < tall {
            return Err(Why::Changed(format!("{down_n} keys made a tree of height {}, not {tall}", down.height())));
        }
        let value = Box::try_new_in(7, alloc)?;
        step(&mut up, tree_values, |m| match m.entry(500) {
            Entry::Vacant(free) => free.try_insert(value).map(|_| ()),
            Entry::Occupied(_) => Ok(()),
        })?;
        let copy = up.try_clone_in(alloc)?;
        if tree_values(&copy) != tree_values(&up) {
            return Err(Why::Changed(String::from("the copy differs from the map")));
        }
        let half = up_n / 2;
        let right = step(&mut up, tree_values, |m| m.try_split_off(&half))?;
        let mut want: StdVec<(u64, u64)> = (half..up_n).map(|k| (k, k)).collect();
        want.push((500, 7));
        if tree_values(&up) != (0..half).map(|k| (k, k)).collect::<StdVec<_>>() || tree_values(&right) != want {
            return Err(Why::Changed(format!("split_off gave {:?} and {:?}", tree_values(&up), tree_values(&right))));
        }
        let cut = 1000 - down_n / 2;
        let low = step(&mut down, down_values, |m| m.try_split_off(&cut))?;
        if down_values(&low) != (cut..=1000).map(|k| (k, 1000 - k)).collect::<StdVec<_>>() {
            return Err(Why::Changed(format!("split_off gave {:?}", down_values(&low))));
        }
        for map in [&up, &copy, &right] {
            map.invariants().map_err(Why::Changed)?;
        }
        for map in [&down, &low] {
            map.invariants().map_err(Why::Changed)?;
        }
        let mut set = OwnSet::new_in(alloc);
        for n in 0..set_n {
            step(&mut set, |s: &OwnSet<u64, &Failing>| s.iter().copied().collect::<StdVec<u64>>(), |s| {
                s.try_insert(n * 7 % 61).map(|_| ())
            })?;
        }
        set.invariants().map_err(Why::Changed)?;
        Ok(())
    })?;
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Making values with the allocator named (`make`), and the split of `Clone`.

#[test]
fn make_try_forms_fail_cleanly_at_every_allocation() -> Result<(), Broke> {
    under_every_failure("make", |alloc| {
        let source = "a text long enough to need a block";
        let text: OwnString<&Failing> = match FromIn::try_from_in(source, alloc) {
            Ok(text) => text,
            Err(Refused { value, why }) if value == source => return Err(Why::Refused(why)),
            Err(_) => return Err(Why::Changed(String::from("another text came back"))),
        };
        let words: Vec<u64, &Failing> = Vec::try_from_in(&[1, 2, 3][..], alloc)?;
        let boxes: Vec<Box<u64, &Failing>, &Failing> =
            Vec::try_from_in([Box::try_new_in(4, alloc)?, Box::try_new_in(5, alloc)?], alloc)?;
        let boxed: Box<[Box<u64, &Failing>], &Failing> = boxes.try_into_in(alloc)?;
        let parsed = <OwnString<&Failing> as FromStrIn<&Failing>>::try_from_str_in("parsed", alloc).map_err(|e| match e {
            ParseIn::Refused(failure) => Why::Refused(failure),
            ParseIn::Invalid(never) => match never {},
        })?;
        let number = <u64 as FromStrIn<&Failing>>::from_str_in("12", alloc);
        let empty: Vec<u64, &Failing> = DefaultIn::try_default_in(alloc)?;
        if text.as_str() != source
            || words.as_slice() != [1, 2, 3]
            || boxed.iter().map(|b| **b).collect::<StdVec<u64>>() != [4, 5]
            || parsed.as_str() != "parsed"
            || number != Ok(12)
            || <u64 as FromStrIn<&Failing>>::from_str_in("x", alloc).is_ok()
            || !empty.is_empty()
        {
            return Err(Why::Changed(String::from("a made value is not what it was made from")));
        }

        // Deep copies: a vector of strings, each string copied too.
        let mut names = Vec::try_with_capacity_in(3, alloc)?;
        for name in ["zero", "one", "two"] {
            names.try_push(OwnString::try_from_str_in(name, alloc)?)?;
        }
        let names_copy = names.try_clone_in(alloc)?;
        let mut deque = VecDeque::new_in(alloc);
        for n in 0..9u64 {
            deque.try_push_back(n)?;
        }
        let deque_copy = deque.try_clone_in(alloc)?;
        let mut sorted = SortedMap::new_in(alloc);
        sorted.try_insert(2u64, OwnString::try_from_str_in("two", alloc)?)?;
        sorted.try_insert(1, OwnString::try_from_str_in("one", alloc)?)?;
        let sorted_copy = sorted.try_clone_in(alloc)?;
        let sorted_set: SortedSet<u64, &Failing> =
            SortedSet::from_sorted(Vec::try_from_in(&[1, 3][..], alloc)?).map_err(|e| Why::Changed(format!("{e}")))?;
        let sorted_set_copy = sorted_set.try_clone_in(alloc)?;
        let one = Box::try_new_in(8u64, alloc)?;
        let one_copy = Box::try_clone_in(&one, alloc)?;
        let boxed_copy = Box::try_clone_in(&boxed, alloc)?;
        let mut soa: Soa<(u32, u64), &Failing> = Soa::new_in(alloc);
        soa.try_push((1, 2))?;
        let soa_copy = soa.try_clone_in(alloc)?;
        let mut packed = Packed::<u64, &Failing>::new_in(alloc);
        packed.try_push(&9)?;
        let packed_copy = packed.try_clone_in(alloc)?;
        let text_copy = text.try_clone_in(alloc)?;
        let mut set = OwnSet::new_in(alloc);
        for n in 0..20u64 {
            set.try_insert(n)?;
        }
        let set_copy = set.try_clone_in(alloc)?;
        if !names_copy.iter().map(|s| s.as_str()).eq(names.iter().map(|s| s.as_str()))
            || !deque_copy.iter().eq(deque.iter())
            || !sorted_copy.iter().map(|(k, v)| (*k, v.as_str())).eq(sorted.iter().map(|(k, v)| (*k, v.as_str())))
            || !sorted_set_copy.iter().eq(sorted_set.iter())
            || *one_copy != *one
            || boxed_copy.iter().map(|b| **b).collect::<StdVec<u64>>() != [4, 5]
            || !soa_copy.iter().eq(soa.iter())
            || !packed_copy.iter().eq(packed.iter())
            || text_copy.as_str() != text.as_str()
            || !set_copy.iter().eq(set.iter())
        {
            return Err(Why::Changed(String::from("a copy differs from its original")));
        }
        Ok(())
    })?;
    Ok(())
}

#[test]
fn share_adds_a_handle_and_allocates_nothing() -> Result<(), Broke> {
    let alloc = Counting::new();
    {
        let arc = Arc::new_in(5u64, &alloc);
        let weak = Arc::downgrade(&arc);
        let rc = Rc::new_in(6u64, &alloc);
        let rc_weak = Rc::downgrade(&rc);
        let counts = |arc: &Arc<u64, &Counting>, rc: &Rc<u64, &Counting>| {
            [Arc::strong_count(arc), Arc::weak_count(arc), Rc::strong_count(rc), Rc::weak_count(rc)]
        };
        let (before, allocs) = (counts(&arc, &rc), alloc.allocs());
        let arc2 = arc.share();
        let weak2 = weak.share();
        let rc2 = rc.share();
        let rc_weak2 = rc_weak.share();
        let r = &*arc;
        let r2 = Share::share(&r);
        let after = counts(&arc, &rc);
        if alloc.allocs() != allocs {
            return Err(Broke::Oracle(format!("share allocated {} blocks", alloc.allocs() - allocs)));
        }
        if after != before.map(|n| n + 1) || !core::ptr::eq(r, r2) || *arc2 != 5 || *rc2 != 6 {
            return Err(Broke::Oracle(format!("share took the counts from {before:?} to {after:?}")));
        }
        drop((arc2, weak2, rc2, rc_weak2));
        if counts(&arc, &rc) != before {
            return Err(Broke::Oracle(String::from("dropping the shares did not put the counts back")));
        }
    }
    alloc.check()
}

#[test]
fn clone_in_leaves_the_first_allocator_alone() -> Result<(), Broke> {
    tokens::fresh();
    let (first, second) = (Counting::new(), Counting::new());
    {
        let (a, b) = (&first, &second);
        let mut names = Vec::new_in(a);
        for name in ["zero", "one", "two"] {
            names.push(OwnString::from_str_in(name, a));
        }
        let mut deque = VecDeque::new_in(a);
        for n in 0..9u64 {
            deque.push_front(n);
        }
        let text = OwnString::from_str_in("some text", a);
        let mut sorted = SortedMap::new_in(a);
        sorted.insert(3u64, OwnString::from_str_in("three", a));
        sorted.insert(1, OwnString::from_str_in("one", a));
        let mut sorted_set = SortedSet::new_in(a);
        sorted_set.insert(4u64);
        let mut soa: Soa<(u32, u64), &Counting> = Soa::new_in(a);
        soa.push((1, 2));
        soa.push((3, 4));
        let mut rows = Rows::new_in(a);
        rows.push(row(1, 2));
        let one = Box::new_in(7u64, a);
        let many = Box::from_slice_in(&[1u64, 2, 3], a);
        let mut packed = Packed::<u64, &Counting>::new_in(a);
        packed.push(&9);
        packed.push(&10);
        let mut tree = OwnMap::new_in(a);
        for n in 0..40u64 {
            tree.insert(n, OwnString::from_str_in("v", a));
        }
        let mut tree_set = OwnSet::new_in(a);
        for n in 0..40u64 {
            tree_set.insert(n);
        }

        let allocs = first.allocs();
        let names_copy = names.clone_in(b);
        let deque_copy = deque.clone_in(b);
        let text_copy = text.clone_in(b);
        let sorted_copy = sorted.clone_in(b);
        let sorted_set_copy = sorted_set.clone_in(b);
        let soa_copy = soa.clone_in(b);
        let rows_copy = rows.clone_in(b);
        let one_copy = Box::clone_in(&one, b);
        let many_copy = Box::clone_in(&many, b);
        let packed_copy = packed.clone_in(b);
        let tree_copy = tree.clone_in(b);
        let tree_set_copy = tree_set.clone_in(b);
        let generic: Vec<OwnString<&Counting>, &Counting> = CloneIn::clone_in(&names, b);
        if first.allocs() != allocs {
            return Err(Broke::Oracle(format!("clone_in took {} blocks from the first allocator", first.allocs() - allocs)));
        }
        if second.allocs() == 0 {
            return Err(Broke::Harness(String::from("nothing was copied into the second allocator")));
        }
        oracle(tree_copy.invariants())?;
        oracle(tree_set_copy.invariants())?;
        if !names_copy.iter().map(|s| s.as_str()).eq(names.iter().map(|s| s.as_str()))
            || !generic.iter().map(|s| s.as_str()).eq(names.iter().map(|s| s.as_str()))
            || !deque_copy.iter().eq(deque.iter())
            || text_copy.as_str() != text.as_str()
            || !sorted_copy.iter().map(|(k, v)| (*k, v.as_str())).eq(sorted.iter().map(|(k, v)| (*k, v.as_str())))
            || !sorted_set_copy.iter().eq(sorted_set.iter())
            || !soa_copy.iter().eq(soa.iter())
            || rows_copy.len() != rows.len()
            || *one_copy != *one
            || *many_copy != *many
            || !packed_copy.iter().eq(packed.iter())
            || !tree_copy.iter().map(|(k, v)| (*k, v.as_str())).eq(tree.iter().map(|(k, v)| (*k, v.as_str())))
            || !tree_set_copy.iter().eq(tree_set.iter())
        {
            return Err(Broke::Oracle(String::from("a copy differs from its original")));
        }
    }
    tokens::check(0)?;
    first.check()?;
    second.check()
}
