//! Multithreaded hashing over every CPU, with concurrent callers served
//! round-robin.
//!
//! This module is crate-internal. The public entry points are
//! [`crate::hash_multithreaded`], [`crate::hash_with`], and their batch
//! forms, whose contracts speak of
//! threads alone; pieces and the pool are how those contracts
//! are met, and this file is where they are explained.
//!
//! # Pieces
//!
//! The input is cut at chunk boundaries into pieces, each a valid BLAKE3
//! subtree (see `tree::left_len`), from the front: each piece
//! is about a thread's share of what remains ([`next_piece_len`]), a power
//! of two chunks within [`MIN_PIECE_LEN`] and [`MAX_PIECE_LEN`]. Pieces
//! shrink toward the end, so a thread that is slow (an efficiency core, or
//! one sharing its CPU) holds a small piece when the others run out, and
//! the finish waits on little. Each thread hashes whole subtrees directly
//! through the one-shot hashing code, producing one chaining value per
//! piece. The caller merges these values a level at a time through the
//! SIMD parent kernels and finishes with the root compression. Inputs
//! under [`MIN_SPLIT_LEN`] stay on the calling thread.
//!
//! # Sources and the pool
//!
//! All the workers' work comes from *sources* ([`sources`]): a stream's
//! tasks, and each calling thread's one-shot jobs. A source numbers its
//! items; its owner publishes them with one store, and workers claim them
//! in order by compare-and-swap, with no lock and no shared queue. A call
//! publishes its job's pieces at once and hashes pieces itself until none
//! remain, then waits for the last ones in flight. Workers, started once per
//! process ([`crate::initialize_multithreaded`]), by default one per CPU beyond the
//! first, take from every registered source, one item at a time: two callers
//! hashing at once each get about half the workers' time, and a slow
//! thread takes fewer pieces than a fast one; there is nothing to tune for
//! fairness or balance. A call arriving when callers already fill the CPUs
//! hashes its input whole on its own thread. Against one locked task list
//! and one condition variable for every worker (i7-12700K, 19 workers:
//! the producer's hand-over 434 ns), every stream cell ran level or faster
//! (messages 5-49%, batches 3-15%), context switches per run 28.5k ->
//! 10k: a pool built for this job beat the general-purpose one.
//!
//! # Kernels
//!
//! On a CPU with SME2, a call that gets the SME2 turn hashes a prefix of
//! its input itself on SME2, in whole subtrees of up to 1 MiB back to back
//! (the flat walk), sized to its speed: about 1.65 NEON threads' worth
//! ([`sme2_prefix`]). Every other piece, the caller's after its prefix
//! included, runs on the NEON hybrids, which run at full speed on every
//! core at once. Measured on an M4 Max against the pool on NEON alone
//! (probe/sme2-thread, job 187): 1-64 MiB 24-32% faster with two threads,
//! 14-21% with four, 1-6% with sixteen; the VM on it alike. Pieces on
//! SME2 handed out one at a time lost (8-128 KiB, with the caller's
//! bookkeeping between them): the SME unit's slow state follows about
//! 0.2 µs of other work between its kernels (`sme2`'s documentation).
//!
//! # Waiting
//!
//! A worker that finds nothing parks on its own thread's token (no two
//! share a lock or a futex word: nineteen workers sleeping through one
//! mutex and condition variable spent 6-15% of all samples in the kernel's
//! futex-bucket spinlock, i7-12700K). While a stream is open it first looks
//! again for [`SPIN`] (more is coming). A call wakes only as many workers
//! as it can use, waking one itself (a system call) and leaving the rest to
//! the woken: a woken worker arrives 15-45 us later on a core whose clock
//! the idle time lowered, so inputs below [`MIN_SPLIT_LEN`] stay on the
//! caller's thread. A caller waiting for its last pieces polls, then parks
//! after [`SPIN_BEFORE_SLEEP`]; its polls yield the CPU every
//! [`YIELD_EVERY`], so a runnable thread never waits a scheduler quantum
//! behind a poller (two threads spinning without yielding took a 128 KiB
//! split from 29 us to 2 ms on a two-CPU machine). Idle threads cost the
//! busy ones unless they sleep: beside eight hashing threads, eight
//! spinning on loads cost them 18% (VM and Mac), `sched_yield` in a loop
//! 36% on the VM.
//!
//! # Tried and set aside
//!
//! - Wakes: a tree of wakes (each woken worker waking half of those owed),
//!   level; every woken worker draining the owed wakes, the first piece at
//!   about 120 us into a 1 MiB call; the caller waking two, 1-4 MiB 9%
//!   slower; a woken worker prefetching its kernels' code, level (the
//!   wake's 15-70 us dwarfs the fetch).
//! - Waiting: `spin_loop` polls that never yield (a scheduler quantum
//!   lost); spinning 10 ms before sleeping (fifteen spinning vCPUs starve
//!   the callers of host time); WFE in place of spinning (it returns every
//!   1.3 us natively too).
//! - Sharing: a pool per caller (two halve each other); one worker per SME
//!   unit with fair-share admission (lost to one thread from 128 KiB under
//!   two callers); backing off a slow thread (it cannot tell another
//!   process from an unrelated thread of its own).
//! - Cutting: splitting from 32 KiB, smaller minimum pieces, equal pieces,
//!   a longest piece of 64 or 256 KiB: slower or level.
//! - SME2: permits per SME unit, pacing SME2 against NEON (both shared
//!   copies locked into a mode slower than NEON), SME-only workers
//!   (superseded by the NEON pool and the turn); the caller's own pieces on
//!   SME2 and the workers on NEON (Mac 256 KiB +8-10%, shared batches
//!   +35-46%); the first K workers on SME2, one an SME unit (the M4 Max
//!   has three): with every core busy the chip's power sets the pace, and
//!   more units add heat, not throughput (4 MiB 2% less time and 7% less
//!   energy at best).

