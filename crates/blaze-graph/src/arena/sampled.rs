//! The sampled blocks still held: what the global heap keeps, by call site.
//!
//! The heap sampler's ring (`blaze_profile::heap`) says what was allocated, which is the churn.
//! What a build's memory is made of is the other number: the blocks nobody freed. A cold build
//! of blaze held 13 GiB of codegen's heap with one task running (2026-11-11, lane mem-1111),
//! and no row said which call sites' blocks they were. So each sampled block is also kept
//! here, under its address, with the words the sampler wrote for it (its stack, tag and the
//! bytes it stands for), and taken out as it is freed. Whatever is in the table when the
//! profile reads it ([`each`]) was sampled and is still held, and a call site's samples'
//! weights summed are the bytes held there, within a sample's.
//!
//! The table is [`BUCKETS`] buckets of [`WAYS`] addresses, a bucket one cache line, found by a
//! hash of the address. A free costs a load of [`ON`] and, where the table is on, the eight
//! loads of the block's bucket; nothing is written unless the block was sampled. A bucket that
//! is full loses the sample ([`lost`]), which only lowers a site's bytes held.
//!
//! The invariant: an address is in the table only between the sampled allocation that returned
//! it and the free (or the move) of that block. A sample is put by the allocating thread
//! before the block is handed out, and taken by the freeing thread before the block goes back
//! to the allocator, so no other block of the same address can be in the table at once. A
//! block that moves ([`lift`], [`land`]) takes its samples with it.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// The buckets: 8192 of 8 addresses hold 65,536 samples, 256 GiB held at one sample in 4 MiB.
pub const BUCKETS: usize = 1 << 13;
/// The addresses of a bucket: one cache line.
pub const WAYS: usize = 8;
const SLOTS: usize = BUCKETS * WAYS;

/// The words kept with a sample: the sampler's own record of it, opaque here.
pub const WORDS: usize = 56;

/// A place a thread holds while it writes or moves its sample: no address, so no free takes it.
const BUSY: u64 = 1;

#[repr(align(64))]
struct Bucket([AtomicU64; WAYS]);

static ADDRESSES: [Bucket; BUCKETS] =
    [const { Bucket([const { AtomicU64::new(0) }; WAYS]) }; BUCKETS];

/// Each place's words. 29 MiB of address space, its pages touched as places are first used.
static RECORDS: [[AtomicU64; WORDS]; SLOTS] =
    [const { [const { AtomicU64::new(0) }; WORDS] }; SLOTS];

/// Whether frees look their block up: set once the sampler keeps what it samples ([`keep`]).
static ON: AtomicBool = AtomicBool::new(false);

/// Samples not kept, their bucket full.
static LOST: AtomicU64 = AtomicU64::new(0);

/// From here every free and move looks its block up, and [`put`] keeps samples. Never turned
/// off: a table that stopped following frees would name freed blocks as held.
pub fn keep() {
    ON.store(true, Ordering::Release);
}

/// How many samples were not kept because their bucket was full.
pub fn lost() -> u64 {
    LOST.load(Ordering::Relaxed)
}

#[inline(always)]
fn bucket(address: u64) -> usize {
    // Blocks are 16-byte aligned at least; Fibonacci hashing of the rest, its top bits.
    ((address >> 4).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> (64 - BUCKETS.trailing_zeros())) as usize
}

/// A free place of `bucket`, held ([`BUSY`]) for the caller.
fn claim(bucket: usize) -> Option<usize> {
    ADDRESSES[bucket].0.iter().position(|place| {
        place.compare_exchange(0, BUSY, Ordering::Acquire, Ordering::Relaxed).is_ok()
    })
}

/// Keeps the sample of the block at `block` with its `words` (at most [`WORDS`]) until the
/// block is freed. By the thread that allocated the block, before it hands it out. Nothing
/// where the table is off; a full bucket loses the sample.
pub fn put(block: usize, words: impl Iterator<Item = u64>) {
    if !ON.load(Ordering::Relaxed) || block as u64 <= BUSY {
        return;
    }
    let at = bucket(block as u64);
    let Some(way) = claim(at) else {
        LOST.fetch_add(1, Ordering::Relaxed);
        return;
    };
    let record = &RECORDS[at * WAYS + way];
    let mut words = words.chain(std::iter::repeat(0));
    for word in record {
        word.store(words.next().unwrap_or(0), Ordering::Relaxed);
    }
    ADDRESSES[at].0[way].store(block as u64, Ordering::Release);
}

/// The block at `ptr` is about to be freed: its samples go. Called before the allocator has
/// the block back.
#[inline(always)]
pub(super) fn freed(ptr: *mut u8) {
    if ON.load(Ordering::Relaxed) {
        forget(ptr.addr() as u64);
    }
}

#[inline(never)]
fn forget(address: u64) {
    for place in &ADDRESSES[bucket(address)].0 {
        if place.load(Ordering::Relaxed) == address {
            place.store(0, Ordering::Release);
        }
    }
}

