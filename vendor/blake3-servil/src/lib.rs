//! `blake3-servil` is a fast [BLAKE3] implementation for 64-bit Arm and
//! x86-64. It produces the same digests as official BLAKE3, with a call
//! for each way programs hash (the table below).
//!
//! # Examples
//!
//! ```
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! // Once per process, before any other call (see Startup self-test).
//! blake3_servil::initialize();
//!
//! // Hash an input all at once.
//! let hash1 = blake3_servil::convenience::hash(b"foobarbaz");
//!
//! // Hash an input incrementally.
//! let mut hasher = blake3_servil::performance::Hasher::new(blake3_servil::Mode::Plain);
//! hasher.update(b"foo");
//! hasher.update(b"bar");
//! hasher.update(b"baz");
//! let hash2 = hasher.finalize();
//! assert_eq!(hash1, hash2);
//!
//! // Extended output. OutputReader also implements Read and Seek.
//! # #[cfg(feature = "std")] {
//! let mut output = [0; 1000];
//! let mut output_reader = hasher.finalize_xof();
//! output_reader.fill(&mut output);
//! assert_eq!(hash1, output[..32]);
//! # }
//!
//! // Print a hash as hex.
//! println!("{}", hash1);
//!
//! // Many short messages in one call: a Batch lays them out in Blocks.
//! # #[cfg(feature = "std")] {
//! use blake3_servil::{performance::{self, Batch}, Mode};
//! let mut batch = Batch::new(64, 3);
//! for i in 0..3 {
//!     batch.set(i, &[i as u8; 64]);
//! }
//! let mut hashes = [[0u8; 32]; 3];
//! performance::hash_many(Mode::Plain, &batch, &mut hashes);
//! assert_eq!(hashes[2], *blake3_servil::convenience::hash(&[2u8; 64]).as_bytes());
//! # }
//! # Ok(())
//! # }
//! ```
//!
//! # Two kinds of call
//!
//! - **[`convenience`]**: one call per hash, every feature of BLAKE3 (the
//!   plain hash, the keyed hash, key derivation, extended output), any
//!   slice, the result returned.
//! - **[`performance`]**, for power users: every way this crate makes
//!   hashing fast (other cores, batches, collections, streams, the
//!   [`Hasher`] for a message fed in pieces). The
//!   program takes on more of the work: inputs come as
//!   [`Block`](performance::Block)s (64 bytes on a 64-byte boundary, so
//!   alignment is the type's), digests go into the program's places, and
//!   every call takes a [`Mode`].
//!
//! Start with the convenience calls; when a program needs more speed, it
//! moves to the performance ones.
//!
//! | Your data | Call |
//! |---|---|
//! | One message in memory | [`convenience::hash`], or for speed [`performance::hash_multithreaded`] |
//! | One message arriving in pieces | [`performance::Hasher`] |
//! | Many messages of one length, in one buffer | [`performance::hash_many_multithreaded`] |
//! | Many messages of different lengths, in memory | [`performance::hash_each_multithreaded`] |
//! | Many messages arriving at once (a server's uploads) | [`performance::update_each`] |
//! | A stream of messages read, made, or arriving | [`performance::stream`] |
//!
//! Each call returns once its hash is done, and uses other CPU cores
//! where that is faster (the forms without `multithreaded` stay on the
//! calling thread). A [stream](performance::stream) is different: your program submits
//! messages and goes on to read or make the next ones while other threads
//! hash them; each digest arrives in the place your program named, a
//! little later. Choose it when reading or making the data keeps your
//! thread busy, such as files read from memory or a fast disk: it hashes
//! the most bytes per second.
//!
//! Short messages hash fastest together: 1024 messages of 64 bytes in one
//! [`performance::hash_many`] call take about a fifth of the time of 1024
//! [`convenience::hash`] calls.
//!
//! **Keyed hashing and key derivation** run at the plain hash's speed
//! through a [`Mode`], which every performance call takes, the
//! [`Hasher`]'s constructor among them.
//!
//! **Hashing now and then** (rather than nonstop) is slower per byte on
//! every hash function, since its code and data must come back into the
//! CPU's caches and a resting core runs slower at first: hashing 64 KiB
//! after a pause takes about three times as long as nonstop on an Apple M4
//! Max.
//!
//! **Apple M4 and later** have one matrix unit that this crate uses for
//! inputs of 16 KiB and more; when several threads of a program hash such
//! inputs at the same moment, one runs at full speed and the others at
//! about two thirds of it. The multithreaded calls share the work among
//! the cores themselves, so a program gets the most from one thread making
//! them.
//!
//! # Memory
//!
//! - **A [`Hasher`] is about 18 KiB**; box it where that matters.
//! - **Single-threaded calls allocate nothing**: [`convenience::hash`],
//!   [`performance::hash`], [`performance::hash_many`], [`Hasher::update`]
//!   and [`Hasher::finalize`], and [`OutputReader`].
//!   [`Hasher::update_reader`] uses a 1 MiB buffer while it reads past
//!   64 KiB, and [`performance::hash_each`] 32 bytes for each chunk of its messages
//!   of 2 to 15 chunks, each freed when it returns.
//! - **Multithreaded calls** allocate nothing below 512 KiB, where they
//!   hash on the calling thread. Above it they keep a list of the pieces
//!   they hand out, freed when they return: 48 bytes for each 128 KiB of
//!   input, and at most 200 bytes for each CPU (1 GiB on 16 CPUs: 390 KiB).
//! - **A [stream](performance::stream)** takes its storage (its 2 MiB ring and its entries)
//!   from a pool the process keeps: a thread's first stream allocates it
//!   and later streams reuse it, never freed.
//! - **Once per process**: the startup self-test's buffers, about 45 KiB
//!   of the calling thread's stack while it runs (it allocates nothing),
//!   and the threads: by default a worker per CPU beyond the first, each with the
//!   standard library's stack, started by [`initialize_multithreaded`].
//!
//! # Startup self-test
//!
//! **Every program calls [`initialize`] (or [`initialize_multithreaded`])
//! once, before any other call of this crate**: a call before it panics, in
//! every build, saying so. The check costs nothing: it is part of the
//! platform load every call makes. **A program that makes multithreaded
//! calls or streams calls [`initialize_multithreaded`]**: a
//! multithreaded call or a stream before it panics (one load per call).
//!
//! `initialize` checks the build and the CPU before the process first
//! hashes, in well under a millisecond. It runs every assembly kernel built
//! for this machine on fixed inputs and compares the outputs with fixed
//! values official BLAKE3 generated. A mismatch stops the process, which no
//! caller can catch: with `std` it names the code path on stderr and
//! aborts; without it, the CPU's trap instruction stops it. Other threads
//! that call `initialize` meanwhile wait for it.
//!
//! # Cargo Features
//!
//! The `std` feature (the only feature enabled by default) enables the
//! [`Write`] implementation and the [`update_reader`](Hasher::update_reader)
//! method for [`Hasher`], and also the [`Read`] and [`Seek`] implementations
//! for [`OutputReader`].
//!
//! The `zeroize` feature (off by default, on for [docs.rs])
//! implements
//! [`Zeroize`](https://docs.rs/zeroize/latest/zeroize/trait.Zeroize.html) for
//! this crate's types.
//!
//! The build needs a C compiler that assembles the kernels (GCC 12 or
//! later, or Clang); without one it stops and says so. This crate runs its
//! assembly or not at all: for portable Rust, use official BLAKE3.
//!
//! The `serde` feature (off by default, on for [docs.rs]) implements
//! [`serde::Serialize`](https://docs.rs/serde/latest/serde/trait.Serialize.html) and
//! [`serde::Deserialize`](https://docs.rs/serde/latest/serde/trait.Deserialize.html)
//! for [`Hash`](struct@Hash).
//!
//! [BLAKE3]: https://blake3.io
//! [docs.rs]: https://docs.rs/
//! [`Read`]: https://doc.rust-lang.org/std/io/trait.Read.html
//! [`Write`]: https://doc.rust-lang.org/std/io/trait.Write.html
//! [`Seek`]: https://doc.rust-lang.org/std/io/trait.Seek.html

