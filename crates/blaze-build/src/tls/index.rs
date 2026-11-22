//! Thread-locals by index (port27/tempo-run THREADS.md, D2): each thread-local static gets an
//! INDEX, numbered by the run in the order the records naming it are placed, so a thread-local
//! that a lazily placed record names for the first time can be added while threads that already
//! touched others are running.
//!
//! # The forms, byte for byte
//!
//! - One run-wide DESCRIPTOR (40 bytes, 8-aligned, writable): `key: u64` at [`DESC_KEY`] (the
//!   pthread key), `root: u64` at [`DESC_ROOT`] (where a thread's table pointer lives, from the
//!   thread register; bit 63, [`ROOT_OWN`], set = the Linux form, where the getter stores the
//!   root itself), `count: u64` at [`DESC_COUNT`] (rows written), `cap: u64` at [`DESC_CAP`]
//!   (rows reserved, at most [`CAP`]), `vars: *const VarRow` at [`DESC_VARS`].
//! - A `VarRow` (32 bytes, [`var_row`]): `size` at [`ROW_SIZE`], `align` at [`ROW_ALIGN`] (a power
//!   of two), `init: *const u8` at [`ROW_INIT`], `init_len` at [`ROW_INIT_LEN`] (at most `size`).
//!   Rows are written once by the control thread before the first record naming the index is
//!   published (the slot word's release store orders them, as it orders the record's literals).
//! - A thread's TABLE: `len` at [`TABLE_LEN`], `rounds` at [`TABLE_ROUNDS`], `desc` at
//!   [`TABLE_DESC`], `root_at: *mut *mut u8` at [`TABLE_ROOT_AT`], then `len` pointers from
//!   [`TABLE_SLOTS`], null until touched. A table grows by `realloc`; only pointers to COPIES are
//!   handed out, never into the table, so growth moves nothing a program holds.
//! - A copy comes from libc's `aligned_alloc`, never the program's global allocator (a global
//!   allocator that uses a thread-local must not recurse into itself).
//!
//! The access sequence that reaches the table without a call is blaze-emit's
//! `pass/tls_index.rs`; the [`getter`] runs only on its miss, and the [`ender`] is the key's
//! destructor.

use core::alloc::Allocator;

use blaze_lir::builder::{MemAttrs, Param, Variadic};
use blaze_lir::func::{BlockKind, Conv, RefKind};
use blaze_lir::{BuildError, Builder, Flags, Full, IntCond, Op, Ty, VerifiedFunc};

use super::NO_MEMORY_TRAP;

// The forms' one home is blaze-code's `tls` (the launcher's run mode writes them too).
pub use blaze_code::tls::{
    CAP, DESC_BYTES, DESC_CAP, DESC_COUNT, DESC_KEY, DESC_ROOT, DESC_VARS, ROOT_OWN, ROW_ALIGN, ROW_BYTES, ROW_INIT,
    ROW_INIT_LEN, ROW_SIZE, TABLE_DESC, TABLE_LEN, TABLE_ROOT_AT, TABLE_ROUNDS, TABLE_SLOTS, var_row,
};

/// The destructor round the ender frees in: PTHREAD_DESTRUCTOR_ITERATIONS is 4 on macOS and
/// glibc (the test asserts `sysconf(_SC_THREAD_DESTRUCTOR_ITERATIONS) >= 4`). The program's own
/// thread-local destructors (std's `run_dtors`, through `_tlv_atexit` on macOS) run in the
/// rounds before.
pub const ROUNDS: u64 = 4;

/// The getter's references, by place: each an outside function called directly.
pub const GETTER_IMPORTS: [&str; 6] =
    ["pthread_getspecific", "pthread_setspecific", "realloc", "aligned_alloc", "memcpy", "memset"];

/// The ender's references, by place: one per call site kind (the re-arming
/// `pthread_setspecific`, the `free` of each copy, the `free` of the table). Place 1 and 2 name
/// the same routine; they are kept apart so the signature the run path builds against holds.
pub const ENDER_IMPORTS: [&str; 3] = ["pthread_setspecific", "free", "free"];

/// The trap code of a `pthread_setspecific` that failed (it may allocate: glibc's second-level
/// key arrays). Nothing is quietly ignored (rule 3).
pub const SET_FAILED_TRAP: u32 = blaze_code::trap::SET_FAILED;

/// The trap code of a row whose `init_len` exceeds its `size` (a malformed descriptor).
pub const BAD_ROW_TRAP: u32 = blaze_code::trap::BAD_ROW;