use crate::CVBytes as ChainingValue;
use crate::platform::Platform;
use crate::{CHUNK_LEN, Hash};
#[cfg(test)]
use crate::{Hasher, KEY_LEN};
use std::sync::atomic::{AtomicPtr, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

/// Inputs below this length are hashed on the calling thread. Every call
/// meets sleeping workers (nothing keeps them awake between calls), and a
/// woken worker arrives 15-45 µs later on a core whose clock the idle time
/// lowered, so a split pays only for inputs that take the caller longer.
/// Measured with 1 ms of sleep before each call, the split came back
/// sooner from 512 KiB on an M4 Max (136 µs against 158; jobs 364-367)
/// and from 768 KiB in the VM (146 against 181; 512 KiB 154 against
/// 130). Zooko chose 512 KiB, where it pays on the Mac (September 28,
/// 2026; native first): after the gap 512 KiB 0.35 -> 0.19-0.24 ns/B and
/// 8192 64-byte messages 22.5 -> 13.3 ns/msg (jobs 538-541), the VM's
/// 512 KiB 20-30% slower than on the caller's thread. Measured again
/// (October 1, 2026, Mac, splits of 32-256 KiB): after other work the
/// length at the split ran slower than on the caller's thread (32 KiB
/// x2.34, 128 KiB x1.25-1.29, 256 KiB x1.08), and gained only nonstop
/// with the workers awake (256 KiB x0.81); a rule splitting from 256 KiB
/// when workers are awake never triggered (calls that do not split wake
/// none), so it went.
pub(crate) const MIN_SPLIT_LEN: usize = 512 * 1024;

/// The shortest piece: eight chunks, a hybrid kernel's worth.
const MIN_PIECE_LEN: usize = 8 * CHUNK_LEN;

/// The longest piece.
const MAX_PIECE_LEN: usize = 128 * CHUNK_LEN;

/// Between polls a thread spins in user space, and every YIELD_EVERY it
/// hands the CPU to any runnable thread (a caller the OS has queued
/// behind a poller then waits this long at most, never a quantum).
const YIELD_EVERY: std::time::Duration = std::time::Duration::from_micros(20);

#[inline]
pub(crate) fn poll_pause(yielded: &mut std::time::Instant) {
    for _ in 0..8 {
        std::hint::spin_loop();
    }
    if yielded.elapsed() >= YIELD_EVERY {
        std::thread::yield_now();
        *yielded = std::time::Instant::now();
    }
}

/// How long a caller polls for its last pieces before sleeping.
pub(crate) const SPIN_BEFORE_SLEEP: std::time::Duration = std::time::Duration::from_micros(200);

/// The length of the next piece when `remaining` bytes are uncut and
/// `threads` threads may share them: about a thread's share of what is
/// left, rounded down to a power of two chunks within the bounds above.
/// `threads` is positive. Pieces therefore shrink toward the end of the input, and the slowest
/// thread's last piece is a small one.
fn next_piece_len(remaining: usize, threads: usize) -> usize {
    debug_assert!(threads > 0, "a cut needs at least one thread");
    let want = (remaining / threads).clamp(MIN_PIECE_LEN, MAX_PIECE_LEN);
    1 << (usize::BITS - 1 - want.leading_zeros())
}

/// Hash `input` over the machine's threads in the mode of `key` and
/// `flags` (IV and 0 for plain hashing).
#[inline]
pub(crate) fn hash_with_key(input: &[u8], key: &crate::CVWords, flags: u8) -> Hash {
    // The short path first and inline, so a small input costs what hash()
    // costs; everything the pool needs is behind the call below.
    if input.len() < MIN_SPLIT_LEN {
        return crate::hash_serial(input, key, flags);
    }
    hash_over_pool(input, key, flags)
}

#[inline(never)]
fn hash_over_pool(input: &[u8], key: &crate::CVWords, flags: u8) -> Hash {
    let Some((pool, _caller)) = spare_pool() else {
        return crate::hash_serial(input, key, flags);
    };
    let (pieces, mut cvs) = tree_cvs(pool, input, key, 0, flags);
    merge_root(&pieces, &mut cvs, key, flags)
}

/// The pool, this caller counted until the guard drops; or None with no
/// CPU to spare (every one has a caller, or the process has one CPU), when
/// the call hashes on its own thread. The count lets concurrent callers
/// see that this CPU already has work.
/// The wide AVX-512 kernel's budget for whole-message trees (Linux): one
/// maximum-sized piece per participant, at least two participants when
/// available; the same budget cuts the pieces and asks for the wakes.
/// Other kernels and work kinds keep their policy (experiment, #125).
fn tree_threads(len: usize, available: usize, degree: usize) -> usize {
    if cfg!(all(target_arch = "x86_64", target_os = "linux")) && degree >= 16 { len.div_ceil(MAX_PIECE_LEN).max(2).min(available) } else { available }
}

fn spare_pool() -> Option<(&'static Pool, Caller<'static>)> {
    let pool = pool();
    let callers = pool.callers.fetch_add(1, Ordering::SeqCst) + 1;
    let caller = Caller(&pool.callers);
    (callers < pool.threads).then_some((pool, caller))
}

/// The subtree pieces of `input` at chunk `counter` and their chaining
/// values, hashed over the pool (the SME2 thread's prefix first, where it
/// takes one).
fn tree_cvs(pool: &Pool, input: &[u8], key: &crate::CVWords, counter: u64, flags: u8) -> (Vec<Piece>, Vec<ChainingValue>) {
    let turn = crate::platform::Sme2Turn::take(Platform::detect(), true);
    let threads = tree_threads(input.len(), pool.threads, turn.platform().simd_degree());
    let (pieces, own) = cut_with_prefix(input.len(), threads, prefix_for(&turn, input.len(), threads));
    let mut cvs = vec![ChainingValue::default(); pieces.len()];
    let work = Work::Tree { input, pieces: &pieces, cvs: cvs.as_mut_ptr(), key: *key, counter, flags };
    pool.run_job(work, pieces.len(), own, turn.platform());
    drop(turn);
    (pieces, cvs)
}

/// The two child chaining values of the subtree `input` at chunk
/// `counter`, over the machine's threads: what
/// `compress_subtree_to_parent_node` returns for a whole subtree.
/// Requires a power-of-two number of chunks, at least MIN_SPLIT_LEN
/// bytes, and `counter` a multiple
/// of that number, as `Hasher::update` hands out.
pub(crate) fn subtree_children(input: &[u8], key: &crate::CVWords, counter: u64, flags: u8) -> [u8; 2 * crate::OUT_LEN] {
    assert!(input.len().is_power_of_two() && input.len() >= MIN_SPLIT_LEN, "a whole subtree of at least MIN_SPLIT_LEN bytes");
    assert_eq!(counter % (input.len() / CHUNK_LEN) as u64, 0, "a subtree starts at a multiple of its chunk count");
    let Some((pool, _caller)) = spare_pool() else { return crate::compress_subtree_to_parent_node(input, 0, key, counter, flags, pool_platform()) };
    let (pieces, mut cvs) = tree_cvs(pool, input, key, counter, flags);
    let count = merge_to_children(&pieces, &mut cvs, key, flags);
    debug_assert_eq!(count, 2);
    let mut children = [0u8; 2 * crate::OUT_LEN];
    children[..crate::OUT_LEN].copy_from_slice(&cvs[0]);
    children[crate::OUT_LEN..].copy_from_slice(&cvs[1]);
    children
}

/// [`crate::hash_many`] over the machine's threads: below MIN_SPLIT_LEN
/// of input, or with one message, on this thread.
#[inline]
pub(crate) fn hash_many(input: &[u8], message_len: usize, key: &crate::CVWords, flags: u8, outputs: &mut [[u8; crate::OUT_LEN]]) {
    // The short path first and inline, so a small batch costs what
    // crate::hash_many costs; everything the pool needs is behind the call.
    if outputs.len() < 2 || input.len() < MIN_SPLIT_LEN {
        return crate::hash_many_serial(input, message_len, key, flags, outputs);
    }
    hash_many_over_pool(input, message_len, key, flags, outputs)
}

#[inline(never)]
fn hash_many_over_pool(input: &[u8], message_len: usize, key: &crate::CVWords, flags: u8, outputs: &mut [[u8; crate::OUT_LEN]]) {
    let slot = crate::many::slot_len(message_len);
    assert_eq!(Some(input.len()), slot.checked_mul(outputs.len()), "input holds one slot of whole blocks per output");
    let Some((pool, _caller)) = spare_pool() else {
        return crate::hash_many_serial(input, message_len, key, flags, outputs);
    };
    let pieces = cut_messages(outputs.len(), slot, pool.threads);
    let work = Work::Messages { input, message_len, key: *key, flags, pieces: &pieces, outputs: outputs.as_mut_ptr() };
    pool.run_job(work, pieces.len(), 0, pool_platform());
}

/// [`crate::hash_each_multithreaded_with`]'s pool job: `items`, every one
/// under MIN_SPLIT_LEN, cut into ranges of about [`next_piece_len`] of the
/// bytes that remain.
pub(crate) fn hash_each(key: &crate::CVWords, flags: u8, items: &[&[u8]], out: &mut [[u8; crate::OUT_LEN]]) {
    assert!(items.iter().all(|i| i.len() < MIN_SPLIT_LEN), "items under MIN_SPLIT_LEN");
    let Some((pool, _caller)) = spare_pool() else {
        let platform = Platform::detect();
        return crate::each::hash_each_on(key, flags, items, out, platform, |item| crate::hash_serial_on(item, key, flags, platform));
    };
    let mut remaining: usize = items.iter().map(|i| i.len()).sum();
    let mut pieces = Vec::with_capacity(64);
    let mut start = 0;
    while start < items.len() {
        let want = next_piece_len(remaining, pool.threads);
        let (mut end, mut bytes) = (start, 0);
        while end < items.len() && (bytes < want || end == start) {
            bytes += items[end].len();
            end += 1;
        }
        remaining -= bytes;
        pieces.push(Piece { offset: start, len: end - start });
        start = end;
    }
    let work = Work::Each { items, key: *key, flags, pieces: &pieces, outputs: out.as_mut_ptr() };
    pool.run_job(work, pieces.len(), 0, pool_platform());
}

/// Cut `count` messages in slots of `message_len` bytes into ranges for `threads`
/// threads, in order: each range holds about [`next_piece_len`] of the
/// bytes that remain, at least one message, so ranges shrink toward the
/// end as subtree pieces do.
fn cut_messages(count: usize, message_len: usize, threads: usize) -> Vec<Piece> {
    let mut pieces = Vec::with_capacity(64);
    let mut start = 0;
    while start < count {
        let remaining = (count - start) * message_len.max(1);
        let take = (next_piece_len(remaining, threads) / message_len.max(1)).clamp(1, count - start);
        pieces.push(Piece { offset: start, len: take });
        start += take;
    }
    pieces
}

/// Ends the process if dropped: held while a job is published, forgotten
/// once its workers are done, so it drops only when the caller unwinds
/// with workers possibly still reading its frame.
struct AbortOnUnwind;

impl Drop for AbortOnUnwind {
    fn drop(&mut self) {
        std::process::abort()
    }
}

/// A counted call (spare_pool), until it returns, unwinding too.
struct Caller<'a>(&'a AtomicUsize);

impl Drop for Caller<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// One call's work, on the caller's stack, reached by workers through the
/// caller's source ([`sources::job_source`]) while the call runs: a
/// worker that took piece `i` hashes it and counts it finished, and the
/// caller returns only once every piece is, so every worker access falls
/// inside the caller's frame.
pub(crate) struct Job<'a> {
    work: Work<'a>,
}

/// What a job's pieces are. Piece `i` writes output slot `i` (a chaining
/// value, or a range of digests) and nothing else, from the thread that
/// took `i` through the cursor.
enum Work<'a> {
    /// Subtrees of one input, in offset order; one chaining value each.
    Tree {
        input: &'a [u8],
        pieces: &'a [Piece],
        cvs: *mut ChainingValue,
        key: crate::CVWords,
        /// The chunk counter at `input`'s first byte.
        counter: u64,
        flags: u8,
    },
    /// Ranges of a batch of messages of one length, each in its slot of
    /// whole blocks in `input` (`offset` and `len` count messages); one
    /// digest each.
    Messages { input: &'a [u8], message_len: usize, key: crate::CVWords, flags: u8, pieces: &'a [Piece], outputs: *mut [u8; crate::OUT_LEN] },
    /// Ranges of a collection's items (`offset` and `len` count items),
    /// each under MIN_SPLIT_LEN; one digest each.
    Each { items: &'a [&'a [u8]], key: crate::CVWords, flags: u8, pieces: &'a [Piece], outputs: *mut [u8; crate::OUT_LEN] },
}

/// Any value on a cache line of its own.
#[repr(align(128))]
pub(crate) struct OwnLine<T>(pub(crate) T);

impl<T> std::ops::Deref for OwnLine<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}

impl Job<'_> {
    /// Hash piece `index` into its slot. The caller has taken `index`
    /// through `cursor`.
    unsafe fn hash_piece(&self, index: usize, platform: Platform) {
        match &self.work {
            Work::Tree { input, pieces, cvs, key, counter, flags } => {
                let piece = pieces[index];
                let bytes = &input[piece.offset..][..piece.len];
                let counter = counter + (piece.offset / CHUNK_LEN) as u64;
                let cv = crate::hash_all_at_once(bytes, key, counter, *flags, platform).chaining_value();
                unsafe { *cvs.add(index) = cv };
            }
            Work::Messages { input, message_len, key, flags, pieces, outputs } => {
                let piece = pieces[index];
                let slot = crate::many::slot_len(*message_len);
                let messages = &input[piece.offset * slot..][..piece.len * slot];
                // Sound: this range of outputs belongs to piece `index` alone.
                let digests = unsafe { core::slice::from_raw_parts_mut(outputs.add(piece.offset), piece.len) };
                crate::many::hash_many_on(messages, *message_len, key, *flags, digests, platform);
            }
            Work::Each { items, key, flags, pieces, outputs } => {
                let piece = pieces[index];
                // Sound: this range of outputs belongs to piece `index` alone.
                let digests = unsafe { core::slice::from_raw_parts_mut(outputs.add(piece.offset), piece.len) };
                let range = &items[piece.offset..][..piece.len];
                crate::each::hash_each_on(key, *flags, range, digests, platform, |item| crate::hash_serial_on(item, key, *flags, platform));
            }
        }
    }
}

