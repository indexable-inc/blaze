//! Many messages of one length, one digest each: [`crate::hash_many`]
//! and the pool's pieces of it.
//!
//! Each message sits in a slot of whole blocks ([`slot_len`]), zero past
//! its end, so every kernel reads whole blocks in place and records the
//! last block's true length. A message of up to a chunk is one chunk, the
//! shape the kernels hash many lanes at a time: sixteen per group on
//! SME2's 512-bit streaming vectors, several beside the integer units on
//! the NEON hybrids, up to [`TABLE`] per call so one entry into streaming
//! mode covers eight groups. Messages of 2 to 15 chunks go side by side on
//! SME2; others through the same code as [`crate::hash`], one at a time.

use crate::platform::Platform;
use crate::{BLOCK_LEN, CHUNK_END, CHUNK_LEN, CHUNK_START, CVWords, IncrementCounter, OUT_LEN, ROOT};

/// Most messages per platform call: eight SME2 groups per entry into
/// streaming mode, the same as the tree walk's `sme2::DEGREE`. Measured on
/// the 16-vCPU VM at 1024 one-block messages, ns per message: 128 → 9.7,
/// 256 → 10.2, 512 and 1024 → 12.5. The slowdown follows the run length
/// handed to the platform, with the kernel call's own group count, the
/// frame size, the buffers' alignment, and the loops below each ruled out
/// by measurement; what remains is scanning far ahead of the kernel. On an
/// M4 the entry into streaming mode costs about a microsecond, so a longer
/// run may pay there; measure before changing this.
pub(crate) const TABLE: usize = 128;

/// The bytes each message of `len` bytes takes in a batch: `len` rounded up
/// to whole blocks, one block for an empty message.
#[inline(always)]
pub(crate) fn slot_len(len: usize) -> usize {
    len.next_multiple_of(BLOCK_LEN).max(BLOCK_LEN)
}

/// hash_many's contract on its input: `count` slots of whole blocks for
/// messages of `len` bytes (checked in every build: the kernels read
/// them), each slot's bytes past its message zero (checked in debug
/// builds: a Batch keeps them so).
#[inline]
pub(crate) fn assert_slots(input: &[u8], len: usize, count: usize) {
    let slot = slot_len(len);
    // In every build: the kernels read `slot * count` bytes from `input`,
    // so a wrong length would be read past its end (AGENTS: a check of a
    // mistake that would mismanage memory, at the cost of a comparison).
    assert_eq!(Some(input.len()), slot.checked_mul(count), "input holds one slot of whole blocks per output");
    // Every caller's slots come from a Batch (performance's calls, the
    // stream's batches) or from the crate's own buffers, which keep them
    // zero past each message (BLAKE3 #205): checked in debug builds.
    debug_assert!(len % BLOCK_LEN == 0 || input.chunks_exact(slot).all(|s| s[len..].iter().all(|&b| b == 0)), "every slot's bytes past its message are zero");
}

/// `outputs[i] = hash(input[i * slot..][..len])` for every message, with
/// `slot = slot_len(len)`, in the mode of `key` and `flags` (IV and 0 for
/// plain hashing), on `platform`: messages of up to a chunk go to
/// the kernels TABLE at a time, 2 to 15 chunks side by side on SME2, others
/// through the one-message path. Requires `input.len() == slot *
/// outputs.len()` and every slot's bytes past `len` zero (a Batch's, or
/// the crate's own).
pub(crate) fn hash_many_on(input: &[u8], len: usize, key: &CVWords, flags: u8, outputs: &mut [[u8; OUT_LEN]], platform: Platform) {
    // One-block messages first, on the path they took before slots (a
    // shared entry cost 64 B x 3 7% in the regression check).
    if len == BLOCK_LEN && outputs.len() >= 2 {
        assert_slots(input, len, outputs.len());
        for (messages, digests) in input.chunks(BLOCK_LEN * TABLE).zip(outputs.chunks_mut(TABLE)) {
            hash_run::<{ BLOCK_LEN }>(messages, key, flags, digests, platform);
        }
        return;
    }
    let slot = slot_len(len);
    assert_slots(input, len, outputs.len());
    if len == 0 {
        outputs.fill(*crate::hash_serial_on(&[], key, flags, platform).as_bytes());
        return;
    }
    #[cfg(blake3_sme2)]
    if chunked(len) && platform_is_sme2(platform) && (outputs.len() >= 16 || outputs.len() >= sme2_chunked_min(len)) {
        return hash_chunked(input, len, key, flags, outputs, platform);
    }
    #[cfg(blake3_neon)]
    if (CHUNK_LEN + 1..=2 * CHUNK_LEN).contains(&len) && outputs.len() >= 2 && neon_plans() {
        return hash_two_chunks(input, len, key, flags, outputs);
    }
    if outputs.len() < 2 || len > CHUNK_LEN {
        #[cfg(target_arch = "x86_64")]
        return hash_long_or_single(input, len, key, flags, outputs, platform);
        #[cfg(not(target_arch = "x86_64"))]
        {
            for (i, output) in outputs.iter_mut().enumerate() {
                *output = *crate::hash_serial_on(&input[i * slot..][..len], key, flags, platform).as_bytes();
            }
            return;
        }
    }
    let plans = neon_plans();
    let last_len = len - (slot - BLOCK_LEN);
    for (messages, digests) in input.chunks(slot * TABLE).zip(outputs.chunks_mut(TABLE)) {
        match slot / BLOCK_LEN {
            1 => hash_run_short::<{ BLOCK_LEN }>(messages, key, flags, digests, platform, plans, last_len),
            2 => hash_blocks::<{ 2 * BLOCK_LEN }>(messages, key, flags, digests, platform, plans, last_len),
            3 => hash_blocks::<{ 3 * BLOCK_LEN }>(messages, key, flags, digests, platform, plans, last_len),
            4 => hash_blocks::<{ 4 * BLOCK_LEN }>(messages, key, flags, digests, platform, plans, last_len),
            5 => hash_blocks::<{ 5 * BLOCK_LEN }>(messages, key, flags, digests, platform, plans, last_len),
            6 => hash_blocks::<{ 6 * BLOCK_LEN }>(messages, key, flags, digests, platform, plans, last_len),
            7 => hash_blocks::<{ 7 * BLOCK_LEN }>(messages, key, flags, digests, platform, plans, last_len),
            8 => hash_blocks::<{ 8 * BLOCK_LEN }>(messages, key, flags, digests, platform, plans, last_len),
            9 => hash_blocks::<{ 9 * BLOCK_LEN }>(messages, key, flags, digests, platform, plans, last_len),
            10 => hash_blocks::<{ 10 * BLOCK_LEN }>(messages, key, flags, digests, platform, plans, last_len),
            11 => hash_blocks::<{ 11 * BLOCK_LEN }>(messages, key, flags, digests, platform, plans, last_len),
            12 => hash_blocks::<{ 12 * BLOCK_LEN }>(messages, key, flags, digests, platform, plans, last_len),
            13 => hash_blocks::<{ 13 * BLOCK_LEN }>(messages, key, flags, digests, platform, plans, last_len),
            14 => hash_blocks::<{ 14 * BLOCK_LEN }>(messages, key, flags, digests, platform, plans, last_len),
            15 => hash_blocks::<{ 15 * BLOCK_LEN }>(messages, key, flags, digests, platform, plans, last_len),
            16 => hash_blocks::<{ 16 * BLOCK_LEN }>(messages, key, flags, digests, platform, plans, last_len),
            _ => unreachable!("messages of up to a chunk"),
        }
    }
}

