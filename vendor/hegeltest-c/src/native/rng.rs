//! The random source the native engine draws from.
//!
//! [`EngineRng`] is the single RNG type threaded through every random draw in
//! the native engine (the `biased_*_sample` samplers, the data-tree novel
//! prefix walk, targeting). It is an enum over two sources:
//!
//!   * [`EngineRng::Prng`] — a seeded [`SmallRng`]. This is the default and
//!     drives reproducible, seed-controlled generation.
//!   * [`EngineRng::Antithesis`] — asks the Antithesis fuzzer for every
//!     draw through its instrumentation library (`fuzz_get_random` from
//!     libvoidstar, the function its SDKs draw from). Selected by
//!     `Backend::Urandom` inside Antithesis, which the shipped `workload`
//!     profile does automatically. It exists so that the fuzzer controls
//!     every choice the engine makes, rather than only the seed of a PRNG
//!     that then expands deterministically, and sees each draw as a request
//!     of its own.
//!   * [`EngineRng::Urandom`] — reads fresh bytes from the OS random device
//!     (`/dev/urandom`) on every draw. What `Backend::Urandom` falls back to
//!     when the Antithesis library is not there to ask: Antithesis's fuzzer
//!     controls the random device too, just without seeing the draws as
//!     draws. Mirrors Hypothesis's `URandomProvider`, which is the ordinary
//!     `HypothesisProvider` with its `Random` swapped for one reading from
//!     `/dev/urandom`.
//!
//! The `Prng` variant delegates each method to the inner `SmallRng`'s native
//! method (rather than routing everything through `fill_bytes`) so that a given
//! seed produces the exact same stream it did before `EngineRng` existed.

use core::convert::Infallible;

use rand::TryRng;
use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};

use crate::sys::{self, FuzzSource};

/// A source of randomness reading directly from the OS random device.
///
/// Stateless: every read opens `/dev/urandom` fresh with no userspace
/// buffering and reads exactly the requested number of bytes. Opening fresh
/// per read (rather than holding an open handle or buffering ahead) is
/// deliberate — it ensures an external controller hooking `/dev/urandom`
/// reads (Antithesis) observes each draw as its own read of exactly the size
/// the engine needs. Mirrors Hypothesis's `URandom._urandom`, which opens
/// with `buffering=0`.
#[derive(Debug, Clone, Copy)]
pub struct UrandomRng;

impl UrandomRng {
    fn read_exact(dst: &mut [u8]) {
        sys::urandom(dst).expect("failed to read from the OS random device");
    }
}

/// The random source the native engine draws from. See the module docs.
#[derive(Debug)]
pub enum EngineRng {
    /// A seeded pseudo-random generator (the default backend).
    Prng(SmallRng),
    /// Every draw requested from the Antithesis fuzzer (the urandom backend
    /// inside Antithesis).
    Antithesis(FuzzSource),
    /// Fresh OS entropy from the OS random device on every draw (the
    /// urandom backend elsewhere).
    Urandom(UrandomRng),
}

impl EngineRng {
    /// A PRNG seeded deterministically from `seed`.
    ///
    /// The stream is `rand::SmallRng`'s, which is not guaranteed stable
    /// across `rand` releases or between 32- and 64-bit targets — so a seed
    /// reproduces a run on the same build, not across arbitrary
    /// architectures or dependency bumps. `seeded_stream_is_pinned` in the
    /// rng tests pins a few literal outputs so a silent stream change on a
    /// `rand` upgrade is caught rather than quietly breaking stored seeds.
    pub fn seeded(seed: u64) -> Self {
        EngineRng::Prng(SmallRng::seed_from_u64(seed))
    }

    /// A PRNG seeded from the operating system's entropy source. Each process
    /// run draws a different stream. If the entropy source fails, a warning
    /// is written to stderr and the seed falls back to zero — a degraded but
    /// functional PRNG whose runs all draw the same stream.
    pub fn from_os() -> Self {
        let mut seed = [0u8; 8];
        let entropy_ok = sys::entropy(&mut seed).is_ok();
        Self::from_os_or_zero(u64::from_le_bytes(seed), entropy_ok)
    }

    fn from_os_or_zero(seed: u64, entropy_ok: bool) -> Self {
        if entropy_ok {
            return Self::seeded(seed);
        }
        sys::stderr_line(
            "warning: reading entropy from the OS failed; falling back to a \
             fixed zero seed, so every run will generate the same sequence.",
        );
        Self::seeded(0)
    }