/// The platform every piece runs on (see the module docs, "Kernels"): on
/// an SME2 CPU the NEON hybrids, which no other core slows down; elsewhere
/// the detected one.
fn pool_platform() -> Platform {
    #[cfg(blake3_sme2)]
    if matches!(Platform::detect(), Platform::SME2) {
        return Platform::NEON;
    }
    Platform::detect()
}

/// One piece of the input: a whole subtree (bytes), or for a batch a
/// range of messages (message indices).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Piece {
    offset: usize,
    len: usize,
}

/// Cut `len` bytes into subtrees for `threads` threads, in offset order,
/// after a prefix of about `prefix` bytes for the SME2 thread. Past the
/// prefix, each piece is [`next_piece_len`] of what remains, and the last
/// is whatever is left. The prefix is whole subtrees of up to
/// [`MAX_PREFIX_PIECE_LEN`], rounded down to a multiple of the first piece
/// after it. A piece's length never exceeds the one before it and every
/// piece starts at a multiple of its own length, so each is a whole
/// subtree at its offset. Returns the pieces and how many form the prefix
/// (none when `prefix` rounds to nothing). `prefix` is below `len`; `len`
/// exceeds [`MIN_PIECE_LEN`] and at least two threads share it, so two or
/// more pieces come back.
fn cut_with_prefix(len: usize, threads: usize, prefix: usize) -> (Vec<Piece>, usize) {
    assert!(threads >= 2, "a serial call hashes the input whole");
    assert!(len > MIN_PIECE_LEN, "one piece has no parent to merge; hash it whole instead");
    assert!(prefix < len, "the SME2 thread's prefix leaves the pool some input");
    let mut pieces = Vec::with_capacity(64);
    let mut offset = 0;
    let first_after = next_piece_len(len - prefix, threads);
    let prefix = prefix / first_after * first_after;
    while offset < prefix {
        let piece = MAX_PREFIX_PIECE_LEN.min(1 << (usize::BITS - 1 - (prefix - offset).leading_zeros()));
        pieces.push(Piece { offset, len: piece });
        offset += piece;
    }
    let own = pieces.len();
    let mut cap = if own > 0 { pieces[own - 1].len } else { MAX_PIECE_LEN };
    while len - offset > MIN_PIECE_LEN {
        let piece = next_piece_len(len - offset, threads).min(cap);
        if len - offset <= piece {
            break;
        }
        pieces.push(Piece { offset, len: piece });
        offset += piece;
        cap = piece;
    }
    pieces.push(Piece { offset, len: len - offset });
    (pieces, own)
}

/// The SME2 thread's prefix for a call holding `turn`: [`sme2_prefix`]
/// when the turn gave it SME2, else nothing.
fn prefix_for(turn: &crate::platform::Sme2Turn, len: usize, threads: usize) -> usize {
    #[cfg(blake3_sme2)]
    if matches!(turn.platform(), Platform::SME2) {
        return sme2_prefix(len, threads);
    }
    let _ = (turn, len, threads);
    0
}

/// The longest piece of the SME2 thread's prefix: the flat walk's largest
/// subtree, which it hashes on SME2 alone.
const MAX_PREFIX_PIECE_LEN: usize = 1 << 20;

/// The SME2 thread's share of an input that `threads` threads hash,
/// itself included: its speed over theirs, about 1.65 NEON threads' worth
/// (M4 Max and the VM on it: the best split of 1 to 64 MiB put 62% on SME2
/// beside one NEON thread, 38% beside three, 12% beside fifteen;
/// probe/sme2-thread, job 187).
/// A share below MAX_PIECE_LEN is no prefix: too little SME2 work to pay
/// for the turn and the streaming session (VM, servil mt 256 KiB over
/// sixteen threads, a 24 KiB prefix: +18-20%).
#[cfg_attr(not(blake3_sme2), allow(dead_code))]
fn sme2_prefix(len: usize, threads: usize) -> usize {
    let share = len / (100 + (threads - 1) * 10000 / 165) * 100;
    if share >= MAX_PIECE_LEN { share } else { 0 }
}

/// Merge a shrinking cut in place, a level at a time. `pieces` tiles the
/// input from zero, with non-increasing power-of-two lengths except the
/// final (possibly short) piece, as produced by `cut_subtrees`. Each CV
/// hashes its corresponding piece. Two or more pieces are required.
///
/// At each level the longer pieces form an untouched prefix; the suffix
/// contains the children of this level. Hash adjacent pairs together with
/// the platform's SIMD parent kernel, carrying an odd final child upward.
/// The final two CVs belong to the root's children, including a short right
/// subtree, on the pool's platform like the pieces.
fn merge_root(pieces: &[Piece], cvs: &mut [ChainingValue], key: &crate::CVWords, flags: u8) -> Hash {
    let count = merge_to_children(pieces, cvs, key, flags);
    debug_assert_eq!(count, 2);
    crate::hasher::parent_output(&cvs[0], &cvs[1], key, flags, pool_platform()).root_hash()
}

/// [`merge_root`] up to the root's two children, left in `cvs[0]` and
/// `cvs[1]`; returns 2.
fn merge_to_children(pieces: &[Piece], cvs: &mut [ChainingValue], key: &crate::CVWords, flags: u8) -> usize {
    debug_assert_eq!(pieces.len(), cvs.len());
    debug_assert!(pieces.len() >= 2);
    debug_assert_eq!(pieces[0].offset, 0);
    debug_assert!(pieces.last().unwrap().len > 0);
    debug_assert!(pieces[..pieces.len() - 1].iter().all(|p| p.len >= MIN_PIECE_LEN));
    debug_assert!(pieces.windows(2).all(|w| w[0].len.is_power_of_two() && w[0].len >= w[1].len && w[0].offset + w[0].len == w[1].offset));
    let platform = pool_platform();
    let mut count = cvs.len();
    let mut width = MIN_PIECE_LEN;
    let mut out = [0u8; crate::MAX_SIMD_DEGREE * crate::OUT_LEN];
    while count > 2 {
        let prefix = pieces.partition_point(|p| p.len > width);
        debug_assert!(prefix < count);
        let mut read = prefix;
        let mut write = prefix;
        while read + 1 < count {
            let children = (count - read).min(2 * crate::MAX_SIMD_DEGREE);
            // The inner loop's variant: each call merges at least one pair.
            assert!(children >= 2, "a parents call with under two children");
            let parents = crate::compress_parents_parallel(cvs[read..read + children].as_flattened(), key, flags, platform, &mut out);
            cvs[write..write + parents].as_flattened_mut().copy_from_slice(&out[..parents * crate::OUT_LEN]);
            read += children;
            write += parents;
        }
        if read < count {
            cvs[write] = cvs[read];
            write += 1;
        }
        // The loop's variant: a level merges at least one pair.
        assert!(write < count, "a level of the tree merged nothing");
        count = write;
        width *= 2;
    }
    count
}

/// [`cut_with_prefix`] with no prefix (the tests').
#[cfg(test)]
fn cut_subtrees(len: usize, threads: usize) -> Vec<Piece> {
    cut_with_prefix(len, threads, 0).0
}

/// The chaining value of the subtree covering `pieces` (in offset order,
/// tiling one subtree): the piece's own value for one piece, else the
/// parent of its two halves, cut at the subtree's left-subtree boundary.
#[cfg(test)]
fn subtree_cv(pieces: &[Piece], cvs: &[ChainingValue], mode: crate::Mode) -> ChainingValue {
    debug_assert_eq!(pieces.len(), cvs.len());
    if pieces.len() == 1 {
        return cvs[0];
    }
    let offset = pieces[0].offset;
    let len: usize = pieces.iter().map(|p| p.len).sum();
    let boundary = offset + crate::tree::left_len(len);
    let split = pieces.partition_point(|p| p.offset < boundary);
    assert!(split > 0 && split < pieces.len() && pieces[split].offset == boundary, "pieces do not tile the tree");
    let left = subtree_cv(&pieces[..split], &cvs[..split], mode);
    let right = subtree_cv(&pieces[split..], &cvs[split..], mode);
    crate::test::parent(&left, &right, mode).chaining_value()
}

/// The pool: threads - 1 workers (the calling thread hashes too) taking from the registered sources
/// ([`sources`]), the SME2 thread beside them where the CPU has SME2, and
/// the record of which workers are parked.
struct Pool {
    /// Calls on the pool now ([`spare_pool`]).
    callers: AtomicUsize,
    /// The threads a call hashes on: its caller and the workers.
    threads: usize,
    /// Whether the SME2 thread runs (this CPU has SME2).
    sme2: bool,
    /// Which workers are parked, how one parks without missing work, and
    /// how a publisher wakes one (crate::park, which Shuttle checks).
    park: crate::park::Park,
}

/// Create the pool and start its threads in the background, once per
/// process ([`crate::initialize_multithreaded`] is its public face): this
/// thread starts the SME2 thread and worker 1, and worker 1 the other
/// workers. Calls run on the workers started so far.
pub(crate) fn initialize(threads: usize) {
    // With one thread, none beside the caller: no SME2 thread either.
    let sme2 = threads > 1 && cfg!(blake3_sme2) && !matches!(pool_platform(), p if core::mem::discriminant(&p) == core::mem::discriminant(&Platform::detect()));
    let pool = Pool { callers: AtomicUsize::new(0), threads, sme2, park: crate::park::Park::new(threads - 1, crate::park::FANOUT) };
    assert!(POOL.set(pool).is_ok(), "initialize_multithreaded creates the pool once");
    if threads == 1 {
        return;
    }
    // Before any thread starts: a child forked from here on has none of
    // them, and its multithreaded calls stop at `pool` (FORKED).
    unsafe extern "C" {
        fn pthread_atfork(prepare: Option<extern "C" fn()>, parent: Option<extern "C" fn()>, child: Option<extern "C" fn()>) -> i32;
    }
    // Sound: the handler only stores to an atomic, which a forked child
    // may do.
    assert_eq!(unsafe { pthread_atfork(None, None, Some(mark_forked)) }, 0, "registering the fork handler");
    if sme2 {
        std::thread::Builder::new().name("blake3-sme2".into()).spawn(sme2_main).expect("spawning the SME2 thread");
    }
    std::thread::Builder::new()
        .name("blake3-worker-1".into())
        .spawn(move || {
            for worker in 2..threads {
                std::thread::Builder::new().name(format!("blake3-worker-{worker}")).spawn(move || worker_main(worker - 1)).expect("spawning a BLAKE3 worker");
            }
            worker_main(0)
        })
        .expect("spawning a BLAKE3 worker");
}