/// The least alignment a copy is asked at: both libcs' `aligned_alloc` accept it, and a smaller
/// alignment a row asks for is met by a larger one.
const LEAST_ALIGN: u64 = 16;

/// The least slots a fresh table has.
const LEAST_SLOTS: u64 = 16;

/// The getter: `fn(desc: *const Desc, index: u64, root_at: *mut *mut u8) -> *mut u8` (C
/// convention), the calling thread's copy of thread-local `index`, made on its first touch.
///
/// `t = pthread_getspecific(desc.key)`; if `t` is null or `t.len <= index`: `n = max(index + 1,
/// 2 * old_len, 16)`, `t = realloc(t, 32 + 8 * n)` (null: trap [`NO_MEMORY_TRAP`]), the new slots
/// zeroed, `len = n`, and on a fresh table `rounds = 0, desc, root_at`;
/// `pthread_setspecific(desc.key, t)` (nonzero: trap [`SET_FAILED_TRAP`]); with [`ROOT_OWN`] set
/// in `desc.root`, `*root_at = t`. Then `p = t.slot[index]`; if null: `row = desc.vars[index]`,
/// `p = aligned_alloc(a, round_up(max(row.size, 1), a))` with `a = max(row.align, 16)` (null:
/// trap), `memcpy(p, row.init, row.init_len)`, the rest of `size` zeroed, `t.slot[index] = p`.
pub fn getter<A: Allocator + Clone>(alloc: A) -> Result<VerifiedFunc<A>, BuildError> {
    let mut b = Builder::<A, Full>::new_in(64, alloc);
    let sig = b.sig(
        Conv::C,
        Variadic::No,
        &[Param::of(Ty::PTR), Param::of(Ty::I64), Param::of(Ty::PTR)],
        &[Param::of(Ty::PTR)],
    );
    b.set_signature(sig);
    let get_sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::PTR)]);
    let set_sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64), Param::of(Ty::PTR)], &[Param::of(Ty::I32)]);
    let realloc_sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::PTR), Param::of(Ty::I64)], &[Param::of(Ty::PTR)]);
    let alloc_sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64), Param::of(Ty::I64)], &[Param::of(Ty::PTR)]);
    let copy_sig = b.sig(
        Conv::C,
        Variadic::No,
        &[Param::of(Ty::PTR), Param::of(Ty::PTR), Param::of(Ty::I64)],
        &[Param::of(Ty::PTR)],
    );
    let fill_sig = b.sig(
        Conv::C,
        Variadic::No,
        &[Param::of(Ty::PTR), Param::of(Ty::I32), Param::of(Ty::I64)],
        &[Param::of(Ty::PTR)],
    );
    let [get, set, realloc, aligned_alloc, memcpy, memset] = [0, 1, 2, 3, 4, 5].map(|p| b.reference(RefKind::Func, p));

    let entry = b.create_block(BlockKind::Normal);
    let have = b.create_block(BlockKind::Normal);
    let fresh = b.create_block(BlockKind::Cold);
    let grow = b.create_block(BlockKind::Cold);
    let grown = b.create_block(BlockKind::Cold);
    let header = b.create_block(BlockKind::Cold);
    let publish = b.create_block(BlockKind::Cold);
    let published = b.create_block(BlockKind::Cold);
    let own = b.create_block(BlockKind::Cold);
    let slot = b.create_block(BlockKind::Normal);
    let make = b.create_block(BlockKind::Cold);
    let sized = b.create_block(BlockKind::Cold);
    let fill = b.create_block(BlockKind::Cold);
    let done = b.create_block(BlockKind::Normal);
    let no_memory = b.create_block(BlockKind::Cold);
    let set_failed = b.create_block(BlockKind::Cold);
    let bad_row = b.create_block(BlockKind::Cold);
    let at8 = MemAttrs::aligned(3);

    // entry: the thread's table, if any.
    b.begin(entry);
    let desc = b.param(Ty::PTR);
    let index = b.param(Ty::I64);
    let root_at = b.param(Ty::PTR);
    let key = b.load(Ty::I64, Flags::NONE, desc, DESC_KEY, at8);
    let table = b.call(Flags::NONE, get, get_sig, &[key]);
    let null = b.null();
    // The constants every later block uses, defined here so the entry dominates each use.
    let zero = b.iconst(Ty::I64, 0);
    let zero32 = b.iconst(Ty::I32, 0);
    let one = b.iconst(Ty::I64, 1);
    let two = b.iconst(Ty::I64, 2);
    let eight = b.iconst(Ty::I64, 8);
    let head = b.iconst(Ty::I64, TABLE_SLOTS as u128);
    let none = b.icmp(IntCond::Eq, table, null);
    b.br(Flags::EXPECT_FALSE, none, (fresh, &[]), (have, &[]));

    // have: a table; long enough for `index`?
    b.begin(have);
    let len = b.load(Ty::I64, Flags::NONE, table, TABLE_LEN, at8);
    let short = b.icmp(IntCond::Ule, len, index);
    b.br(Flags::EXPECT_FALSE, short, (grow, &[table, len]), (slot, &[table]));

    // fresh: no table yet; grow from nothing.
    b.begin(fresh);
    b.jump((grow, &[null, zero]));

    // grow(old, old_len): n = max(index + 1, 2 * old_len, 16) slots.
    b.begin(grow);
    let old = b.param(Ty::PTR);
    let old_len = b.param(Ty::I64);
    let least = b.iconst(Ty::I64, u128::from(LEAST_SLOTS));
    let past = b.binary(Op::Add, Flags::NONE, index, one);
    let doubled = b.binary(Op::Mul, Flags::NONE, old_len, two);
    let n = b.binary(Op::Umax, Flags::NONE, past, doubled);
    let n = b.binary(Op::Umax, Flags::NONE, n, least);
    let slot_bytes = b.binary(Op::Mul, Flags::NONE, n, eight);
    let bytes = b.binary(Op::Add, Flags::NONE, head, slot_bytes);
    let grown_table = b.call(Flags::NONE, realloc, realloc_sig, &[old, bytes]);
    let out = b.icmp(IntCond::Eq, grown_table, null);
    b.br(Flags::EXPECT_FALSE, out, (no_memory, &[]), (grown, &[]));

    // grown: zero the new slots, set `len`; a fresh table also gets its header.
    b.begin(grown);
    let kept_bytes = b.binary(Op::Mul, Flags::NONE, old_len, eight);
    let kept_end = b.binary(Op::Add, Flags::NONE, head, kept_bytes);
    let new_slots = b.ptr_add(Flags::NONE, grown_table, kept_end);
    let added = b.binary(Op::Sub, Flags::NONE, n, old_len);
    let added_bytes = b.binary(Op::Mul, Flags::NONE, added, eight);
    b.call(Flags::NONE, memset, fill_sig, &[new_slots, zero32, added_bytes]);
    b.store(Flags::NONE, n, grown_table, TABLE_LEN, at8);
    // A table always has at least 16 slots, so `old_len == 0` is exactly a fresh table: a grown
    // one keeps its `rounds` (a table grown in a destructor round must still be freed in round 4).
    let was_fresh = b.icmp(IntCond::Eq, old_len, zero);
    b.br(Flags::NONE, was_fresh, (header, &[]), (publish, &[]));

    b.begin(header);
    b.store(Flags::NONE, zero, grown_table, TABLE_ROUNDS, at8);
    b.store(Flags::NONE, desc, grown_table, TABLE_DESC, at8);
    b.store(Flags::NONE, root_at, grown_table, TABLE_ROOT_AT, at8);
    b.jump((publish, &[]));

    // publish: the key holds the (maybe moved) table; the Linux form's root too.
    b.begin(publish);
    let status = b.call(Flags::NONE, set, set_sig, &[key, grown_table]);
    let zero_status = b.iconst(Ty::I32, 0);
    let failed = b.icmp(IntCond::Ne, status, zero_status);
    b.br(Flags::EXPECT_FALSE, failed, (set_failed, &[]), (published, &[]));

    b.begin(published);
    let root = b.load(Ty::I64, Flags::NONE, desc, DESC_ROOT, at8);
    let own_bit = b.iconst(Ty::I64, u128::from(ROOT_OWN));
    let flag = b.binary(Op::And, Flags::NONE, root, own_bit);
    let is_own = b.icmp(IntCond::Ne, flag, zero);
    b.br(Flags::NONE, is_own, (own, &[]), (slot, &[grown_table]));

    b.begin(own);
    b.store(Flags::NONE, grown_table, root_at, 0, at8);
    b.jump((slot, &[grown_table]));

    // slot(t): the copy, if made.
    b.begin(slot);
    let t = b.param(Ty::PTR);
    let index_bytes = b.binary(Op::Mul, Flags::NONE, index, eight);
    let slot_off = b.binary(Op::Add, Flags::NONE, head, index_bytes);
    let slot_at = b.ptr_add(Flags::NONE, t, slot_off);
    let copy = b.load(Ty::PTR, Flags::NONE, slot_at, 0, at8);
    let untouched = b.icmp(IntCond::Eq, copy, null);
    b.br(Flags::EXPECT_FALSE, untouched, (make, &[]), (done, &[copy]));

    // make: the thread's first touch of `index`: a copy from its row.
    b.begin(make);
    let vars = b.load(Ty::PTR, Flags::NONE, desc, DESC_VARS, at8);
    let row_bytes = b.iconst(Ty::I64, ROW_BYTES as u128);
    let row_off = b.binary(Op::Mul, Flags::NONE, index, row_bytes);
    let row = b.ptr_add(Flags::NONE, vars, row_off);
    let size = b.load(Ty::I64, Flags::NONE, row, ROW_SIZE, at8);
    let align = b.load(Ty::I64, Flags::NONE, row, ROW_ALIGN, at8);
    let init = b.load(Ty::PTR, Flags::NONE, row, ROW_INIT, at8);
    let init_len = b.load(Ty::I64, Flags::NONE, row, ROW_INIT_LEN, at8);
    let over = b.icmp(IntCond::Ugt, init_len, size);
    b.br(Flags::EXPECT_FALSE, over, (bad_row, &[]), (sized, &[]));

    b.begin(sized);
    let least_align = b.iconst(Ty::I64, u128::from(LEAST_ALIGN));
    let align = b.binary(Op::Umax, Flags::NONE, align, least_align);
    let at_least_one = b.binary(Op::Umax, Flags::NONE, size, one);
    let low = b.binary(Op::Sub, Flags::NONE, align, one);
    let padded = b.binary(Op::Add, Flags::NONE, at_least_one, low);
    let high = b.binary(Op::Sub, Flags::NONE, zero, align);
    let rounded = b.binary(Op::And, Flags::NONE, padded, high);
    let fresh_copy = b.call(Flags::NONE, aligned_alloc, alloc_sig, &[align, rounded]);
    let no_copy = b.icmp(IntCond::Eq, fresh_copy, null);
    b.br(Flags::EXPECT_FALSE, no_copy, (no_memory, &[]), (fill, &[]));

    b.begin(fill);
    b.call(Flags::NONE, memcpy, copy_sig, &[fresh_copy, init, init_len]);
    let tail = b.ptr_add(Flags::NONE, fresh_copy, init_len);
    let tail_len = b.binary(Op::Sub, Flags::NONE, size, init_len);
    b.call(Flags::NONE, memset, fill_sig, &[tail, zero32, tail_len]);
    b.store(Flags::NONE, fresh_copy, slot_at, 0, at8);
    b.jump((done, &[fresh_copy]));

    b.begin(done);
    let found = b.param(Ty::PTR);
    b.ret(&[found]);

    b.begin(no_memory);
    b.trap(NO_MEMORY_TRAP);
    b.begin(set_failed);
    b.trap(SET_FAILED_TRAP);
    b.begin(bad_row);
    b.trap(BAD_ROW_TRAP);
    b.finish_verified()
}

