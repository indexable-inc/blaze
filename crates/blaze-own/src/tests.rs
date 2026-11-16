//! Each collection against std's as the oracle, over random operation sequences
//! (`blaze_sim::check`), under a counting allocator that proves every block is freed exactly
//! once with the layout it was made with; panics in the middle of `extend`, `drain`, `retain`,
//! `truncate` and `clone`; values sized 0; alignment over 16; `Arc` under threads; unsizing; and
//! a planted-bug control per oracle (rule 53).
//!
//! The counting allocator writes a pattern into every block it hands out, so a planted bug that
//! reads a slot never written reads defined bytes (the oracle sees the wrong value) rather than
//! uninitialized memory. The tests use no `unsafe` but the allocator's.

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
    tokens::fresh();
    let alloc = Counting::new();
    {
        let mut ours: Vec<Token, &Counting> = Vec::new_in(&alloc);
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
    tokens::check(0)?;
    alloc.check()
}

#[test]
fn vec_matches_std() {
    blaze_sim::check(Config::cases(300), vec_case);
}

#[test]
fn the_oracle_catches_truncate_forgetting_drops() {
    let run = |tape: &Tape, p: Plant| plant::with(p, || vec_case(tape));
    blaze_sim::control(Config::cases(300), Plant::Clean, Plant::TruncateForgetsDrop, "never dropped", run);
}

#[test]
fn the_oracle_catches_retain_forgetting_drops() {
    let run = |tape: &Tape, p: Plant| plant::with(p, || vec_case(tape));
    blaze_sim::control(Config::cases(300), Plant::Clean, Plant::RetainForgetsDrop, "never dropped", run);
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
            for _ in 0..8 {
                let mine = shared.clone();
                s.spawn(move || {
                    for _ in 0..500 {
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

#[test]
fn the_oracle_catches_a_clone_that_skips_the_count() {
    let run = |tape: &Tape, p: Plant| plant::with(p, || arc_clone_case(tape));
    blaze_sim::control(Config::cases(64), Plant::Clean, Plant::ArcCloneSkipsCount, "missed a clone", run);
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
    tokens::fresh();
    let alloc = Counting::new();
    {
        let mut ours: VecDeque<Token, &Counting> = VecDeque::new_in(&alloc);
        let mut model: std::collections::VecDeque<u64> = std::collections::VecDeque::new();
        for _ in 0..tape.index(64, "steps") {
            let len = model.len();
            let v = tape.below(100, "value");
            match tape.below(12, "op") {
                0 | 1 => {
                    ours.push_back(Token::new(v));
                    model.push_back(v);
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
                _ => {
                    ours.clear();
                    model.clear();
                }
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
    tokens::check(0)?;
    alloc.check()
}

#[test]
fn deque_matches_std() {
    blaze_sim::check(Config::cases(300), deque_case);
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

#[test]
fn the_oracle_catches_growth_that_keeps_the_wrap() {
    let run = |tape: &Tape, p: Plant| plant::with(p, || deque_wrap_case(tape));
    blaze_sim::control(Config::cases(64), Plant::Clean, Plant::DequeGrowKeepsWrap, "differs from the model", run);
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

// ---------------------------------------------------------------------------------------------
// SortedMap and SortedSet.

fn sorted_case(tape: &Tape) -> Result<(), Broke> {
    tokens::fresh();
    let alloc = Counting::new();
    {
        let mut ours: SortedMap<u64, Token, &Counting> = SortedMap::new_in(&alloc);
        let mut model: BTreeMap<u64, u64> = BTreeMap::new();
        let mut set: SortedSet<u64, &Counting> = SortedSet::new_in(&alloc);
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
    tokens::check(0)?;
    alloc.check()
}

#[test]
fn sorted_map_and_set_match_std() {
    blaze_sim::check(Config::cases(300), sorted_case);
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
    tokens::fresh();
    let alloc = Counting::new();
    {
        let mut ours = Rows::new_in(&alloc);
        let mut model: StdVec<Model> = StdVec::new();
        for step in 0..tape.index(64, "steps") {
            let len = model.len();
            let id = 1000 + step as u64;
            let tag = tape.below(5, "tag") as u8;
            let capacity = ours.capacity();
            let allocs = alloc.allocs();
            match tape.below(9, "op") {
                0..=2 => {
                    ours.push(row(id, tag));
                    model.push(model_row(id, tag));
                    if ours.capacity() != capacity && alloc.allocs() != allocs + 1 {
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
            if alloc.live() > 1 {
                return Err(Broke::Oracle(format!("{} blocks live for one vector", alloc.live())));
            }
            tokens::check(model.len())?;
        }
        if let Some(first) = ours.get(0) {
            same("get", [unrow(first)].into_iter(), model.first().copied().into_iter())?;
        }
    }
    tokens::check(0)?;
    alloc.check()
}

#[test]
fn soa_matches_a_vec_of_rows() {
    blaze_sim::check(Config::cases(300), soa_case);
}

soa! {
    struct Pair, Pairs, PairRef {
        big: u64 => bigs / bigs_mut,
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

#[test]
fn the_oracle_catches_growth_that_skips_a_column() {
    let run = |tape: &Tape, p: Plant| plant::with(p, || soa_grow_case(tape));
    blaze_sim::control(Config::cases(64), Plant::Clean, Plant::SoaGrowSkipsColumn, "differs from the model", run);
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