/// The contract's check at every multithreaded call and stream: a call
/// before [`crate::initialize_multithreaded`] panics, in every build. It
/// costs one load and a branch the CPU predicts.
#[inline]
pub(crate) fn require() {
    pool();
}

/// Whether a call may leave the calling thread: the process has more
/// than one CPU. Requires the pool (`require`).
pub(crate) fn splits() -> bool {
    pool().threads > 1
}

/// The pool, once [`initialize`] has created it.
static POOL: OnceLock<Pool> = OnceLock::new();

/// Set in a child forked after the pool started its threads (the fork
/// handler, `pthread_atfork`'s child): the child has none of them.
static FORKED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

extern "C" fn mark_forked() {
    FORKED.store(true, Ordering::Relaxed);
}

/// A multithreaded call or stream in a forked child: the threads it would
/// use stayed in the parent.
#[cold]
#[inline(never)]
fn forked() -> ! {
    panic!(
        "blake3-servil: this process was forked after initialize_multithreaded started its threads, and a forked child has none: \
         initialize the pool in the process that hashes, after any fork (single-threaded calls work anywhere)"
    )
}

/// Whether every worker is parked: false before the pool exists
/// ([`crate::__all_workers_parked`], for tests).
pub(crate) fn all_workers_parked() -> bool {
    POOL.get().is_some_and(|pool| pool.park.parked_count() == pool.threads - 1)
}

/// The pool. The caller must have called
/// [`crate::initialize_multithreaded`] before; a call before it panics. The
/// check is the load of the pool, which every call makes anyway.
#[inline]
fn pool() -> &'static Pool {
    match POOL.get() {
        Some(_) if FORKED.load(Ordering::Relaxed) => forked(),
        Some(pool) => pool,
        None => no_pool(),
    }
}

/// A multithreaded call before `initialize_multithreaded`: the caller broke
/// the contract. The crate's own unit tests, many small programs in one,
/// initialize here instead.
#[cold]
#[inline(never)]
fn no_pool() -> &'static Pool {
    #[cfg(test)]
    {
        crate::initialize_multithreaded(None);
        POOL.get().expect("initialize_multithreaded created the pool")
    }
    #[cfg(not(test))]
    panic!("blake3-servil: call blake3_servil::initialize_multithreaded(None) once, before any multithreaded call or stream: it starts the threads they use")
}

impl Pool {
    /// Publish `work` of `pieces` pieces as this thread's job source, take
    /// pieces on this thread until none remain, and return once every
    /// piece is finished, on whichever thread took it. The caller first
    /// hashes pieces `0..own` on `own_platform`, back to back (the SME2
    /// thread's prefix; the workers' pieces start after it). A caller that
    /// finds no source slot free hashes every piece itself.
    fn run_job(&self, work: Work, pieces: usize, own: usize, own_platform: Platform) {
        crate::ran(crate::Ran::Pool);
        let job = Job { work };
        let source = sources::job_source();
        let slot = source.register();
        // Sound: the caller's frame outlives every access: it waits below,
        // and should it unwind before that (a bug in this crate), the
        // process ends rather than leaving workers reading a gone frame and
        // the next call waiting forever (the guard is forgotten once the
        // wait is over: nothing runs on the normal path).
        let ptr = &job as *const Job as *mut Job<'static>;
        let published = AbortOnUnwind;
        let target = source.publish_job(ptr, pieces, own);
        if slot.is_some() && pieces > own + 1 {
            let budget = match &job.work {
                Work::Tree { input, .. } => tree_threads(input.len(), self.threads, own_platform.simd_degree()).saturating_sub(1),
                _ => pieces - own - 1,
            };
            self.wake_for((pieces - own - 1).min(budget));
        }
        for index in 0..own {
            // Sound: the workers' pieces start past these.
            unsafe { job.hash_piece(index, own_platform) };
        }
        source.finished(own);
        while let Some(sources::Item::Pieces(_, index, n)) = source.take() {
            // Sound: this thread took these, so it alone writes their slots.
            for i in index..index + n {
                unsafe { job.hash_piece(i, pool_platform()) };
            }
            source.finished(n);
        }
        source.wait(target);
        core::mem::forget(published);
        if let Some(slot) = slot {
            sources::Source::unregister(slot);
        }
    }

    /// Wake workers for `wanted` pieces of new work (crate::park).
    pub(crate) fn wake_for(&self, wanted: usize) {
        self.park.wake_for(wanted, self.threads - 1);
    }

    /// Worker `me`'s loop: the sources' tasks and pieces; spinning a while
    /// when a stream is open (more is coming), else parked, whenever it
    /// finds nothing.
    fn serve(&self, me: usize) -> ! {
        loop {
            if sources::run_one() {
                continue;
            }
            if sources::open() && spin_for_work() {
                continue;
            }
            // Park unless work shows (crate::park's handshake); woken, pay
            // the wakes owed before working.
            if self.park.park_unless(me, || sources::visible(false)) {
                self.park.pay_owed();
            }
        }
    }
}

/// How long a worker that finds nothing keeps looking while a stream is
/// open (more is coming) before it parks: 5 us. i7-12700K, against the
/// stream on the earlier pool: none cost small messages 9-12% (a park and
/// a wake at every gap); 20 us cost 22% more CPU time a run for no speed;
/// a cap on how many workers spin did not help.
const SPIN: std::time::Duration = std::time::Duration::from_micros(5);

/// Look for work for up to SPIN: whether some appeared.
fn spin_for_work() -> bool {
    let started = std::time::Instant::now();
    loop {
        for _ in 0..16 {
            if sources::visible(false) {
                return true;
            }
            core::hint::spin_loop();
        }
        if started.elapsed() >= SPIN || !sources::open() {
            return false;
        }
    }
}

/// The workers' work: sources, each a numbered sequence of items that its
/// owner publishes and workers claim. One owner writes `published` (one
/// Release store); workers advance `claimed` by compare-and-swap, only
/// below `published`. Both only count up, across everything a source ever
/// carries, so a stale claim is impossible. A stream's source carries its
/// tasks in a ring (a slot written again only after its task was taken,
/// copied out); a calling thread's carries its one-shot jobs' pieces,
/// numbered on from job to job (job k owns [base, base + pieces)). Sources
/// are never freed (recycled), so a worker may read one after its owner is
/// done with it.
pub(crate) mod sources {
    use super::*;
    use core::cell::UnsafeCell;
    use core::mem::MaybeUninit;

    /// Sources registered at once. A stream or a job that finds none free
    /// is hashed on its own thread.
    const SLOTS: usize = 64;

    static REGISTERED: [AtomicPtr<Source>; SLOTS] = [const { AtomicPtr::new(core::ptr::null_mut()) }; SLOTS];
    static ACTIVE: AtomicU64 = AtomicU64::new(0);
    /// Streams open: workers spin a while for more work only when some are.
    static OPEN: OwnLine<AtomicUsize> = OwnLine(AtomicUsize::new(0));

    #[repr(align(128))]
    pub(crate) struct Source {
        claimed: OwnLine<AtomicUsize>,
        /// A stream's tasks; empty for a caller's jobs.
        ring: Box<[UnsafeCell<MaybeUninit<Task>>]>,
        /// A caller's current job and its first number; the job stays the
        /// claimed numbers' until `finish.finished` passes them.
        job: AtomicPtr<Job<'static>>,
        base: AtomicUsize,
        /// What is published and what is finished, and the handshake
        /// between the waiting caller and the worker finishing its last
        /// piece (crate::finish, which Loom checks).
        finish: crate::finish::Finish,
    }

    // Sound: a ring slot is written by its stream only above `published`
    // and below `claimed` + len, and read by the one worker whose CAS took
    // it, across the Release store of `published` and the Acquire load;
    // `owner` is behind its lock; the rest is atomics.
    unsafe impl Sync for Source {}
    unsafe impl Send for Source {}

    /// What a claim takes: a stream's task, or a run of a job's pieces
    /// (`n` from `index`, consecutive).
    pub(crate) enum Item {
        Task(Task),
        Pieces(*const Job<'static>, usize, usize),
    }

    impl Source {
        /// A source with a ring of `len` tasks (a stream's), or none (a
        /// caller's jobs).
        pub(crate) fn new(len: usize) -> Source {
            Source {
                claimed: OwnLine(AtomicUsize::new(0)),
                ring: (0..len).map(|_| UnsafeCell::new(MaybeUninit::uninit())).collect(),
                job: AtomicPtr::new(core::ptr::null_mut()),
                base: AtomicUsize::new(0),
                finish: crate::finish::Finish::new(),
            }
        }

        /// Register for the workers: the slot, or None when all are taken.
        pub(crate) fn register(&'static self) -> Option<usize> {
            let ptr = self as *const Source as *mut Source;
            let slot = (0..SLOTS).find(|&i| REGISTERED[i].compare_exchange(core::ptr::null_mut(), ptr, Ordering::SeqCst, Ordering::Relaxed).is_ok())?;
            ACTIVE.fetch_or(1 << slot, Ordering::SeqCst);
            Some(slot)
        }

        pub(crate) fn unregister(slot: usize) {
            ACTIVE.fetch_and(!(1 << slot), Ordering::SeqCst);
            REGISTERED[slot].store(core::ptr::null_mut(), Ordering::SeqCst);
        }

        /// A stream's task to the workers: one write, one Release store;
        /// then the SME2 thread first, and with two or more waiting and a
        /// worker parked, a worker.
        pub(crate) fn publish(&self, task: Task) {
            let p = self.finish.published.load(Ordering::Relaxed);
            debug_assert!(p - self.claimed.load(Ordering::Relaxed) < self.ring.len(), "a source's ring holds every task not yet taken");
            // Sound: slot p % len is free (its last task was taken), and
            // only this stream writes it.
            unsafe { (*self.ring[p % self.ring.len()].get()).write(task) };
            self.finish.published.store(p + 1, Ordering::Release);
            core::sync::atomic::fence(Ordering::SeqCst);
            let pool = pool();
            if pool.sme2 {
                wake_sme2();
            }
            if p + 1 - self.claimed.load(Ordering::Relaxed) >= 2 {
                pool.park.wake_one();
            }
        }