/// The places of a block's samples, lifted out of the table while the block may move.
#[derive(Clone, Copy)]
pub(super) struct Lifted {
    ways: [bool; WAYS],
    any: bool,
}

/// The block at `ptr` is about to be reallocated: its samples' places are held ([`BUSY`]) so
/// that neither its free inside the realloc nor another block handed its address meanwhile
/// can touch them. [`land`] puts them back.
#[inline(always)]
pub(super) fn lift(ptr: *mut u8) -> Lifted {
    match ON.load(Ordering::Relaxed) {
        true => lifted(ptr.addr() as u64),
        false => Lifted { ways: [false; WAYS], any: false },
    }
}

#[inline(never)]
fn lifted(address: u64) -> Lifted {
    let mut out = Lifted { ways: [false; WAYS], any: false };
    for (way, place) in ADDRESSES[bucket(address)].0.iter().enumerate() {
        if place.load(Ordering::Relaxed) == address {
            place.store(BUSY, Ordering::Release);
            out.ways[way] = true;
            out.any = true;
        }
    }
    out
}

/// The realloc of the block that was at `old` gave `new` (null: it failed and the block is
/// where it was): the samples [`lift`] held are the block's at its address now.
#[inline(always)]
pub(super) fn land(lifted: Lifted, old: *mut u8, new: *mut u8) {
    if lifted.any {
        landed(lifted, old.addr() as u64, new.addr() as u64);
    }
}

#[cold]
#[inline(never)]
fn landed(lifted: Lifted, old: u64, new: u64) {
    let from = bucket(old);
    let stays = new == 0 || new == old;
    let to = bucket(new);
    for way in (0..WAYS).filter(|way| lifted.ways[*way]) {
        let place = &ADDRESSES[from].0[way];
        if stays {
            place.store(old, Ordering::Release);
            continue;
        }
        match claim(to) {
            Some(into) => {
                let (was, now) = (&RECORDS[from * WAYS + way], &RECORDS[to * WAYS + into]);
                for (word, value) in now.iter().zip(was) {
                    word.store(value.load(Ordering::Relaxed), Ordering::Relaxed);
                }
                ADDRESSES[to].0[into].store(new, Ordering::Release);
            }
            None => _ = LOST.fetch_add(1, Ordering::Relaxed),
        }
        place.store(0, Ordering::Release);
    }
}

