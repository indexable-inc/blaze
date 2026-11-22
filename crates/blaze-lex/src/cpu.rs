//! The vector path a call runs on, chosen once by the caller and passed in (rule 75).

#[cfg(target_arch = "x86_64")]
use crate::vector::Avx2;

/// Which of the three paths a call takes. Every path gives the same answer on every input; the
/// tests run each against the others.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cpu {
    pub(crate) path: Path,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Path {
    /// A byte at a time: the twin every vector path is tested against.
    Scalar,
    /// `core::simd` at 16 lanes, the target's baseline vectors (NEON, SSE2).
    Portable,
    /// `core::simd` at 32 lanes inside `#[target_feature(enable = "avx2")]` functions, with
    /// `vpshufb` for the table lookups (`swizzle_dyn` picks its instruction by the crate's
    /// static features, so it cannot see AVX2 enabled per function).
    #[cfg(target_arch = "x86_64")]
    Avx2(Avx2),
}

impl Cpu {
    /// The scalar twin.
    pub const SCALAR: Cpu = Cpu { path: Path::Scalar };

    /// The target's baseline vectors: NEON on aarch64, SSE2 on x86-64. On x86-64 without SSSE3
    /// the table lookups are emulated a lane at a time, so an x86-64 caller passes
    /// [`Cpu::avx2_unchecked`] where the CPU has AVX2.
    pub const BASELINE: Cpu = Cpu { path: Path::Portable };

    /// AVX2's 32-byte vectors.
    ///
    /// # Safety
    ///
    /// The CPU this value is used on has AVX2 (for example, `is_x86_feature_detected!("avx2")`
    /// said so): every function that takes this `Cpu` runs AVX2 instructions.
    #[cfg(target_arch = "x86_64")]
    pub const unsafe fn avx2_unchecked() -> Cpu {
        // SAFETY: this function's caller promised the CPU has AVX2, which is `Avx2`'s contract.
        let avx2 = unsafe { Avx2::new_unchecked() };
        Cpu { path: Path::Avx2(avx2) }
    }

    /// The path's name, for reports.
    pub const fn name(self) -> &'static str {
        match self.path {
            Path::Scalar => "scalar",
            Path::Portable => "baseline vectors",
            #[cfg(target_arch = "x86_64")]
            Path::Avx2(_) => "avx2",
        }
    }

    /// This path and every narrower one, widest first: what a test runs against each other.
    pub fn and_narrower(self) -> impl Iterator<Item = Cpu> {
        let wide = match self.path {
            Path::Scalar | Path::Portable => None,
            #[cfg(target_arch = "x86_64")]
            Path::Avx2(_) => Some(self),
        };
        let baseline = match self.path {
            Path::Scalar => None,
            _ => Some(Cpu::BASELINE),
        };
        wide.into_iter().chain(baseline).chain(core::iter::once(Cpu::SCALAR))
    }
}