        /// Make `job` the caller's current job, its pieces `0..own` the
        /// caller's own: the number `finished` must reach when it is done.
        pub(crate) fn publish_job(&self, job: *mut Job<'static>, pieces: usize, own: usize) -> usize {
            let base = self.finish.published.load(Ordering::Relaxed);
            debug_assert_eq!(self.finish.finished_count(), base, "the last job is finished");
            self.job.store(job, Ordering::Relaxed);
            self.base.store(base, Ordering::Relaxed);
            // No worker can claim a number yet (claimed == published), so
            // this store alone moves `claimed` past the caller's own pieces.
            self.claimed.store(base + own, Ordering::Relaxed);
            self.finish.published.store(base + pieces, Ordering::Release);
            core::sync::atomic::fence(Ordering::SeqCst);
            base + pieces
        }

        /// Claim the next published item, if any.
        pub(crate) fn take(&self) -> Option<Item> {
            let mut c = self.claimed.load(Ordering::Acquire);
            loop {
                let p = self.finish.published.load(Ordering::Acquire);
                if c >= p {
                    return None;
                }
                // A job's pieces go in runs, consecutive, sized to the work
                // left (guided self-scheduling): each worker reads its input
                // front to back, so a mapped file from storage meets the
                // kernel's readahead stream by stream (official's I/O
                // pattern), and runs shrink to one piece near the end for
                // balance. A stream's tasks go one at a time.
                let n = if self.ring.is_empty() { ((p - c) / (2 * pool().threads)).max(1) } else { 1 };
                match self.claimed.compare_exchange_weak(c, c + n, Ordering::AcqRel, Ordering::Acquire) {
                    Ok(_) if self.ring.is_empty() => return Some(Item::Pieces(self.job.load(Ordering::Relaxed), c - self.base.load(Ordering::Relaxed), n)),
                    // Sound: this thread alone took slot c, written before
                    // `published` passed it.
                    Ok(_) => return Some(Item::Task(unsafe { (*self.ring[c % self.ring.len()].get()).assume_init_read() })),
                    Err(now) => c = now,
                }
            }
        }

        /// A stream's next task, if any (this source is a stream's).
        pub(crate) fn take_task(&self) -> Option<Task> {
            match self.take() {
                Some(Item::Task(task)) => Some(task),
                Some(Item::Pieces(..)) => unreachable!("a stream's source carries tasks"),
                None => None,
            }
        }

        fn waiting(&self) -> usize {
            self.finish.published.load(Ordering::Acquire).saturating_sub(self.claimed.load(Ordering::Acquire))
        }

        /// Wake a parked worker for a task this stream has published that no
        /// thread has taken: `publish` wakes one for the second task in
        /// waiting, expecting the caller to take a lone one, and a caller
        /// that only collects never does (it waited forever, the workers
        /// asleep: the C API's stream test on an i7-12700K and on GitHub's
        /// 4-CPU runners; Devon Jonte, #99).
        pub(crate) fn wake_for_waiting(&self) {
            let pool = pool();
            if self.waiting() > 0 {
                pool.park.wake_one();
            }
        }

        /// Count `n` of the caller's job's pieces finished (their outputs
        /// written before); the last one wakes a caller parked for it.
        pub(crate) fn finished(&self, n: usize) {
            self.finish.finished(n);
        }

        /// Wait until `finished` reaches `target`: polling for
        /// SPIN_BEFORE_SLEEP, then parked until the worker finishing the
        /// last piece unparks this thread.
        pub(crate) fn wait(&self, target: usize) {
            let started = std::time::Instant::now();
            let mut yielded = started;
            self.finish.wait(target, || {
                poll_pause(&mut yielded);
                started.elapsed() < SPIN_BEFORE_SLEEP
            });
        }
    }

    /// Callers' job sources no thread holds.
    static FREE_JOBS: Mutex<std::vec::Vec<&'static Source>> = Mutex::new(std::vec::Vec::new());

    struct Held(&'static Source);

    impl Drop for Held {
        /// A thread's exit gives its source back, never freed.
        fn drop(&mut self) {
            FREE_JOBS.lock().unwrap().push(self.0);
        }
    }

    std::thread_local! {
        static MINE: std::cell::OnceCell<Held> = const { std::cell::OnceCell::new() };
    }

    /// This thread's source for its one-shot jobs.
    pub(crate) fn job_source() -> &'static Source {
        MINE.with(|mine| {
            mine.get_or_init(|| {
                let source = FREE_JOBS.lock().unwrap().pop().unwrap_or_else(|| Box::leak(Box::new(Source::new(0))));
                source.finish.own();
                Held(source)
            })
            .0
        })
    }

    pub(crate) fn opened() {
        OPEN.fetch_add(1, Ordering::SeqCst);
    }

    pub(crate) fn closed() {
        OPEN.fetch_sub(1, Ordering::SeqCst);
    }

    pub(crate) fn open() -> bool {
        OPEN.load(Ordering::Relaxed) > 0
    }

    /// The first registered source for which `f` says yes.
    fn find(mut f: impl FnMut(&'static Source) -> bool) -> bool {
        let mut active = ACTIVE.load(Ordering::SeqCst);
        while active != 0 {
            let slot = active.trailing_zeros() as usize;
            active &= active - 1;
            let ptr = REGISTERED[slot].load(Ordering::Acquire);
            // Sound: sources are never freed (they are recycled).
            if !ptr.is_null() && f(unsafe { &*ptr }) {
                return true;
            }
        }
        false
    }

    /// Run one item from any source: whether there was one. When two or
    /// more wait and a worker is parked, this one wakes the next first
    /// (the chain).
    pub(crate) fn run_one() -> bool {
        find(|source| {
            let Some(item) = source.take() else { return false };
            if source.waiting() >= 2 {
                pool().park.wake_one();
            }
            match item {
                Item::Task(task) => task.run(pool_platform()),
                Item::Pieces(job, index, n) => {
                    // Sound: the job lives until its pieces are finished,
                    // these among them; this thread alone took them.
                    for i in index..index + n {
                        unsafe { (*job).hash_piece(i, pool_platform()) };
                    }
                    source.finished(n);
                }
            }
            true
        })
    }

    /// A task from any stream (the SME2 thread takes these alone).
    pub(crate) fn take_stream_task() -> Option<Task> {
        let mut found = None;
        find(|source| {
            found = if source.ring.is_empty() { None } else { source.take_task() };
            found.is_some()
        });
        found
    }

    /// Whether any source has work waiting (`streams`: any stream).
    pub(crate) fn visible(streams: bool) -> bool {
        find(|source| (!streams || !source.ring.is_empty()) && source.waiting() > 0)
    }

    #[cfg(test)]
    mod test {
        use super::*;

        /// Unregistering takes a source out of the walk: its slot is
        /// given back. (A leaked slot is a slowdown: after enough streams
        /// every stream hashes on its caller alone.)
        #[test]
        fn test_unregister_gives_the_slot_back() {
            let source: &'static Source = &*Box::leak(Box::new(Source::new(0)));
            let slot = source.register().expect("a free slot");
            assert!(find(|t| core::ptr::eq(t, source)), "a registered source is reached");
            Source::unregister(slot);
            assert!(!find(|t| core::ptr::eq(t, source)), "an unregistered source is still reached");
        }

        /// What a worker looks for before it parks: a registered source
        /// with an item published and not yet claimed, any source's for a
        /// worker (visible(false)), a stream's alone for the SME2 thread
        /// (visible(true)). The predicate alone, no thread: its four
        /// survivors under the mutation check had stood as scheduling.
        #[test]
        fn test_visible_sees_a_waiting_item_and_its_kind() {
            let job: &'static Source = &*Box::leak(Box::new(Source::new(0)));
            let stream: &'static Source = &*Box::leak(Box::new(Source::new(4)));
            let slots = [job.register().expect("a free slot"), stream.register().expect("a free slot")];
            assert!(!visible(false) && !visible(true), "nothing published: nothing visible");
            job.finish.published.store(1, Ordering::SeqCst);
            assert!(visible(false), "a job's piece waits: a worker sees it");
            assert!(!visible(true), "the SME2 thread looks at streams alone");
            job.claimed.store(1, Ordering::SeqCst);
            assert!(!visible(false), "the piece was claimed: nothing waits");
            stream.finish.published.store(1, Ordering::SeqCst);
            assert!(visible(false) && visible(true), "a stream's task waits: both see it");
            for slot in slots {
                Source::unregister(slot);
            }
        }

        /// A thread's exit gives its job source back, and the next thread
        /// to need one takes it: none is leaked per thread (a source that
        /// stayed with its finished thread was one more allocation for
        /// every thread a program ever started).
        #[test]
        fn test_a_finished_threads_source_is_the_next_threads() {
            let first = std::thread::spawn(|| job_source() as *const Source as usize).join().unwrap();
            let second = std::thread::spawn(|| job_source() as *const Source as usize).join().unwrap();
            assert_eq!(first, second, "the second thread takes the source the first gave back");
        }

        /// Unregistering one source leaves the others registered: its bit
        /// alone leaves ACTIVE (a change that cleared every other bit
        /// would strand the rest, unseen by a test of one source).
        #[test]
        fn test_unregister_leaves_the_other_sources_registered() {
            let sources: Vec<&'static Source> = (0..3).map(|_| &*Box::leak(Box::new(Source::new(0)))).collect();
            let slots: Vec<usize> = sources.iter().map(|s| s.register().expect("a free slot")).collect();
            Source::unregister(slots[1]);
            assert!(!find(|t| core::ptr::eq(t, sources[1])), "the unregistered source is still reached");
            for &i in &[0, 2] {
                assert!(find(|t| core::ptr::eq(t, sources[i])), "source {i} lost its registration with another's");
            }
            for &i in &[0, 2] {
                Source::unregister(slots[i]);
            }
        }

        /// `find` reaches every registered source, the last of several
        /// included: a test alone in its process registers one source at
        /// a time, so only a test of several sees the walk past the first.
        #[test]
        fn test_find_reaches_every_registered_source() {
            let sources: Vec<&'static Source> = (0..3).map(|_| &*Box::leak(Box::new(Source::new(0)))).collect();
            // Registered for the rest of the process, which is this test's.
            for s in &sources {
                s.register().expect("a free slot");
            }
            for (i, s) in sources.iter().enumerate() {
                assert!(find(|t| core::ptr::eq(t, *s)), "find reaches registered source {i} of 3");
            }
        }
    }
}

/// The longest part of a stream's input planned as tasks
/// (a stream's piece): a task is a whole subtree within one part.
pub(crate) const TASK_LEN: usize = 64 * CHUNK_LEN;

/// One piece of a stream's work (crate::stream), hashed by whichever thread
/// takes it from the stream's source: a whole subtree at chunk `counter` of a message,
/// as [`crate::hasher::subtree_len`] cuts it, its result a 64-byte block at `out`;
/// or, with `batch` the messages' length, a range of a batch's messages in
/// their slots, their digests at `out`. Then it counts down `left`; the
/// stream keeps the bytes, `out`, and `left` in place until `left` is zero.
pub(crate) struct Task {
    pub(crate) input: *const u8,
    pub(crate) len: usize,
    pub(crate) counter: u64,
    pub(crate) batch: Option<usize>,
    pub(crate) key: crate::CVWords,
    pub(crate) flags: u8,
    pub(crate) out: *mut u8,
    pub(crate) left: *const AtomicUsize,
    /// With `members` above zero, the task is that many separate short
    /// messages instead (or, with `batch`, batches of that length's
    /// messages): each one's digest (digests) to its `out`, then `left`
    /// counted down by `members`; `len` sums their bytes.
    pub(crate) members: usize,
    pub(crate) member: [Member; MEMBERS],
}

/// The most short messages (or small batches) one task takes.
pub(crate) const MEMBERS: usize = 64;

/// A short message in a task of several ([`Task::members`]).
#[derive(Clone, Copy)]
pub(crate) struct Member {
    pub(crate) input: *const u8,
    pub(crate) len: usize,
    pub(crate) out: *mut u8,
}

const NO_MEMBER: Member = Member { input: core::ptr::null(), len: 0, out: core::ptr::null_mut() };

// Sound: a task's pointers stay valid until its `left` reaches zero (above).
unsafe impl Send for Task {}

impl Task {
    /// A task over `input` at chunk `counter`, its mode, kind, and
    /// destinations still to fill in.
    pub(crate) fn of(input: &[u8], counter: u64) -> Task {
        Task {
            input: input.as_ptr(),
            len: input.len(),
            counter,
            batch: None,
            key: [0; 8],
            flags: 0,
            out: core::ptr::null_mut(),
            left: core::ptr::null(),
            members: 0,
            member: [NO_MEMBER; MEMBERS],
        }
    }