/// Fewest messages of 2 to 16 blocks that go to SME2 as one more group of
/// sixteen, its spare lanes reading the last message again: 10 in a batch
/// of fewer than sixteen, 5 left over after the SME2 groups (where NEON
/// work beside the SME2 kernels pays the SME unit's slow state as well).
/// Fewer run on the integer + NEON parent plans. ns per message, the plans
/// against the extra group: VM 256 B, 9 messages 60 / 69, 10 68 / 62, 12
/// 65 / 52; 128 B and 1 KiB cross at the same counts. Mac P-core (jobs
/// 261-264), 21 x 256 B 67.5 / 58.3, 53 x 256 B 51.1 / 45.5, 37 x 1 KiB
/// 201 / 193; the VM level at 21. With the program's work between calls,
/// 5 after the groups is best or level against 3 and 1 at nearly every
/// count (Mac, 64 B to 1 KiB, 16-129 messages: 1 KiB x 19 345 ns/msg
/// against 430 and 431); 3 and 1 win only back to back.
pub(crate) const SME2_TAIL_MIN: usize = 10;
pub(crate) const SME2_TAIL_MIN_AFTER_GROUPS: usize = 5;

/// Fewest messages of `len` bytes (whole blocks, 2 to 15 chunks) that SME2
/// hashes side by side as one group of sixteen, padded, by chunk count.
/// Fewer, alone or left over past whole groups, run through hash() one at
/// a time. The count at which a padded group's time equals that many
/// hash() calls, VM: 2 chunks 5.9, 3 8.4, 4 8.1, 6 9.5, 8 10.4, 12 11.4, 15
/// 11.6; Mac P-core (jobs 268-271) about the same or lower. Each entry is
/// the next count up.
const SME2_CHUNKED_MIN: [usize; 16] = [0, 0, 7, 9, 9, 10, 10, 11, 11, 12, 12, 12, 12, 12, 12, 12];

pub(crate) fn sme2_chunked_min(len: usize) -> usize {
    SME2_CHUNKED_MIN[len.div_ceil(CHUNK_LEN)]
}

/// Whether hash_many_on takes messages of `len` bytes side by side on
/// SME2: 2 to 15 chunks.
#[inline(always)]
fn chunked(len: usize) -> bool {
    cfg!(blake3_sme2) && (CHUNK_LEN + 1..=15 * CHUNK_LEN).contains(&len)
}

/// Messages of 2 to 15 chunks on SME2, sixteen side by side per group;
/// messages left over past whole groups make one more group, its spare
/// lanes repeating the last message, when there are sme2_chunked_min of
/// them, and otherwise run through hash() one at a time, first.
#[cfg(blake3_sme2)]
#[inline(never)]
fn hash_chunked(input: &[u8], len: usize, key: &CVWords, flags: u8, outputs: &mut [[u8; OUT_LEN]], platform: Platform) {
    const GROUP: usize = 16;
    let slot = slot_len(len);
    let left = outputs.len() % GROUP;
    // Past whole groups a group pays from two messages sooner: hash() calls
    // beside the SME2 kernels run in their slow state (VM, 22 x 2 KiB: 630
    // ns per message with six alone, 486 with a padded group).
    let min = if outputs.len() > GROUP { sme2_chunked_min(len) - 2 } else { sme2_chunked_min(len) };
    let alone = if left >= min { 0 } else { left };
    let grouped = outputs.len() - alone;
    let (groups, rest) = outputs.split_at_mut(grouped);
    for (i, output) in rest.iter_mut().enumerate() {
        *output = *crate::hash_serial_on(&input[(grouped + i) * slot..][..len], key, flags, platform).as_bytes();
    }
    for (g, digests) in groups.chunks_mut(GROUP).enumerate() {
        let mut lanes = [core::ptr::null::<u8>(); GROUP];
        for (i, lane) in lanes.iter_mut().enumerate() {
            let message = g * GROUP + i.min(digests.len() - 1);
            *lane = input[message * slot..][..slot].as_ptr();
        }
        // Sound: the caller found SME2 (platform_is_sme2), and every lane
        // points at a whole slot inside `input`, zero past its message.
        unsafe { crate::sme2::hash_chunked_messages(&lanes, len, key, flags, digests.as_flattened_mut()) };
    }
}

/// Messages of two chunks (1025 to 2048 bytes) side by side on the integer
/// + NEON parent plans, sixteen at a time: every message's first chunk in
/// one plan call at counter 0, every second chunk in another at counter 1,
/// then every root. hash() gives each message a NEON pair with a spare
/// lane (q1) or a pair alone (k2); side by side the plans fill their
/// lanes with the batch. VM, ns per message, one hash() each / side by
/// side: 2000 B x 2 1018 / 927, x 3 1036 / 653, x 4 1050 / 697, x 8 690
/// (SME2 from 7); 1100 B x 3 442 / 379. Commonware's four NEON lanes: 2000
/// B x 4 824 (Mac P-core 914 against servil's 1050 before, job 283).
#[cfg(blake3_neon)]
#[inline(never)]
fn hash_two_chunks(input: &[u8], len: usize, key: &CVWords, flags: u8, outputs: &mut [[u8; OUT_LEN]]) {
    use crate::PARENT;
    let slot = slot_len(len);
    let second = len - CHUNK_LEN;
    let second_blocks = second.div_ceil(BLOCK_LEN);
    let last_len = second - (second_blocks - 1) * BLOCK_LEN;
    // Sixteen at a time, as the plans take them: small arrays to zero.
    const GROUP: usize = 16;
    for (messages, digests) in input.chunks(slot * GROUP).zip(outputs.chunks_mut(GROUP)) {
        let count = digests.len();
        let mut firsts: [core::mem::MaybeUninit<&[u8; CHUNK_LEN]>; GROUP] = [core::mem::MaybeUninit::uninit(); GROUP];
        let mut seconds: [core::mem::MaybeUninit<*const u8>; GROUP] = [core::mem::MaybeUninit::uninit(); GROUP];
        for (i, message) in messages.chunks_exact(slot).enumerate() {
            firsts[i].write(message[..CHUNK_LEN].try_into().expect("a whole first chunk"));
            seconds[i].write(message[CHUNK_LEN..].as_ptr());
        }
        let mut cvs = [[0u8; OUT_LEN]; 2 * GROUP];
        let mut pairs = [[0u8; BLOCK_LEN]; GROUP];
        // Sound: the first `count` entries of both tables were written just
        // above; each second-chunk pointer reaches `second_blocks` whole
        // blocks inside its slot, zero past the message; NEON is every
        // AArch64 CPU's (neon_plans).
        unsafe {
            let firsts = core::slice::from_raw_parts(firsts.as_ptr() as *const &[u8; CHUNK_LEN], count);
            crate::neon_hybrid::hash_many_last_len(
                firsts,
                key,
                0,
                IncrementCounter::No,
                flags,
                CHUNK_START,
                CHUNK_END,
                BLOCK_LEN,
                cvs[..count].as_flattened_mut(),
            );
            crate::neon_hybrid::hash_messages_raw(
                seconds.as_ptr() as *const *const u8,
                count,
                second_blocks,
                key,
                1,
                flags,
                CHUNK_START,
                CHUNK_END,
                last_len,
                cvs[count..2 * count].as_flattened_mut(),
            );
        }
        for (i, pair) in pairs[..count].iter_mut().enumerate() {
            pair[..OUT_LEN].copy_from_slice(&cvs[i]);
            pair[OUT_LEN..].copy_from_slice(&cvs[count + i]);
        }
        let parents: arrayvec::ArrayVec<&[u8; BLOCK_LEN], GROUP> = pairs[..count].iter().collect();
        // Sound: as above.
        unsafe {
            crate::neon_hybrid::hash_many_last_len(&parents, key, 0, IncrementCounter::No, flags | PARENT, 0, ROOT, BLOCK_LEN, digests.as_flattened_mut())
        };
    }
}