/// The ender, the key's destructor: `fn(t: *mut Table)` (C convention).
///
/// `t.rounds += 1`; before round [`ROUNDS`] it re-arms the key with `t` (nonzero: trap
/// [`SET_FAILED_TRAP`]) and returns, so the program's own thread-local destructors, which run in
/// the earlier rounds, still find their copies; in round 4 it frees every non-null slot, stores
/// null to `*t.root_at` when [`ROOT_OWN`] is set, and frees `t`. The slots are freed by a loop of
/// blocks (a work list over the indices, no recursion).
///
/// Gap, stated: a destructor that first runs in the LAST round beside this one may find the
/// table gone (macOS orders the keys of one round by no promise). On Linux glibc runs
/// `__call_tls_dtors` (std's cxa path) before any key destructor, so there is no gap there.
pub fn ender<A: Allocator + Clone>(alloc: A) -> Result<VerifiedFunc<A>, BuildError> {
    let mut b = Builder::<A, Full>::new_in(64, alloc);
    let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::PTR)], &[]);
    b.set_signature(sig);
    let set_sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64), Param::of(Ty::PTR)], &[Param::of(Ty::I32)]);
    let free_sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::PTR)], &[]);
    let [set, free_copy, free_table] = [0, 1, 2].map(|p| b.reference(RefKind::Func, p));

    let entry = b.create_block(BlockKind::Normal);
    let again = b.create_block(BlockKind::Normal);
    let rearmed = b.create_block(BlockKind::Normal);
    let walk = b.create_block(BlockKind::Normal);
    let visit = b.create_block(BlockKind::Normal);
    let free_one = b.create_block(BlockKind::Normal);
    let next = b.create_block(BlockKind::Normal);
    let tail = b.create_block(BlockKind::Normal);
    let clear = b.create_block(BlockKind::Normal);
    let last = b.create_block(BlockKind::Normal);
    let set_failed = b.create_block(BlockKind::Cold);
    let at8 = MemAttrs::aligned(3);

    b.begin(entry);
    let t = b.param(Ty::PTR);
    let rounds = b.load(Ty::I64, Flags::NONE, t, TABLE_ROUNDS, at8);
    let one = b.iconst(Ty::I64, 1);
    let rounds = b.binary(Op::Add, Flags::NONE, rounds, one);
    b.store(Flags::NONE, rounds, t, TABLE_ROUNDS, at8);
    let last_round = b.iconst(Ty::I64, u128::from(ROUNDS));
    let early = b.icmp(IntCond::Ult, rounds, last_round);
    let zero = b.iconst(Ty::I64, 0);
    b.br(Flags::NONE, early, (again, &[]), (walk, &[zero]));

    // again: rounds 1 to 3: the key holds the table once more.
    b.begin(again);
    let desc = b.load(Ty::PTR, Flags::NONE, t, TABLE_DESC, at8);
    let key = b.load(Ty::I64, Flags::NONE, desc, DESC_KEY, at8);
    let status = b.call(Flags::NONE, set, set_sig, &[key, t]);
    let zero_status = b.iconst(Ty::I32, 0);
    let failed = b.icmp(IntCond::Ne, status, zero_status);
    b.br(Flags::EXPECT_FALSE, failed, (set_failed, &[]), (rearmed, &[]));
    b.begin(rearmed);
    b.ret(&[]);

    // walk(i): round 4: free slot i of `len`, then the next.
    b.begin(walk);
    let i = b.param(Ty::I64);
    let len = b.load(Ty::I64, Flags::NONE, t, TABLE_LEN, at8);
    let finished = b.icmp(IntCond::Uge, i, len);
    b.br(Flags::NONE, finished, (tail, &[]), (visit, &[]));

    b.begin(visit);
    let eight = b.iconst(Ty::I64, 8);
    let head = b.iconst(Ty::I64, TABLE_SLOTS as u128);
    let i_bytes = b.binary(Op::Mul, Flags::NONE, i, eight);
    let off = b.binary(Op::Add, Flags::NONE, head, i_bytes);
    let slot_at = b.ptr_add(Flags::NONE, t, off);
    let copy = b.load(Ty::PTR, Flags::NONE, slot_at, 0, at8);
    let null = b.null();
    let untouched = b.icmp(IntCond::Eq, copy, null);
    b.br(Flags::NONE, untouched, (next, &[]), (free_one, &[]));

    b.begin(free_one);
    b.call(Flags::NONE, free_copy, free_sig, &[copy]);
    b.jump((next, &[]));

    b.begin(next);
    let i_next = b.binary(Op::Add, Flags::NONE, i, one);
    b.jump((walk, &[i_next]));

    // tail: the Linux form's root forgets the table; then the table goes.
    b.begin(tail);
    let desc = b.load(Ty::PTR, Flags::NONE, t, TABLE_DESC, at8);
    let root = b.load(Ty::I64, Flags::NONE, desc, DESC_ROOT, at8);
    let own_bit = b.iconst(Ty::I64, u128::from(ROOT_OWN));
    let flag = b.binary(Op::And, Flags::NONE, root, own_bit);
    let is_own = b.icmp(IntCond::Ne, flag, zero);
    b.br(Flags::NONE, is_own, (clear, &[]), (last, &[]));

    b.begin(clear);
    let root_at = b.load(Ty::PTR, Flags::NONE, t, TABLE_ROOT_AT, at8);
    let nothing = b.null();
    b.store(Flags::NONE, nothing, root_at, 0, at8);
    b.jump((last, &[]));

    b.begin(last);
    b.call(Flags::NONE, free_table, free_sig, &[t]);
    b.ret(&[]);

    b.begin(set_failed);
    b.trap(SET_FAILED_TRAP);
    b.finish_verified()
}