    /// An empty task of short messages (with `batch`, batches of messages
    /// of that length) in the mode of `key` and `flags`.
    pub(crate) fn members(key: &crate::CVWords, flags: u8, batch: Option<usize>) -> Task {
        Task { key: *key, flags, batch, ..Task::of(&[], 0) }
    }

    /// Hash on `platform` into `out`: for a subtree of two chunks or more at
    /// chunk zero (it may be the whole message), its pair of child chaining
    /// values; for any other, its chaining value (first 32 bytes). Then
    /// count down.
    pub(crate) fn run(self, platform: Platform) {
        if self.members > 0 {
            self.run_members(platform);
            // Sound: the stream keeps `left` in place until it is zero.
            unsafe { &*self.left }.fetch_sub(self.members, Ordering::Release);
            return;
        }
        // A task without members is a piece of a long message: a stream's
        // batch parts are always members (Stream::add_part adds one to the
        // task it opens, and only an opened task is handed over).
        assert!(self.batch.is_none(), "a task of batch parts has members");
        // Sound: the stream keeps these in place until `left` is zero.
        let bytes = unsafe { core::slice::from_raw_parts(self.input, self.len) };
        // Sound: a subtree's `out` is one 64-byte block.
        let out = unsafe { &mut *(self.out as *mut [u8; crate::BLOCK_LEN]) };
        if self.counter == 0 && self.len > CHUNK_LEN {
            *out = crate::compress_subtree_to_parent_node(bytes, 0, &self.key, 0, self.flags, platform);
        } else {
            out[..crate::OUT_LEN].copy_from_slice(&crate::hash_all_at_once(bytes, &self.key, self.counter, self.flags, platform).chaining_value());
        }
        unsafe { &*self.left }.fetch_sub(1, Ordering::Release);
    }
}

impl Task {
    /// The members' digests: messages of one length side by side on the
    /// multi-lane kernels (`hash_many`'s, from a table of pointers) where
    /// that length is one block, or on x86 two to sixteen blocks of a power
    /// of two (128 B to 1 KiB); any others one at a time.
    fn run_members(&self, platform: Platform) {
        let members = &self.member[..self.members];
        if let Some(message_len) = self.batch {
            for m in members {
                // Sound: the stream keeps each batch's bytes and digest
                // space in place until the task's `left` is zero.
                let bytes = unsafe { core::slice::from_raw_parts(m.input, m.len) };
                let count = m.len / crate::many::slot_len(message_len);
                let digests = unsafe { core::slice::from_raw_parts_mut(m.out as *mut [u8; crate::OUT_LEN], count) };
                crate::many::hash_many_on(bytes, message_len, &self.key, self.flags, digests, platform);
            }
            return;
        }
        // Sound: the stream keeps every member's bytes and `out` in place
        // until the task's `left` is zero.
        if members.len() >= 2 && members.iter().all(|m| m.len == members[0].len) {
            match members[0].len {
                crate::BLOCK_LEN => return self.run_equal_members::<{ crate::BLOCK_LEN }>(platform),
                #[cfg(target_arch = "x86_64")]
                128 => return self.run_equal_members::<128>(platform),
                #[cfg(target_arch = "x86_64")]
                256 => return self.run_equal_members::<256>(platform),
                #[cfg(target_arch = "x86_64")]
                512 => return self.run_equal_members::<512>(platform),
                #[cfg(target_arch = "x86_64")]
                CHUNK_LEN => return self.run_equal_members::<CHUNK_LEN>(platform),
                _ => {}
            }
        }
        for m in members {
            let bytes = unsafe { core::slice::from_raw_parts(m.input, m.len) };
            let hash = crate::hash_serial_on(bytes, &self.key, self.flags, platform);
            unsafe { core::ptr::copy_nonoverlapping(hash.as_bytes().as_ptr(), m.out, crate::OUT_LEN) };
        }
    }
}

impl Task {
    /// The members' digests, each a message of `N` bytes (whole blocks, at
    /// most a chunk; run_members checked), side by side on the multi-lane
    /// kernel from a table of pointers.
    fn run_equal_members<const N: usize>(&self, platform: Platform) {
        let members = &self.member[..self.members];
        // Sound: the stream keeps every member's N bytes and `out` in place
        // until the task's `left` is zero.
        let table: arrayvec::ArrayVec<&[u8; N], MEMBERS> = members.iter().map(|m| unsafe { &*(m.input as *const [u8; N]) }).collect();
        let mut digests = [[0u8; crate::OUT_LEN]; MEMBERS];
        // One block carries every flag at once, and passes them as the
        // 64-byte batches do (flags_start and flags_end 0): with every
        // caller of hash_many::<64> passing those zeros, LLVM specializes it
        // (AArch64: x5 and x6 left out, `out` in a register; a queue passing
        // its flags here instead cost the VM's lent batches of 16).
        let (flags, start, end) = if N == crate::BLOCK_LEN {
            (self.flags | crate::CHUNK_START | crate::CHUNK_END | crate::ROOT, 0, 0)
        } else {
            (self.flags, crate::CHUNK_START, crate::CHUNK_END | crate::ROOT)
        };
        platform.hash_many::<N>(&table, &self.key, 0, crate::IncrementCounter::No, flags, start, end, digests[..members.len()].as_flattened_mut());
        for (m, digest) in members.iter().zip(&digests) {
            unsafe { core::ptr::copy_nonoverlapping(digest.as_ptr(), m.out, crate::OUT_LEN) };
        }
    }
}

/// The SME2 thread, on CPUs with SME2: it hashes tasks only, on SME2 (the
/// SME unit hashes a 64 KiB piece in 14 us where a NEON worker takes 22,
/// Mac, probe/queue-timeline), under the process's turn so that a caller of
/// hash() elsewhere keeps the unit; without the turn, on NEON. It takes
/// streams' tasks while any wait, and parks as soon as none does; every
/// publish wakes it first.
fn sme2_main() {
    // As for a worker: a panic here ends the process rather than hanging
    // the callers waiting on it.
    let _ = std::panic::catch_unwind(sme2_serve);
    std::process::abort()
}

fn sme2_serve() -> ! {
    assert!(SME2_THREAD.set(std::thread::current()).is_ok(), "one SME2 thread");
    loop {
        if let Some(task) = sources::take_stream_task() {
            if task.members > 0 {
                // Short messages and small batches, gathered, run faster
                // on NEON than through an SME2 session per task (Mac:
                // 64-byte messages 14-15% faster, batches of 16 and 64
                // 10-15%); below 16 KiB the other members run the NEON
                // kernels on either platform.
                task.run(pool_platform());
            } else {
                let turn = crate::platform::Sme2Turn::take(Platform::detect(), true);
                task.run(turn.platform());
                drop(turn);
            }
            continue;
        }
        // Park as a worker does: the flag, one more look, the token.
        SME2_PARKED.store(true, Ordering::SeqCst);
        if sources::visible(true) {
            SME2_PARKED.store(false, Ordering::SeqCst);
            continue;
        }
        while SME2_PARKED.load(Ordering::SeqCst) {
            std::thread::park();
        }
    }
}

/// The SME2 thread, and whether it is parked: every publish wakes it first.
static SME2_THREAD: OnceLock<std::thread::Thread> = OnceLock::new();
static SME2_PARKED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

fn wake_sme2() {
    if SME2_PARKED.swap(false, Ordering::SeqCst) {
        if let Some(thread) = SME2_THREAD.get() {
            thread.unpark();
        }
    }
}

/// For tests that must reach the pool: the call this thread made since
/// `RAN` was cleared went to the pool, or the process has one CPU and no
/// pool. (Each test runs alone in its process, so no other caller holds
/// the pool's CPUs.)
#[cfg(test)]
pub(crate) fn assert_past_the_split(what: &str) {
    let kinds = crate::RAN.with(|r| r.get());
    assert!(pool().threads == 1 || kinds & crate::Ran::Pool as u32 != 0, "{what} went to the pool");
}

/// Run `task` on this thread, as a worker would.
pub(crate) fn run_task(task: Task) {
    task.run(pool_platform());
}

fn worker_main(me: usize) {
    // A worker that panicked would die quietly while callers wait for its
    // piece forever (their stacks are the work); the process ends instead,
    // the panic's message printed first by the panic hook.
    let _ = std::panic::catch_unwind(move || {
        let pool = pool();
        pool.park.register(me);
        pool.serve(me)
    });
    std::process::abort()
}

#[cfg(test)]
mod test {
    use super::*;