/// Keep long-message tree work outside the short-message dispatcher's frame.
#[cfg(target_arch = "x86_64")]
#[inline(never)]
fn hash_long_or_single(input: &[u8], len: usize, key: &CVWords, flags: u8, outputs: &mut [[u8; OUT_LEN]], platform: Platform) {
    if across_from(len, platform).is_some_and(|least| outputs.len() >= least) {
        return hash_across(input, len, key, flags, outputs, platform);
    }
    let slot = slot_len(len);
    for (i, output) in outputs.iter_mut().enumerate() {
        *output = *crate::hash_serial_on(&input[i * slot..][..len], key, flags, platform).as_bytes();
    }
}

/// The chunks a message has below which [`hash_across`] takes it: from 16,
/// a message's own chunks fill the widest lanes alone (as each.rs's
/// ALONE_CHUNKS).
#[cfg(target_arch = "x86_64")]
const ACROSS_CHUNKS: usize = 16;

/// The fewest messages of `len` bytes that [`hash_across`] takes on
/// `platform` (AVX2 and AVX-512, messages of 2 to ACROSS_CHUNKS - 1
/// chunks): as many as the lanes, or 4 where each message's chunks leave
/// the lanes short and its own tree is a long chain (a partial chunk after
/// its whole ones, or two chunks). i7-12700K, six messages: 1500-6000 B
/// x0.58-0.84 across; 4 KiB x1.13, 8 KiB x1.55, 12-15 KiB x1.15-1.23
/// (their own lanes and parents ahead).
#[cfg(target_arch = "x86_64")]
pub(crate) fn across_from(len: usize, platform: Platform) -> Option<usize> {
    let chunks = len.div_ceil(CHUNK_LEN);
    if !(2..ACROSS_CHUNKS).contains(&chunks) || !takes_platform(platform) {
        return None;
    }
    let chain = chunks < platform.simd_degree() && (len % CHUNK_LEN != 0 || chunks == 2);
    Some(if chain { 4 } else { platform.simd_degree() })
}

/// Messages of one length, over a chunk and under ACROSS_CHUNKS chunks,
/// hashed across up to sixteen messages at every step of their tree (all
/// share one shape): each whole chunk index on the platform's hash_many;
/// the partial chunk's blocks but its last likewise, its last block for
/// each message alone (its chaining value so far is that message's own);
/// then each parent node, from the leaves up, across the messages, ROOT on
/// the top one. Every value on the stack: the batch allocates nothing.
#[cfg(target_arch = "x86_64")]
#[inline(never)]
fn hash_across(input: &[u8], len: usize, key: &CVWords, flags: u8, outputs: &mut [[u8; OUT_LEN]], platform: Platform) {
    let slot = slot_len(len);
    for (group, digests) in outputs.chunks_mut(ACROSS_GROUP).enumerate() {
        let messages: arrayvec::ArrayVec<&[u8], ACROSS_GROUP> = (0..digests.len()).map(|m| &input[(group * ACROSS_GROUP + m) * slot..][..len]).collect();
        hash_group_across(&messages, len, key, flags, digests, platform);
    }
}

/// The most messages [`hash_group_across`] takes at once: the widest lanes.
#[cfg(target_arch = "x86_64")]
pub(crate) const ACROSS_GROUP: usize = 16;

/// [`hash_across`] for up to ACROSS_GROUP messages of `len` bytes each,
/// wherever each lies (a batch's slots, or a queue's members).
#[cfg(target_arch = "x86_64")]
pub(crate) fn hash_group_across(messages: &[&[u8]], len: usize, key: &CVWords, flags: u8, digests: &mut [[u8; OUT_LEN]], platform: Platform) {
    const GROUP: usize = ACROSS_GROUP;
    let (full, chunks) = (len / CHUNK_LEN, len.div_ceil(CHUNK_LEN));
    assert!(full >= 1 && chunks >= 2 && chunks < ACROSS_CHUNKS, "messages of 2 chunks to under ACROSS_CHUNKS");
    assert!(
        messages.len() <= GROUP && messages.len() == digests.len() && messages.iter().all(|m| m.len() == len),
        "up to ACROSS_GROUP messages of len bytes, a digest each"
    );
    {
        let count = digests.len();
        let message = |m: usize| messages[m];
        // cvs[chunk][message]
        let mut cvs = [[[0u8; OUT_LEN]; GROUP]; ACROSS_CHUNKS];
        for k in 0..full {
            let refs: arrayvec::ArrayVec<&[u8; CHUNK_LEN], GROUP> = (0..count).map(|m| message(m)[k * CHUNK_LEN..][..CHUNK_LEN].try_into().unwrap()).collect();
            platform.hash_many(&refs, key, k as u64, IncrementCounter::No, flags, CHUNK_START, CHUNK_END, cvs[k][..count].as_flattened_mut());
        }
        if full < chunks {
            let partial = len - full * CHUNK_LEN;
            let before_last = (partial - 1) / BLOCK_LEN;
            let mut so_far = [[0u8; OUT_LEN]; GROUP];
            if before_last > 0 {
                // Sound: each pointer reaches before_last whole blocks of its message.
                let ptrs: arrayvec::ArrayVec<*const u8, GROUP> = (0..count).map(|m| message(m)[full * CHUNK_LEN..].as_ptr()).collect();
                unsafe { hash_many_blocks(platform, &ptrs, before_last, key, full as u64, false, flags, CHUNK_START, 0, so_far[..count].as_flattened_mut()) };
            }
            for m in 0..count {
                let mut cv = if before_last > 0 { crate::platform::words_from_le_bytes_32(&so_far[m]) } else { *key };
                let last = &message(m)[full * CHUNK_LEN + before_last * BLOCK_LEN..];
                let mut block = [0u8; BLOCK_LEN];
                block[..last.len()].copy_from_slice(last);
                let start = if before_last == 0 { CHUNK_START } else { 0 };
                platform.compress_in_place(&mut cv, &block, last.len() as u8, full as u64, flags | start | CHUNK_END);
                cvs[full][m] = crate::hasher::le_bytes(&cv);
            }
        }
        subtree_across(&mut cvs, 0, chunks, count, key, flags, platform, true);
        digests.copy_from_slice(&cvs[0][..count]);
    }
}