#![cfg_attr(not(feature = "std"), no_std)]

// The tests use std in every build, the no_std one included.
#[cfg(all(test, not(feature = "std")))]
#[macro_use]
extern crate std;

#[cfg(test)]
mod test;

mod block;
pub mod convenience;
#[cfg(feature = "std")]
mod each;
pub mod performance;

mod platform;

// Platform-specific implementations of the compression function. These
// BLAKE3-specific cfg flags are set in build.rs.
#[cfg(blake3_avx2_ffi)]
#[path = "ffi_avx2.rs"]
mod avx2;
#[cfg(blake3_avx512_ffi)]
#[path = "ffi_avx512.rs"]
mod avx512;
#[cfg(all(blake3_avx512_ffi, target_os = "linux"))]
#[path = "ffi_avx512_32.rs"]
mod avx512_32;
#[cfg(blake3_sse41_ffi)]
#[path = "ffi_chunk_root.rs"]
mod chunk_root;
#[cfg(blake3_neon)]
#[path = "ffi_neon_hybrid.rs"]
mod neon_hybrid;
#[cfg(blake3_sme2)]
#[path = "ffi_sme2.rs"]
mod sme2;
// Whole chunks and a partial one in one pass, on x86-64 (generated by
// tools/gen_x86.py, built with the SSE4.1 and AVX2 assembly).
#[cfg(blake3_sse41_ffi)]
mod ffi_partial;
#[cfg(blake3_sse41_ffi)]
#[path = "ffi_sse41.rs"]
mod sse41;

#[cfg(feature = "std")]
mod io;
/// The primitives src/finish.rs runs on: the standard library's here, and
/// Shuttle's in tests/shuttle_finish.rs, which compiles that file beside its own
/// `finish_sync`.
#[cfg(feature = "std")]
mod finish_sync {
    pub use std::sync::Mutex;
    pub use std::sync::atomic::{AtomicUsize, Ordering};
    pub use std::thread;
}
#[cfg(feature = "std")]
mod finish;
/// The primitives src/park.rs runs on: the standard library's here, and
/// Shuttle's in tests/shuttle_park.rs, which compiles that file beside its
/// own `park_sync`.
#[cfg(feature = "std")]
mod park_sync {
    pub use std::sync::OnceLock;
    pub use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    pub use std::thread;
}
#[cfg(feature = "std")]
mod lanes;
mod many;
mod pair;
#[cfg(feature = "std")]
mod park;
mod self_test;
#[cfg(feature = "std")]
mod stream;
mod tree;

/// A kind of kernel or path a call ran, for the tests that check which
/// path a call took. Each build notes the kinds it has.
#[derive(Clone, Copy)]
#[allow(dead_code)]
pub(crate) enum Ran {
    /// The call went to the pool.
    Pool = 1,
    /// The call wanted SME2 and another thread held the turn.
    TurnTaken = 2,
}

#[cfg(all(test, feature = "std"))]
std::thread_local! {
    /// The kinds this thread's calls ran since the test last cleared it.
    pub(crate) static RAN: core::cell::Cell<u32> = const { core::cell::Cell::new(0) };
}

/// Note that `kind` happened: in the tests' record, and nothing in any
/// other build (some of which have no kernel to note).
#[inline(always)]
#[allow(dead_code)]
pub(crate) fn ran(kind: Ran) {
    #[cfg(all(test, feature = "std"))]
    RAN.with(|r| r.set(r.get() | kind as u32));
    let _ = kind;
}

#[cfg(all(test, feature = "std"))]
std::thread_local! {
    /// The assembly entry points this thread's calls ran since the test
    /// last cleared it, by address.
    pub(crate) static RAN_ENTRIES: core::cell::RefCell<std::collections::BTreeSet<usize>> = const { core::cell::RefCell::new(std::collections::BTreeSet::new()) };
}

/// Note that the assembly entry point at `entry` ran (in the tests'
/// record; nothing in any other build).
#[inline(always)]
#[allow(dead_code)]
pub(crate) fn ran_kernel(entry: *const ()) {
    #[cfg(all(test, feature = "std"))]
    RAN_ENTRIES.with(|r| r.borrow_mut().insert(entry as usize));
    let _ = entry;
}

use arrayvec::{ArrayString, ArrayVec};
use core::cmp;
use core::fmt;
use platform::{MAX_SIMD_DEGREE, Platform};
#[cfg(feature = "zeroize")]
use zeroize::Zeroize;

/// The number of bytes in a [`Hash`](struct.Hash.html), 32.
pub const OUT_LEN: usize = 32;

/// The number of bytes in a key, 32.
pub const KEY_LEN: usize = 32;

/// The number of bytes in a block, 64.
///
/// You don't usually need to think about this number. One case where it matters is calling
/// [`OutputReader::fill`] in a loop, where using a `buf` argument that's a multiple of `BLOCK_LEN`
/// avoids repeating work.
pub const BLOCK_LEN: usize = 64;

/// The number of bytes in a chunk, 1024.
///
/// You don't usually need to think about this number, but it often comes up in benchmarks, because
/// the maximum degree of parallelism used by the implementation equals the number of chunks.
pub const CHUNK_LEN: usize = 1024;

const MAX_DEPTH: usize = 54; // 2^54 * CHUNK_LEN = 2^64

// A chunk's CV is words while its blocks compress (the kernels' form); the
// hash_many interface hashes input bytes and parent nodes alike, so its
// output CVs are bytes.
type CVWords = [u32; 8];
type CVBytes = [u8; 32]; // little-endian

const IV: &CVWords = &[0x6A09E667, 0xBB67AE85, 0x3C6EF372, 0xA54FF53A, 0x510E527F, 0x9B05688C, 0x1F83D9AB, 0x5BE0CD19];

// These are the internal flags that we use to domain separate root/non-root,
// chunk/parent, and chunk beginning/middle/end. These get set at the high end
// of the block flags word in the compression function, so their values start
// high and go down.
const CHUNK_START: u8 = 1 << 0;
const CHUNK_END: u8 = 1 << 1;
const PARENT: u8 = 1 << 2;
const ROOT: u8 = 1 << 3;
const KEYED_HASH: u8 = 1 << 4;
const DERIVE_KEY_CONTEXT: u8 = 1 << 5;
const DERIVE_KEY_MATERIAL: u8 = 1 << 6;

/// An output of the default size, 32 bytes, which provides constant-time
/// equality checking.
///
/// `Hash` implements [`From`] and [`Into`] for `[u8; 32]`, and it provides
/// [`from_bytes`] and [`as_bytes`] for explicit conversions between itself and
/// `[u8; 32]`. However, byte arrays and slices don't provide constant-time
/// equality checking, which is often a security requirement in software that
/// handles private data. `Hash` doesn't implement [`Deref`] or [`AsRef`], to
/// avoid situations where a type conversion happens implicitly and the
/// constant-time property is accidentally lost.
///
/// `Hash` provides the [`to_hex`] and [`from_hex`] methods for converting to
/// and from hexadecimal. It also implements [`Display`] and [`FromStr`].
///
/// [`From`]: https://doc.rust-lang.org/std/convert/trait.From.html
/// [`Into`]: https://doc.rust-lang.org/std/convert/trait.Into.html
/// [`as_bytes`]: #method.as_bytes
/// [`from_bytes`]: #method.from_bytes
/// [`Deref`]: https://doc.rust-lang.org/stable/std/ops/trait.Deref.html
/// [`AsRef`]: https://doc.rust-lang.org/std/convert/trait.AsRef.html
/// [`to_hex`]: #method.to_hex
/// [`from_hex`]: #method.from_hex
/// [`Display`]: https://doc.rust-lang.org/std/fmt/trait.Display.html
/// [`FromStr`]: https://doc.rust-lang.org/std/str/trait.FromStr.html
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Copy, Hash, Eq)]
#[repr(transparent)]
pub struct Hash([u8; OUT_LEN]);