    /// A call gives its place among the pool's callers back: alone in its
    /// process, as each test is, the count is zero again after one call
    /// that went to the pool (a place kept would leave the pool "busy" for
    /// later calls, which would then hash alone, right and slower).
    /// Arbitrary CVs isolate the merge's tree shape from chunk hashing:
    /// a cut of a few pieces, and one whose first level holds more
    /// children than one parents call takes (2 x MAX_SIMD_DEGREE),
    /// one left over to carry up.
    #[test]
    fn test_simd_merge_matches_recursive() {
        let key = [93u8; KEY_LEN];
        let context_key = crate::performance::context_key("parallel merge test");
        for (threads, len) in [(3, MIN_SPLIT_LEN + 1025), (2, (64 << 20) + 1)] {
            let pieces = cut_subtrees(len, threads);
            let mut cvs = vec![[0u8; crate::OUT_LEN]; pieces.len()];
            crate::test::paint_test_input(cvs.as_flattened_mut());
            let split = pieces.partition_point(|p| p.offset < crate::tree::left_len(len));
            for mode in [crate::Mode::Plain, crate::Mode::Keyed(&key), crate::Mode::DeriveKeyWith(&context_key)] {
                let left = subtree_cv(&pieces[..split], &cvs[..split], mode);
                let right = subtree_cv(&pieces[split..], &cvs[split..], mode);
                let want = crate::test::parent(&left, &right, mode).root_hash();
                assert_eq!(want, merge_root(&pieces, &mut cvs.clone(), &mode.key_and_flags().0, mode.key_and_flags().1), "len={len}, threads={threads}");
            }
        }
    }

    #[test]
    fn test_a_caller_gives_its_place_back() {
        let pool = pool();
        let input = std::vec![0x3Cu8; MIN_SPLIT_LEN];
        crate::RAN.with(|r| r.set(0));
        assert_eq!(crate::performance::hash_multithreaded(crate::Mode::Plain, &crate::test::message(&input), input.len()), crate::convenience::hash(&input));
        assert_past_the_split("the call");
        assert_eq!(pool.callers.load(Ordering::SeqCst), 0, "the caller's place given back");
    }

    /// A caller whose last piece outlasts its polling parks, and the
    /// worker finishing that piece wakes it (the pool's pieces shrink
    /// toward the end, so a job's own wait rarely parks; this one must).
    /// Keep: it detects, by hanging, mutants the clock then stops
    /// (tools/mutants/timeouts-x86_64.txt).
    #[test]
    fn test_a_parked_caller_is_woken_by_the_last_piece() {
        // The pool, and with it the startup self-test, on this thread
        // first: a failure there fails the test here, where on the worker
        // thread below it would end that thread alone and leave the wait
        // waiting forever (a broken build's every mutant hung this test).
        let _ = pool();
        let source = sources::job_source();
        let target = source.publish_job(core::ptr::null_mut(), 1, 0);
        // An unpark this thread received earlier would let one park()
        // return at once and hide a missing wake-up.
        std::thread::park_timeout(core::time::Duration::ZERO);
        let started = std::time::Instant::now();
        // A plain thread: a scope's thread unparks the scope's owner as it
        // ends, which would hide a missing wake-up too.
        let worker = std::thread::spawn(move || {
            std::thread::sleep(10 * SPIN_BEFORE_SLEEP);
            let Some(sources::Item::Pieces(_, _, n)) = source.take() else { panic!("the one piece") };
            assert_eq!(n, 1, "the one piece");
            source.finished(n);
        });
        source.wait(target);
        assert!(started.elapsed() >= 10 * SPIN_BEFORE_SLEEP, "the caller waited for the piece");
        worker.join().unwrap();
    }

    /// Members of one length side by side, against fixed values (official
    /// BLAKE3's digests): every platform this machine runs, the three modes,
    /// equal and mixed lengths, unaligned inputs; the slots beside the
    /// outputs untouched.
    /// Keep: it detects, by hanging, mutants the clock then stops
    /// (tools/mutants/timeouts-x86_64.txt).
    #[test]
    fn equal_members_match_the_reference() {
        #[allow(unused_mut)]
        let mut platforms: Vec<Platform> = std::vec::Vec::new();
        #[cfg(target_arch = "x86_64")]
        platforms.extend([Platform::sse41(), Platform::avx2()].into_iter().flatten());
        #[cfg(blake3_avx512_ffi)]
        platforms.extend(Platform::avx512());
        platforms.push(Platform::detect());
        let key = [0x5Au8; crate::KEY_LEN];
        let context = "equal queue members";
        let context_key = crate::performance::context_key(context);
        let modes = [
            (*crate::IV, 0),
            (crate::platform::words_from_le_bytes_32(&key), crate::KEYED_HASH),
            (crate::platform::words_from_le_bytes_32(&context_key), crate::DERIVE_KEY_MATERIAL),
        ];
        let counts: &[usize] = &[2, 3, 4, 7, 8, 15, 16, 17, 63, MEMBERS];
        let offsets: &[usize] = &[0, 1, 7, 63];
        let api_modes = [crate::Mode::Plain, crate::Mode::Keyed(&key), crate::Mode::DeriveKey(context)];
        let mut golden = crate::test::Golden::new("equal_members");
        for len in [64, 128, 192, 256, 512, 1024] {
            for &count in counts {
                for mixed in [false, true] {
                    for &offset in offsets {
                        let inputs: Vec<Vec<u8>> = (0..count)
                            .map(|m| {
                                let length = if mixed && m % 2 == 1 { len + 1 } else { len };
                                (0..offset + length).map(|i| ((m * 1031 + i) % 251) as u8).collect()
                            })
                            .collect();
                        for (mode, (words, flags)) in modes.iter().enumerate() {
                            let api_mode = api_modes[mode];
                            let expected: Vec<[u8; crate::OUT_LEN]> =
                                inputs.iter().map(|input| *crate::convenience::hash_with(api_mode, &input[offset..]).as_bytes()).collect();
                            golden.add(expected.as_flattened(), || {
                                inputs.iter().flat_map(|input| crate::test::official::hash(api_mode, &input[offset..])).collect()
                            });
                            for &platform in &platforms {
                                let done = AtomicUsize::new(count);
                                let mut out = vec![[0xAA; crate::OUT_LEN]; count + 2];
                                let mut task = Task::members(words, *flags, None);
                                task.members = count;
                                task.left = &done;
                                // Every destination from one raw base (Stacked Borrows).
                                let base = out.as_mut_ptr();
                                for m in 0..count {
                                    let out = unsafe { base.add(m + 1) }.cast::<u8>();
                                    task.member[m] = Member { input: inputs[m][offset..].as_ptr(), len: inputs[m].len() - offset, out };
                                }
                                task.run(platform);
                                assert_eq!(&out[1..count + 1], &expected[..], "{platform:?}, {count} x {len}, mixed {mixed}, offset {offset}, mode {mode}");
                                assert_eq!(out[0], [0xAA; crate::OUT_LEN]);
                                assert_eq!(out[count + 1], [0xAA; crate::OUT_LEN]);
                                assert_eq!(done.load(Ordering::Acquire), 0);
                            }
                        }
                    }
                }
            }
        }
        golden.check(0xd9842e3410d3c4ae);
    }

    #[test]
    fn test_tree_thread_budget() {
        let enabled = cfg!(all(target_arch = "x86_64", target_os = "linux"));
        for degree in [1, 8, 16, 32] {
            for (len, available, budget) in [
                (512 * CHUNK_LEN, 48, 4),
                (1024 * CHUNK_LEN, 48, 8),
                (8192 * CHUNK_LEN, 48, 48),
                (1024 * CHUNK_LEN, 2, 2),
                (1024 * CHUNK_LEN, 1, 1),
                (1, 48, 2),
                (MAX_PIECE_LEN, 48, 2),
                (MAX_PIECE_LEN + 1, 48, 2),
                (2 * MAX_PIECE_LEN + 1, 48, 3),
            ] {
                assert_eq!(tree_threads(len, available, degree), if enabled && degree >= 16 { budget } else { available });
            }
        }
    }

    #[test]
    fn test_next_piece_len() {
        assert_eq!(next_piece_len(64 * CHUNK_LEN, 16), MIN_PIECE_LEN);
        assert_eq!(next_piece_len(1 << 20, 16), 64 * CHUNK_LEN);
        assert_eq!(next_piece_len(3 << 20, 16), 128 * CHUNK_LEN, "192 KiB caps at the longest piece");
        assert_eq!(next_piece_len(1 << 30, 16), MAX_PIECE_LEN);
        assert_eq!(next_piece_len((3 << 20) / 4, 16), 32 * CHUNK_LEN, "48 KiB rounds down to 32 KiB");
        for remaining in [MIN_PIECE_LEN + 1, 100_000, 1 << 20, (3 << 20) + 5, 1 << 27] {
            for threads in [2, 3, 16, 64] {
                let piece = next_piece_len(remaining, threads);
                assert!(piece.is_power_of_two() && piece >= MIN_PIECE_LEN && piece <= MAX_PIECE_LEN);
            }
        }
    }