    /// The urandom backend: inside Antithesis, every draw is requested from
    /// its fuzzer; elsewhere, every draw reads fresh from the OS random
    /// device.
    ///
    /// On platforms without an OS random device (Windows) there is nothing
    /// to read, so this falls back to an OS-seeded PRNG — matching
    /// Hypothesis, which warns and falls back to `backend="hypothesis"`
    /// there.
    pub fn urandom() -> Self {
        Self::urandom_or_fallback(crate::antithesis::fuzz_source(), sys::urandom_available())
    }

    fn urandom_or_fallback(fuzz: Option<FuzzSource>, available: bool) -> Self {
        if let Some(source) = fuzz {
            return EngineRng::Antithesis(source);
        }
        if available {
            return EngineRng::Urandom(UrandomRng);
        }
        sys::stderr_line(
            "warning: the urandom backend reads the OS random device, which is \
             not available on this platform; falling back to an OS-seeded PRNG \
             (equivalent to the default backend).",
        );
        Self::from_os()
    }

    /// Derive an independent child RNG for a sub-task (e.g. a single test
    /// case in a batch).
    ///
    /// For [`EngineRng::Prng`] this seeds a fresh `SmallRng` from the parent —
    /// the same `SmallRng::from_rng` derivation the engine used before
    /// `EngineRng` existed, so seeded trajectories are unchanged. The other
    /// two are stateless views of an external source — the fuzzer or the OS
    /// random device — so the child is simply another reader of the same.
    pub fn spawn(&mut self) -> EngineRng {
        match self {
            EngineRng::Prng(rng) => EngineRng::Prng(SmallRng::from_rng(rng)),
            EngineRng::Antithesis(source) => EngineRng::Antithesis(*source),
            EngineRng::Urandom(_) => EngineRng::Urandom(UrandomRng),
        }
    }

    /// One byte of randomness: exactly the byte a one-byte `fill_bytes` would
    /// write, without the slice bookkeeping that call pays.
    ///
    /// For [`EngineRng::Prng`] this is the low byte of one native word of the
    /// inner `SmallRng`, which is what `SmallRng::fill_bytes` writes for a
    /// one-byte destination (it fills little-endian words and truncates the
    /// last one), so seeded streams are unchanged. The word width mirrors
    /// `SmallRng`'s own choice of generator: 64 bits on 64-bit targets, 32
    /// bits elsewhere. The other variants read one byte from their external
    /// source, so the fuzzer still sees a one-byte request.
    #[inline]
    pub fn next_byte(&mut self) -> u8 {
        match self {
            #[cfg(target_pointer_width = "64")]
            EngineRng::Prng(rng) => rng.next_u64() as u8,
            #[cfg(not(target_pointer_width = "64"))]
            EngineRng::Prng(rng) => rng.next_u32() as u8,
            _ => self.external_byte(),
        }
    }

    /// One byte from the external source, the non-PRNG arm of
    /// [`Self::next_byte`].
    #[inline(never)]
    fn external_byte(&self) -> u8 {
        let mut buf = [0u8; 1];
        self.fill_external(&mut buf);
        buf[0]
    }

    /// Fill `dst` from the external source, for the two non-PRNG variants.
    fn fill_external(&self, dst: &mut [u8]) {
        match self {
            EngineRng::Antithesis(source) => source.fill(dst),
            _ => UrandomRng::read_exact(dst),
        }
    }
}

impl TryRng for EngineRng {
    type Error = Infallible;

    fn try_next_u32(&mut self) -> Result<u32, Infallible> {
        Ok(match self {
            EngineRng::Prng(rng) => rng.next_u32(),
            _ => {
                let mut buf = [0u8; 4];
                self.fill_external(&mut buf);
                u32::from_le_bytes(buf)
            }
        })
    }

    fn try_next_u64(&mut self) -> Result<u64, Infallible> {
        Ok(match self {
            EngineRng::Prng(rng) => rng.next_u64(),
            _ => {
                let mut buf = [0u8; 8];
                self.fill_external(&mut buf);
                u64::from_le_bytes(buf)
            }
        })
    }

    fn try_fill_bytes(&mut self, dst: &mut [u8]) -> Result<(), Infallible> {
        match self {
            EngineRng::Prng(rng) => rng.fill_bytes(dst),
            _ => self.fill_external(dst),
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "../../tests/embedded/native/rng_tests.rs"]
mod tests;