/// Every sample held now, to `each` with its words: a reading beside threads that allocate
/// and free, so a sample put or freed meanwhile is in it or not, and one whose place changed
/// hands under the read is left out, never half of two.
pub fn each(mut each: impl FnMut(&[u64; WORDS])) {
    for (at, bucket) in ADDRESSES.iter().enumerate() {
        for (way, place) in bucket.0.iter().enumerate() {
            let address = place.load(Ordering::Acquire);
            if address <= BUSY {
                continue;
            }
            let mut words = [0u64; WORDS];
            for (out, word) in words.iter_mut().zip(&RECORDS[at * WAYS + way]) {
                *out = word.load(Ordering::Relaxed);
            }
            std::sync::atomic::fence(Ordering::Acquire);
            if place.load(Ordering::Relaxed) == address {
                each(&words);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn held(first: u64) -> usize {
        let mut found = 0;
        each(|words| found += usize::from(words[0] == first));
        found
    }

    /// The invariant: a sample is in the table from its put to its block's free, follows the
    /// block through a move, and stays through a realloc that failed or moved nothing. One
    /// test, since the table is the process's: each case uses addresses of its own.
    #[test]
    fn a_sample_is_held_until_its_block_is_freed_and_follows_a_move() {
        keep();
        let block = |n: usize| (0x7000_0000usize + n * 64) as *mut u8;
        put(block(1).addr(), [11, 2, 3].into_iter());
        assert_eq!(held(11), 1);
        // A free of another block of the same bucket or any other leaves it.
        freed(block(2));
        assert_eq!(held(11), 1);
        // A realloc that moved the block: the sample is at its new address, with its words.
        let lifted = lift(block(1));
        assert_eq!(held(11), 0, "lifted: no reading sees it mid-move");
        // Another block is handed the old address meanwhile and freed: the lifted sample stays.
        put(block(1).addr(), [12].into_iter());
        freed(block(1));
        land(lifted, block(1), block(3));
        assert_eq!(held(11), 1);
        freed(block(1));
        assert_eq!(held(11), 1, "the old address is no longer its block");
        // A realloc in place, and one that failed, put it back where it was.
        let lifted = lift(block(3));
        land(lifted, block(3), block(3));
        let lifted = lift(block(3));
        land(lifted, block(3), std::ptr::null_mut());
        assert_eq!(held(11), 1);
        // Two samples of one block (it grew in place past a second sample) both go at its free.
        put(block(3).addr(), [11].into_iter());
        assert_eq!(held(11), 2);
        freed(block(3));
        assert_eq!(held(11), 0);
    }

    /// A bucket holds [`WAYS`] samples: one more is lost and counted, never put over another.
    #[test]
    fn a_full_bucket_loses_the_sample_and_counts_it() {
        keep();
        // Addresses of one bucket: found by search, as the hash is not invertible by eye.
        let home = bucket(0x9000_0000);
        // alloc: a test's nine addresses
        let same: Vec<u64> = (0..)
            .map(|n| 0x9000_0000u64 + n * 16)
            .filter(|address| bucket(*address) == home)
            .take(WAYS + 1)
            .collect();
        let before = lost();
        for address in &same {
            put(*address as usize, [21].into_iter());
        }
        assert_eq!(held(21), WAYS);
        assert_eq!(lost(), before + 1);
        for address in &same {
            freed(*address as usize as *mut u8);
        }
        assert_eq!(held(21), 0);
    }

    /// The invariant under threads: with eight threads putting, moving and freeing blocks of
    /// their own (no two share an address, as no two live blocks do) beside a reader, the
    /// table ends holding exactly the samples of the blocks each thread still holds, each
    /// with its own words, less those a full bucket lost. A stress test on real threads, not
    /// a simulation: the table is atomics the allocator reads in place, which `Sim` does not
    /// drive.
    #[test]
    fn threads_end_with_exactly_their_held_blocks() {
        keep();
        const THREADS: u64 = 8;
        const STEPS: u64 = 20_000;
        let lost_before = lost();
        let stop = std::sync::atomic::AtomicBool::new(false);
        let held: Vec<Vec<(u64, u64)>> = std::thread::scope(|scope| {
            let reader = scope.spawn(|| {
                while !stop.load(Ordering::Relaxed) {
                    // A reading beside the writers hands out only whole samples: words a
                    // thread of this test put, its marker first.
                    each(|words| {
                        if words[0] >> 32 == 0xb1a2e {
                            assert_eq!(words[1], words[0] ^ 0x5555, "a torn sample");
                        }
                    });
                }
            });
            let workers: Vec<_> = (0..THREADS)
                .map(|thread| {
                    scope.spawn(move || {
                        // Each thread's addresses are its own: 16-byte blocks from its base.
                        let base = 0x1_0000_0000u64 * (thread + 2);
                        let mut state = 0x9E37_79B9u64 ^ thread;
                        let mut next = || {
                            state ^= state << 13;
                            state ^= state >> 7;
                            state ^= state << 17;
                            state
                        };
                        let mut mine: Vec<(u64, u64)> = Vec::new(); // alloc: the test's oracle
                        let mut made = 0u64;
                        for _ in 0..STEPS {
                            match next() % 4 {
                                0 | 1 if mine.len() < 96 => {
                                    made += 1;
                                    let address = base + made * 16;
                                    let id = 0xb1a2e << 32 | thread << 24 | (made & 0xff_ffff);
                                    put(address as usize, [id, id ^ 0x5555].into_iter());
                                    mine.push((address, id));
                                }
                                2 if !mine.is_empty() => {
                                    let at = (next() % mine.len() as u64) as usize;
                                    let (address, _) = mine.swap_remove(at);
                                    freed(address as usize as *mut u8);
                                }
                                3 if !mine.is_empty() => {
                                    let at = (next() % mine.len() as u64) as usize;
                                    made += 1;
                                    let to = base + made * 16;
                                    let lifted = lift(mine[at].0 as usize as *mut u8);
                                    land(lifted, mine[at].0 as usize as *mut u8, to as usize as *mut u8);
                                    mine[at].0 = to;
                                }
                                _ => {}
                            }
                        }
                        mine
                    })
                })
                .collect(); // alloc: the test's threads
            let held = workers.into_iter().map(|it| it.join().expect("a worker")).collect();
            stop.store(true, Ordering::Relaxed);
            reader.join().expect("the reader");
            held
        });
        let mut found: Vec<u64> = Vec::new(); // alloc: the test's reading
        each(|words| {
            if words[0] >> 32 == 0xb1a2e {
                found.push(words[0]);
            }
        });
        found.sort_unstable();
        let mut expected: Vec<u64> = held.iter().flatten().map(|(_, id)| *id).collect(); // alloc: the oracle
        expected.sort_unstable();
        let lost = (lost() - lost_before) as usize;
        // Every sample found is of a block still held, once; what is missing was lost to a
        // full bucket (a put or a move), and counted.
        assert!(found.windows(2).all(|pair| pair[0] != pair[1]), "a sample twice");
        assert!(found.iter().all(|id| expected.binary_search(id).is_ok()), "a freed block's sample");
        assert!(expected.len() - found.len() <= lost, "{} missing, {lost} lost", expected.len() - found.len());
        for (address, _) in held.iter().flatten() {
            freed(*address as usize as *mut u8);
        }
    }
}