/// The chaining values (at the top, the root hashes) of the subtree of
/// chunks `first..first + n` of every message, into `cvs[first]`, from the
/// chunk values in `cvs` (per chunk, then message), each parent across the
/// messages.
#[cfg(target_arch = "x86_64")]
#[allow(clippy::too_many_arguments)]
fn subtree_across(
    cvs: &mut [[[u8; OUT_LEN]; 16]; ACROSS_CHUNKS],
    first: usize,
    n: usize,
    count: usize,
    key: &CVWords,
    flags: u8,
    platform: Platform,
    top: bool,
) {
    if n == 1 {
        return;
    }
    let split = crate::tree::left_len(n * CHUNK_LEN) / CHUNK_LEN;
    subtree_across(cvs, first, split, count, key, flags, platform, false);
    subtree_across(cvs, first + split, n - split, count, key, flags, platform, false);
    let mut blocks = [[0u8; BLOCK_LEN]; 16];
    for m in 0..count {
        blocks[m][..OUT_LEN].copy_from_slice(&cvs[first][m]);
        blocks[m][OUT_LEN..].copy_from_slice(&cvs[first + split][m]);
    }
    let lanes: arrayvec::ArrayVec<&[u8; BLOCK_LEN], 16> = blocks[..count].iter().collect();
    let parent_flags = flags | crate::PARENT | if top { ROOT } else { 0 };
    platform.hash_many(&lanes, key, 0, IncrementCounter::No, parent_flags, 0, 0, cvs[first][..count].as_flattened_mut());
}

/// Whether a batch of `count` messages of `len` bytes runs SME2 kernels
/// (so takes the SME2 turn): sixteen messages or more; one-block messages
/// from ONE_BLOCK_PAD_MIN; messages of up to a chunk from SME2_TAIL_MIN;
/// messages of 2 to 15 chunks from sme2_chunked_min; and any message long
/// enough for hash() to take the turn itself.
pub(crate) fn sme2_sized(len: usize, count: usize) -> bool {
    count >= crate::SME2_SIZED_BATCH
        || (len > 0 && len <= BLOCK_LEN && count >= ONE_BLOCK_PAD_MIN)
        || (len > BLOCK_LEN && len <= CHUNK_LEN && count >= SME2_TAIL_MIN)
        || (chunked(len) && count >= sme2_chunked_min(len))
        || (len >= crate::SME2_SIZED_LEN && count >= 1)
}

/// Up to TABLE one-block messages (N = BLOCK_LEN, whole blocks) in
/// `messages`: whole SME2 groups on the message kernel, the last padded,
/// where the count calls for it; otherwise the platform's `hash_many` (the
/// parent kernels and the NEON plans). Kept out of line:
/// with all sixteen lengths inlined into hash_many_on, batches of two
/// 64-byte messages took 30% longer (VM, bench-hashes); out of line they
/// cost what they did before.
#[inline(never)]
fn hash_run<const N: usize>(messages: &[u8], key: &CVWords, flags: u8, outputs: &mut [[u8; OUT_LEN]], platform: Platform) {
    #[cfg(blake3_sme2)]
    if platform_is_sme2(platform) && outputs.len() % 16 >= if outputs.len() > 16 { ONE_BLOCK_PAD_AFTER_GROUPS } else { ONE_BLOCK_PAD_MIN } {
        return hash_run_padded::<N>(messages, key, flags, outputs, BLOCK_LEN);
    }
    let mut table: [core::mem::MaybeUninit<&[u8; N]>; TABLE] = [core::mem::MaybeUninit::uninit(); TABLE];
    for (slot, message) in table[..outputs.len()].iter_mut().zip(messages.chunks_exact(N)) {
        slot.write(message.try_into().expect("messages of N bytes"));
    }
    // Sound: the first outputs.len() slots were written just above.
    let filled: &[&[u8; N]] = unsafe { core::slice::from_raw_parts(table.as_ptr() as *const &[u8; N], outputs.len()) };
    platform.hash_many::<N>(filled, key, 0, IncrementCounter::No, flags | CHUNK_START | CHUNK_END | ROOT, 0, 0, outputs.as_flattened_mut());
}

/// One-block messages on SME2, every group on the message kernel, the
/// last one padded, their last block `last_len` bytes.
#[cfg(blake3_sme2)]
#[inline(never)]
fn hash_run_padded<const N: usize>(messages: &[u8], key: &CVWords, flags: u8, outputs: &mut [[u8; OUT_LEN]], last_len: usize) {
    let mut slots: Slots<N> = [core::mem::MaybeUninit::uninit(); TABLE + 16];
    let lanes = fill_table(&mut slots, messages, outputs.len(), outputs.len().next_multiple_of(16));
    // Sound: the caller found SME2 (platform_is_sme2).
    unsafe { crate::sme2::hash_messages::<N>(lanes, key, flags, CHUNK_START, CHUNK_END | ROOT, last_len, outputs.as_flattened_mut()) };
}

/// [`hash_run`] for messages of 1 to 63 bytes (one short block): on SME2
/// the message kernel from ONE_BLOCK_PAD_MIN messages (the parent kernel
/// records 64 for every block), else the NEON plans, or one call each
/// without them.
#[inline(never)]
fn hash_run_short<const N: usize>(messages: &[u8], key: &CVWords, flags: u8, outputs: &mut [[u8; OUT_LEN]], platform: Platform, plans: bool, last_len: usize) {
    #[cfg(blake3_sme2)]
    if platform_is_sme2(platform) && outputs.len() >= ONE_BLOCK_PAD_MIN {
        return hash_run_padded::<N>(messages, key, flags, outputs, last_len);
    }
    if plans {
        let mut slots: Slots<N> = [core::mem::MaybeUninit::uninit(); TABLE + 16];
        let lanes = fill_table(&mut slots, messages, outputs.len(), outputs.len());
        neon_plans_last_len::<N>(lanes, key, flags, last_len, outputs);
    } else {
        for (output, message) in outputs.iter_mut().zip(messages.chunks_exact(N)) {
            *output = *crate::hash_serial_on(&message[..last_len], key, flags, platform).as_bytes();
        }
    }
}

/// Fewest one-block messages that SME2 hashes as whole groups on the
/// message kernel, the last one padded: 11 in a batch of fewer than
/// sixteen (fewer run on the NEON parent plans), 5 left over past whole
/// groups (fewer left over run on the NEON plans). VM, ns per message,
/// before / padded: 10 18.3 / 20.9, 11 19.1 / 15.0, 15 18.2 / 10.8.
/// Past whole groups the padded group replaces the plans and the overlap
/// group. The plans after a group run fast while the SME unit stays in
/// its fast state and 2.5x slower when it sits in its slow one; the
/// padded group runs at one speed. Mac M4 Max P-cores, ns per message,
/// left-overs from 13 / from 5: in a sweep where the unit sat slow at
/// 20 and 22-28 messages (probe onepad-after5, jobs 279-281), 22-28
/// 25.5-31.5 / 11.4-15.7 and 53 18.4 / 12.2; slower at 21 (13.6 / 15.6)
/// and 37-40 (10.1-11.8 / 11.9-13.0). In the benchmark, every sample
/// fast (jobs 291-294), 24 10.6 / 13.3 (official 21.0-21.7). The VM, in
/// the plans' fast state, 22, 24, 26 12-16% slower (their slow state is
/// twice as slow). The worst case halves; the fast case pays up to 25%.
pub(crate) const ONE_BLOCK_PAD_MIN: usize = 11;
#[cfg_attr(not(blake3_sme2), allow(dead_code))]
pub(crate) const ONE_BLOCK_PAD_AFTER_GROUPS: usize = 5;