    /// Cuts with an SME2 prefix tile the input with non-increasing whole
    /// subtrees, the prefix's pieces first, at every length and share, and
    /// hash to the serial result.
    #[test]
    fn test_cut_with_prefix_shapes_and_hashes() {
        let mut input = vec![0u8; 3 * (1 << 20) + 12345];
        crate::test::paint_test_input(&mut input);
        for len in [MIN_PIECE_LEN + 1, 64 * CHUNK_LEN, 100 * CHUNK_LEN + 7, 1 << 20, 3 << 20, input.len()] {
            for threads in [2, 3, 4, 16] {
                let prefix = sme2_prefix(len, threads);
                let (pieces, own) = cut_with_prefix(len, threads, prefix);
                assert!(pieces.len() >= 2 && own < pieces.len());
                assert!(pieces[..own].iter().map(|p| p.len).sum::<usize>() <= prefix);
                let mut offset = 0;
                for (i, p) in pieces.iter().enumerate() {
                    assert_eq!(p.offset, offset);
                    if i + 1 < pieces.len() {
                        assert!(p.len.is_power_of_two() && p.offset % p.len == 0 && p.len >= pieces[i + 1].len, "{len} {threads}: {pieces:?}");
                    }
                    offset += p.len;
                }
                assert_eq!(offset, len);
                let mut cvs: Vec<ChainingValue> = pieces
                    .iter()
                    .map(|p| {
                        crate::hash_all_at_once(&input[p.offset..][..p.len], crate::IV, (p.offset / CHUNK_LEN) as u64, 0, Platform::detect()).chaining_value()
                    })
                    .collect();
                assert_eq!(merge_root(&pieces, &mut cvs, crate::IV, 0), crate::convenience::hash(&input[..len]), "{len} {threads}");
            }
        }
    }

    #[test]
    fn test_hash_matches_serial() {
        let mut input = vec![0u8; (3 << 20) + 12345];
        crate::test::paint_test_input(&mut input);
        for len in [
            0,
            1,
            MIN_SPLIT_LEN - 1,
            MIN_SPLIT_LEN,
            MIN_SPLIT_LEN + 1,
            MIN_SPLIT_LEN + 64,
            MIN_SPLIT_LEN + 1023,
            MIN_SPLIT_LEN + 1024,
            MIN_SPLIT_LEN + 1025,
            2 * MIN_SPLIT_LEN + 999,
            3 << 20,
            input.len(),
        ] {
            let want = crate::convenience::hash(&input[..len]);
            assert_eq!(want, hash_with_key(&input[..len], crate::IV, 0), "len = {len}");
            let key = [42u8; KEY_LEN];
            assert_eq!(
                crate::convenience::hash_with(crate::Mode::Keyed(&key), &input[..len]),
                hash_with_key(&input[..len], &crate::platform::words_from_le_bytes_32(&key), crate::KEYED_HASH),
                "keyed len = {len}"
            );
            let context_key = crate::performance::context_key("lanes test");
            assert_eq!(
                *Hasher::new(crate::Mode::DeriveKeyWith(&context_key)).update(&input[..len]).finalize().as_bytes(),
                *hash_with_key(&input[..len], &crate::platform::words_from_le_bytes_32(&context_key), crate::DERIVE_KEY_MATERIAL).as_bytes(),
                "derive len = {len}"
            );
        }
    }

    /// `Hasher::update_multithreaded` against `hash`, fed in pieces of
    /// several sizes after a first piece that leaves the counter unaligned
    /// (so `update` shrinks the subtrees it hands the pool), keyed too.
    #[test]
    fn test_update_multithreaded_matches_hash() {
        let mut input = vec![0u8; (5 * MIN_SPLIT_LEN + 1025).max(3 << 20) + 12345];
        crate::test::paint_test_input(&mut input);
        let key = [42u8; KEY_LEN];
        for len in [0, 1, MIN_SPLIT_LEN, MIN_SPLIT_LEN + 1, 5 * MIN_SPLIT_LEN + 1025, 3 << 20, input.len()] {
            for first in [0, 1, 1024, 3000, MIN_SPLIT_LEN] {
                for piece in [1000, MIN_SPLIT_LEN, 4 * MIN_SPLIT_LEN, 1 << 20, usize::MAX] {
                    let first = first.min(len);
                    let mut hasher = Hasher::new(crate::Mode::Plain);
                    let mut keyed = Hasher::new(crate::Mode::Keyed(&key));
                    hasher.update_multithreaded(&input[..first]);
                    keyed.update_multithreaded(&input[..first]);
                    for part in input[first..len].chunks(piece.min(len.max(1))) {
                        hasher.update_multithreaded(part);
                        keyed.update_multithreaded(part);
                    }
                    let what = format!("len {len}, first {first}, pieces of {piece}");
                    assert_eq!(hasher.finalize(), crate::convenience::hash(&input[..len]), "{what}");
                    assert_eq!(keyed.finalize(), crate::convenience::hash_with(crate::Mode::Keyed(&key), &input[..len]), "keyed {what}");
                }
            }
        }
    }

    /// Many concurrent callers on one process: every result is right, and
    /// each call waits for its workers and leaves every result complete.
    #[test]
    fn test_concurrent_callers_agree() {
        let mut input = vec![0u8; 8 * MIN_SPLIT_LEN + 1];
        crate::test::paint_test_input(&mut input);
        let want = crate::convenience::hash(&input);
        let input = &input[..];
        let barrier = std::sync::Barrier::new(32);
        std::thread::scope(|scope| {
            for _ in 0..32 {
                let barrier = &barrier;
                scope.spawn(move || {
                    barrier.wait();
                    for _ in 0..20 {
                        assert_eq!(want, hash_with_key(input, crate::IV, 0));
                    }
                });
            }
        });
        // Global pool counters can change as other tests start calls.
        // Each call's digest assertion above checks its own completion.
    }

    /// Message ranges: every message in exactly one range, ranges shrink
    /// toward the end, and a batch that fits one range stays whole.
    #[test]
    fn test_cut_messages_shapes() {
        for message_len in [crate::BLOCK_LEN, 256, 3 * CHUNK_LEN + 5] {
            let count = 40 * MIN_SPLIT_LEN / message_len;
            let pieces = cut_messages(count, message_len, 16);
            assert!(pieces.len() >= 16, "{} ranges of {message_len}-byte messages", pieces.len());
            assert_eq!(pieces[0].offset, 0);
            assert!(pieces.windows(2).all(|w| w[0].offset + w[0].len == w[1].offset && w[0].len >= w[1].len));
            assert_eq!(pieces.last().unwrap().offset + pieces.last().unwrap().len, count);
            let bytes = |p: &Piece| p.len * message_len;
            assert!(pieces[..pieces.len() - 1].iter().all(|p| bytes(p) <= MAX_PIECE_LEN && bytes(p) >= MIN_PIECE_LEN / 2));
        }
        assert_eq!(cut_messages(100, crate::BLOCK_LEN, 16).len(), 1);
    }

    /// The pool gives the single-threaded digests, for one-block
    /// messages, whole-block messages, and messages longer than a chunk.
    /// Keep: it detects, by hanging, mutants the clock then stops
    /// (tools/mutants/timeouts-x86_64.txt).
    #[test]
    fn test_hash_many_pool_agrees() {
        let mut buffer = vec![0u8; 4 * MIN_SPLIT_LEN];
        crate::test::paint_test_input(&mut buffer);
        for message_len in [crate::BLOCK_LEN, 256, 100, 3 * CHUNK_LEN + 5] {
            let slot = crate::many::slot_len(message_len);
            let count = buffer.len() / slot;
            let mut owned = buffer[..count * slot].to_vec();
            for message in owned.chunks_exact_mut(slot) {
                message[message_len..].fill(0);
            }
            let input = &owned[..];
            let mut want = vec![[0u8; crate::OUT_LEN]; count];
            crate::performance::hash_many(crate::Mode::Plain, &crate::performance::Batch::in_place(&crate::test::blocks(input), message_len), &mut want);
            for (i, digest) in want.iter().enumerate().take(200) {
                assert_eq!(*digest, *crate::convenience::hash(&input[i * slot..][..message_len]).as_bytes(), "message {i}");
            }
            let mut got = vec![[0u8; crate::OUT_LEN]; count];
            hash_many(input, message_len, crate::IV, 0, &mut got);
            assert_eq!(want, got, "{message_len}-byte messages");
        }
    }

    /// Concurrent batch callers beside tree callers: every digest right.
    #[test]
    fn test_concurrent_batch_callers_agree() {
        let mut buffer = vec![0u8; 8 * MIN_SPLIT_LEN];
        crate::test::paint_test_input(&mut buffer);
        let count = buffer.len() / crate::BLOCK_LEN;
        let mut want = vec![[0u8; crate::OUT_LEN]; count];
        crate::performance::hash_many(crate::Mode::Plain, &crate::performance::Batch::in_place(&crate::test::blocks(&buffer), crate::BLOCK_LEN), &mut want);
        let tree_want = crate::convenience::hash(&buffer);
        let (want, buffer) = (&want[..], &buffer[..]);
        let barrier = std::sync::Barrier::new(16);
        std::thread::scope(|scope| {
            for i in 0..16 {
                let barrier = &barrier;
                scope.spawn(move || {
                    barrier.wait();
                    let mut got = vec![[0u8; crate::OUT_LEN]; count];
                    for _ in 0..10 {
                        if i % 2 == 0 {
                            hash_many(buffer, crate::BLOCK_LEN, crate::IV, 0, &mut got);
                            assert_eq!(want, &got[..]);
                        } else {
                            assert_eq!(tree_want, hash_with_key(buffer, crate::IV, 0));
                        }
                    }
                });
            }
        });
    }

    /// Idle workers park while a stream is open: each one that finds no
    /// work spins at most SPIN, then parks, so an open stream between
    /// submissions costs no CPU. It waits for the moment every worker is
    /// parked; a worker that never parks hangs it, and nextest's one clock
    /// (.config/nextest.toml) kills and names it. (It once waited at most
    /// ten seconds itself, when other tests ran beside it in one process.)
    #[test]
    fn test_idle_workers_park_while_a_stream_is_open() {
        let pool = pool();
        let workers = pool.threads - 1;
        let all_parked = || pool.park.parked_count() == workers;
        crate::performance::stream(
            crate::Mode::Plain,
            |_: &mut [Hash]| {},
            |s| {
                while !all_parked() {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                s.finish();
            },
        );
    }
}
