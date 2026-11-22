//! The one seam every strategy of the crate is chosen at (rule 78), chosen by the caller once and
//! passed in (rule 75): how a lone block is compressed ([`Single`]), how many chunks, parents or
//! short inputs go side by side ([`Degree`]), how a batch of two runs ([`Pair`]), and which
//! vector instructions the wide code may use
//! ([`Isa`], a proof that the CPU has them). Every [`Engine`] computes the same bytes, BLAKE3's;
//! `tests/engines.rs` holds each one to the official crate at every length to five chunks and
//! one byte, one shot and streamed in drawn pieces, in every mode.
//!
//! The entries that take no engine (`hash`, `Hasher::new`, `hash_many`, ...) run
//! [`Engine::detected`]: the defaults below on this CPU, unchanged from before the seam
//! existed. On aarch64 that is a constant; on x86-64 it reads [`crate::cpu`]'s process-wide
//! cache of CPUID, the one ambient read left in the crate (D170 moves it to blaze-sys, which
//! will make the value once and hand it in).

/// How one block is compressed where only one is ready: a chunk hashed alone, a chunk's last
/// block, a lone parent, the root, every input of at most a chunk hashed by itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Single {
    /// The sixteen state words in sixteen general registers, each half-round's four G
    /// functions side by side on the core's integer units (`crate::compress`). The default.
    Scalar,
    /// The state as four rows of four `u32` lanes (`core::simd::u32x4`): a round is the column
    /// step on whole rows, the diagonal turned into columns by rotating rows 1 to 3 by one, two
    /// and three lanes, the diagonal step, and the turn undone (`crate::rows`). What the
    /// official crate runs on SSE4.1 and AVX-512; on NEON it has no such form.
    Rows,
}

/// How many independent compressions run side by side: whole chunks of one input, parents of
/// one tree level, or short inputs of `hash_many`. A batch narrower than the degree runs at the
/// narrowest power of two that holds it (three chunks at four lanes, the fourth lane idle).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Degree {
    /// One at a time, by [`Single`]: no lanes.
    One,
    /// Two lanes: half a NEON or SSE register a word.
    Two,
    /// Four lanes: one NEON or SSE register a word.
    Four,
    /// Eight lanes: one AVX2 register a word, or two NEON registers interleaved.
    Eight,
    /// Sixteen lanes: one AVX-512 register a word, or four NEON registers.
    Sixteen,
}

impl Degree {
    /// Every degree, narrowest first: what the tests and the cost table run.
    pub const ALL: [Degree; 5] = [Degree::One, Degree::Two, Degree::Four, Degree::Eight, Degree::Sixteen];

    /// The lanes this degree runs.
    pub const fn lanes(self) -> usize {
        match self {
            Degree::One => 1,
            Degree::Two => 2,
            Degree::Four => 4,
            Degree::Eight => 8,
            Degree::Sixteen => 16,
        }
    }
}

/// How a batch exactly two wide runs, where the degree is two or more: two chunks of one input
/// (a 2 KiB input, or a subtree's last two), two parents of one level (the last level below a
/// root of four chunks). Wider batches always run in lanes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Pair {
    /// Two `core::simd` lanes: half a NEON or SSE register a word. The default on x86-64
    /// ([`Isa::default_pair`]).
    Lanes,
    /// Two scalar compressions in general registers, step for step (`crate::pair`): eight
    /// independent chains a half-round on the integer units, one `ror` a rotation.
    Interleaved,
}

impl Pair {
    /// Both forms: what the tests and the cost table run.
    pub const ALL: [Pair; 2] = [Pair::Lanes, Pair::Interleaved];
}

/// The vector instructions the wide code may use, and the proof that this CPU has them: a value
/// other than [`Isa::BASELINE`] is made only by an `unsafe` constructor whose caller promised
/// the feature, or by [`Isa::detected`] after CPUID said so.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Isa {
    pub(crate) kind: IsaKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum IsaKind {
    /// The target's baseline: NEON on aarch64, SSE2 on x86-64.
    Baseline,
    /// x86-64's AVX2: the lanes compiled inside `#[target_feature(enable = "avx2")]` functions.
    #[cfg(target_arch = "x86_64")]
    Avx2,
    /// x86-64's AVX-512F: the lanes compiled inside `#[target_feature(enable = "avx512f")]`.
    #[cfg(target_arch = "x86_64")]
    Avx512,
}

impl Isa {
    /// The target's baseline vectors, on every CPU of the target.
    pub const BASELINE: Isa = Isa { kind: IsaKind::Baseline };

    /// AVX2's 256-bit vectors.
    ///
    /// # Safety
    ///
    /// The CPU this value is used on has AVX2 and the OS saves its registers (for example,
    /// `is_x86_feature_detected!("avx2")` said so): every function given this value may run
    /// AVX2 instructions.
    #[cfg(target_arch = "x86_64")]
    #[allow(unsafe_code, reason = "a constructor whose contract is the CPU's AVX2")]
    pub const unsafe fn avx2_unchecked() -> Isa {
        Isa { kind: IsaKind::Avx2 }
    }

    /// AVX-512F's 512-bit vectors.
    ///
    /// # Safety
    ///
    /// The CPU this value is used on has AVX-512F and AVX2 and the OS saves their registers:
    /// every function given this value may run AVX-512F and AVX2 instructions.
    #[cfg(target_arch = "x86_64")]
    #[allow(unsafe_code, reason = "a constructor whose contract is the CPU's AVX-512F")]
    pub const unsafe fn avx512_unchecked() -> Isa {
        Isa { kind: IsaKind::Avx512 }
    }

