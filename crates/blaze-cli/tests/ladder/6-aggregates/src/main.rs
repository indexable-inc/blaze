// Rung 6: aggregates and places. Its shape is tempo's transaction pool
// (crates/transaction-pool/src/tt_2d_pool.rs, `pending_and_queued_txn_count` and the scan that
// rechecks it), without the closures and traits rungs 7 on bring:
// - structs, tuples and arrays passed and returned by value;
// - shared and mutable references, with a `&mut` passed where a `&` is taken;
// - field, tuple-field and index places, read, written and compound-assigned;
// - tuple and array patterns in `let`;
// - moves of non-`Copy` values (a struct field moved out whole), copies of integers.
#![feature(no_core, lang_items)]
#![no_core]
#![no_main]

#[lang = "sized"]
trait Sized {}

#[lang = "copy"]
trait Copy {}

// core's own impls (core/src/marker.rs `marker_impls!`): a primitive is `Copy` only by an impl.
impl Copy for usize {}
impl Copy for u32 {}
impl Copy for bool {}

// What an index past an array's end calls (nightly's MIR asserts every index); never returns.
#[lang = "panic_bounds_check"]
fn panic_bounds_check(index: usize, len: usize) -> ! {
    loop {}
}

// What a division by zero calls: the check is in every build (the Reference, "Arithmetic and
// Logical Binary Operators": integer division by zero panics; the pinned core's
// `panic_const::panic_const_div_by_zero`); never returns.
#[lang = "panic_const_div_by_zero"]
fn panic_const_div_by_zero() -> ! {
    loop {}
}

struct Tx {
    nonce: u64,
    size: u32,
    pending: bool,
}

struct Pool {
    txs: [Tx; 4],
    pending_count: usize,
    queued_count: usize,
}

fn tx(nonce: u64, size: u32, pending: bool) -> Tx {
    Tx { nonce, size, pending }
}

fn pending_and_queued_txn_count(pool: &Pool) -> (usize, usize) {
    (pool.pending_count, pool.queued_count)
}

fn scan_pending_and_queued_txn_count(pool: &Pool) -> (usize, usize) {
    let mut acc = (0, 0);
    let mut i = 0;
    while i < 4 {
        if pool.txs[i].pending {
            acc.0 += 1;
        } else {
            acc.1 += 1;
        }
        i += 1;
    }
    acc
}

fn recount(pool: &mut Pool) {
    let (pending, queued) = scan_pending_and_queued_txn_count(pool);
    pool.pending_count = pending;
    pool.queued_count = queued;
}

fn total_size(txs: &[Tx; 4]) -> u64 {
    let mut total = 0;
    let mut i = 0;
    while i < 4 {
        total += txs[i].size as u64;
        i += 1;
    }
    total
}

fn swap(t: (usize, usize)) -> (usize, usize) {
    (t.1, t.0)
}

fn into_last(pool: Pool) -> Tx {
    let txs = pool.txs;
    let [_, _, _, last] = txs;
    last
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let mut pool = Pool {
        txs: [tx(1, 100, true), tx(2, 200, false), tx(3, 300, true), tx(4, 50, true)],
        pending_count: 0,
        queued_count: 0,
    };
    recount(&mut pool);
    // 3 pending, 1 queued; 650 bytes; the last is nonce 4, 50 bytes.
    let (p, q) = pending_and_queued_txn_count(&pool);
    let size = total_size(&pool.txs);
    let swapped = swap((p, q));
    let last = into_last(pool);
    // 31 + 1 + 1 + 4 + 5 = 42.
    let n = p * 10 + q + swapped.0 + (size as usize - 600) / 50 + last.nonce as usize + last.size as usize / 10;
    n as i32
}