impl Hash {
    /// The raw bytes of the `Hash`. Note that byte arrays don't provide
    /// constant-time equality checking, so if you need to compare hashes,
    /// prefer the `Hash` type.
    #[inline]
    pub const fn as_bytes(&self) -> &[u8; OUT_LEN] {
        &self.0
    }

    /// Create a `Hash` from its raw bytes representation.
    pub const fn from_bytes(bytes: [u8; OUT_LEN]) -> Self {
        Self(bytes)
    }

    /// The raw bytes of the `Hash`, as a slice. Useful for serialization. Note that byte arrays
    /// don't provide constant-time equality checking, so if you need to compare hashes, prefer
    /// the `Hash` type.
    #[inline]
    pub const fn as_slice(&self) -> &[u8] {
        self.0.as_slice()
    }

    /// Create a `Hash` from its raw bytes representation as a slice.
    ///
    /// Returns an error if the slice is not exactly 32 bytes long.
    pub fn from_slice(bytes: &[u8]) -> Result<Self, core::array::TryFromSliceError> {
        Ok(Self::from_bytes(bytes.try_into()?))
    }

    /// Encode a `Hash` in lowercase hexadecimal.
    ///
    /// The returned [`ArrayString`] is a fixed size and doesn't allocate memory
    /// on the heap. Note that [`ArrayString`] doesn't provide constant-time
    /// equality checking, so if you need to compare hashes, prefer the `Hash`
    /// type.
    ///
    /// [`ArrayString`]: https://docs.rs/arrayvec/0.5.1/arrayvec/struct.ArrayString.html
    pub fn to_hex(&self) -> ArrayString<{ 2 * OUT_LEN }> {
        let mut s = ArrayString::new();
        let table = b"0123456789abcdef";
        for &b in self.0.iter() {
            s.push(table[(b >> 4) as usize] as char);
            s.push(table[(b & 0xf) as usize] as char);
        }
        s
    }

    /// Decode a `Hash` from hexadecimal. Both uppercase and lowercase ASCII
    /// bytes are supported.
    ///
    /// Any byte outside the ranges `'0'...'9'`, `'a'...'f'`, and `'A'...'F'`
    /// results in an error. An input length other than 64 also results in an
    /// error.
    ///
    /// Note that `Hash` also implements `FromStr`, so `Hash::from_hex("...")`
    /// is equivalent to `"...".parse()`.
    pub fn from_hex(hex: impl AsRef<[u8]>) -> Result<Self, HexError> {
        fn hex_val(byte: u8) -> Result<u8, HexError> {
            match byte {
                b'A'..=b'F' => Ok(byte - b'A' + 10),
                b'a'..=b'f' => Ok(byte - b'a' + 10),
                b'0'..=b'9' => Ok(byte - b'0'),
                _ => Err(HexError(HexErrorInner::InvalidByte(byte))),
            }
        }
        let hex_bytes: &[u8] = hex.as_ref();
        if hex_bytes.len() != OUT_LEN * 2 {
            return Err(HexError(HexErrorInner::InvalidLen(hex_bytes.len())));
        }
        let mut hash_bytes: [u8; OUT_LEN] = [0; OUT_LEN];
        for i in 0..OUT_LEN {
            hash_bytes[i] = 16 * hex_val(hex_bytes[2 * i])? + hex_val(hex_bytes[2 * i + 1])?;
        }
        Ok(Hash::from(hash_bytes))
    }
}

impl From<[u8; OUT_LEN]> for Hash {
    #[inline]
    fn from(bytes: [u8; OUT_LEN]) -> Self {
        Self::from_bytes(bytes)
    }
}

impl From<Hash> for [u8; OUT_LEN] {
    #[inline]
    fn from(hash: Hash) -> Self {
        hash.0
    }
}

impl core::str::FromStr for Hash {
    type Err = HexError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Hash::from_hex(s)
    }
}

#[cfg(feature = "zeroize")]
impl Zeroize for Hash {
    fn zeroize(&mut self) {
        // Destructuring to trigger compile error as a reminder to update this impl.
        let Self(bytes) = self;
        bytes.zeroize();
    }
}

/// This implementation is constant-time.
impl PartialEq for Hash {
    #[inline]
    fn eq(&self, other: &Hash) -> bool {
        constant_time_eq::constant_time_eq_32(&self.0, &other.0)
    }
}

/// This implementation is constant-time.
impl PartialEq<[u8; OUT_LEN]> for Hash {
    #[inline]
    fn eq(&self, other: &[u8; OUT_LEN]) -> bool {
        constant_time_eq::constant_time_eq_32(&self.0, other)
    }
}

/// This implementation is constant-time if the target is 32 bytes long.
impl PartialEq<[u8]> for Hash {
    #[inline]
    fn eq(&self, other: &[u8]) -> bool {
        constant_time_eq::constant_time_eq(&self.0, other)
    }
}

impl fmt::Display for Hash {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        // Formatting field as `&str` to reduce code size since the `Debug`
        // dynamic dispatch table for `&str` is likely needed elsewhere already,
        // but that for `ArrayString<[u8; 64]>` is not.
        let hex = self.to_hex();
        let hex: &str = hex.as_str();

        f.write_str(hex)
    }
}

impl fmt::Debug for Hash {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        // Formatting field as `&str` to reduce code size since the `Debug`
        // dynamic dispatch table for `&str` is likely needed elsewhere already,
        // but that for `ArrayString<[u8; 64]>` is not.
        let hex = self.to_hex();
        let hex: &str = hex.as_str();

        f.debug_tuple("Hash").field(&hex).finish()
    }
}

/// The error type for [`Hash::from_hex`].
///
/// The `.to_string()` representation of this error currently distinguishes between bad length
/// errors and bad character errors. This is to help with logging and debugging, but it isn't a
/// stable API detail, and it may change at any time.
#[derive(Clone, Debug)]
pub struct HexError(HexErrorInner);

#[derive(Clone, Debug)]
enum HexErrorInner {
    InvalidByte(u8),
    InvalidLen(usize),
}

impl fmt::Display for HexError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self.0 {
            HexErrorInner::InvalidByte(byte) => {
                if byte < 128 {
                    write!(f, "invalid hex character: {:?}", byte as char)
                } else {
                    write!(f, "invalid hex character: 0x{:x}", byte)
                }
            }
            HexErrorInner::InvalidLen(len) => {
                write!(f, "expected 64 hex bytes, received {}", len)
            }
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for HexError {}

type Output = hasher::Output<Platform>;

impl Clone for Output {
    fn clone(&self) -> Self {
        hasher::Output {
            block: self.block,
            input_chaining_value: self.input_chaining_value,
            block_len: self.block_len,
            counter: self.counter,
            flags: self.flags,
            platform: self.platform,
        }
    }
}

impl Output {
    fn root_hash(&self) -> Hash {
        debug_assert_eq!(self.counter, 0);
        Hash(self.root_bytes())
    }

