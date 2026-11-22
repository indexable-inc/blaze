//! Many independent inputs' digests: the checked entry ([`many_checked`], a count mismatch a
//! typed error) and the streaming form ([`Many`], a fixed-capacity batcher a caller pushes
//! inputs into as it makes them).
//!
//! Both run the crate's one side-by-side path, [`crate::many`] (what [`crate::hash_many`],
//! [`crate::keyed_hash_many`] and [`crate::derive_key_many`] run): an input of at most a chunk
//! is one SIMD lane, compressed a block a step, each lane with its own chunk state (the BLAKE3
//! paper, section 2.4 "Chunk state" and 2.5 "Root node": counter 0, `CHUNK_START` on its first
//! block, `CHUNK_END | ROOT` on its last, the last block's length and zero padding); a lane
//! takes the next input the step after its last block, so inputs of different block counts
//! never wait for each other and nothing is sorted or grouped by length first. An input longer
//! than a chunk is hashed one shot where it comes (the hasher's tree path), unchanged here.
//!
//! Which vector code runs is the crate's existing dispatch, not a new one: the lane compression
//! is [`crate::lanes::compress_each`], one `core::simd` body. On aarch64 it compiles at the
//! baseline to NEON (NEON is baseline there: no run-time detection), 8 lanes as two 128-bit
//! registers a word; on x86-64 to AVX-512F at 16 lanes or AVX2 at 8 where the CPU has them,
//! else SSE2. The same body is the portable path everywhere else and under Miri (`core::simd`
//! has no unsafe in this crate, which denies `unsafe_code`). No hand-written NEON intrinsics
//! are added beside it: they would be a second body of one compression (AGENTS.md rule 30) and
//! the first unsafe in the crate, for what the compiler already emits from the `core::simd`
//! one.
//!
//! No allocation: a batch's scratch is fixed-size arrays on the stack; no recursion.

use core::fmt;

use crate::{DERIVE_KEY_MATERIAL, Engine, IV, KEYED_HASH, OUT_LEN, many, many_on, words_of};

/// The BLAKE3 mode a batch hashes in, its key with it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// [`crate::hash`]: the plain mode.
    Hash,
    /// [`crate::keyed_hash`] under this key.
    Keyed([u8; OUT_LEN]),
    /// [`crate::derive_key_with`] under this context key ([`crate::context_key`] of the
    /// context string, derived once by the caller).
    DeriveKey([u8; OUT_LEN]),
}

impl Mode {
    /// The key words and the mode's flag a compression of this mode starts from (the BLAKE3
    /// paper, section 2.3 "Modes": the IV for the plain mode, the key's words otherwise).
    pub(crate) fn key_and_flags(self) -> ([u32; 8], u32) {
        match self {
            Mode::Hash => (IV, 0),
            Mode::Keyed(key) => (words_of(&key), KEYED_HASH),
            Mode::DeriveKey(key) => (words_of(&key), DERIVE_KEY_MATERIAL),
        }
    }
}

/// [`many_checked`] was given a number of digest slots other than its number of inputs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CountMismatch {
    /// The inputs given.
    pub inputs: usize,
    /// The digest slots given.
    pub digests: usize,
}

impl fmt::Display for CountMismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "blaze_blake3::many_checked: {} inputs but {} digest slots (one digest for each input)",
            self.inputs, self.digests
        )
    }
}

impl core::error::Error for CountMismatch {}

/// The digest of each of `inputs` in `mode`, into the same place of `out`, in the caller's
/// order: exactly the bytes the one-at-a-time entry of `mode` gives each input, at any lengths
/// (empty, a block, a chunk, many chunks, all different in one call). Refuses, writing nothing,
/// when `out` is not one slot an input.
pub fn many_checked(mode: Mode, inputs: &[&[u8]], out: &mut [[u8; OUT_LEN]]) -> Result<(), CountMismatch> {
    if inputs.len() != out.len() {
        return Err(CountMismatch { inputs: inputs.len(), digests: out.len() });
    }
    let (key, flags) = mode.key_and_flags();
    many(&key, flags, inputs, out);
    Ok(())
}

/// [`many_checked`] by `engine`: its degree's lanes (one at a time at [`crate::Degree::One`])
/// on its instructions, the same digests.
pub fn many_checked_on(
    engine: Engine,
    mode: Mode,
    inputs: &[&[u8]],
    out: &mut [[u8; OUT_LEN]],
) -> Result<(), CountMismatch> {
    if inputs.len() != out.len() {
        return Err(CountMismatch { inputs: inputs.len(), digests: out.len() });
    }
    let (key, flags) = mode.key_and_flags();
    many_on(engine, &key, flags, inputs, out);
    Ok(())
}

/// The inputs a [`Many`] holds by default before it hashes them: the outline's batch
/// (`blaze-file-items` digest.rs `BATCH`), enough that the lanes idle at a call's end are few
/// beside its steps.
pub const MANY_DEFAULT: usize = 64;

/// A batcher of up to `N` inputs and the slots their digests go to: [`Many::push`] an input as
/// it is made; the batch is hashed side by side when it is full, on [`Many::finish`], and on
/// drop, so every pushed slot holds its input's digest once the batcher is gone, whatever the
/// flush points. The inputs and slots are borrowed for `'a`, so both outlive the batch and a
/// slot cannot be read before its digest is written (the borrow checker says so, not a
/// comment).
pub struct Many<'a, const N: usize = MANY_DEFAULT> {
    key: [u32; 8],
    flags: u32,
    /// The pushed inputs not yet hashed: `inputs[..len]`.
    inputs: [&'a [u8]; N],
    /// Their digests' slots: `slots[..len]` are all `Some`.
    slots: [Option<&'a mut [u8; OUT_LEN]>; N],
    len: usize,
}

impl<'a, const N: usize> Many<'a, N> {
    /// An empty batcher hashing in `mode`.
    pub fn new(mode: Mode) -> Many<'a, N> {
        const { assert!(N > 0, "a batch holds at least one input") };
        let (key, flags) = mode.key_and_flags();
        Many { key, flags, inputs: [&[]; N], slots: [const { None }; N], len: 0 }
    }

    /// Adds `input`, its digest to go to `slot`; hashes the batch if this fills it.
    pub fn push(&mut self, input: &'a [u8], slot: &'a mut [u8; OUT_LEN]) {
        self.inputs[self.len] = input;
        self.slots[self.len] = Some(slot);
        self.len += 1;
        if self.len == N {
            self.flush();
        }
    }

    /// The inputs pushed and not yet hashed.
    pub fn pending(&self) -> usize {
        self.len
    }

    /// Hashes what is left and ends the batcher.
    pub fn finish(mut self) {
        self.flush();
    }

    /// Hashes `inputs[..len]` side by side and writes each digest to its slot.
    fn flush(&mut self) {
        let len = self.len;
        if len == 0 {
            return;
        }
        let mut digests = [[0u8; OUT_LEN]; N];
        many(&self.key, self.flags, &self.inputs[..len], &mut digests[..len]);
        for (slot, digest) in self.slots[..len].iter_mut().zip(&digests[..len]) {
            let slot = slot.take().expect("a slot for each pushed input (`Many::push` sets both)");
            *slot = *digest;
        }
        self.inputs[..len].fill(&[]);
        self.len = 0;
    }
}

impl<const N: usize> Drop for Many<'_, N> {
    /// A batcher dropped without [`Many::finish`] still writes every pushed slot: no digest is
    /// quietly left unwritten.
    fn drop(&mut self) {
        self.flush();
    }
}
