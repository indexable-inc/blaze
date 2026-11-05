//! The convenience API: one call per hash, every feature of BLAKE3 (the
//! plain hash, the keyed hash, key derivation, extended output), any
//! slice, the result returned. It does without what makes hashing fast:
//! other cores, batches, streams, a message fed in pieces. Those are the
//! [`performance`](crate::performance) calls, for power users.

use crate::{Hash, IV, Mode, OutputReader};

/// The hash of `input`, on the calling thread.
///
/// ```
/// # blake3_servil::initialize();
/// let hash = blake3_servil::convenience::hash(b"foo");
/// # let hash1 = hash;
///
/// let hash = blake3_servil::performance::Hasher::new(blake3_servil::Mode::Plain).update(b"foo").finalize();
/// # let hash2 = hash;
/// # assert_eq!(hash1, hash2);
/// ```
///
/// For a keyed hash or key derivation, see [`hash_with`]; for output sizes
/// other than 32 bytes, [`hash_xof`]. For speed (other cores, batches,
/// streams, a message fed in pieces), the [`performance`](crate::performance)
/// calls.
///
/// **Speed**, nonstop on an Apple M4 Max: about 50 ns for 64 bytes, 0.7 µs
/// for 1 KiB, 4 GB/s at 16 KiB, and 6 GB/s from 1 MiB. A call after a pause
/// or after other work takes longer (its code and data have left the
/// CPU's caches): 64 bytes then take about 0.2 to 0.6 µs. On Apple M4 and
/// later, threads that hash inputs of 16 KiB or more at the same moment
/// share one accelerator: one runs at full speed and the others at about
/// two thirds of it.
///
/// The first hash in a process first runs a self-test (see the crate's
/// documentation), under 0.2 ms; [`initialize`](crate::initialize) moves it to start-up.
// Inlined into its caller: its one call, out of line, sat apart from
// hash_serial, one more code line to fetch from DRAM in a call after a pause
// (i7-12700K, after idling, 64 B: -16.5%, 8 fresh layouts a side).
#[inline]
pub fn hash(input: &[u8]) -> Hash {
    crate::hash_serial(input, IV, 0)
}

/// [`hash`] in any [`Mode`]: the plain hash, the keyed hash, or key
/// derivation.
///
/// ```
/// # blake3_servil::initialize();
/// use blake3_servil::{Mode, convenience};
/// let key = [7u8; 32];
/// let mac = convenience::hash_with(Mode::Keyed(&key), b"foo");
/// assert_eq!(mac, blake3_servil::performance::Hasher::new(Mode::Keyed(&key)).update(b"foo").finalize());
/// ```
#[inline]
pub fn hash_with(mode: Mode, input: &[u8]) -> Hash {
    let mut room = core::mem::MaybeUninit::uninit();
    let (key, flags) = mode.key(&mut room);
    crate::hash_serial(input, key, flags)
}

/// The extended output of `input` in `mode`: an [`OutputReader`] that
/// gives as many bytes as the program reads (the first 32 are
/// [`hash_with`]'s).
///
/// ```
/// # blake3_servil::initialize();
/// use blake3_servil::{Mode, convenience};
/// let mut out = [0u8; 100];
/// convenience::hash_xof(Mode::Plain, b"foo").fill(&mut out);
/// assert_eq!(&out[..32], convenience::hash(b"foo").as_bytes());
/// ```
pub fn hash_xof(mode: Mode, input: &[u8]) -> OutputReader {
    crate::Hasher::new(mode).update(input).finalize_xof()
}