    fn root_output_block(&self) -> [u8; 2 * OUT_LEN] {
        self.platform.compress_xof(&self.input_chaining_value, &self.block, self.block_len, self.counter, self.flags | ROOT)
    }
}

#[cfg(feature = "zeroize")]
impl Zeroize for Output {
    fn zeroize(&mut self) {
        // Destructuring to trigger compile error as a reminder to update this impl.
        let Self { input_chaining_value, block, block_len, counter, flags, platform: _ } = self;

        input_chaining_value.zeroize();
        block.zeroize();
        block_len.zeroize();
        counter.zeroize();
        flags.zeroize();
    }
}

type ChunkState = hasher::ChunkState<Platform>;

impl hasher::Blocks for Platform {
    #[inline(always)]
    fn compress_in_place(&self, cv: &mut CVWords, block: &[u8; BLOCK_LEN], block_len: u8, counter: u64, flags: u8) {
        Platform::compress_in_place(self, cv, block, block_len, counter, flags)
    }

    #[inline(always)]
    fn compress_blocks(&self, cv: &mut CVWords, blocks: &[u8], count: usize, counter: u64, flags: u8, flags_start: u8) {
        Platform::compress_blocks(self, cv, blocks, count, counter, flags, flags_start)
    }
}

impl Clone for ChunkState {
    fn clone(&self) -> Self {
        hasher::ChunkState {
            buf: self.buf,
            cv: self.cv,
            chunk_counter: self.chunk_counter,
            buf_len: self.buf_len,
            blocks_compressed: self.blocks_compressed,
            flags: self.flags,
            platform: self.platform,
        }
    }
}

impl ChunkState {
    // Try to avoid buffering as much as possible, by compressing directly from
    // the input slice when full blocks are available (src/hasher.rs).
    #[inline(always)]
    fn update(&mut self, input: &[u8]) -> &mut Self {
        self.absorb(input);
        debug_assert!(self.count() <= CHUNK_LEN);
        self
    }
}

#[cfg(feature = "zeroize")]
impl Zeroize for ChunkState {
    fn zeroize(&mut self) {
        // Destructuring to trigger compile error as a reminder to update this impl.
        let Self { cv, chunk_counter, buf, buf_len, blocks_compressed, flags, platform: _ } = self;

        cv.zeroize();
        chunk_counter.zeroize();
        buf.zeroize();
        buf_len.zeroize();
        blocks_compressed.zeroize();
        flags.zeroize();
    }
}

// IMPLEMENTATION NOTE
// ===================
/// Undocumented and unstable, for benchmarks only.
#[doc(hidden)]
#[derive(Clone, Copy)]
pub enum IncrementCounter {
    Yes,
    No,
}

impl IncrementCounter {
    #[inline]
    fn yes(&self) -> bool {
        match self {
            IncrementCounter::Yes => true,
            IncrementCounter::No => false,
        }
    }
}

/// The lanes of the one pass that hashes `whole` whole chunks with a
/// partial one of `partial` bytes after them, where the pass costs less
/// than the partial chunk's blocks after the whole ones: one whole chunk
/// and a partial one of two blocks or more in four lanes (AVX2 and
/// AVX-512: the chain falls from 25 blocks to 16 at 1536 bytes), three
/// whole chunks in four lanes (SSE4.1 and AVX2), and in eight (AVX2) six or
/// seven, or five with a partial chunk of six blocks or more (with fewer,
/// an i7-12700K's efficiency core ran the eight lanes up to 7% slower; a
/// pass of eight lanes costs 1.6 times one of four on its performance
/// core). AVX-512 otherwise keeps its own widths (unmeasured).
#[cfg(blake3_sse41_ffi)]
#[inline]
fn spare_lane(platform: Platform, whole: usize, partial: usize) -> Option<usize> {
    match (platform, whole) {
        (Platform::AVX2, 1) if partial > BLOCK_LEN => Some(4),
        #[cfg(blake3_avx512_ffi)]
        (Platform::AVX512, 1) if partial > BLOCK_LEN => Some(4),
        (Platform::SSE41 | Platform::AVX2, 3) => Some(4),
        (Platform::AVX2, 6..=7) => Some(8),
        (Platform::AVX2, 5) if partial > 5 * BLOCK_LEN => Some(8),
        _ => None,
    }
}

// Use SIMD parallelism to hash up to MAX_SIMD_DEGREE chunks at the same time
// on a single thread. Write out the chunk chaining values and return the
// number of chunks hashed. These chunks are never the root and never empty;
// those cases use a different codepath.
fn compress_chunks_parallel(input: &[u8], key: &CVWords, chunk_counter: u64, flags: u8, platform: Platform, out: &mut [u8]) -> usize {
    debug_assert!(!input.is_empty(), "empty chunks below the root");
    debug_assert!(input.len() <= MAX_SIMD_DEGREE * CHUNK_LEN);

    let mut chunks_exact = input.chunks_exact(CHUNK_LEN);
    let mut chunks_array = ArrayVec::<&[u8; CHUNK_LEN], MAX_SIMD_DEGREE>::new();
    for chunk in &mut chunks_exact {
        chunks_array.push(chunk.try_into().unwrap());
    }
    // A whole chunk and a partial one, on AVX2 or AVX-512: their first
    // blocks side by side (pair.rs).
    if pair::takes(platform, input.len()) {
        out[..2 * OUT_LEN].copy_from_slice(&pair::parent(input, key, chunk_counter, flags, platform));
        return 2;
    }
    // Whole chunks that leave a lane spare, and a partial one: one pass,
    // the partial chunk in the spare lane.
    #[cfg(blake3_sse41_ffi)]
    if let (false, Some(lanes)) = (chunks_exact.remainder().is_empty(), spare_lane(platform, chunks_array.len(), chunks_exact.remainder().len())) {
        let (whole, partial) = (&chunks_array[..], chunks_exact.remainder());
        // Sound: spare_lane admits each width only on a platform whose CPU
        // has its instructions.
        if lanes == 4 {
            unsafe { ffi_partial::hash_chunks_with_partial_sse41(whole, partial, key, chunk_counter, flags, out) };
        } else {
            unsafe { ffi_partial::hash_chunks_with_partial_avx2(whole, partial, key, chunk_counter, flags, out) };
        }
        return whole.len() + 1;
    }
    // Whole chunks and a partial one, on the NEON hybrids: where a q kernel
    // covers the count, the partial chunk runs on the scalar units beside
    // the whole chunks rather than after them.
    #[cfg(blake3_neon)]
    let hybrids = matches!(platform, Platform::NEON);
    #[cfg(blake3_sme2)]
    let hybrids = hybrids || matches!(platform, Platform::SME2);
    #[cfg(blake3_neon)]
    if !chunks_exact.remainder().is_empty()
        && hybrids
        && neon_hybrid::partial_pays(chunks_array.len(), chunks_exact.remainder().len())
        && neon_hybrid::sha3_detected()
    {
        // Safe: the kernel exists for this count and the CPU has SHA-3.
        unsafe {
            neon_hybrid::hash_chunks_with_partial(&chunks_array, chunks_exact.remainder(), key, chunk_counter, flags, CHUNK_START, CHUNK_END, out);
        }
        return chunks_array.len() + 1;
    }
    platform.hash_many(&chunks_array, key, chunk_counter, IncrementCounter::Yes, flags, CHUNK_START, CHUNK_END, out);

    // Hash the remaining partial chunk, if there is one. Note that the empty
    // chunk (meaning the empty message) is a different codepath.
    let chunks_so_far = chunks_array.len();
    if !chunks_exact.remainder().is_empty() {
        let counter = chunk_counter + chunks_so_far as u64;
        let mut chunk_state = ChunkState::new(key, counter, flags, platform);
        chunk_state.update(chunks_exact.remainder());
        *<&mut [u8; OUT_LEN]>::try_from(&mut out[chunks_so_far * OUT_LEN..][..OUT_LEN]).unwrap() = chunk_state.output().chaining_value();
        chunks_so_far + 1
    } else {
        chunks_so_far
    }
}

// Use SIMD parallelism to hash up to MAX_SIMD_DEGREE parents at the same time
// on a single thread. Write out the parent chaining values and return the
// number of parents hashed. (If there's an odd input chaining value left over,
// return it as an additional output.) These parents are never the root and
// never empty; those cases use a different codepath.
fn compress_parents_parallel(child_chaining_values: &[u8], key: &CVWords, flags: u8, platform: Platform, out: &mut [u8]) -> usize {
    debug_assert_eq!(child_chaining_values.len() % OUT_LEN, 0, "wacky hash bytes");
    let num_children = child_chaining_values.len() / OUT_LEN;
    debug_assert!(num_children >= 2, "not enough children");
    debug_assert!(num_children <= 2 * MAX_SIMD_DEGREE, "too many");

    let mut parents_exact = child_chaining_values.chunks_exact(BLOCK_LEN);
    let mut parents_array = ArrayVec::<&[u8; BLOCK_LEN], MAX_SIMD_DEGREE>::new();
    for parent in &mut parents_exact {
        parents_array.push(parent.try_into().unwrap());
    }
    platform.hash_many(
        &parents_array,
        key,
        0, // Parents always use counter 0.
        IncrementCounter::No,
        flags | PARENT,
        0, // Parents have no start flags.
        0, // Parents have no end flags.
        out,
    );

    // If there's an odd child left over, it becomes an output.
    let parents_so_far = parents_array.len();
    if !parents_exact.remainder().is_empty() {
        out[parents_so_far * OUT_LEN..][..OUT_LEN].copy_from_slice(parents_exact.remainder());
        parents_so_far + 1
    } else {
        parents_so_far
    }
}

// Hash a subtree with the walk (src/tree.rs, `children`), condensing its
// chaining values down to a single parent node. Don't compress that
// last parent node, however. Instead, return its message bytes (the
// concatenated chaining values of its children). This is necessary when the
// first call to update() supplies a complete subtree, because the topmost
// parent node of that subtree could end up being the root. It's also necessary
// for extended output in the general case.
//
// This function is not used on inputs of 1 chunk or less. That's a
// different codepath. Its arrays live in tree::root_children, out of line.
#[inline]
fn compress_subtree_to_parent_node(input: &[u8], ahead: usize, key: &CVWords, chunk_counter: u64, flags: u8, platform: Platform) -> [u8; BLOCK_LEN] {
    debug_assert!(input.len() > CHUNK_LEN);
    let kernels = tree::PlatformKernels { key, flags, platform };
    let [left, right] = tree::root_children(&kernels, input, ahead, chunk_counter);
    let mut block = [0u8; BLOCK_LEN];
    block[..OUT_LEN].copy_from_slice(&left);
    block[OUT_LEN..].copy_from_slice(&right);
    block
}

// The root hash of an input of one chunk or less, in one kernel call. The
// whole blocks are read in place, the final one too when it is full; a
// short final block (and the empty block for the empty input) is copied
// into a zero-padded buffer, which is the copy ChunkState::update() makes
// too. The scalar kernel runs on every AArch64 core, so this path needs no
// platform check.
#[cfg(blake3_neon)]
#[inline]
fn hash_one_chunk_root(input: &[u8], key: &CVWords, flags: u8) -> Hash {
    debug_assert!(input.len() <= CHUNK_LEN);
    let blocks = cmp::max(1, input.len().div_ceil(BLOCK_LEN));
    let whole = (blocks - 1) * BLOCK_LEN;
    let tail = &input[whole..];
    /// The padded last block, 64-byte aligned as hasher::Output's is
    /// (src/hasher.rs explains).
    #[repr(align(64))]
    struct Aligned64([u8; BLOCK_LEN]);
    let padded;
    let last: &[u8; BLOCK_LEN] = match tail.try_into() {
        Ok(full) => full,
        Err(_) => {
            let mut block = Aligned64([0; BLOCK_LEN]);
            block.0[..tail.len()].copy_from_slice(tail);
            padded = block;
            &padded.0
        }
    };
    let mut out = [0u8; OUT_LEN];
    // Safe: `input` holds `whole` bytes before `tail`, and blocks is 1..=16.
    unsafe {
        neon_hybrid::hash_chunk(input.as_ptr(), blocks, last, tail.len() as u8, key, 0, flags, CHUNK_START, CHUNK_END | ROOT, &mut out);
    }
    Hash(out)
}

// Hash a whole subtree at its chunk counter (or the whole input at zero).
// Unlike the wider helpers above, this also handles one chunk or less.
// The pool's pieces (lanes.rs) use it; the one-shot calls go through
// tree::hash.
#[cfg(feature = "std")]
#[inline]
fn hash_all_at_once(input: &[u8], key: &CVWords, chunk_counter: u64, flags: u8, platform: Platform) -> Output {
    // The input is a complete subtree at chunk_counter, or the whole
    // input (including empty) at counter zero. Its byte offset fits u64.
    debug_assert!(chunk_counter <= u64::MAX / CHUNK_LEN as u64);
    debug_assert!(chunk_counter == 0 || (!input.is_empty() && input.len() as u64 <= max_subtree_len(chunk_counter * CHUNK_LEN as u64).unwrap()));

    // If the whole subtree is one chunk, hash it directly with a ChunkState.
    if input.len() <= CHUNK_LEN {
        return ChunkState::new(key, chunk_counter, flags, platform).update(input).output();
    }

    // Otherwise construct an Output object from the parent node returned by
    // compress_subtree_to_parent_node().
    hasher::Output {
        input_chaining_value: *key,
        block: compress_subtree_to_parent_node(input, 0, key, chunk_counter, flags, platform),
        block_len: BLOCK_LEN as u8,
        counter: 0,
        flags: flags | PARENT,
        platform,
    }
}

/// Run the startup self-test and detect the CPU: every program calls this
/// (or [`initialize_multithreaded`]) once, before any other call of this
/// crate, and a call before it panics. Under 0.2 ms, once per process
/// (see the crate's documentation); later calls return at once.
///
/// ```
/// blake3_servil::initialize();
/// let hash = blake3_servil::convenience::hash(b"foo");
/// # assert_eq!(hash, blake3_servil::performance::Hasher::new(blake3_servil::Mode::Plain).update(b"foo").finalize());
/// ```
pub fn initialize() {
    self_test::ensure();
    platform::Platform::detect_unchecked().record();
}

/// [`initialize`], and start the threads that the multithreaded calls and
/// [`stream`](performance::stream) use: every program that makes such calls calls this once,
/// before them, in place of `initialize`. It starts `threads - 1` workers,
/// and each multithreaded call hashes on its calling thread and the
/// workers (calls made at once share the workers), and on CPUs with SME2
/// (Apple M4) the SME2 thread too; `None` means one thread
/// a CPU (`std::thread::available_parallelism`, which follows the CPU
/// affinity the program runs under). With 1, calls hash on their calling
/// thread alone: no workers start, nor the SME2 thread. Otherwise it
/// starts the first worker and returns; the first worker starts the others
/// (each 20-30 us on Linux x86-64), and calls run on the workers started
/// so far. The
/// threads wait asleep whenever no call has work for them (their memory:
/// see [Memory](crate#memory)). Later calls return at once, and must ask
/// for the same number: another `Some` panics. A panic on a worker thread
/// (a bug in this crate) ends the process, its message printed, rather
/// than leaving the call that waits for that worker's piece waiting
/// forever.
///
/// `None` counts logical CPUs, so on a CPU with SMT (Hyper-Threading) both
/// threads of each core, and that is the faster default. On an EPYC 9255,
/// the 16-input kernels leave much of a core waiting on its loads, and a
/// core's second thread fills that time: two threads hash 1.4-1.7 times
/// what one does, as with official BLAKE3. The 32-input AVX-512 kernel
/// (Linux) keeps a core busier on its own, so a second thread adds less
/// (1.1-1.3 times); even so, on 1 GiB 24 threads, one a core, took 4-14%
/// longer than 48. A thread slowed by the other thread on its core, by an
/// efficiency core or by another program takes fewer of a call's pieces,
/// and the pieces shrink toward the end of the input, so the slowest thread
/// finishes holding a small one. To leave CPUs to other work, pass fewer
/// threads.
///
/// The threads are this process's: in a child forked after they started,
/// every multithreaded call and stream panics, saying so (the child has
/// none of them). A program that forks initializes the pool in the process
/// that hashes, after any fork; its single-threaded calls work anywhere.
///
/// ```
/// blake3_servil::initialize_multithreaded(None);
/// let message = vec![blake3_servil::performance::Block::ZERO; (1 << 20) / 64];
/// let hash = blake3_servil::performance::hash_multithreaded(blake3_servil::Mode::Plain, &message, 1 << 20);
/// assert_eq!(hash, blake3_servil::convenience::hash(&[0u8; 1 << 20]));
/// ```
#[cfg(feature = "std")]
pub fn initialize_multithreaded(threads: Option<core::num::NonZeroUsize>) {
    static STARTED: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    let cpus = || std::thread::available_parallelism().map_or(1, |n| n.get());
    let started = *STARTED.get_or_init(|| {
        let threads = threads.map_or_else(cpus, |n| n.get());
        initialize();
        lanes::initialize(threads);
        threads
    });
    if let Some(threads) = threads {
        assert_eq!(
            started,
            threads.get(),
            "blake3-servil: the multithreaded calls were started with {started} threads; initialize_multithreaded cannot change that to {threads}"
        );
    }
}

/// Whether every worker of the multithreaded calls' pool is parked (false
/// before [`initialize_multithreaded`]). Undocumented and unstable, for
/// tests only: a test that needs the pool idle waits for this, not for a
/// sleep that a loaded machine could outlast.
#[cfg(feature = "std")]
#[doc(hidden)]
pub fn __all_workers_parked() -> bool {
    lanes::all_workers_parked()
}

/// An alias to distinguish [`context_key`](performance::context_key) outputs from other keys.
pub type ContextKey = [u8; KEY_LEN];

/// The most bytes a subtree starting at byte `input_offset` of a message
/// can hold: `None` at offset zero (the whole message), else the largest
/// power-of-two multiple of a chunk the offset is aligned to. Requires a
/// whole-chunk offset.
#[inline(always)]
pub(crate) fn max_subtree_len(input_offset: u64) -> Option<u64> {
    if input_offset == 0 {
        return None;
    }
    assert_eq!(input_offset % CHUNK_LEN as u64, 0);
    let counter = input_offset / CHUNK_LEN as u64;
    let max_chunks = 1 << counter.trailing_zeros();
    Some(max_chunks * CHUNK_LEN as u64)
}

/// Which of BLAKE3's three functions a call computes: the plain hash
/// ([`hash`](convenience::hash)), the keyed hash, or key derivation. Every call that takes a mode runs each one at the same
/// speed, but for `DeriveKey`, which first hashes its context string (once
/// per call; [`DeriveKeyWith`](Mode::DeriveKeyWith) takes it hashed
/// once for many calls).
#[derive(Clone, Copy)]
pub enum Mode<'a> {
    /// The plain hash function, as [`hash`](convenience::hash).
    Plain,
    /// The keyed hash function with this key.
    Keyed(&'a [u8; KEY_LEN]),
    /// The key derivation function with this context string; the input
    /// is the key material.
    DeriveKey(&'a str),
    /// The key derivation function with its context string already hashed
    /// ([`context_key`](performance::context_key)); the input is the key material.
    DeriveKeyWith(&'a ContextKey),
}

impl Mode<'_> {
    /// The key words and flags this mode hashes with, the plain mode's IV
    /// where it is: a public call's one way to its key. A copy of the IV
    /// put 32 bytes of stores on the stack before the message's loads of
    /// every call (October 24, the i7's disassembly of a caller); a
    /// constant mode folds to its constants, inlined.
    #[inline]
    pub(crate) fn key(self, room: &mut core::mem::MaybeUninit<CVWords>) -> (&CVWords, u8) {
        match self {
            Mode::Plain => (IV, 0),
            _ => {
                let (key, flags) = self.key_and_flags();
                (room.write(key), flags)
            }
        }
    }

    /// The key words and flags this mode hashes with.
    #[inline]
    fn key_and_flags(self) -> (CVWords, u8) {
        match self {
            Mode::Plain => (*IV, 0),
            Mode::Keyed(key) => (platform::words_from_le_bytes_32(key), KEYED_HASH),
            Mode::DeriveKey(context) => (platform::words_from_le_bytes_32(&performance::context_key(context)), DERIVE_KEY_MATERIAL),
            Mode::DeriveKeyWith(context_key) => (platform::words_from_le_bytes_32(context_key), DERIVE_KEY_MATERIAL),
        }
    }
}

/// The whole input on the calling thread, in the given mode: the one-chunk
/// kernel call up to CHUNK_LEN, the subtree walk above. hash(),
/// hash() in its modes and hash_multithreaded()'s short path all come
/// through here.
#[inline]
fn hash_serial(input: &[u8], key: &CVWords, flags: u8) -> Hash {
    hash_serial_from(Platform::detect(), input, key, flags)
}

/// [`hash_serial`] from `platform`: the startup self-test's way in, which
/// has not run `Platform::detect`'s check (it is that check).
#[inline]
fn hash_serial_from(platform: Platform, input: &[u8], key: &CVWords, flags: u8) -> Hash {
    let turn = platform::Sme2Turn::take(platform, input.len() >= SME2_SIZED_LEN);
    // One comparison on the path of a chunk or less: everything else is
    // out of line, so that path's code stays as small as it was (a call
    // after other work fetches every line of it from DRAM).
    #[cfg(all(blake3_neon, feature = "std"))]
    if (CHUNK_LEN + 1..PREFETCH_BELOW).contains(&input.len()) {
        prefetch_after_pause(input.len(), turn.platform(), false);
    }
    hash_serial_on(input, key, flags, turn.platform())
}

/// Below this length a one-shot call that follows a pause prefetches its
/// kernels' code: after other work the call then fetches it from DRAM in
/// parallel, not one miss at a time (M4 Max, after other work: 4 KiB
/// x0.61, 8 KiB x0.51, 16 KiB x0.66; 32 KiB and up level, the hashing's
/// own time dwarfing it; jobs 971-975). The streaming paths (Hasher,
/// batches, the streams' tasks) run back to back with their code near.
/// A cold call of 2-8 KiB spends about a third of its time on its code
/// (10-24 KB of kernels and glue); from 3 KiB what remains is the kernels
/// starting before their lines arrive. Tried and set aside: prefetching
/// for a chunk or less (2-10% gained, less than the pause check costs);
/// prefetching the 16 KiB of Rust around the kernels too (every call
/// 400-500 ns slower: prefetches beyond some number in flight are
/// dropped); a rolled four-lane NEON path for calls after a pause (1.3 KB,
/// 1.2-1.5x slower: a four-lane NEON BLAKE3 is bound by its chains'
/// latency, where the hybrids' integer lanes fill the idle issue slots).
#[cfg(all(blake3_neon, feature = "std"))]
const PREFETCH_BELOW: usize = 64 * 1024;

/// After a pause (code_may_be_cold), prefetch the kernels hashing `len`
/// bytes runs: a one-shot call's (prefetch_kernels), or with `fresh_hasher`
/// a fresh Hasher's first update's (prefetch_update_kernels).
#[cfg(all(blake3_neon, feature = "std"))]
// Cold for layout: the path of a chunk or less falls through past its
// call, on the lines it had before the call existed.
#[cold]
#[inline(never)]
fn prefetch_after_pause(len: usize, platform: Platform, fresh_hasher: bool) {
    if code_may_be_cold() {
        if fresh_hasher {
            prefetch_update_kernels(len, platform);
        } else {
            prefetch_kernels(len, platform);
        }
    }
}

/// Whether this thread's last call in the prefetching range was over
/// 100 us ago (or never). Calls back to back find their code near, where
/// the prefetch would cost them 2-4% (jobs 972-975); one that follows a
/// pause is the one other work may have taken the code from.
#[cfg(all(blake3_neon, feature = "std"))]
#[inline]
fn code_may_be_cold() -> bool {
    std::thread_local! {
        static LAST_CALL: core::cell::Cell<u64> = const { core::cell::Cell::new(0) };
    }
    let now = platform::counter();
    let last = LAST_CALL.with(|last| last.replace(now));
    now.wrapping_sub(last) > platform::ticks_in_100_us()
}

/// Batches of one-block messages up to this many prefetch their code after
/// a pause: past it the hashing's own time dwarfs a cold kernel's.
#[cfg(all(blake3_neon, feature = "std"))]
const PREFETCH_BATCH_BELOW: usize = 64;

/// Batches of 1 to this many one-block messages run on the pair kernel
/// alone after a pause (neon_hybrid::hash_pairs): its code (2.9 KB) is a
/// fraction of the plans' for 3 to 16 (up to 17 KB) and less than the one
/// message's kernel (3.9 KB), and after a pause the code comes from memory
/// (Mac jobs 1320-1337: 12 and 16 run faster after idling and slower after
/// other work this way).
#[cfg(all(blake3_neon, feature = "std"))]
const PAIRS_AFTER_PAUSE_UPTO: usize = 8;

/// After a pause (code_may_be_cold), a batch of `count` one-block
/// messages: 1 to PAIRS_AFTER_PAUSE_UPTO hashed here on the pair kernel
/// (true: done); otherwise its kernels' code prefetched (false: hash it as
/// usual), SME2's message kernel where it takes the batch, else the
/// hybrids' parent plans.
#[cfg(all(blake3_neon, feature = "std"))]
#[cold]
#[inline(never)]
fn batch_after_pause(input: &[u8], message_len: usize, key: &CVWords, flags: u8, out: &mut [[u8; OUT_LEN]], platform: Platform) -> bool {
    if !code_may_be_cold() {
        return false;
    }
    let count = out.len();
    #[cfg(blake3_sme2)]
    if matches!(platform, Platform::SME2) && count >= many::ONE_BLOCK_PAD_MIN {
        sme2::prefetch_code();
        return false;
    }
    let _ = platform;
    if !neon_hybrid::sha3_detected() {
        return false;
    }
    if message_len == 0 || !(1..=PAIRS_AFTER_PAUSE_UPTO).contains(&count) {
        neon_hybrid::prefetch_message_code(count);
        return false;
    }
    many::assert_slots(input, message_len, count);
    neon_hybrid::prefetch_pair_code();
    let mut table: [&[u8; BLOCK_LEN]; PAIRS_AFTER_PAUSE_UPTO] = [&[0; BLOCK_LEN]; PAIRS_AFTER_PAUSE_UPTO];
    for (slot, message) in table.iter_mut().zip(input.chunks_exact(BLOCK_LEN)) {
        *slot = message.try_into().unwrap();
    }
    // Sound: the SHA-3 extension is here (above).
    unsafe { neon_hybrid::hash_pairs(&table[..count], key, flags, CHUNK_START, CHUNK_END | ROOT, message_len, out.as_flattened_mut()) };
    true
}

/// Prefetch the code of the kernels hashing `len` bytes on `platform`
/// runs: SME2's from 16 chunks, else the hybrids', chunks to root.
#[cfg(all(blake3_neon, feature = "std"))]
fn prefetch_kernels(len: usize, platform: Platform) {
    #[cfg(blake3_sme2)]
    if matches!(platform, Platform::SME2) && len >= SME2_SIZED_LEN {
        sme2::prefetch_code();
        // Below the flat walk, sixteen chunks' values merge on the hybrids.
        if !sme2::flat_takes(len) {
            neon_hybrid::prefetch_parent_code(16);
        }
        return;
    }
    // Below 16 chunks the SME2 platform runs the hybrids too.
    #[cfg(blake3_sme2)]
    let hybrids = matches!(platform, Platform::NEON | Platform::SME2);
    #[cfg(not(blake3_sme2))]
    let hybrids = matches!(platform, Platform::NEON);
    if hybrids && neon_hybrid::sha3_detected() {
        let whole = (len / CHUNK_LEN).min(16);
        neon_hybrid::prefetch_tree_code(whole, if whole == 16 { 0 } else { len % CHUNK_LEN });
    }
}

/// Prefetch the code a fresh Hasher's update of `len` bytes runs: the
/// subtrees its loop cuts (hasher::subtree_len from counter 0, up to the last
/// chunk, which waits in the chunk state), each one's kernels.
#[cfg(all(blake3_neon, feature = "std"))]
fn prefetch_update_kernels(len: usize, platform: Platform) {
    if !neon_hybrid::sha3_detected() {
        return;
    }
    let (mut counter, mut left) = (0u64, len);
    while left > CHUNK_LEN {
        let subtree = hasher::subtree_len(counter, left);
        let chunks = subtree / CHUNK_LEN;
        #[cfg(blake3_sme2)]
        if matches!(platform, Platform::SME2) && chunks >= 16 {
            sme2::prefetch_code();
            if !sme2::flat_takes(subtree) {
                neon_hybrid::prefetch_parent_code(16);
            }
            counter += chunks as u64;
            left -= subtree;
            continue;
        }
        neon_hybrid::prefetch_tree_code(chunks.min(16), 0);
        counter += chunks as u64;
        left -= subtree;
    }
    let _ = platform;
}

/// The smallest input the SME2 kernels take part in: one group of sixteen
/// chunks. Below it the SME2 platform runs the NEON hybrids.
pub(crate) const SME2_SIZED_LEN: usize = 16 * CHUNK_LEN;

/// The fewest messages a batch has when the SME2 kernels take part: one
/// group of sixteen messages.
pub(crate) const SME2_SIZED_BATCH: usize = 16;

/// [`hash_serial`] with the tree's kernels chosen by the caller (a pool
/// piece runs the NEON hybrids); one chunk or less runs the scalar kernel
/// on every platform.
#[inline]
fn hash_serial_on(input: &[u8], key: &CVWords, flags: u8, platform: Platform) -> Hash {
    Hash(tree::hash(&tree::PlatformKernels { key, flags, platform }, input))
}

/// [`hash_many`] in the mode of `key` and `flags`, on the calling thread.
#[inline]
pub(crate) fn hash_many_serial(input: &[u8], message_len: usize, key: &CVWords, flags: u8, out: &mut [[u8; OUT_LEN]]) {
    hash_many_serial_from(Platform::detect(), input, message_len, key, flags, out);
}

/// [`hash_many_serial`] from `platform` (the self-test's way in, as
/// [`hash_serial_from`]).
#[inline]
fn hash_many_serial_from(platform: Platform, input: &[u8], message_len: usize, key: &CVWords, flags: u8, out: &mut [[u8; OUT_LEN]]) {
    let turn = platform::Sme2Turn::take(platform, many::sme2_sized(message_len, out.len()));
    // A small batch of one-block messages after a pause fetches its kernels'
    // code in parallel (as hash_serial does): its time is mostly that code.
    #[cfg(all(blake3_neon, feature = "std"))]
    if message_len <= BLOCK_LEN && (1..=PREFETCH_BATCH_BELOW).contains(&out.len()) && batch_after_pause(input, message_len, key, flags, out, turn.platform()) {
        return;
    }
    many::hash_many_on(input, message_len, key, flags, out, turn.platform());
}

mod hasher;
use hasher::CvStack;

mod incremental;
pub(crate) use incremental::*;

/// An incremental reader for extended output, returned by
/// [`Hasher::finalize_xof`](struct.Hasher.html#method.finalize_xof).
///
/// Shorter BLAKE3 outputs are prefixes of longer ones, and explicitly requesting a short output is
/// equivalent to truncating the default-length output. Note that this is a difference between
/// BLAKE2 and BLAKE3.
///
/// # Security notes
///
/// Outputs shorter than the default length of 32 bytes (256 bits) provide less security. An N-bit
/// BLAKE3 output is intended to provide N bits of first and second preimage resistance and N/2
/// bits of collision resistance, for any N up to 256. Longer outputs don't provide any additional
/// security.
///
/// Avoid relying on the secrecy of the output offset, that is, the number of output bytes read or
/// the arguments to [`seek`](struct.OutputReader.html#method.seek) or
/// [`set_position`](struct.OutputReader.html#method.set_position). [_Block-Cipher-Based Tree
/// Hashing_ by Aldo Gunsing](https://eprint.iacr.org/2022/283) shows that an attacker who knows
/// both the message and the key (if any) can easily determine the offset of an extended output.
/// For comparison, AES-CTR has a similar property: if you know the key, you can decrypt a block
/// from an unknown position in the output stream to recover its block index. Callers with strong
/// secret keys aren't affected in practice, but secret offsets are a [design
/// smell](https://en.wikipedia.org/wiki/Design_smell) in any case.
#[derive(Clone)]
pub struct OutputReader {
    inner: Output,
    position_within_block: u8,
}

impl OutputReader {
    fn new(inner: Output) -> Self {
        Self { inner, position_within_block: 0 }
    }

    // This helper function handles both the case where the output buffer is
    // shorter than one block, and the case where our position_within_block is
    // non-zero.
    fn fill_one_block(&mut self, buf: &mut &mut [u8]) {
        let output_block: [u8; BLOCK_LEN] = self.inner.root_output_block();
        let output_bytes = &output_block[self.position_within_block as usize..];
        let take = cmp::min(buf.len(), output_bytes.len());
        buf[..take].copy_from_slice(&output_bytes[..take]);
        self.position_within_block += take as u8;
        if self.position_within_block == BLOCK_LEN as u8 {
            self.inner.counter += 1;
            self.position_within_block = 0;
        }
        // Advance the dest buffer. mem::take() is a borrowck workaround.
        *buf = &mut core::mem::take(buf)[take..];
    }

    /// Fill a buffer with output bytes and advance the position of the
    /// `OutputReader`. This is equivalent to [`Read::read`], except that it
    /// doesn't return a `Result`. Both methods always fill the entire buffer.
    ///
    /// Note that `OutputReader` doesn't buffer output bytes internally, so
    /// calling `fill` repeatedly with a short-length or odd-length slice will
    /// end up performing the same compression multiple times. If you're
    /// reading output in a loop, prefer a slice length that's a multiple of
    /// [`BLOCK_LEN`] (64 bytes).
    ///
    /// The maximum output size of BLAKE3 is 2<sup>64</sup>-1 bytes. If you try
    /// to extract more than that, for example by seeking near the end and
    /// reading further, the behavior is unspecified.
    ///
    /// [`Read::read`]: #method.read
    pub fn fill(&mut self, mut buf: &mut [u8]) {
        if buf.is_empty() {
            return;
        }

        // If we're partway through a block, try to get to a block boundary.
        if self.position_within_block != 0 {
            self.fill_one_block(&mut buf);
        }

        let full_blocks = buf.len() / BLOCK_LEN;
        let full_blocks_len = full_blocks * BLOCK_LEN;
        if full_blocks > 0 {
            debug_assert_eq!(0, self.position_within_block);
            self.inner.platform.xof_many(
                &self.inner.input_chaining_value,
                &self.inner.block,
                self.inner.block_len,
                self.inner.counter,
                self.inner.flags | ROOT,
                &mut buf[..full_blocks_len],
            );
            self.inner.counter += full_blocks as u64;
            buf = &mut buf[full_blocks * BLOCK_LEN..];
        }

        if !buf.is_empty() {
            debug_assert!(buf.len() < BLOCK_LEN);
            self.fill_one_block(&mut buf);
            debug_assert!(buf.is_empty());
        }
    }

    /// Return the current read position in the output stream. This is
    /// equivalent to [`Seek::stream_position`], except that it doesn't return
    /// a `Result`. The position of a new `OutputReader` starts at 0, and each
    /// call to [`fill`] or [`Read::read`] moves the position forward by the
    /// number of bytes read.
    ///
    /// [`Seek::stream_position`]: #method.stream_position
    /// [`fill`]: #method.fill
    /// [`Read::read`]: #method.read
    pub fn position(&self) -> u64 {
        self.inner.counter * BLOCK_LEN as u64 + self.position_within_block as u64
    }

    /// Seek to a new read position in the output stream. This is equivalent to
    /// calling [`Seek::seek`] with [`SeekFrom::Start`], except that it doesn't
    /// return a `Result`.
    ///
    /// [`Seek::seek`]: #method.seek
    /// [`SeekFrom::Start`]: https://doc.rust-lang.org/std/io/enum.SeekFrom.html
    pub fn set_position(&mut self, position: u64) {
        self.position_within_block = (position % BLOCK_LEN as u64) as u8;
        self.inner.counter = position / BLOCK_LEN as u64;
    }
}

// Don't derive(Debug), because the state may be secret.
impl fmt::Debug for OutputReader {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("OutputReader").field("position", &self.position()).finish()
    }
}

#[cfg(feature = "std")]
impl std::io::Read for OutputReader {
    #[inline]
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.fill(buf);
        Ok(buf.len())
    }
}

#[cfg(feature = "std")]
impl std::io::Seek for OutputReader {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        let max_position = u64::MAX as i128;
        let target_position: i128 = match pos {
            std::io::SeekFrom::Start(x) => x as i128,
            std::io::SeekFrom::Current(x) => self.position() as i128 + x as i128,
            std::io::SeekFrom::End(_) => {
                return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "seek from end not supported"));
            }
        };
        if target_position < 0 {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "seek before start"));
        }
        self.set_position(cmp::min(target_position, max_position) as u64);
        Ok(self.position())
    }
}

#[cfg(feature = "zeroize")]
impl Zeroize for OutputReader {
    fn zeroize(&mut self) {
        // Destructuring to trigger compile error as a reminder to update this impl.
        let Self { inner, position_within_block } = self;

        inner.zeroize();
        position_within_block.zeroize();
    }
}