/// Room for a pointer table: TABLE messages and a padded group's spare
/// lanes, uninitialised on the caller's stack.
type Slots<'a, const N: usize> = [core::mem::MaybeUninit<&'a [u8; N]>; TABLE + 16];

/// Fill `slots` with pointers to the first `count` slots of N bytes in
/// `messages`, then spare lanes repeating the last up to `lanes`; the
/// table's filled part.
#[inline(always)]
fn fill_table<'a, 's, const N: usize>(slots: &'s mut Slots<'a, N>, messages: &'a [u8], count: usize, lanes: usize) -> &'s [&'a [u8; N]] {
    assert!(0 < count && count <= lanes && lanes <= TABLE + 16 && messages.len() >= count * N, "a table of 1 to TABLE + 16 lanes over the messages");
    let mut last: &[u8; N] = &[0; N];
    for (slot, message) in slots[..count].iter_mut().zip(messages.chunks_exact(N)) {
        last = message.try_into().expect("slots of N bytes");
        slot.write(last);
    }
    for slot in &mut slots[count..lanes] {
        slot.write(last);
    }
    // Sound: the first `lanes` slots were written just above.
    unsafe { core::slice::from_raw_parts(slots.as_ptr() as *const &[u8; N], lanes) }
}

/// The integer + NEON parent plans over `lanes`, each message's last block
/// `last_len` bytes.
#[inline(always)]
fn neon_plans_last_len<const N: usize>(lanes: &[&[u8; N]], key: &CVWords, flags: u8, last_len: usize, outputs: &mut [[u8; OUT_LEN]]) {
    #[cfg(blake3_neon)]
    // Sound: every AArch64 CPU has NEON.
    unsafe {
        crate::neon_hybrid::hash_many_last_len(lanes, key, 0, IncrementCounter::No, flags, CHUNK_START, CHUNK_END | ROOT, last_len, outputs.as_flattened_mut())
    };
    #[cfg(not(blake3_neon))]
    {
        let _ = (lanes, key, flags, last_len, outputs);
        unreachable!("the NEON plans exist on AArch64 alone");
    }
}

/// [`hash_run`] for messages of 2 to 16 blocks (N), the last block
/// `last_len` bytes, on one of four plans:
///
/// - SME2, SME2_TAIL_MIN or more messages in all (below sixteen) or
///   SME2_TAIL_MIN_AFTER_GROUPS or more past the groups of sixteen: whole
///   groups, the last one padded, its spare lanes pointing at the last
///   message again (no bytes move; the kernel computes every lane anyway
///   and stores the real ones alone).
/// - SME2 with fewer left over past the groups: those on the integer +
///   NEON plans first, then the groups (NEON work right after the SME2
///   kernels runs in the SME unit's slow state on the VM: 17 x 256 B, 68
///   ns per message after, 44 before).
/// - The integer + NEON plans (no SME2, or fewer messages): all at once.
/// - Elsewhere (`plans` false: x86-64), whole last blocks:
///   four at a time, a fourth, spare lane for three left over, one call
///   each for one or two; with a short last block, one call each.
#[inline(never)]
fn hash_blocks<const N: usize>(messages: &[u8], key: &CVWords, flags: u8, outputs: &mut [[u8; OUT_LEN]], platform: Platform, plans: bool, last_len: usize) {
    const GROUP: usize = 16;
    let count = outputs.len();
    if count == 0 {
        return;
    }
    let sme2 = platform_is_sme2(platform);
    let left = count % GROUP;
    let padded = sme2 && left >= if count > GROUP { SME2_TAIL_MIN_AFTER_GROUPS } else { SME2_TAIL_MIN };
    // Messages hashed on SME2 (the first `on_sme2`), and the table's lanes.
    let on_sme2 = if padded {
        count
    } else if sme2 {
        count - left
    } else {
        0
    };
    let lanes = if padded { count.next_multiple_of(GROUP) } else { count };
    let mut slots: Slots<N> = [core::mem::MaybeUninit::uninit(); TABLE + 16];
    let table = fill_table(&mut slots, messages, count, lanes);
    let (groups, rest) = outputs.split_at_mut(on_sme2);
    if !rest.is_empty() {
        let rest_lanes = &table[on_sme2..count];
        if plans {
            neon_plans_last_len::<N>(rest_lanes, key, flags, last_len, rest);
        } else if last_len == BLOCK_LEN {
            hash_four_at_a_time::<N>(rest_lanes, key, flags, rest, platform);
        } else {
            for (output, message) in rest.iter_mut().zip(rest_lanes) {
                *output = *crate::hash_serial_on(&message[..(N - BLOCK_LEN) + last_len], key, flags, platform).as_bytes();
            }
        }
    }
    #[cfg(blake3_sme2)]
    if on_sme2 > 0 {
        // Sound: platform_is_sme2 means detect() found SME2 with 512-bit
        // streaming vectors.
        let sme2_lanes = &table[..if padded { lanes } else { on_sme2 }];
        unsafe { crate::sme2::hash_messages::<N>(sme2_lanes, key, flags, CHUNK_START, CHUNK_END | ROOT, last_len, groups.as_flattened_mut()) };
    }
    #[cfg(not(blake3_sme2))]
    let _ = groups;
}

/// The arrangement for messages of whole blocks where the NEON plans are
/// absent: groups of four through the platform's `hash_many`, a fourth,
/// spare lane for three left over (its value kept in a scratch array), one
/// call each for one or two.
#[inline(never)]
fn hash_four_at_a_time<const N: usize>(lanes: &[&[u8; N]], key: &CVWords, flags: u8, outputs: &mut [[u8; OUT_LEN]], platform: Platform) {
    let (start, end) = (CHUNK_START, CHUNK_END | ROOT);
    // Keep the padded fourth lane for remainders of three. For other
    // x86 tails, the existing platform kernel avoids separate scalar hashes.
    if cfg!(target_arch = "x86_64") && lanes.len() % 4 != 3 {
        platform.hash_many::<N>(lanes, key, 0, IncrementCounter::No, flags, start, end, outputs.as_flattened_mut());
        return;
    }
    let fours = lanes.len() / 4 * 4;
    if fours > 0 {
        platform.hash_many::<N>(&lanes[..fours], key, 0, IncrementCounter::No, flags, start, end, outputs[..fours].as_flattened_mut());
    }
    match lanes.len() - fours {
        3 => {
            let spare = [lanes[fours], lanes[fours + 1], lanes[fours + 2], lanes[fours + 2]];
            let mut digests = [[0u8; OUT_LEN]; 4];
            platform.hash_many::<N>(&spare, key, 0, IncrementCounter::No, flags, start, end, digests.as_flattened_mut());
            outputs[fours..].copy_from_slice(&digests[..3]);
        }
        _ => {
            for (output, message) in outputs[fours..].iter_mut().zip(&lanes[fours..]) {
                *output = *crate::hash_serial_on(&message[..], key, flags, platform).as_bytes();
            }
        }
    }
}

/// Whether this build runs the integer + NEON kernels (hash_many's plans
/// for messages of whole blocks at one counter): every AArch64 CPU does,
/// with SHA-3 or without.
#[inline(always)]
fn neon_plans() -> bool {
    cfg!(blake3_neon)
}

#[inline(always)]
fn platform_is_sme2(platform: Platform) -> bool {
    #[cfg(blake3_sme2)]
    return matches!(platform, Platform::SME2);
    #[cfg(not(blake3_sme2))]
    {
        let _ = platform;
        false
    }
}

// The AVX2 and AVX-512 assembly's hash_many at a block count chosen at
// run time, for the batches across messages above.

/// Whether [`hash_many_blocks`] runs on `platform`: AVX2 or AVX-512, whose
/// assembly is in this build.
#[inline]
#[cfg(target_arch = "x86_64")]
#[cfg_attr(not(any(blake3_avx2_ffi, blake3_avx512_ffi)), allow(unused_variables, unreachable_code))]
fn takes_platform(platform: Platform) -> bool {
    match platform {
        #[cfg(blake3_avx2_ffi)]
        Platform::AVX2 => true,
        #[cfg(blake3_avx512_ffi)]
        Platform::AVX512 => true,
        _ => false,
    }
}

/// The platform's assembly `hash_many`, with the block count at run time
/// (AVX2 and AVX-512; elsewhere each input in turn). Unsafe: each of `inputs` reaches `blocks`
/// blocks, and the CPU has the platform.
#[allow(clippy::too_many_arguments)]
#[cfg(target_arch = "x86_64")]
#[cfg_attr(not(any(blake3_avx2_ffi, blake3_avx512_ffi)), allow(unused_variables, unreachable_code))]
unsafe fn hash_many_blocks(
    platform: Platform,
    inputs: &[*const u8],
    blocks: usize,
    key: &CVWords,
    counter: u64,
    increment: bool,
    flags: u8,
    flags_start: u8,
    flags_end: u8,
    out: &mut [u8],
) {
    assert!(out.len() >= inputs.len() * OUT_LEN && (1..=CHUNK_LEN / BLOCK_LEN).contains(&blocks));
    if !takes_platform(platform) {
        // Elsewhere one input after another on the platform's compression
        // (for the tests of hash_many's batches on every platform).
        for (i, &p) in inputs.iter().enumerate() {
            let mut cv = *key;
            for b in 0..blocks {
                // Sound: `p` reaches `blocks` blocks.
                let block: &[u8; BLOCK_LEN] = unsafe { &*(p.add(b * BLOCK_LEN) as *const [u8; BLOCK_LEN]) };
                let f = flags | if b == 0 { flags_start } else { 0 } | if b + 1 == blocks { flags_end } else { 0 };
                platform.compress_in_place(&mut cv, block, BLOCK_LEN as u8, counter + if increment { i as u64 } else { 0 }, f);
            }
            out[i * OUT_LEN..][..OUT_LEN].copy_from_slice(&crate::hasher::le_bytes(&cv));
        }
        return;
    }
    let (n, input, key, out) = (inputs.len(), inputs.as_ptr(), key.as_ptr(), out.as_mut_ptr());
    match platform {
        #[cfg(blake3_avx2_ffi)]
        Platform::AVX2 => unsafe {
            crate::avx2::ffi::blake3_servil_hash_many_avx2(input, n, blocks, key, counter, increment, flags, flags_start, flags_end, out)
        },
        #[cfg(blake3_avx512_ffi)]
        Platform::AVX512 => unsafe {
            crate::avx512::ffi::blake3_servil_hash_many_avx512(input, n, blocks, key, counter, increment, flags, flags_start, flags_end, out)
        },
        _ => unreachable!("takes_platform admitted AVX2 or AVX-512"),
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use std::vec::Vec;

    /// `count` deterministic messages of `len` bytes, each in its slot
    /// (slot_len), zero past its end: little-endian 64-bit words `len << 48
    /// | index`, so every block differs and a kernel that mixed up its lanes
    /// would be caught.
    fn messages(len: usize, count: usize) -> Vec<u8> {
        let slot = slot_len(len);
        let mut bytes: Vec<u8> = (0..(slot * count).div_ceil(8) as u64).flat_map(|i| ((len as u64) << 48 | i).to_le_bytes()).collect();
        bytes.truncate(slot * count);
        for message in bytes.chunks_exact_mut(slot) {
            message[len..].fill(0);
        }
        bytes
    }

    /// Message `i` of a batch of `len`-byte messages.
    fn message(input: &[u8], len: usize, i: usize) -> &[u8] {
        &input[i * slot_len(len)..][..len]
    }

    /// hash_many (and hash_many_multithreaded) of `count` messages of
    /// `len` bytes against hash() one message at a time.
    fn check(len: usize, count: usize) {
        let input = messages(len, count);
        let mut out = vec![[0u8; OUT_LEN]; count];
        crate::performance::hash_many(crate::Mode::Plain, &crate::performance::Batch::in_place(&crate::test::blocks(&input), len), &mut out);
        for (i, digest) in out.iter().enumerate() {
            assert_eq!(*digest, *crate::convenience::hash(message(&input, len, i)).as_bytes(), "message {i} of {count}, {len} bytes");
        }
        #[cfg(feature = "std")]
        {
            let mut mt = vec![[0u8; OUT_LEN]; count];
            crate::performance::hash_many_multithreaded(crate::Mode::Plain, &crate::performance::Batch::in_place(&crate::test::blocks(&input), len), &mut mt);
            assert_eq!(mt, out, "multithreaded, {count} messages of {len} bytes");
        }
    }

    /// Messages of 2 to 16 whole blocks, at counts that fill SME2 groups
    /// and leave remainders.
    #[test]
    fn test_hash_many_whole_blocks() {
        for blocks in 2..=16 {
            for count in [0, 1, 2, 3, 4, 5, 6, 7, 15, 16, 17, 18, 31, 32, 33, 127, 128, 129, 130, 300] {
                check(blocks * BLOCK_LEN, count);
            }
        }
    }

    /// Whole-block x86 batches on every available SIMD implementation,
    /// including tails and table boundaries, against fixed values (official
    /// BLAKE3's digests) in all three modes, with output sentinels.
    #[cfg(all(feature = "std", target_arch = "x86_64"))]
    #[test]
    fn test_whole_blocks_x86_full_width() {
        #[allow(unused_mut)]
        let mut platforms: Vec<_> = [Platform::sse41(), Platform::avx2()].into_iter().flatten().collect();
        #[cfg(blake3_avx512_ffi)]
        platforms.extend(Platform::avx512());
        let key = [0x5Au8; crate::KEY_LEN];
        let context = "Linux whole-block batch dispatch";
        let context_key = crate::performance::context_key(context);
        let modes = [
            (*crate::IV, 0),
            (crate::platform::words_from_le_bytes_32(&key), crate::KEYED_HASH),
            (crate::platform::words_from_le_bytes_32(&context_key), crate::DERIVE_KEY_MATERIAL),
        ];
        let api_modes = [crate::Mode::Plain, crate::Mode::Keyed(&key), crate::Mode::DeriveKey(context)];
        let mut golden = crate::test::Golden::new("whole_blocks_x86_full_width");
        for len in [128, 256, 512, CHUNK_LEN, 2047, 2048, 2049, 4095, 4096, 4097] {
            for count in (0..=33).chain([63, 64, 127, 128, 129, 255]) {
                let bytes = messages(len, count);
                // The digests, once per mode, one message at a time, against
                // the fixed values: the input's alignment below changes
                // nothing about them.
                let wants: Vec<Vec<[u8; OUT_LEN]>> = api_modes
                    .iter()
                    .map(|&mode| (0..count).map(|i| *crate::convenience::hash_with(mode, message(&bytes, len, i)).as_bytes()).collect())
                    .collect();
                for (&mode, want) in api_modes.iter().zip(&wants) {
                    golden.add(want.as_flattened(), || (0..count).flat_map(|i| crate::test::official::hash(mode, message(&bytes, len, i))).collect());
                }
                // Four alignments of the input (a wrong pointer step shows
                // at any of them): 64 took the suite from 2 s to 16 s.
                for offset in [0, 1, 7, 63] {
                    let mut allocation = vec![0; offset + bytes.len()];
                    allocation[offset..].copy_from_slice(&bytes);
                    let input = &allocation[offset..];
                    for ((key_words, flags), want) in modes.iter().zip(&wants) {
                        for &platform in &platforms {
                            let mut out = vec![[0xAA; OUT_LEN]; count + 1];
                            hash_many_on(input, len, key_words, *flags, &mut out[..count], platform);
                            assert_eq!(&out[..count], want, "{platform:?}, {count} x {len}, offset {offset}, flags {flags}");
                            assert_eq!(out[count], [0xAA; OUT_LEN]);
                        }
                    }
                }
            }
        }
        golden.check(0xba67a57a036242e7);
    }

    /// hash_across with partial chunks (1025, 3073, 7169 bytes: one whole
    /// chunk and a byte, three and a byte, seven and a byte), on every
    /// platform, against the published vectors
    /// (test_vectors/test_vectors.json, byte i = i % 251), each message in
    /// its zero-padded slot.
    #[cfg(all(feature = "std", target_arch = "x86_64"))]
    #[test]
    fn across_partial_chunks_matches_published_vectors() {
        let key = b"whats the Elvish word for friend";
        let context = "BLAKE3 2019-12-27 16:29:52 test vectors context";
        let context_key = crate::performance::context_key(context);
        let modes = [
            (*crate::IV, 0),
            (crate::platform::words_from_le_bytes_32(key), crate::KEYED_HASH),
            (crate::platform::words_from_le_bytes_32(&context_key), crate::DERIVE_KEY_MATERIAL),
        ];
        let cases = [
            (
                1025,
                [
                    "d00278ae47eb27b34faecf67b4fe263f82d5412916c1ffd97c8cb7fb814b8444",
                    "357dc55de0c7e382c900fd6e320acc04146be01db6a8ce7210b7189bd664ea69",
                    "effaa245f065fbf82ac186839a249707c3bddf6d3fdda22d1b95a3c970379bcb",
                ],
            ),
            (
                3073,
                [
                    "7124b49501012f81cc7f11ca069ec9226cecb8a2c850cfe644e327d22d3e1cd3",
                    "68dede9bef00ba89e43f31a6825f4cf433389fedae75c04ee9f0cf16a427c95a",
                    "72613c9ec9ff7e40f8f5c173784c532ad852e827dba2bf85b2ab4b76f7079081",
                ],
            ),
            (
                7169,
                [
                    "a003fc7a51754a9b3c7fae0367ab3d782dccf28855a03d435f8cfe74605e7817",
                    "ed9b1a922c046fdb3d423ae34e143b05ca1bf28b710432857bf738bcedbfa511",
                    "554b0a5efea9ef183f2f9b931b7497995d9eb26f5c5c6dad2b97d62fc5ac31d9",
                ],
            ),
        ];
        for (len, anchors) in cases {
            let slot = slot_len(len);
            for count in &[4usize, 17] {
                let mut input = vec![0u8; slot * count];
                for m in 0..*count {
                    for (i, b) in input[m * slot..][..len].iter_mut().enumerate() {
                        *b = (i % 251) as u8;
                    }
                }
                for ((words, flags), anchor) in modes.iter().zip(anchors) {
                    let expected: [u8; OUT_LEN] = hex::decode(anchor).unwrap().try_into().unwrap();
                    for platform in crate::test::every_platform() {
                        let mut out = vec![[0u8; OUT_LEN]; *count];
                        hash_across(&input, len, words, *flags, &mut out, platform);
                        assert!(out.iter().all(|digest| *digest == expected), "{platform:?}: {count} x {len} B");
                    }
                }
            }
        }
    }

    /// Published anchors exercise the two-chunk helper on every platform.
    #[cfg(all(feature = "std", target_arch = "x86_64"))]
    #[test]
    fn two_chunk_batch_matches_published_vectors() {
        let key = b"whats the Elvish word for friend";
        let context = "BLAKE3 2019-12-27 16:29:52 test vectors context";
        let context_key = crate::performance::context_key(context);
        let modes = [
            (*crate::IV, 0),
            (crate::platform::words_from_le_bytes_32(key), crate::KEYED_HASH),
            (crate::platform::words_from_le_bytes_32(&context_key), crate::DERIVE_KEY_MATERIAL),
        ];
        // test_vectors/test_vectors.json: 2048 bytes, byte i = i % 251.
        let anchors = [
            "e776b6028c7cd22a4d0ba182a8bf62205d2ef576467e838ed6f2529b85fba24a",
            "879cf1fa2ea0e79126cb1063617a05b6ad9d0b696d0d757cf053439f60a99dd1",
            "7b2945cb4fef70885cc5d78a87bf6f6207dd901ff239201351ffac04e1088a23",
        ];
        let message: Vec<u8> = (0..2 * CHUNK_LEN).map(|i| (i % 251) as u8).collect();
        for count in [4, 17] {
            let input = message.repeat(count);
            for ((words, flags), anchor) in modes.iter().zip(anchors) {
                let expected: [u8; OUT_LEN] = hex::decode(anchor).unwrap().try_into().unwrap();
                for platform in crate::test::every_platform() {
                    let mut out = vec![[0u8; OUT_LEN]; count];
                    hash_across(&input, 2 * CHUNK_LEN, words, *flags, &mut out, platform);
                    assert!(out.iter().all(|digest| *digest == expected), "{platform:?}");
                }
            }
        }
    }

    #[cfg(all(feature = "std", target_arch = "x86_64"))]
    #[test]
    fn four_chunk_batch_matches_published_vectors() {
        let key = b"whats the Elvish word for friend";
        let context = "BLAKE3 2019-12-27 16:29:52 test vectors context";
        let context_key = crate::performance::context_key(context);
        let modes = [
            (*crate::IV, 0),
            (crate::platform::words_from_le_bytes_32(key), crate::KEYED_HASH),
            (crate::platform::words_from_le_bytes_32(&context_key), crate::DERIVE_KEY_MATERIAL),
        ];
        // Published test_vectors/test_vectors.json, 4096 bytes, i % 251.
        let anchors = [
            "015094013f57a5277b59d8475c0501042c0b642e531b0a1c8f58d2163229e969",
            "befc660aea2f1718884cd8deb9902811d332f4fc4a38cf7c7300d597a081bfc0",
            "1e0d7f3db8c414c97c6307cbda6cd27ac3b030949da8e23be1a1a924ad2f25b9",
        ];
        let message: Vec<u8> = (0..4 * CHUNK_LEN).map(|i| (i % 251) as u8).collect();
        for count in [4, 17] {
            let input = message.repeat(count);
            for ((words, flags), anchor) in modes.iter().zip(anchors) {
                let expected: [u8; OUT_LEN] = hex::decode(anchor).unwrap().try_into().unwrap();
                for platform in crate::test::every_platform() {
                    let mut out = vec![[0u8; OUT_LEN]; count];
                    hash_across(&input, 4 * CHUNK_LEN, words, *flags, &mut out, platform);
                    assert!(out.iter().all(|digest| *digest == expected), "{platform:?}");
                }
            }
        }
    }

    /// The arrangement for whole blocks where the NEON plans are absent
    /// (x86-64: groups of four, a spare fourth lane for three
    /// left over, one call each for one or two), run here with `plans`
    /// false on every platform this CPU has, so the AArch64 tests and
    /// mutants reach it too.
    /// Keep: it detects, by hanging, mutants the clock then stops
    /// (tools/mutants/timeouts-x86_64.txt).
    #[test]
    fn test_hash_blocks_without_plans() {
        #[allow(unused_mut)]
        let mut platforms = vec![Platform::detect()];
        #[cfg(blake3_neon)]
        platforms.push(Platform::neon().expect("NEON on AArch64"));
        for count in 0..=40 {
            let input = messages(4 * BLOCK_LEN, count);
            for &platform in &platforms {
                let mut out = vec![[0xAAu8; OUT_LEN]; count + 16];
                hash_blocks::<{ 4 * BLOCK_LEN }>(&input, crate::IV, 0, &mut out[..count], platform, false, BLOCK_LEN);
                assert!(out[count..].iter().all(|d| *d == [0xAA; OUT_LEN]), "{platform:?}, {count}: a store past the last output");
                for (i, digest) in out[..count].iter().enumerate() {
                    assert_eq!(*digest, *crate::convenience::hash(message(&input, 256, i)).as_bytes(), "{platform:?}, message {i} of {count}");
                }
            }
        }
    }

    /// The keyed and derive-key modes through every batch path on every
    /// platform this CPU has (one-block runs, short blocks, whole blocks,
    /// two chunks, 2-15 chunks, longer messages),
    /// against keyed_hash() and derive_key() one message at a time.
    #[test]
    fn test_hash_many_modes_every_platform() {
        #[allow(unused_mut)]
        let mut platforms = vec![Platform::detect()];
        #[cfg(blake3_neon)]
        platforms.push(Platform::neon().expect("NEON on AArch64"));
        let key = [0x5Au8; crate::KEY_LEN];
        let context = "blake3-servil many modes test";
        let context_key = crate::performance::context_key(context);
        let modes: [(CVWords, u8, &dyn Fn(&[u8]) -> [u8; OUT_LEN]); 2] = [
            (crate::platform::words_from_le_bytes_32(&key), crate::KEYED_HASH, &|m| *crate::convenience::hash_with(crate::Mode::Keyed(&key), m).as_bytes()),
            (crate::platform::words_from_le_bytes_32(&context_key), crate::DERIVE_KEY_MATERIAL, &|m| {
                *crate::convenience::hash_with(crate::Mode::DeriveKey(context), m).as_bytes()
            }),
        ];
        for len in [0, 1, 63, BLOCK_LEN, 100, 256, 1000, CHUNK_LEN, CHUNK_LEN + 1, 2000, 2 * CHUNK_LEN, 4000, 15 * CHUNK_LEN, 16 * CHUNK_LEN + 5] {
            for count in [1, 2, 3, 4, 5, 10, 11, 12, 16, 17, 21, 33, 129] {
                let input = messages(len, count);
                for (key_words, flags, one) in &modes {
                    let want: Vec<[u8; OUT_LEN]> = (0..count).map(|i| one(message(&input, len, i))).collect();
                    for &platform in &platforms {
                        let mut out = vec![[0u8; OUT_LEN]; count];
                        hash_many_on(&input, len, key_words, *flags, &mut out, platform);
                        assert_eq!(out, want, "{platform:?}, flags {flags}, {count} x {len} B");
                    }
                }
            }
        }
    }

    /// Batches of 1 to 17 messages of up to a block after a pause (the pair
    /// kernel's path for 2 to 8, prefetched plans beyond), every mode: each
    /// digest is hash_with's.
    #[cfg(feature = "std")]
    #[test]
    fn test_small_batches_after_a_pause() {
        let key = [7u8; 32];
        for mode in [crate::Mode::Plain, crate::Mode::Keyed(&key), crate::Mode::DeriveKey("after a pause")] {
            for len in [0, 1, 31, 63, 64] {
                for count in 1..=17 {
                    let slot = slot_len(len);
                    let mut input = vec![0u8; slot * count];
                    for (k, s) in input.chunks_mut(slot).enumerate() {
                        for (j, b) in s[..len].iter_mut().enumerate() {
                            *b = (k * 31 + j) as u8;
                        }
                    }
                    std::thread::sleep(std::time::Duration::from_micros(300));
                    let mut out = vec![[0u8; OUT_LEN]; count];
                    crate::performance::hash_many(mode, &crate::performance::Batch::in_place(&crate::test::blocks(&input), len), &mut out);
                    for (k, digest) in out.iter().enumerate() {
                        assert_eq!(digest, crate::convenience::hash_with(mode, &input[k * slot..][..len]).as_bytes(), "len {len} count {count} message {k}");
                    }
                }
            }
        }
    }

    /// The length check holds in every build, inside the crate too: it
    /// stands before the kernels, which would read a short input past its
    /// end.
    #[test]
    #[should_panic(expected = "input holds one slot of whole blocks per output")]
    fn test_a_short_input_is_refused_before_the_kernels() {
        crate::initialize();
        hash_many_on(&[0u8; 64], 64, crate::IV, 0, &mut [[0u8; OUT_LEN]; 2], crate::platform::Platform::detect());
    }

    #[test]
    #[should_panic(expected = "input holds one slot of whole blocks per output")]
    fn test_hash_many_needs_the_input_it_names() {
        crate::performance::hash_many(
            crate::Mode::Plain,
            &crate::performance::Batch::from_blocks(&[crate::performance::Block::ZERO], 64),
            &mut [[0u8; OUT_LEN]; 2],
        );
    }
}