    /// The widest this CPU has, by [`crate::cpu`]: AVX-512F, else AVX2, else the baseline. On
    /// aarch64 the baseline, a constant.
    pub fn detected() -> Isa {
        #[cfg(target_arch = "x86_64")]
        {
            use crate::cpu;
            // AVX-512F's CPUs all have AVX2 (every one Intel and AMD made), and the OS check of
            // `cpu` covers both register files.
            if cpu::has(cpu::Feature::Avx512f) && cpu::has(cpu::Feature::Avx2) {
                return Isa { kind: IsaKind::Avx512 };
            }
            if cpu::has(cpu::Feature::Avx2) {
                return Isa { kind: IsaKind::Avx2 };
            }
        }
        Isa::BASELINE
    }

    /// The instructions' name, for reports.
    pub const fn name(self) -> &'static str {
        match self.kind {
            IsaKind::Baseline => {
                if cfg!(target_arch = "aarch64") {
                    "neon"
                } else if cfg!(target_arch = "x86_64") {
                    "sse2"
                } else {
                    "baseline"
                }
            }
            #[cfg(target_arch = "x86_64")]
            IsaKind::Avx2 => "avx2",
            #[cfg(target_arch = "x86_64")]
            IsaKind::Avx512 => "avx512f",
        }
    }

    /// The degree the crate's entries run with these instructions: 16 lanes on AVX-512F, else
    /// 8 (one AVX2 register a word, two NEON or SSE2 ones). On the owner's M5 Max a block a lane
    /// took 25.1 ns at 4 lanes, 16.0 at 8 and 14.3 at 16 (blaze-int-1112-notes/nightly-probes/
    /// blake3_lanes.rs); `examples/cost.rs` prints MB/s by degree so the default per CPU is a
    /// measured choice, and it stays this until that table says otherwise.
    ///
    /// That table on the owner's M5 Max, 2026-11-20 22:13 (machine at load 14; notes
    /// rounds/wave-1119/t34/blake3-cost.txt), MB/s at 8 against 16 lanes on NEON: 3306 and 3314
    /// at 8 KiB, 3482 and 3755 at 64 KiB, 3470 and 3638 at 16 MiB, and equal below 8 KiB: so
    /// NEON runs 16. SSE2 has no such table yet and stays at 8.
    pub const fn default_degree(self) -> Degree {
        match self.kind {
            IsaKind::Baseline => {
                if cfg!(target_arch = "aarch64") {
                    Degree::Sixteen
                } else {
                    Degree::Eight
                }
            }
            #[cfg(target_arch = "x86_64")]
            IsaKind::Avx2 => Degree::Eight,
            #[cfg(target_arch = "x86_64")]
            IsaKind::Avx512 => Degree::Sixteen,
        }
    }

    /// How the crate's entries run a batch two wide with these instructions. The same table:
    /// a 2 KiB input (two chunks) ran at 1138 MB/s in two NEON lanes and 1602 interleaved in
    /// general registers (the official crate: 1329), with every other size within 2%: so NEON
    /// interleaves. The x86-64 forms have no such table yet and stay in lanes.
    pub const fn default_pair(self) -> Pair {
        match self.kind {
            IsaKind::Baseline => {
                if cfg!(target_arch = "aarch64") {
                    Pair::Interleaved
                } else {
                    Pair::Lanes
                }
            }
            #[cfg(target_arch = "x86_64")]
            IsaKind::Avx2 | IsaKind::Avx512 => Pair::Lanes,
        }
    }
}

/// What an [`Engine`] is made of: every choice named, none defaulted (rule 83).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EngineOpts {
    pub single: Single,
    pub degree: Degree,
    pub pair: Pair,
    pub isa: Isa,
}

/// The strategies a hash runs by: made once by the caller, passed to every `*_on` entry and
/// kept by a [`crate::Hasher`] for its life.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Engine {
    pub(crate) single: Single,
    pub(crate) degree: Degree,
    pub(crate) pair: Pair,
    pub(crate) isa: Isa,
}

impl Engine {
    /// One block at a time, by the scalar compression, on every target: the twin every other
    /// engine is tested against.
    pub const SCALAR: Engine =
        Engine { single: Single::Scalar, degree: Degree::One, pair: Pair::Lanes, isa: Isa::BASELINE };

    pub const fn new(opts: EngineOpts) -> Engine {
        Engine { single: opts.single, degree: opts.degree, pair: opts.pair, isa: opts.isa }
    }

    /// What the entries without an engine run: [`Single::Scalar`], and this CPU's widest
    /// [`Isa`] at its [`Isa::default_degree`] and [`Isa::default_pair`].
    #[inline]
    pub fn detected() -> Engine {
        let isa = Isa::detected();
        Engine { single: Single::Scalar, degree: isa.default_degree(), pair: isa.default_pair(), isa }
    }

    pub const fn single(self) -> Single {
        self.single
    }

    pub const fn degree(self) -> Degree {
        self.degree
    }

    pub const fn pair(self) -> Pair {
        self.pair
    }

    pub const fn isa(self) -> Isa {
        self.isa
    }
}
