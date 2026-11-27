//! Joint AArch64 Machine-Op Equality Saturation + Meet-in-the-Middle Bitvector Superoptimizer
//! with Persistent BLAKE3 Cache (`blaze_lir::simplify::mach_egraph`).
//!
//! Solves the classical compiler phase-ordering gap between algebraic simplification and AArch64
//! instruction selection by saturating pure scalar integer, comparison, and conditional-select
//! DAGs directly under an explicit **Apple M5 Max execution-port & macro-op fusion cost model**
//! ([`AppleM5PortModel`]):
//!
//! - Models Apple M4/M5 hardware reality: 10-wide decode, 6 ALU pipes, 4 NEON/FP pipes, 3 LD/ST
//!   pipes, zero-cycle `mov`/`fmov` renaming, single-cycle fused `bic`/`orn`/`eon`, shifted and
//!   rotated ALU (`add/sub/and/orr/eor` + `lsl`/`lsr`/`asr`/`ror`), `extr`/`ror`, `ubfx`/`bfxil`,
//!   `csinc`/`csinv`/`csneg`, CSSC `smin`/`smax`/`umin`/`umax`, `madd`/`msub`, and macro-op
//!   branch fusion (`cmp+b.cc`, `adds+b.cc`, `and+cbz`).
//! - Performs bounded multi-round e-graph saturation with fused child-pattern extraction
//!   (explicit iterative worklists; zero recursion).
//! - For compact multi-op pure integer sub-DAGs (`<= 6` ops, `<= 3` inputs, `i32`/`i64`),
//!   computes a machine-independent canonical BLAKE3 digest (domain
//!   `"blaze v2 mach superopt dag 1"`), queries a persistent content-addressable
//!   [`SuperoptCache`], and on cache miss runs a bounded cost-ordered + meet-in-the-middle
//!   enumerative bitvector synthesizer verified against a 64-vector edge-case + SplitMix64
//!   bitvector oracle plus exact 8-bit/4-bit exhaustive equivalence checking.
//! - Serializes and deserializes [`SuperoptCache`] with `BSO1` framing, BLAKE3 integrity seal,
//!   and zero-trust bounds checking (Rule 26 round-trip).
//!
//! Pure sans-IO, `#![no_std]`, `#![forbid(unsafe_code)]`, deterministic (Rule 25), bounded
//! (Rule 18), explicit allocator (`blaze_own::Vec<T, A>` / `blaze_hash::Map`) only.

use core::alloc::Allocator;
use core::fmt;

use blaze_hash::{BuildKeyHasher, Map, MapOpts};
use blaze_ids::Hasher as Blake3Hasher;
use blaze_own::Vec;

use super::{
    SimplifyError,
    dce::{NONE, dead, filled},
};
use crate::{
    Block, Flags, Func, IntCond, Op, Ty, View,
    arith::{binary, compare, iconst_value, sext, unary, wide_value},
    op::{Field, Layout},
    verify,
};

/// Domain separator for canonical superoptimizer input DAG digests (Rule 25).
const DAG_HASH_DOMAIN: &str = "blaze v2 mach superopt dag 1";
/// Domain separator for persistent [`SuperoptCache`] integrity seals.
const CACHE_SEAL_DOMAIN: &str = "blaze v2 mach superopt cache 1";
/// Wire format magic bytes for [`SuperoptCache`].
const CACHE_MAGIC: [u8; 4] = *b"BSO1";
/// Current wire format version for [`SuperoptCache`].
const CACHE_VERSION: u32 = 1;
/// Hard cap on entries accepted by [`SuperoptCache::decode_bytes`] (zero-trust bound).
const MAX_CACHE_ENTRIES: usize = 65_536;
/// Maximum ops in a sub-DAG submitted to the bounded enumerative bitvector synthesizer.
const MAX_DAG_OPS: usize = 6;
/// Maximum distinct external inputs in a sub-DAG submitted to the synthesizer.
const MAX_DAG_INPUTS: usize = 3;
/// Maximum instruction length of a synthesized replacement program.
pub const MAX_SYNTH_LEN: usize = 3;
/// Maximum number of 1-op intermediate states held in the meet-in-the-middle table.
const MAX_MITM_STATES: usize = 64;
/// Number of deterministic full-width verification vectors in the bitvector oracle.
const ORACLE_VECTORS: usize = 64;
/// Sentinel infinite cost for Bellman-Ford extraction (small enough that additions never overflow).
const INF_COST: u32 = u32::MAX / 4;

type EId = u32;

// ============================================================================
// Public Types & Options (Rule 83)
// ============================================================================

/// Apple M4/M5 execution-port and macro-op fusion cost model.
///
/// Costs are fixed-point hundredths of a cycle (`100` = 1 cycle / 1 ALU uop on a 6-ALU-pipe,
/// 10-wide-decode Apple M4/M5 P-core with 4 NEON/FP pipes, 3 LD/ST pipes, and zero-cycle
/// `mov`/`fmov` renaming).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AppleM5PortModel {
    /// Frontend decode width (10-wide on Apple M4/M5 P-core).
    pub decode_width: u8,
    /// Integer ALU execution pipes (6 on Apple M4/M5 P-core).
    pub alu_pipes: u8,
    /// NEON/FP execution pipes (4 on Apple M4/M5 P-core).
    pub fp_pipes: u8,
    /// Load/store execution pipes (3 on Apple M4/M5 P-core).
    pub ldst_pipes: u8,
    /// Single-cycle simple ALU (`add`, `sub`, `and`, `orr`, `eor`, `lsl`, `lsr`, `asr`, `ror`,
    /// `csel`, `csinc`, `csinv`, `csneg`, `smin`/`smax`/`umin`/`umax` CSSC).
    pub simple_alu: u32,
    /// Single-cycle fused AArch64 2-op instruction (`bic`, `orn`, `eon`, shifted/rotated ALU,
    /// `extr`/`ror`, `ubfx`/`bfxil`, `csinc`/`csinv`/`csneg`, macro-fused `cmp+b.cc`/`and+cbz`).
    pub fused_alu: u32,
    /// Standalone integer multiply (`mul`).
    pub mul: u32,
    /// Fused multiply-add / multiply-subtract (`madd` / `msub`).
    pub madd: u32,
    /// High-half integer multiply (`umulhi` / `smulhi`).
    pub mulhi: u32,
    /// Integer division (`udiv` / `sdiv`).
    pub div: u32,
    /// Integer remainder (`urem` / `srem`).
    pub rem: u32,
}

impl AppleM5PortModel {
    /// Apple M5 Max P-core execution-port and fusion cost model.
    pub const fn apple_m5() -> Self {
        Self {
            decode_width: 10,
            alu_pipes: 6,
            fp_pipes: 4,
            ldst_pipes: 3,
            simple_alu: 100,
            fused_alu: 100,
            mul: 250,
            madd: 260,
            mulhi: 300,
            div: 900,
            rem: 1000,
        }
    }

    /// Cost of materializing an integer constant into a register on AArch64 (`0` for zero register
    /// `xzr`/`wzr`, small penalty for immediate-sized literals, higher for multi-`movk` literals).
    #[inline]
    pub const fn const_cost(&self, val: u64) -> u32 {
        if val == 0 {
            0
        } else if val <= 4095 {
            10
        } else if is_single_mov_const(val) {
            40
        } else {
            80
        }
    }

    /// Combined cost of macro-op fused compare/test + conditional branch on Apple M5
    /// (`cmp + b.cc`, `adds/subs + b.cc`, `and/tst + cbz/b.cc`).
    #[inline]
    pub const fn macro_op_branch_cost(&self, cond_op: Op) -> u32 {
        match cond_op {
            Op::Icmp | Op::Add | Op::Sub | Op::And => self.fused_alu,
            _ => self.simple_alu + self.simple_alu,
        }
    }

    /// Base standalone execution cost of a LIR integer op under the M5 port model.
    #[inline]
    pub const fn op_base_cost(&self, op: Op) -> u32 {
        match op {
            Op::Add
            | Op::Sub
            | Op::And
            | Op::Or
            | Op::Xor
            | Op::Not
            | Op::Neg
            | Op::Shl
            | Op::Lshr
            | Op::Ashr
            | Op::Rotr
            | Op::Smin
            | Op::Smax
            | Op::Umin
            | Op::Umax
            | Op::Icmp
            | Op::Select
            | Op::Zext
            | Op::Clz
            | Op::Ctz
            | Op::Bswap
            | Op::Bitrev => self.simple_alu,
            // AArch64 has native `ror`, not `rol`; constant `rol` lowers to `ror(W-k)`, variable `rol` needs `neg + ror`.
            Op::Rotl => self.simple_alu + 20,
            Op::Popcnt => 200,
            Op::Mul => self.mul,
            Op::Umulhi | Op::Smulhi => self.mulhi,
            Op::Udiv | Op::Sdiv => self.div,
            Op::Urem | Op::Srem => self.rem,
            _ => 120,
        }
    }

    /// Returns `true` if `val` is a valid low-bitmask `(1 << w) - 1` (`1 <= w < bits`)
    /// encodable as an AArch64 logical bitmask immediate (`and`/`ubfm`/`ubfx`).
    #[inline]
    pub const fn is_low_bitmask(val: u64, bits: u32) -> bool {
        let m = mask_of(bits);
        val > 0 && val < m && val.wrapping_add(1).is_power_of_two()
    }

    /// Returns `true` if `val` is a contiguous run of 1-bits (optionally rotated) in `bits` width,
    /// encodable as an AArch64 logical bitmask immediate.
    #[inline]
    pub const fn is_bitmask_imm(val: u64, bits: u32) -> bool {
        let m = mask_of(bits);
        let v = val & m;
        if v == 0 || v == m {
            return false;
        }
        // Check contiguous run of 1s: `(v >> tz) + 1` is a power of two.
        let tz = v.trailing_zeros();
        let shifted = v >> tz;
        if shifted < u64::MAX && shifted.wrapping_add(1).is_power_of_two() {
            return true;
        }
        // Check rotated run of 1s (i.e. complement is a contiguous run of 1s).
        let inv = (!v) & m;
        let itz = inv.trailing_zeros();
        let ishifted = inv >> itz;
        ishifted < u64::MAX && ishifted.wrapping_add(1).is_power_of_two()
    }
}

/// Options for [`optimize_mach_egraph`] (Rule 83: single plain options struct).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MachEGraphOpts {
    /// Maximum equality-saturation rounds per basic block.
    pub max_iters: u32,
    /// Hard upper bound on total e-nodes per basic block (Rule 18 bounded progress).
    pub max_enodes: u32,
    /// Enable the bounded enumerative / meet-in-the-middle bitvector superoptimizer on cache misses.
    pub enable_synth: bool,
    /// Hard upper bound on candidate evaluations per sub-DAG during enumerative synthesis.
    pub synth_budget: u32,
}

impl MachEGraphOpts {
    /// Default joint machine-op e-graph + bitvector superoptimizer settings for Apple M5.
    pub const fn apple_m5() -> Self {
        Self { max_iters: 4, max_enodes: 1024, enable_synth: true, synth_budget: 2048 }
    }
}

/// Telemetry returned by [`optimize_mach_egraph`].
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct MachEGraphStats {
    pub blocks_optimized: u32,
    pub rewrites_applied: u32,
    pub cost_before: u64,
    pub cost_after: u64,
    pub cache_hits: u32,
    pub synth_wins: u32,
}

/// Output of [`optimize_mach_egraph`]: verified LIR function plus telemetry.
#[derive(Debug)]
pub struct MachEGraphOutput<A: Allocator> {
    pub func: Func<A>,
    pub stats: MachEGraphStats,
}

// ============================================================================
// Persistent Content-Addressed Superoptimizer Cache (`SuperoptCache`)
// ============================================================================

/// Primitive operation inside a synthesized bitvector program (`1..=3` ops).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SynthOp {
    Add,
    Sub,
    Mul,
    And,
    Or,
    Xor,
    Shl,
    Lshr,
    Ashr,
    Rotr,
    Smin,
    Smax,
    Umin,
    Umax,
    Neg,
    Not,
    /// Fused AArch64 `bic` (`a & !b`).
    Bic,
    /// Fused AArch64 `orn` (`a | !b`).
    Orn,
    /// Fused AArch64 `eon` (`a ^ !b`).
    Eon,
    /// Identity copy of operand `a` (0 instructions).
    Copy,
    /// Constant literal `imm`.
    Const,
}

impl SynthOp {
    #[inline]
    const fn to_byte(self) -> u8 {
        match self {
            SynthOp::Add => 0,
            SynthOp::Sub => 1,
            SynthOp::Mul => 2,
            SynthOp::And => 3,
            SynthOp::Or => 4,
            SynthOp::Xor => 5,
            SynthOp::Shl => 6,
            SynthOp::Lshr => 7,
            SynthOp::Ashr => 8,
            SynthOp::Rotr => 9,
            SynthOp::Smin => 10,
            SynthOp::Smax => 11,
            SynthOp::Umin => 12,
            SynthOp::Umax => 13,
            SynthOp::Neg => 14,
            SynthOp::Not => 15,
            SynthOp::Bic => 16,
            SynthOp::Orn => 17,
            SynthOp::Eon => 18,
            SynthOp::Copy => 19,
            SynthOp::Const => 20,
        }
    }

    #[inline]
    const fn from_byte(b: u8) -> Option<Self> {
        match b {
            0 => Some(SynthOp::Add),
            1 => Some(SynthOp::Sub),
            2 => Some(SynthOp::Mul),
            3 => Some(SynthOp::And),
            4 => Some(SynthOp::Or),
            5 => Some(SynthOp::Xor),
            6 => Some(SynthOp::Shl),
            7 => Some(SynthOp::Lshr),
            8 => Some(SynthOp::Ashr),
            9 => Some(SynthOp::Rotr),
            10 => Some(SynthOp::Smin),
            11 => Some(SynthOp::Smax),
            12 => Some(SynthOp::Umin),
            13 => Some(SynthOp::Umax),
            14 => Some(SynthOp::Neg),
            15 => Some(SynthOp::Not),
            16 => Some(SynthOp::Bic),
            17 => Some(SynthOp::Orn),
            18 => Some(SynthOp::Eon),
            19 => Some(SynthOp::Copy),
            20 => Some(SynthOp::Const),
            _ => None,
        }
    }

    #[inline]
    const fn arity(self) -> u8 {
        match self {
            SynthOp::Const => 0,
            SynthOp::Neg | SynthOp::Not | SynthOp::Copy => 1,
            _ => 2,
        }
    }
}

/// One instruction in a synthesized bitvector program.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SynthInst {
    pub op: SynthOp,
    /// Operand `a`: `0..num_inputs` addresses canonical inputs `v0..`; `num_inputs..` addresses
    /// earlier instructions in the same [`SynthProg`].
    pub a: u8,
    /// Operand `b` (for binary ops; `0` for unary/const/copy).
    pub b: u8,
    /// Constant literal value when `op == SynthOp::Const` (else `0`).
    pub imm: u64,
}

impl SynthInst {
    const NOP: Self = Self { op: SynthOp::Const, a: 0, b: 0, imm: 0 };
}

/// Verified synthesized replacement program (`1..=MAX_SYNTH_LEN` ops) over `<= 3` canonical inputs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SynthProg {
    pub num_inputs: u8,
    pub len: u8,
    pub bits: u8,
    pub cost: u32,
    pub insts: [SynthInst; MAX_SYNTH_LEN],
}

/// Cached outcome for a canonical DAG BLAKE3 digest.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CachedRewrite {
    /// Exhaustively searched within budget; no strictly cheaper program exists.
    NoImprovement,
    /// Verified strictly lower-M5-cost equivalent program.
    Better(SynthProg),
}

/// Errors returned by [`SuperoptCache::decode_bytes`] (zero-trust validation).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CacheDecodeError {
    /// Input buffer is shorter than the minimum header + BLAKE3 seal (`44` bytes) or truncated mid-entry.
    Truncated,
    /// Header magic does not equal `b"BSO1"`.
    BadMagic,
    /// Trailing 32-byte BLAKE3 integrity seal does not match payload bytes.
    BadDigest,
    /// Format version is unsupported.
    UnsupportedVersion(u32),
    /// Entry count exceeds [`MAX_CACHE_ENTRIES`].
    TooManyEntries(u32),
    /// An entry contains an invalid tag, width, opcode, length, or out-of-bounds operand reference.
    InvalidEntry,
    /// Unexpected trailing bytes remain between the last entry and the integrity seal.
    TrailingBytes,
}

impl fmt::Display for CacheDecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CacheDecodeError::Truncated => write!(f, "superopt cache bytes are truncated"),
            CacheDecodeError::BadMagic => write!(f, "superopt cache magic is not BSO1"),
            CacheDecodeError::BadDigest => write!(f, "superopt cache BLAKE3 integrity digest mismatch"),
            CacheDecodeError::UnsupportedVersion(v) => {
                write!(f, "superopt cache version {v} is unsupported")
            }
            CacheDecodeError::TooManyEntries(n) => {
                write!(f, "superopt cache entry count {n} exceeds limit {MAX_CACHE_ENTRIES}")
            }
            CacheDecodeError::InvalidEntry => write!(f, "superopt cache contains an invalid rewrite entry"),
            CacheDecodeError::TrailingBytes => write!(f, "superopt cache contains unexpected trailing bytes"),
        }
    }
}

impl core::error::Error for CacheDecodeError {}

/// Persistent, content-addressed BLAKE3 cache mapping canonical DAG digests (`[u8; 32]`) to
/// verified synthesized rewrites.
#[derive(Debug)]
pub struct SuperoptCache<A: Allocator> {
    entries: Map<[u8; 32], CachedRewrite, BuildKeyHasher, A>,
}

impl<A: Allocator + Copy> SuperoptCache<A> {
    /// Creates an empty persistent superoptimizer cache in `alloc`.
    pub fn new_in(alloc: A) -> Self {
        Self {
            entries: Map::new(MapOpts {
                hasher: BuildKeyHasher::default(),
                alloc,
                capacity: None,
            }),
        }
    }

    /// Number of cached DAG digests.
    #[inline]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns `true` if no digests are cached.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// O(1) lookup by canonical DAG BLAKE3 digest.
    #[inline]
    pub fn get(&self, digest: &[u8; 32]) -> Option<CachedRewrite> {
        self.entries.get(digest).copied()
    }

    /// Inserts or updates a cached rewrite for `digest`.
    #[inline]
    pub fn insert(&mut self, digest: [u8; 32], rewrite: CachedRewrite) {
        let _ = self.entries.insert(digest, rewrite);
    }

    /// Deterministically encodes all cache entries into `BSO1` format with a trailing 32-byte
    /// BLAKE3 integrity seal (Rule 25: sorted by digest so output bytes never depend on map order).
    pub fn encode_bytes(&self, alloc: A) -> Vec<u8, A> {
        let mut items: Vec<([u8; 32], CachedRewrite), A> =
            Vec::with_capacity_in(self.entries.len(), alloc);
        for (&digest, &rewrite) in self.entries.iter() {
            items.push((digest, rewrite));
        }
        items.sort_unstable_by(|a, b| a.0.cmp(&b.0));

        let count = to_u32(items.len());
        let mut out: Vec<u8, A> = Vec::with_capacity_in(44 + items.len().saturating_mul(48), alloc);
        out.extend_from_slice(&CACHE_MAGIC);
        out.extend_from_slice(&CACHE_VERSION.to_le_bytes());
        out.extend_from_slice(&count.to_le_bytes());

        for &(digest, rewrite) in items.as_slice() {
            out.extend_from_slice(&digest);
            match rewrite {
                CachedRewrite::NoImprovement => {
                    out.push(0);
                }
                CachedRewrite::Better(prog) => {
                    out.push(1);
                    out.push(prog.num_inputs);
                    out.push(prog.len);
                    out.push(prog.bits);
                    out.extend_from_slice(&prog.cost.to_le_bytes());
                    let n = usize::from(prog.len).min(MAX_SYNTH_LEN);
                    for i in 0..n {
                        if let Some(inst) = prog.insts.get(i) {
                            out.push(inst.op.to_byte());
                            out.push(inst.a);
                            out.push(inst.b);
                            out.extend_from_slice(&inst.imm.to_le_bytes());
                        }
                    }
                }
            }
        }

        let mut h = Blake3Hasher::new(CACHE_SEAL_DOMAIN);
        h.update(out.as_slice());
        let seal = h.finish().0;
        out.extend_from_slice(&seal);
        out
    }

    /// Zero-trust bounds-checked decoder for `BSO1` superoptimizer cache bytes (Rule 26 round-trip).
    ///
    /// Verifies magic, BLAKE3 integrity seal, version, entry count bound, and every entry's
    /// internal DAG invariants before accepting the cache.
    pub fn decode_bytes(bytes: &[u8], alloc: A) -> Result<Self, CacheDecodeError> {
        if bytes.len() < 44 {
            return Err(CacheDecodeError::Truncated);
        }
        let magic = bytes.get(0..4).ok_or(CacheDecodeError::Truncated)?;
        if magic != CACHE_MAGIC {
            return Err(CacheDecodeError::BadMagic);
        }
        let payload_len = bytes.len().checked_sub(32).ok_or(CacheDecodeError::Truncated)?;
        let payload = bytes.get(0..payload_len).ok_or(CacheDecodeError::Truncated)?;
        let stored_seal = bytes.get(payload_len..).ok_or(CacheDecodeError::Truncated)?;

        let mut h = Blake3Hasher::new(CACHE_SEAL_DOMAIN);
        h.update(payload);
        let expected_seal = h.finish().0;
        if stored_seal != expected_seal.as_slice() {
            return Err(CacheDecodeError::BadDigest);
        }

        let mut pos = 4usize;
        let version = read_u32_le(payload, &mut pos).ok_or(CacheDecodeError::Truncated)?;
        if version != CACHE_VERSION {
            return Err(CacheDecodeError::UnsupportedVersion(version));
        }
        let count_u32 = read_u32_le(payload, &mut pos).ok_or(CacheDecodeError::Truncated)?;
        let count = usize::try_from(count_u32).map_err(|_| CacheDecodeError::TooManyEntries(count_u32))?;
        if count > MAX_CACHE_ENTRIES {
            return Err(CacheDecodeError::TooManyEntries(count_u32));
        }

        let mut entries = Map::new(MapOpts {
            hasher: BuildKeyHasher::default(),
            alloc,
            capacity: Some(count),
        });

        for _ in 0..count {
            let digest = read_digest(payload, &mut pos).ok_or(CacheDecodeError::Truncated)?;
            let kind = read_u8(payload, &mut pos).ok_or(CacheDecodeError::Truncated)?;
            let rewrite = match kind {
                0 => CachedRewrite::NoImprovement,
                1 => {
                    let num_inputs = read_u8(payload, &mut pos).ok_or(CacheDecodeError::Truncated)?;
                    let len = read_u8(payload, &mut pos).ok_or(CacheDecodeError::Truncated)?;
                    let bits = read_u8(payload, &mut pos).ok_or(CacheDecodeError::Truncated)?;
                    let cost = read_u32_le(payload, &mut pos).ok_or(CacheDecodeError::Truncated)?;
                    let max_inputs_u8 = u8::try_from(MAX_DAG_INPUTS).unwrap_or(3);
                    let max_len_u8 = u8::try_from(MAX_SYNTH_LEN).unwrap_or(3);
                    if num_inputs > max_inputs_u8 || len == 0 || len > max_len_u8 || (bits != 32 && bits != 64) {
                        return Err(CacheDecodeError::InvalidEntry);
                    }
                    let mut insts = [SynthInst::NOP; MAX_SYNTH_LEN];
                    let n = usize::from(len);
                    for i in 0..n {
                        let op_b = read_u8(payload, &mut pos).ok_or(CacheDecodeError::Truncated)?;
                        let op = SynthOp::from_byte(op_b).ok_or(CacheDecodeError::InvalidEntry)?;
                        let a = read_u8(payload, &mut pos).ok_or(CacheDecodeError::Truncated)?;
                        let b = read_u8(payload, &mut pos).ok_or(CacheDecodeError::Truncated)?;
                        let imm = read_u64_le(payload, &mut pos).ok_or(CacheDecodeError::Truncated)?;
                        let avail = num_inputs.saturating_add(u8::try_from(i).unwrap_or(u8::MAX));
                        match op.arity() {
                            0 => {}
                            1 => {
                                if a >= avail {
                                    return Err(CacheDecodeError::InvalidEntry);
                                }
                            }
                            _ => {
                                if a >= avail || b >= avail {
                                    return Err(CacheDecodeError::InvalidEntry);
                                }
                            }
                        }
                        if let Some(slot) = insts.get_mut(i) {
                            *slot = SynthInst { op, a, b, imm };
                        }
                    }
                    CachedRewrite::Better(SynthProg { num_inputs, len, bits, cost, insts })
                }
                _ => return Err(CacheDecodeError::InvalidEntry),
            };
            let _ = entries.insert(digest, rewrite);
        }

        if pos != payload.len() {
            return Err(CacheDecodeError::TrailingBytes);
        }

        Ok(Self { entries })
    }
}

// ============================================================================
// Low-Level Safe Helpers (Zero `as` Casts, Zero Unchecked Indexing)
// ============================================================================

#[inline]
fn to_idx(v: u32) -> usize {
    usize::try_from(v).unwrap_or(0)
}

#[inline]
fn to_u32(v: usize) -> u32 {
    u32::try_from(v).unwrap_or(u32::MAX)
}

#[inline]
const fn trunc_u32(v: u64) -> u32 {
    let [b0, b1, b2, b3, ..] = v.to_le_bytes();
    u32::from_le_bytes([b0, b1, b2, b3])
}

#[inline]
const fn trunc_u64(v: u128) -> u64 {
    let [b0, b1, b2, b3, b4, b5, b6, b7, ..] = v.to_le_bytes();
    u64::from_le_bytes([b0, b1, b2, b3, b4, b5, b6, b7])
}

#[inline]
fn at<T: Copy + Default>(s: &[T], i: usize) -> T {
    s.get(i).copied().unwrap_or_default()
}

#[inline]
fn set_at<T>(s: &mut [T], i: usize, v: T) {
    if let Some(slot) = s.get_mut(i) {
        *slot = v;
    }
}

#[inline]
fn op_byte(op: Op) -> u8 {
    Op::ALL.iter().position(|&c| c == op).and_then(|i| u8::try_from(i).ok()).unwrap_or(0)
}

#[inline]
const fn int_cond_word(c: IntCond) -> u32 {
    match c {
        IntCond::Eq => 0,
        IntCond::Ne => 1,
        IntCond::Ult => 2,
        IntCond::Ule => 3,
        IntCond::Ugt => 4,
        IntCond::Uge => 5,
        IntCond::Slt => 6,
        IntCond::Sle => 7,
        IntCond::Sgt => 8,
        IntCond::Sge => 9,
    }
}

#[inline]
const fn swap_int_cond(c: IntCond) -> IntCond {
    match c {
        IntCond::Eq => IntCond::Eq,
        IntCond::Ne => IntCond::Ne,
        IntCond::Ult => IntCond::Ugt,
        IntCond::Ule => IntCond::Uge,
        IntCond::Ugt => IntCond::Ult,
        IntCond::Uge => IntCond::Ule,
        IntCond::Slt => IntCond::Sgt,
        IntCond::Sle => IntCond::Sge,
        IntCond::Sgt => IntCond::Slt,
        IntCond::Sge => IntCond::Sle,
    }
}

#[inline]
const fn invert_int_cond(c: IntCond) -> IntCond {
    match c {
        IntCond::Eq => IntCond::Ne,
        IntCond::Ne => IntCond::Eq,
        IntCond::Ult => IntCond::Uge,
        IntCond::Ule => IntCond::Ugt,
        IntCond::Ugt => IntCond::Ule,
        IntCond::Uge => IntCond::Ult,
        IntCond::Slt => IntCond::Sge,
        IntCond::Sle => IntCond::Sgt,
        IntCond::Sgt => IntCond::Sle,
        IntCond::Sge => IntCond::Slt,
    }
}

#[inline]
const fn mask_of(bits: u32) -> u64 {
    if bits >= 64 { u64::MAX } else if bits == 0 { 0 } else { (1u64 << bits) - 1 }
}

/// Returns `true` if `val` fits in a single AArch64 `movz` or `movn` instruction.
#[inline]
const fn is_single_mov_const(val: u64) -> bool {
    if val == 0 {
        return true;
    }
    let inv = !val;
    let mut hw = 0u32;
    while hw < 4 {
        let shift = hw * 16;
        let chunk = (val >> shift) & 0xffff;
        if val == (chunk << shift) {
            return true;
        }
        let ichunk = (inv >> shift) & 0xffff;
        if inv == (ichunk << shift) {
            return true;
        }
        hw += 1;
    }
    false
}

#[inline]
fn read_u8(bytes: &[u8], pos: &mut usize) -> Option<u8> {
    let b = bytes.get(*pos).copied()?;
    *pos = pos.checked_add(1)?;
    Some(b)
}

#[inline]
fn read_u32_le(bytes: &[u8], pos: &mut usize) -> Option<u32> {
    let end = pos.checked_add(4)?;
    let chunk: [u8; 4] = bytes.get(*pos..end)?.try_into().ok()?;
    *pos = end;
    Some(u32::from_le_bytes(chunk))
}

#[inline]
fn read_u64_le(bytes: &[u8], pos: &mut usize) -> Option<u64> {
    let end = pos.checked_add(8)?;
    let chunk: [u8; 8] = bytes.get(*pos..end)?.try_into().ok()?;
    *pos = end;
    Some(u64::from_le_bytes(chunk))
}

#[inline]
fn read_digest(bytes: &[u8], pos: &mut usize) -> Option<[u8; 32]> {
    let end = pos.checked_add(32)?;
    let chunk: [u8; 32] = bytes.get(*pos..end)?.try_into().ok()?;
    *pos = end;
    Some(chunk)
}

// ============================================================================
// Machine-Aware E-Graph (`MachENode`, `UnionFind`, `MachEGraph`)
// ============================================================================

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum MachENode {
    /// External LIR value (block param, load, call, or value from a dominating block).
    Var(u32),
    /// Known scalar integer constant (masked to class bit width `1`, `32`, or `64`).
    Const(u64),
    /// Pure unary op (`Neg`, `Not`, `Clz`, `Ctz`, `Popcnt`, `Bswap`, `Bitrev`).
    Unary(Op, EId),
    /// Zero-extension from narrower integer e-class (e.g. `i1 -> i32/i64`).
    Zext(EId),
    /// Pure binary op (`Add`, `Sub`, `Mul`, `And`, `Or`, `Xor`, `Shl`, `Lshr`, `Ashr`,
    /// `Rotl`, `Rotr`, `Smin`, `Smax`, `Umin`, `Umax`, `Umulhi`, `Smulhi`,
    /// or non-trapping constant-divisor `Udiv`/`Urem`/`Sdiv`/`Srem`).
    Binary(Op, EId, EId),
    /// Integer comparison producing `Ty::I1`.
    Icmp(IntCond, EId, EId),
    /// Conditional select `Select(cond_i1, if_true, if_false)`.
    Select(EId, EId, EId),
}

impl MachENode {
    #[inline]
    fn canonicalize(self, uf: &mut UnionFind<impl Allocator>) -> Self {
        match self {
            MachENode::Var(v) => MachENode::Var(v),
            MachENode::Const(c) => MachENode::Const(c),
            MachENode::Unary(op, a) => MachENode::Unary(op, uf.find(a)),
            MachENode::Zext(a) => MachENode::Zext(uf.find(a)),
            MachENode::Binary(op, a, b) => {
                let fa = uf.find(a);
                let fb = uf.find(b);
                let commutative = matches!(
                    op,
                    Op::Add
                        | Op::Mul
                        | Op::And
                        | Op::Or
                        | Op::Xor
                        | Op::Smin
                        | Op::Smax
                        | Op::Umin
                        | Op::Umax
                        | Op::Umulhi
                        | Op::Smulhi
                );
                if commutative && fa > fb {
                    MachENode::Binary(op, fb, fa)
                } else {
                    MachENode::Binary(op, fa, fb)
                }
            }
            MachENode::Icmp(cond, a, b) => {
                let fa = uf.find(a);
                let fb = uf.find(b);
                if fa > fb {
                    MachENode::Icmp(swap_int_cond(cond), fb, fa)
                } else {
                    MachENode::Icmp(cond, fa, fb)
                }
            }
            MachENode::Select(c, t, f) => MachENode::Select(uf.find(c), uf.find(t), uf.find(f)),
        }
    }
}

struct UnionFind<A: Allocator> {
    parent: Vec<EId, A>,
}

impl<A: Allocator> UnionFind<A> {
    fn new(cap: usize, alloc: A) -> Self {
        Self { parent: Vec::with_capacity_in(cap, alloc) }
    }

    #[inline]
    fn make_set(&mut self) -> EId {
        let id = to_u32(self.parent.len());
        self.parent.push(id);
        id
    }

    #[inline]
    fn find(&mut self, mut x: EId) -> EId {
        while at(&self.parent, to_idx(x)) != x {
            let p = at(&self.parent, to_idx(x));
            let gp = at(&self.parent, to_idx(p));
            set_at(&mut self.parent, to_idx(x), gp);
            x = p;
        }
        x
    }

    #[inline]
    fn find_ro(&self, mut x: EId) -> EId {
        let mut steps = 0usize;
        let cap = self.parent.len();
        while steps < cap {
            let p = at(&self.parent, to_idx(x));
            if p == x {
                break;
            }
            x = p;
            steps += 1;
        }
        x
    }

    #[inline]
    fn union(&mut self, a: EId, b: EId) -> EId {
        let ra = self.find(a);
        let rb = self.find(b);
        if ra == rb {
            return ra;
        }
        // Deterministic canonical leader: always pick the smaller class id (Rule 25).
        let (keep, drop) = if ra < rb { (ra, rb) } else { (rb, ra) };
        set_at(&mut self.parent, to_idx(drop), keep);
        keep
    }
}

struct MachEClass<A: Allocator> {
    ty: Ty,
    bits: u32,
    nodes: Vec<MachENode, A>,
    const_val: Option<u64>,
}

struct MachEGraph<A: Allocator> {
    uf: UnionFind<A>,
    classes: Vec<MachEClass<A>, A>,
    total_nodes: u32,
    max_nodes: u32,
    alloc: A,
}

impl<A: Allocator + Copy> MachEGraph<A> {
    fn new(max_nodes: u32, alloc: A) -> Self {
        Self {
            uf: UnionFind::new(64, alloc),
            classes: Vec::with_capacity_in(64, alloc),
            total_nodes: 0,
            max_nodes,
            alloc,
        }
    }

    #[inline]
    fn at_budget(&self) -> bool {
        self.total_nodes >= self.max_nodes
    }

    #[inline]
    fn class_const(&self, id: EId) -> Option<u64> {
        let root = self.uf.find_ro(id);
        self.classes.get(to_idx(root)).and_then(|c| c.const_val)
    }

    #[inline]
    fn class_ty_bits(&self, id: EId) -> Option<(Ty, u32)> {
        let root = self.uf.find_ro(id);
        self.classes.get(to_idx(root)).map(|c| (c.ty, c.bits))
    }

    fn add(&mut self, node: MachENode, ty: Ty, bits: u32) -> Option<EId> {
        let canon = node.canonicalize(&mut self.uf);
        for id in 0..self.classes.len() {
            let id_e = to_u32(id);
            if self.uf.find(id_e) != id_e {
                continue;
            }
            if let Some(cls) = self.classes.get(id)
                && cls.ty == ty
                && cls.nodes.scan_for(&canon).is_some()
            {
                return Some(id_e);
            }
        }
        if self.total_nodes >= self.max_nodes {
            return None;
        }
        let id = self.uf.make_set();
        let mut nodes = Vec::with_capacity_in(4, self.alloc);
        nodes.push(canon);
        let const_val = match canon {
            MachENode::Const(c) => Some(c & mask_of(bits)),
            _ => None,
        };
        self.classes.push(MachEClass { ty, bits, nodes, const_val });
        self.total_nodes = self.total_nodes.saturating_add(1);
        Some(id)
    }

    fn union(&mut self, a: EId, b: EId) -> bool {
        let ra = self.uf.find(a);
        let rb = self.uf.find(b);
        if ra == rb {
            return false;
        }
        let ty_a = self.classes.get(to_idx(ra)).map(|c| c.ty);
        let ty_b = self.classes.get(to_idx(rb)).map(|c| c.ty);
        if ty_a != ty_b {
            return false;
        }
        let keep = self.uf.union(ra, rb);
        let drop = if keep == ra { rb } else { ra };
        let drop_const = self.classes.get(to_idx(drop)).and_then(|c| c.const_val);
        if let Some(kc) = self.classes.get_mut(to_idx(keep))
            && kc.const_val.is_none()
        {
            kc.const_val = drop_const;
        }
        let mut moved: Vec<MachENode, A> = Vec::new_in(self.alloc);
        if let Some(dc) = self.classes.get_mut(to_idx(drop)) {
            core::mem::swap(&mut moved, &mut dc.nodes);
        }
        if let Some(kc) = self.classes.get_mut(to_idx(keep)) {
            for n in moved {
                if kc.nodes.scan_for(&n).is_none() {
                    kc.nodes.push(n);
                }
            }
        }
        true
    }

    fn rebuild(&mut self) {
        for _ in 0..64 {
            let mut merged_any = false;
            for cid in 0..self.classes.len() {
                let cid_e = to_u32(cid);
                if self.uf.find(cid_e) != cid_e {
                    continue;
                }
                let mut i = 0usize;
                while let Some(old) = self.classes.get(cid).and_then(|c| c.nodes.get(i).copied()) {
                    let new = old.canonicalize(&mut self.uf);
                    if new != old {
                        if let Some(cls) = self.classes.get_mut(cid) {
                            if cls.nodes.scan_for(&new).is_some() {
                                let _ = cls.nodes.remove_swap(i);
                                continue;
                            }
                            set_at(&mut cls.nodes, i, new);
                        }
                    }
                    i += 1;
                }
            }
            'outer: for i in 0..self.classes.len() {
                let i_e = to_u32(i);
                if self.uf.find(i_e) != i_e {
                    continue;
                }
                let Some(ty_i) = self.classes.get(i).map(|c| c.ty) else {
                    continue;
                };
                for j in (i + 1)..self.classes.len() {
                    let j_e = to_u32(j);
                    if self.uf.find(j_e) != j_e {
                        continue;
                    }
                    let overlap = match (self.classes.get(i), self.classes.get(j)) {
                        (Some(ci), Some(cj)) if cj.ty == ty_i => {
                            ci.nodes.iter().any(|n| cj.nodes.scan_for(n).is_some())
                        }
                        _ => false,
                    };
                    if overlap {
                        let _ = self.union(i_e, j_e);
                        merged_any = true;
                        break 'outer;
                    }
                }
            }
            if !merged_any {
                break;
            }
        }
    }
}

// ============================================================================
// Joint Machine-Aware Cost Evaluation & Fused Extraction (`BestChoice`)
// ============================================================================

/// Specifies whether one or two operand slots of an extracted e-node are fused directly with a
/// specific child e-node (e.g. `bic`, shifted/rotated ALU, `extr`, `ubfx`, `madd`/`msub`,
/// `csinc`/`csinv`/`csneg`) so materialization emits the exact fused AArch64 pattern.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum FusedPattern {
    None,
    /// Operand slot `slot` (0, 1, or 2) is materialized from `node` instead of that child class's
    /// standalone default node.
    One { slot: u8, node: MachENode },
    /// Both binary operand slots (0 and 1) are fused (e.g. `extr`: `(a >> k) | (b << (W - k))`).
    Two { left: MachENode, right: MachENode },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct BestChoice {
    cost: u32,
    node: MachENode,
    fused: FusedPattern,
}

#[inline]
fn class_cost(best: &[Option<BestChoice>], id: EId) -> u32 {
    best.get(to_idx(id)).and_then(|o| o.map(|b| b.cost)).unwrap_or(INF_COST)
}

#[inline]
fn try_update_choice(cur: &mut Option<BestChoice>, cost: u32, node: MachENode, fused: FusedPattern) {
    if cost >= INF_COST {
        return;
    }
    let better = match *cur {
        None => true,
        Some(prev) => cost < prev.cost,
    };
    if better {
        *cur = Some(BestChoice { cost, node, fused });
    }
}

/// Evaluates the minimum Apple M5 port cost of realizing `node` in e-class `cid`, considering all
/// AArch64 single-instruction fused patterns available across its child e-classes.
fn evaluate_m5_node<A: Allocator + Copy>(
    eg: &MachEGraph<A>,
    cid: EId,
    node: MachENode,
    bits: u32,
    best: &[Option<BestChoice>],
    model: &AppleM5PortModel,
) -> Option<BestChoice> {
    let mut out: Option<BestChoice> = None;

    match node {
        MachENode::Var(_) => {
            try_update_choice(&mut out, 0, node, FusedPattern::None);
        }
        MachENode::Const(c) => {
            try_update_choice(&mut out, model.const_cost(c), node, FusedPattern::None);
        }
        MachENode::Unary(op, a) => {
            let ra = eg.uf.find_ro(a);
            if ra == cid {
                return None;
            }
            let ca = class_cost(best, ra);
            let c = ca.saturating_add(model.op_base_cost(op));
            try_update_choice(&mut out, c, node, FusedPattern::None);
        }
        MachENode::Zext(a) => {
            let ra = eg.uf.find_ro(a);
            if ra == cid {
                return None;
            }
            let ca = class_cost(best, ra);
            // `zext i1 -> i32/i64` is either free after `cset` or 1 ALU op (`and #1`).
            let c = ca.saturating_add(model.simple_alu);
            try_update_choice(&mut out, c, node, FusedPattern::None);
        }
        MachENode::Icmp(_cond, a, b) => {
            let ra = eg.uf.find_ro(a);
            let rb = eg.uf.find_ro(b);
            if ra == cid || rb == cid {
                return None;
            }
            let ca = class_cost(best, ra);
            let cb = class_cost(best, rb);
            let const_a = eg.class_const(ra);
            let const_b = eg.class_const(rb);
            // `cmp rn, #imm12` folds small immediate with 0 extra constant cost.
            let c = if const_b.is_some_and(|v| v <= 4095) {
                ca.saturating_add(model.simple_alu)
            } else if const_a.is_some_and(|v| v <= 4095) {
                cb.saturating_add(model.simple_alu)
            } else {
                ca.saturating_add(cb).saturating_add(model.simple_alu)
            };
            try_update_choice(&mut out, c, node, FusedPattern::None);
        }
        MachENode::Select(c_id, t_id, f_id) => {
            let rc = eg.uf.find_ro(c_id);
            let rt = eg.uf.find_ro(t_id);
            let rf = eg.uf.find_ro(f_id);
            if rc == cid || rt == cid || rf == cid {
                return None;
            }
            let cc = class_cost(best, rc);
            let ct = class_cost(best, rt);
            let cf = class_cost(best, rf);

            // Baseline `csel rd, rt, rf, cond`.
            try_update_choice(
                &mut out,
                cc.saturating_add(ct).saturating_add(cf).saturating_add(model.simple_alu),
                node,
                FusedPattern::None,
            );

            // Check AArch64 `csinc` (`x + 1`), `csinv` (`!x`), `csneg` (`-x`) on either branch!
            for &(slot, branch_r, other_cost) in &[(1u8, rt, cf), (2u8, rf, ct)] {
                if let Some(cls) = eg.classes.get(to_idx(branch_r)) {
                    for &bn in cls.nodes.as_slice() {
                        match bn {
                            MachENode::Unary(Op::Not | Op::Neg, inner) => {
                                let ri = eg.uf.find_ro(inner);
                                if ri != cid {
                                    let total = cc
                                        .saturating_add(other_cost)
                                        .saturating_add(class_cost(best, ri))
                                        .saturating_add(model.fused_alu);
                                    try_update_choice(
                                        &mut out,
                                        total,
                                        node,
                                        FusedPattern::One { slot, node: bn },
                                    );
                                }
                            }
                            MachENode::Binary(Op::Add, x, y) => {
                                let rx = eg.uf.find_ro(x);
                                let ry = eg.uf.find_ro(y);
                                let inc_src = if eg.class_const(ry) == Some(1) {
                                    Some(rx)
                                } else if eg.class_const(rx) == Some(1) {
                                    Some(ry)
                                } else {
                                    None
                                };
                                if let Some(ri) = inc_src
                                    && ri != cid
                                {
                                    let total = cc
                                        .saturating_add(other_cost)
                                        .saturating_add(class_cost(best, ri))
                                        .saturating_add(model.fused_alu);
                                    try_update_choice(
                                        &mut out,
                                        total,
                                        node,
                                        FusedPattern::One { slot, node: bn },
                                    );
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
        MachENode::Binary(op, a, b) => {
            let ra = eg.uf.find_ro(a);
            let rb = eg.uf.find_ro(b);
            if ra == cid || rb == cid {
                return None;
            }
            let ca = class_cost(best, ra);
            let cb = class_cost(best, rb);
            let const_a = eg.class_const(ra);
            let const_b = eg.class_const(rb);
            let base = model.op_base_cost(op);

            // Baseline unfused binary op.
            try_update_choice(
                &mut out,
                ca.saturating_add(cb).saturating_add(base),
                node,
                FusedPattern::None,
            );

            match op {
                Op::Add => {
                    // Immediate `add rd, rn, #imm12`: constant folds for free!
                    if const_b.is_some_and(|v| v <= 4095) {
                        try_update_choice(
                            &mut out,
                            ca.saturating_add(model.simple_alu),
                            node,
                            FusedPattern::None,
                        );
                    }
                    if const_a.is_some_and(|v| v <= 4095) {
                        try_update_choice(
                            &mut out,
                            cb.saturating_add(model.simple_alu),
                            node,
                            FusedPattern::None,
                        );
                    }
                    // Fused shifted `add rd, rn, rm, lsl/lsr/asr #k` (100 units!) and `madd` (260 units!).
                    for &(slot, child_r, other_cost) in &[(0u8, ra, cb), (1u8, rb, ca)] {
                        if let Some(cls) = eg.classes.get(to_idx(child_r)) {
                            for &cn in cls.nodes.as_slice() {
                                match cn {
                                    MachENode::Binary(Op::Shl | Op::Lshr | Op::Ashr, sx, sk) => {
                                        let rsx = eg.uf.find_ro(sx);
                                        if rsx != cid
                                            && eg.class_const(sk).is_some_and(|k| k < u64::from(bits))
                                        {
                                            let total = other_cost
                                                .saturating_add(class_cost(best, rsx))
                                                .saturating_add(model.fused_alu);
                                            try_update_choice(
                                                &mut out,
                                                total,
                                                node,
                                                FusedPattern::One { slot, node: cn },
                                            );
                                        }
                                    }
                                    MachENode::Binary(Op::Mul, mx, my) => {
                                        let rmx = eg.uf.find_ro(mx);
                                        let rmy = eg.uf.find_ro(my);
                                        if rmx != cid && rmy != cid {
                                            let total = other_cost
                                                .saturating_add(class_cost(best, rmx))
                                                .saturating_add(class_cost(best, rmy))
                                                .saturating_add(model.madd);
                                            try_update_choice(
                                                &mut out,
                                                total,
                                                node,
                                                FusedPattern::One { slot, node: cn },
                                            );
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }
                }
                Op::Sub => {
                    if const_b.is_some_and(|v| v <= 4095) {
                        try_update_choice(
                            &mut out,
                            ca.saturating_add(model.simple_alu),
                            node,
                            FusedPattern::None,
                        );
                    }
                    // Fused shifted `sub rd, rn, rm, lsl/lsr/asr #k` (100 units!) and `msub` (260 units!) on RHS.
                    if let Some(cls) = eg.classes.get(to_idx(rb)) {
                        for &cn in cls.nodes.as_slice() {
                            match cn {
                                MachENode::Binary(Op::Shl | Op::Lshr | Op::Ashr, sx, sk) => {
                                    let rsx = eg.uf.find_ro(sx);
                                    if rsx != cid
                                        && eg.class_const(sk).is_some_and(|k| k < u64::from(bits))
                                    {
                                        let total = ca
                                            .saturating_add(class_cost(best, rsx))
                                            .saturating_add(model.fused_alu);
                                        try_update_choice(
                                            &mut out,
                                            total,
                                            node,
                                            FusedPattern::One { slot: 1, node: cn },
                                        );
                                    }
                                }
                                MachENode::Binary(Op::Mul, mx, my) => {
                                    let rmx = eg.uf.find_ro(mx);
                                    let rmy = eg.uf.find_ro(my);
                                    if rmx != cid && rmy != cid {
                                        let total = ca
                                            .saturating_add(class_cost(best, rmx))
                                            .saturating_add(class_cost(best, rmy))
                                            .saturating_add(model.madd);
                                        try_update_choice(
                                            &mut out,
                                            total,
                                            node,
                                            FusedPattern::One { slot: 1, node: cn },
                                        );
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                }
                Op::And | Op::Or | Op::Xor => {
                    // Logical bitmask immediate (`and`/`orr`/`eor rd, rn, #bitmask`): 0 extra const cost!
                    if const_b.is_some_and(|v| AppleM5PortModel::is_bitmask_imm(v, bits)) {
                        try_update_choice(
                            &mut out,
                            ca.saturating_add(model.simple_alu),
                            node,
                            FusedPattern::None,
                        );
                    }
                    if const_a.is_some_and(|v| AppleM5PortModel::is_bitmask_imm(v, bits)) {
                        try_update_choice(
                            &mut out,
                            cb.saturating_add(model.simple_alu),
                            node,
                            FusedPattern::None,
                        );
                    }
                    // Fused `bic`/`orn`/`eon` (`op(a, not(b))`) AND shifted/rotated logic
                    // (`and/orr/eor rd, rn, rm, lsl/lsr/asr/ror #k`): 100 units single cycle!
                    for &(slot, child_r, other_cost) in &[(0u8, ra, cb), (1u8, rb, ca)] {
                        if let Some(cls) = eg.classes.get(to_idx(child_r)) {
                            for &cn in cls.nodes.as_slice() {
                                match cn {
                                    MachENode::Unary(Op::Not, nx) => {
                                        let rnx = eg.uf.find_ro(nx);
                                        if rnx != cid {
                                            let total = other_cost
                                                .saturating_add(class_cost(best, rnx))
                                                .saturating_add(model.fused_alu);
                                            try_update_choice(
                                                &mut out,
                                                total,
                                                node,
                                                FusedPattern::One { slot, node: cn },
                                            );
                                        }
                                    }
                                    MachENode::Binary(
                                        Op::Shl | Op::Lshr | Op::Ashr | Op::Rotr,
                                        sx,
                                        sk,
                                    ) => {
                                        let rsx = eg.uf.find_ro(sx);
                                        if rsx != cid
                                            && eg.class_const(sk).is_some_and(|k| k < u64::from(bits))
                                        {
                                            let total = other_cost
                                                .saturating_add(class_cost(best, rsx))
                                                .saturating_add(model.fused_alu);
                                            try_update_choice(
                                                &mut out,
                                                total,
                                                node,
                                                FusedPattern::One { slot, node: cn },
                                            );
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }
                    // Fused `ubfx` (`(x >> lsb) & ((1 << width) - 1)`): 100 units single instruction!
                    if op == Op::And {
                        for &(slot, shift_r, mask_val) in &[(0u8, ra, const_b), (1u8, rb, const_a)] {
                            if mask_val.is_some_and(|m| AppleM5PortModel::is_low_bitmask(m, bits))
                                && let Some(cls) = eg.classes.get(to_idx(shift_r))
                            {
                                for &cn in cls.nodes.as_slice() {
                                    if let MachENode::Binary(Op::Lshr, sx, sk) = cn {
                                        let rsx = eg.uf.find_ro(sx);
                                        if rsx != cid
                                            && eg.class_const(sk).is_some_and(|k| k < u64::from(bits))
                                        {
                                            let total = class_cost(best, rsx)
                                                .saturating_add(model.fused_alu);
                                            try_update_choice(
                                                &mut out,
                                                total,
                                                node,
                                                FusedPattern::One { slot, node: cn },
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    }
                    // Fused `extr` (`(x >> k) | (y << (W - k))` for `x != y`) and `bfxil`
                    // (`(x & !mask) | (y & mask)` for low bitmask `mask`): 100 units single cycle!
                    // Note: `x == y` is intentionally excluded from `extr` so canonical single-op
                    // `Op::Rotr(x, k)` (100) strictly beats `(x >> k) | (x << (W - k))` (200).
                    if op == Op::Or
                        && let (Some(cls_a), Some(cls_b)) =
                            (eg.classes.get(to_idx(ra)), eg.classes.get(to_idx(rb)))
                    {
                        let width_mask = mask_of(bits);
                        for &an in cls_a.nodes.as_slice() {
                            for &bn in cls_b.nodes.as_slice() {
                                let extr_pair = match (an, bn) {
                                    (
                                        MachENode::Binary(Op::Lshr, x, k1),
                                        MachENode::Binary(Op::Shl, y, k2),
                                    )
                                    | (
                                        MachENode::Binary(Op::Shl, y, k2),
                                        MachENode::Binary(Op::Lshr, x, k1),
                                    ) => Some((x, k1, y, k2)),
                                    _ => None,
                                };
                                if let Some((x, k1, y, k2)) = extr_pair {
                                    let rx = eg.uf.find_ro(x);
                                    let ry = eg.uf.find_ro(y);
                                    if rx != ry
                                        && rx != cid
                                        && ry != cid
                                        && let (Some(v1), Some(v2)) =
                                            (eg.class_const(k1), eg.class_const(k2))
                                        && v1 > 0
                                        && v2 > 0
                                        && v1.saturating_add(v2) == u64::from(bits)
                                    {
                                        let total = class_cost(best, rx)
                                            .saturating_add(class_cost(best, ry))
                                            .saturating_add(model.fused_alu);
                                        try_update_choice(
                                            &mut out,
                                            total,
                                            node,
                                            FusedPattern::Two { left: an, right: bn },
                                        );
                                    }
                                }
                                if let (
                                    MachENode::Binary(Op::And, x1, x2),
                                    MachENode::Binary(Op::And, y1, y2),
                                ) = (an, bn)
                                {
                                    let mx = if let Some(c) = eg.class_const(x2) {
                                        Some((eg.uf.find_ro(x1), c))
                                    } else if let Some(c) = eg.class_const(x1) {
                                        Some((eg.uf.find_ro(x2), c))
                                    } else {
                                        None
                                    };
                                    let my = if let Some(c) = eg.class_const(y2) {
                                        Some((eg.uf.find_ro(y1), c))
                                    } else if let Some(c) = eg.class_const(y1) {
                                        Some((eg.uf.find_ro(y2), c))
                                    } else {
                                        None
                                    };
                                    if let (Some((rx, cmx)), Some((ry, cmy))) = (mx, my)
                                        && rx != cid
                                        && ry != cid
                                        && (cmx ^ cmy) == width_mask
                                        && (AppleM5PortModel::is_low_bitmask(cmx, bits)
                                            || AppleM5PortModel::is_low_bitmask(cmy, bits))
                                    {
                                        let total = class_cost(best, rx)
                                            .saturating_add(class_cost(best, ry))
                                            .saturating_add(model.fused_alu);
                                        try_update_choice(
                                            &mut out,
                                            total,
                                            node,
                                            FusedPattern::Two { left: an, right: bn },
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
                Op::Shl | Op::Lshr | Op::Ashr | Op::Rotr => {
                    // Immediate shift/rotate (`lsl`/`lsr`/`asr`/`ror #k`): shift constant costs 0!
                    if const_b.is_some() {
                        try_update_choice(
                            &mut out,
                            ca.saturating_add(model.simple_alu),
                            node,
                            FusedPattern::None,
                        );
                    }
                }
                _ => {}
            }
        }
    }

    out
}

/// Computes minimum M5-port-cost [`BestChoice`] for every active e-class via bounded Bellman-Ford.
fn extract_best<A: Allocator + Copy>(
    eg: &MachEGraph<A>,
    limit_classes: usize,
    model: &AppleM5PortModel,
    alloc: A,
) -> Vec<Option<BestChoice>, A> {
    let n = limit_classes.min(eg.classes.len());
    let mut best: Vec<Option<BestChoice>, A> = filled(n, None, &alloc);

    for _ in 0..16 {
        let mut changed = false;
        for cid in 0..n {
            let cid_e = to_u32(cid);
            if eg.uf.find_ro(cid_e) != cid_e {
                continue;
            }
            let Some(cls) = eg.classes.get(cid) else {
                continue;
            };
            let bits = cls.bits;
            let mut cur = best.get(cid).copied().flatten();
            for &node in cls.nodes.as_slice() {
                if let Some(cand) = evaluate_m5_node(eg, cid_e, node, bits, best.as_slice(), model) {
                    let is_better = match cur {
                        None => true,
                        Some(prev) => cand.cost < prev.cost,
                    };
                    if is_better {
                        cur = Some(cand);
                        changed = true;
                    }
                }
            }
            set_at(&mut best, cid, cur);
        }
        if !changed {
            break;
        }
    }

    best
}

// ============================================================================
// Joint AArch64 Equality Saturation Rules (`saturate_round`)
// ============================================================================

/// Exact Granlund-Montgomery unsigned division magic multiplier `(magic, post_shift)` for small
/// divisors `c` where `umulhi(x, magic) >> post_shift == x / c` holds identically for all `2^bits`
/// unsigned inputs.
#[inline]
const fn udiv_exact_magic(c: u64, bits: u32) -> Option<(u64, u64)> {
    match (bits, c) {
        (64, 3) => Some((0xAAAA_AAAA_AAAA_AAAB, 1)),
        (64, 5) => Some((0xCCCC_CCCC_CCCC_CCCD, 2)),
        (64, 10) => Some((0xCCCC_CCCC_CCCC_CCCD, 3)),
        (32, 3) => Some((0xAAAA_AAAB, 1)),
        (32, 5) => Some((0xCCCC_CCCD, 2)),
        (32, 10) => Some((0xCCCC_CCCD, 3)),
        _ => None,
    }
}

/// Runs one deterministic round of AArch64-aware equality saturation rules over `eg`.
fn saturate_round<A: Allocator + Copy>(eg: &mut MachEGraph<A>) -> u32 {
    let mut unions: Vec<(EId, EId), A> = Vec::new_in(eg.alloc);
    let num_classes = eg.classes.len();

    for cid in 0..num_classes {
        if eg.at_budget() {
            break;
        }
        let cid_e = to_u32(cid);
        if eg.uf.find(cid_e) != cid_e {
            continue;
        }
        let Some((ty, bits, node_len)) = eg.classes.get(cid).map(|c| (c.ty, c.bits, c.nodes.len()))
        else {
            continue;
        };
        let mask = mask_of(bits);

        let mut snap: Vec<MachENode, A> = Vec::with_capacity_in(node_len, eg.alloc);
        if let Some(cls) = eg.classes.get(cid) {
            snap.extend_from_slice(cls.nodes.as_slice());
        }

        for node in snap {
            if eg.at_budget() {
                break;
            }
            match node {
                MachENode::Var(_) | MachENode::Const(_) => {}
                MachENode::Unary(op, a) => {
                    let ra = eg.uf.find(a);
                    // Constant fold unary op.
                    if let Some(ca) = eg.class_const(ra) {
                        let res = trunc_u64(unary(op, u128::from(ca), bits)) & mask;
                        if let Some(c_id) = eg.add(MachENode::Const(res), ty, bits) {
                            unions.push((cid_e, c_id));
                        }
                    }
                    let mut inner: Vec<MachENode, A> = Vec::new_in(eg.alloc);
                    if let Some(cls) = eg.classes.get(to_idx(ra)) {
                        inner.extend_from_slice(cls.nodes.as_slice());
                    }
                    for in_node in inner {
                        if eg.at_budget() {
                            break;
                        }
                        match (op, in_node) {
                            // Involutions: `not(not(x)) -> x`, `neg(neg(x)) -> x`.
                            (Op::Not, MachENode::Unary(Op::Not, x))
                            | (Op::Neg, MachENode::Unary(Op::Neg, x)) => {
                                unions.push((cid_e, x));
                            }
                            // `neg(sub(a, b)) -> sub(b, a)`.
                            (Op::Neg, MachENode::Binary(Op::Sub, x, y)) => {
                                if let Some(s) = eg.add(MachENode::Binary(Op::Sub, y, x), ty, bits) {
                                    unions.push((cid_e, s));
                                }
                            }
                            // `not(icmp(cond, x, y)) -> icmp(invert(cond), x, y)` on `i1`.
                            (Op::Not, MachENode::Icmp(cond, x, y)) if ty == Ty::I1 => {
                                if let Some(inv) =
                                    eg.add(MachENode::Icmp(invert_int_cond(cond), x, y), ty, bits)
                                {
                                    unions.push((cid_e, inv));
                                }
                            }
                            // `not(xor(x, y)) -> xor(x, not(y))` (exposes `eon`!).
                            (Op::Not, MachENode::Binary(Op::Xor, x, y)) => {
                                if let Some(ny) = eg.add(MachENode::Unary(Op::Not, y), ty, bits)
                                    && let Some(e) =
                                        eg.add(MachENode::Binary(Op::Xor, x, ny), ty, bits)
                                {
                                    unions.push((cid_e, e));
                                }
                            }
                            _ => {}
                        }
                    }
                }
                MachENode::Zext(a) => {
                    let ra = eg.uf.find(a);
                    if let Some(ca) = eg.class_const(ra)
                        && let Some(c_id) = eg.add(MachENode::Const(ca & mask), ty, bits)
                    {
                        unions.push((cid_e, c_id));
                    }
                }
                MachENode::Icmp(cond, a, b) => {
                    let ra = eg.uf.find(a);
                    let rb = eg.uf.find(b);
                    let op_bits = eg.class_ty_bits(ra).map_or(bits, |(_, ob)| ob);
                    if let (Some(ca), Some(cb)) = (eg.class_const(ra), eg.class_const(rb)) {
                        let res = if compare(cond, u128::from(ca), u128::from(cb), op_bits) {
                            1u64
                        } else {
                            0u64
                        };
                        if let Some(c_id) = eg.add(MachENode::Const(res), Ty::I1, 1) {
                            unions.push((cid_e, c_id));
                        }
                    } else if ra == rb {
                        let res = match cond {
                            IntCond::Eq | IntCond::Ule | IntCond::Uge | IntCond::Sle | IntCond::Sge => {
                                1u64
                            }
                            IntCond::Ne | IntCond::Ult | IntCond::Ugt | IntCond::Slt | IntCond::Sgt => {
                                0u64
                            }
                        };
                        if let Some(c_id) = eg.add(MachENode::Const(res), Ty::I1, 1) {
                            unions.push((cid_e, c_id));
                        }
                    }
                }
                MachENode::Select(c_id, t_id, f_id) => {
                    let rc = eg.uf.find(c_id);
                    let rt = eg.uf.find(t_id);
                    let rf = eg.uf.find(f_id);

                    // Constant condition or equal branches.
                    if let Some(cv) = eg.class_const(rc) {
                        unions.push((cid_e, if (cv & 1) != 0 { rt } else { rf }));
                    } else if rt == rf {
                        unions.push((cid_e, rt));
                    } else {
                        // `Select(c, 1, 0) -> Zext(c)`.
                        if eg.class_const(rt) == Some(1)
                            && eg.class_const(rf) == Some(0)
                            && let Some(zx) = eg.add(MachENode::Zext(rc), ty, bits)
                        {
                            unions.push((cid_e, zx));
                        }
                        // `Select(c, x + 1, x) <-> Add(x, Zext(c))` (lets M5 cost model choose `csinc` vs `add+zext`).
                        let mut t_nodes: Vec<MachENode, A> = Vec::new_in(eg.alloc);
                        if let Some(cls) = eg.classes.get(to_idx(rt)) {
                            t_nodes.extend_from_slice(cls.nodes.as_slice());
                        }
                        for &tn in t_nodes.as_slice() {
                            if let MachENode::Binary(Op::Add, x, y) = tn {
                                let rx = eg.uf.find(x);
                                let ry = eg.uf.find(y);
                                let is_inc_rf = (rx == rf && eg.class_const(ry) == Some(1))
                                    || (ry == rf && eg.class_const(rx) == Some(1));
                                if is_inc_rf
                                    && let Some(zx) = eg.add(MachENode::Zext(rc), ty, bits)
                                    && let Some(sum) =
                                        eg.add(MachENode::Binary(Op::Add, rf, zx), ty, bits)
                                {
                                    unions.push((cid_e, sum));
                                }
                            }
                        }

                        // Inspect condition class for `Not(c)` and `Icmp(cond, a, b)` -> `smin`/`smax`/`umin`/`umax`!
                        let mut c_nodes: Vec<MachENode, A> = Vec::new_in(eg.alloc);
                        if let Some(cls) = eg.classes.get(to_idx(rc)) {
                            c_nodes.extend_from_slice(cls.nodes.as_slice());
                        }
                        for &cn in c_nodes.as_slice() {
                            if eg.at_budget() {
                                break;
                            }
                            match cn {
                                MachENode::Unary(Op::Not, inner_c) => {
                                    if let Some(swapped) =
                                        eg.add(MachENode::Select(inner_c, rf, rt), ty, bits)
                                    {
                                        unions.push((cid_e, swapped));
                                    }
                                }
                                MachENode::Icmp(cond, a, b) => {
                                    let ra = eg.uf.find(a);
                                    let rb = eg.uf.find(b);
                                    // Direct ordering: `Select(cmp(a, b), a, b)` or `Select(cmp(a, b), b, a)`.
                                    if (rt == ra && rf == rb) || (rt == rb && rf == ra) {
                                        let direct = rt == ra;
                                        match cond {
                                            IntCond::Eq => {
                                                // If `a == b`, both arms equal `rf`.
                                                unions.push((cid_e, rf));
                                            }
                                            IntCond::Ne => {
                                                // If `a != b`, choose `rt`; if `a == b`, `rt == rf`.
                                                unions.push((cid_e, rt));
                                            }
                                            IntCond::Slt | IntCond::Sle => {
                                                let mm = if direct { Op::Smin } else { Op::Smax };
                                                if let Some(n) =
                                                    eg.add(MachENode::Binary(mm, ra, rb), ty, bits)
                                                {
                                                    unions.push((cid_e, n));
                                                }
                                            }
                                            IntCond::Sgt | IntCond::Sge => {
                                                let mm = if direct { Op::Smax } else { Op::Smin };
                                                if let Some(n) =
                                                    eg.add(MachENode::Binary(mm, ra, rb), ty, bits)
                                                {
                                                    unions.push((cid_e, n));
                                                }
                                            }
                                            IntCond::Ult | IntCond::Ule => {
                                                let mm = if direct { Op::Umin } else { Op::Umax };
                                                if let Some(n) =
                                                    eg.add(MachENode::Binary(mm, ra, rb), ty, bits)
                                                {
                                                    unions.push((cid_e, n));
                                                }
                                            }
                                            IntCond::Ugt | IntCond::Uge => {
                                                let mm = if direct { Op::Umax } else { Op::Umin };
                                                if let Some(n) =
                                                    eg.add(MachENode::Binary(mm, ra, rb), ty, bits)
                                                {
                                                    unions.push((cid_e, n));
                                                }
                                            }
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                }
                MachENode::Binary(op, a, b) => {
                    let ra = eg.uf.find(a);
                    let rb = eg.uf.find(b);
                    let ca = eg.class_const(ra);
                    let cb = eg.class_const(rb);

                    // 1. Exact constant folding via `blaze_lir::arith::binary`.
                    if let (Some(va), Some(vb)) = (ca, cb)
                        && let Ok(res) = binary(op, u128::from(va), u128::from(vb), bits)
                        && let Some(c_id) = eg.add(MachENode::Const(trunc_u64(res) & mask), ty, bits)
                    {
                        unions.push((cid_e, c_id));
                    }

                    // 2. Local algebraic identities & strength reduction.
                    match op {
                        Op::Add => {
                            if cb == Some(0) {
                                unions.push((cid_e, ra));
                            } else if ca == Some(0) {
                                unions.push((cid_e, rb));
                            } else if ra == rb {
                                if let Some(one) = eg.add(MachENode::Const(1), ty, bits)
                                    && let Some(sh) =
                                        eg.add(MachENode::Binary(Op::Shl, ra, one), ty, bits)
                                {
                                    unions.push((cid_e, sh));
                                }
                            }
                        }
                        Op::Sub => {
                            if cb == Some(0) {
                                unions.push((cid_e, ra));
                            } else if ra == rb {
                                if let Some(z) = eg.add(MachENode::Const(0), ty, bits) {
                                    unions.push((cid_e, z));
                                }
                            } else if ca == Some(0)
                                && let Some(neg) = eg.add(MachENode::Unary(Op::Neg, rb), ty, bits)
                            {
                                unions.push((cid_e, neg));
                            }
                        }
                        Op::Mul => {
                            if ca == Some(0) || cb == Some(0) {
                                if let Some(z) = eg.add(MachENode::Const(0), ty, bits) {
                                    unions.push((cid_e, z));
                                }
                            } else if cb == Some(1) {
                                unions.push((cid_e, ra));
                            } else if ca == Some(1) {
                                unions.push((cid_e, rb));
                            } else {
                                let pair = if let Some(c) = cb {
                                    Some((ra, c))
                                } else if let Some(c) = ca {
                                    Some((rb, c))
                                } else {
                                    None
                                };
                                if let Some((x, c)) = pair {
                                    let neg_c = (0u64.wrapping_sub(c)) & mask;
                                    if c == mask {
                                        // `x * -1 -> neg(x)`
                                        if let Some(ng) = eg.add(MachENode::Unary(Op::Neg, x), ty, bits) {
                                            unions.push((cid_e, ng));
                                        }
                                    } else if c > 1 && c.is_power_of_two() {
                                        let k = c.trailing_zeros();
                                        if k < bits
                                            && let Some(kc) =
                                                eg.add(MachENode::Const(u64::from(k)), ty, bits)
                                            && let Some(sh) =
                                                eg.add(MachENode::Binary(Op::Shl, x, kc), ty, bits)
                                        {
                                            unions.push((cid_e, sh));
                                        }
                                    } else if c > 2 && (c - 1).is_power_of_two() {
                                        // `x * (2^k + 1) -> add(x, shl(x, k))` = ONE M5 `add_lsl` (100 units)!
                                        let k = (c - 1).trailing_zeros();
                                        if k < bits
                                            && let Some(kc) =
                                                eg.add(MachENode::Const(u64::from(k)), ty, bits)
                                            && let Some(sh) =
                                                eg.add(MachENode::Binary(Op::Shl, x, kc), ty, bits)
                                            && let Some(sum) =
                                                eg.add(MachENode::Binary(Op::Add, x, sh), ty, bits)
                                        {
                                            unions.push((cid_e, sum));
                                        }
                                    } else if c >= 7 && c < u64::MAX && (c + 1).is_power_of_two() {
                                        // `x * (2^k - 1) -> sub(shl(x, k), x)` (200 units vs `mul` 260)!
                                        let k = (c + 1).trailing_zeros();
                                        if k < bits
                                            && let Some(kc) =
                                                eg.add(MachENode::Const(u64::from(k)), ty, bits)
                                            && let Some(sh) =
                                                eg.add(MachENode::Binary(Op::Shl, x, kc), ty, bits)
                                            && let Some(diff) =
                                                eg.add(MachENode::Binary(Op::Sub, sh, x), ty, bits)
                                        {
                                            unions.push((cid_e, diff));
                                        }
                                    } else if neg_c >= 7
                                        && neg_c < u64::MAX
                                        && (neg_c + 1).is_power_of_two()
                                    {
                                        // `x * -(2^k - 1) -> sub(x, shl(x, k))` = ONE M5 `sub_lsl` (100 units)!
                                        let k = (neg_c + 1).trailing_zeros();
                                        if k < bits
                                            && let Some(kc) =
                                                eg.add(MachENode::Const(u64::from(k)), ty, bits)
                                            && let Some(sh) =
                                                eg.add(MachENode::Binary(Op::Shl, x, kc), ty, bits)
                                            && let Some(diff) =
                                                eg.add(MachENode::Binary(Op::Sub, x, sh), ty, bits)
                                        {
                                            unions.push((cid_e, diff));
                                        }
                                    } else if c > 4 && (c & 1) == 0 {
                                        // `x * ((2^k + 1) << m)` e.g. 6, 10, 12, 18, 20, 24, 36 -> `(x + (x << k)) << m`
                                        let m = c.trailing_zeros();
                                        let odd = c >> m;
                                        if odd > 2 && (odd - 1).is_power_of_two() {
                                            let k = (odd - 1).trailing_zeros();
                                            if k < bits
                                                && m < bits
                                                && let (Some(kc), Some(mc)) = (
                                                    eg.add(MachENode::Const(u64::from(k)), ty, bits),
                                                    eg.add(MachENode::Const(u64::from(m)), ty, bits),
                                                )
                                                && let Some(sh1) = eg.add(
                                                    MachENode::Binary(Op::Shl, x, kc),
                                                    ty,
                                                    bits,
                                                )
                                                && let Some(sum) = eg.add(
                                                    MachENode::Binary(Op::Add, x, sh1),
                                                    ty,
                                                    bits,
                                                )
                                                && let Some(sh2) = eg.add(
                                                    MachENode::Binary(Op::Shl, sum, mc),
                                                    ty,
                                                    bits,
                                                )
                                            {
                                                unions.push((cid_e, sh2));
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        Op::Udiv => {
                            if let Some(c) = cb {
                                if c == 1 {
                                    unions.push((cid_e, ra));
                                } else if c > 1 && c.is_power_of_two() {
                                    let k = c.trailing_zeros();
                                    if k < bits
                                        && let Some(kc) =
                                            eg.add(MachENode::Const(u64::from(k)), ty, bits)
                                        && let Some(sh) =
                                            eg.add(MachENode::Binary(Op::Lshr, ra, kc), ty, bits)
                                    {
                                        unions.push((cid_e, sh));
                                    }
                                } else if let Some((magic, shift)) = udiv_exact_magic(c, bits)
                                    && let (Some(mc), Some(sc)) = (
                                        eg.add(MachENode::Const(magic), ty, bits),
                                        eg.add(MachENode::Const(shift), ty, bits),
                                    )
                                    && let Some(hi) =
                                        eg.add(MachENode::Binary(Op::Umulhi, ra, mc), ty, bits)
                                    && let Some(q) =
                                        eg.add(MachENode::Binary(Op::Lshr, hi, sc), ty, bits)
                                {
                                    unions.push((cid_e, q));
                                }
                            }
                        }
                        Op::Urem => {
                            if let Some(c) = cb {
                                if c == 1 {
                                    if let Some(z) = eg.add(MachENode::Const(0), ty, bits) {
                                        unions.push((cid_e, z));
                                    }
                                } else if c > 1 && c.is_power_of_two() {
                                    if let Some(mc) = eg.add(MachENode::Const(c - 1), ty, bits)
                                        && let Some(and) =
                                            eg.add(MachENode::Binary(Op::And, ra, mc), ty, bits)
                                    {
                                        unions.push((cid_e, and));
                                    }
                                }
                            }
                        }
                        Op::Sdiv => {
                            if cb == Some(1) {
                                unions.push((cid_e, ra));
                            }
                        }
                        Op::Srem => {
                            if cb == Some(1)
                                && let Some(z) = eg.add(MachENode::Const(0), ty, bits)
                            {
                                unions.push((cid_e, z));
                            }
                        }
                        Op::And => {
                            if ca == Some(0) || cb == Some(0) {
                                if let Some(z) = eg.add(MachENode::Const(0), ty, bits) {
                                    unions.push((cid_e, z));
                                }
                            } else if ra == rb || cb == Some(mask) {
                                unions.push((cid_e, ra));
                            } else if ca == Some(mask) {
                                unions.push((cid_e, rb));
                            }
                        }
                        Op::Or => {
                            if ca == Some(mask) || cb == Some(mask) {
                                if let Some(m) = eg.add(MachENode::Const(mask), ty, bits) {
                                    unions.push((cid_e, m));
                                }
                            } else if ra == rb || cb == Some(0) {
                                unions.push((cid_e, ra));
                            } else if ca == Some(0) {
                                unions.push((cid_e, rb));
                            }
                        }
                        Op::Xor => {
                            if ra == rb {
                                if let Some(z) = eg.add(MachENode::Const(0), ty, bits) {
                                    unions.push((cid_e, z));
                                }
                            } else if cb == Some(0) {
                                unions.push((cid_e, ra));
                            } else if ca == Some(0) {
                                unions.push((cid_e, rb));
                            } else if cb == Some(mask) {
                                // `b ^ -1 -> Not(b)` (unlocks AArch64 `bic`/`orn`/`eon`!)
                                if let Some(nt) = eg.add(MachENode::Unary(Op::Not, ra), ty, bits) {
                                    unions.push((cid_e, nt));
                                }
                            } else if ca == Some(mask)
                                && let Some(nt) = eg.add(MachENode::Unary(Op::Not, rb), ty, bits)
                            {
                                unions.push((cid_e, nt));
                            }
                        }
                        Op::Shl | Op::Lshr | Op::Ashr | Op::Rotr => {
                            if let Some(sh) = cb
                                && (sh % u64::from(bits)) == 0
                            {
                                unions.push((cid_e, ra));
                            }
                        }
                        Op::Rotl => {
                            if let Some(sh) = cb {
                                let k = sh % u64::from(bits);
                                if k == 0 {
                                    unions.push((cid_e, ra));
                                } else {
                                    // AArch64 has native `ror`, not `rol`: `rotl(x, k) -> rotr(x, W - k)`.
                                    let rk = u64::from(bits) - k;
                                    if let Some(rkc) = eg.add(MachENode::Const(rk), ty, bits)
                                        && let Some(rr) =
                                            eg.add(MachENode::Binary(Op::Rotr, ra, rkc), ty, bits)
                                    {
                                        unions.push((cid_e, rr));
                                    }
                                }
                            }
                        }
                        Op::Smin | Op::Smax | Op::Umin | Op::Umax => {
                            if ra == rb {
                                unions.push((cid_e, ra));
                            }
                        }
                        _ => {}
                    }

                    // 3. Structural multi-node AArch64 machine rewrites.
                    let mut a_nodes: Vec<MachENode, A> = Vec::new_in(eg.alloc);
                    let mut b_nodes: Vec<MachENode, A> = Vec::new_in(eg.alloc);
                    if let Some(cls) = eg.classes.get(to_idx(ra)) {
                        a_nodes.extend_from_slice(cls.nodes.as_slice());
                    }
                    if let Some(cls) = eg.classes.get(to_idx(rb)) {
                        b_nodes.extend_from_slice(cls.nodes.as_slice());
                    }

                    // 3a. Rotate discovery: `(x >> k1) |/^/+ (x << k2)` where `k1 + k2 == bits` -> `Rotr(x, k1)`!
                    if matches!(op, Op::Or | Op::Xor | Op::Add) {
                        for &an in a_nodes.as_slice() {
                            if eg.at_budget() {
                                break;
                            }
                            for &bn in b_nodes.as_slice() {
                                let rot_pair = match (an, bn) {
                                    (
                                        MachENode::Binary(Op::Lshr, x1, k1),
                                        MachENode::Binary(Op::Shl, x2, k2),
                                    )
                                    | (
                                        MachENode::Binary(Op::Shl, x2, k2),
                                        MachENode::Binary(Op::Lshr, x1, k1),
                                    ) => Some((x1, k1, x2, k2)),
                                    _ => None,
                                };
                                if let Some((x1, k1, x2, k2)) = rot_pair {
                                    let rx1 = eg.uf.find(x1);
                                    let rx2 = eg.uf.find(x2);
                                    if rx1 == rx2
                                        && let (Some(v1), Some(v2)) =
                                            (eg.class_const(k1), eg.class_const(k2))
                                        && v1 > 0
                                        && v2 > 0
                                        && v1.saturating_add(v2) == u64::from(bits)
                                        && let Some(kc) = eg.add(MachENode::Const(v1), ty, bits)
                                        && let Some(rot) =
                                            eg.add(MachENode::Binary(Op::Rotr, rx1, kc), ty, bits)
                                    {
                                        unions.push((cid_e, rot));
                                    }
                                }
                            }
                        }
                    }

                    // 3b. Associative constant folding + shifted/rotated operand reassociation
                    //     for `Add`, `Xor`, `And`, `Or`, `Mul`.
                    if matches!(op, Op::Add | Op::Xor | Op::And | Op::Or | Op::Mul) {
                        // `(x op c1) op c2 -> x op (c1 op c2)`.
                        for &(inner_nodes, outer_c) in &[(&a_nodes, cb), (&b_nodes, ca)] {
                            if let Some(c2) = outer_c {
                                for &in_n in inner_nodes.as_slice() {
                                    if eg.at_budget() {
                                        break;
                                    }
                                    if let MachENode::Binary(iop, x, y) = in_n
                                        && iop == op
                                    {
                                        let rx = eg.uf.find(x);
                                        let ry = eg.uf.find(y);
                                        let folded = if let Some(c1) = eg.class_const(ry) {
                                            Some((rx, c1))
                                        } else if let Some(c1) = eg.class_const(rx) {
                                            Some((ry, c1))
                                        } else {
                                            None
                                        };
                                        if let Some((var_e, c1)) = folded
                                            && let Ok(c12) =
                                                binary(op, u128::from(c1), u128::from(c2), bits)
                                            && let Some(c_id) = eg.add(
                                                MachENode::Const(trunc_u64(c12) & mask),
                                                ty,
                                                bits,
                                            )
                                            && let Some(re) = eg.add(
                                                MachENode::Binary(op, var_e, c_id),
                                                ty,
                                                bits,
                                            )
                                        {
                                            unions.push((cid_e, re));
                                        }
                                    }
                                }
                            }
                        }

                        // Reassociate shifted/rotated operand to outer RHS so `add+lsl` / `eor+ror` fuses!
                        // `(a op shift(b, k)) op c -> (a op c) op shift(b, k)`.
                        if matches!(op, Op::Add | Op::Xor | Op::Or | Op::And) {
                            for &(inner_nodes, other_r) in &[(&a_nodes, rb), (&b_nodes, ra)] {
                                for &in_n in inner_nodes.as_slice() {
                                    if eg.at_budget() {
                                        break;
                                    }
                                    if let MachENode::Binary(iop, x, y) = in_n
                                        && iop == op
                                    {
                                        let rx = eg.uf.find(x);
                                        let ry = eg.uf.find(y);
                                        let x_has_shift = has_const_shift_or_rot(eg, rx, op);
                                        let y_has_shift = has_const_shift_or_rot(eg, ry, op);
                                        if y_has_shift && !x_has_shift {
                                            if let Some(ac) =
                                                eg.add(MachENode::Binary(op, rx, other_r), ty, bits)
                                                && let Some(re) =
                                                    eg.add(MachENode::Binary(op, ac, ry), ty, bits)
                                            {
                                                unions.push((cid_e, re));
                                            }
                                        } else if x_has_shift && !y_has_shift {
                                            if let Some(ac) =
                                                eg.add(MachENode::Binary(op, ry, other_r), ty, bits)
                                                && let Some(re) =
                                                    eg.add(MachENode::Binary(op, ac, rx), ty, bits)
                                            {
                                                unions.push((cid_e, re));
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }

                    // 3c. Op-specific structural rewrites.
                    match op {
                        Op::Add => {
                            // `Add(Not(x), 1) -> Neg(x)`.
                            if cb == Some(1) {
                                for &an in a_nodes.as_slice() {
                                    if let MachENode::Unary(Op::Not, x) = an
                                        && let Some(ng) =
                                            eg.add(MachENode::Unary(Op::Neg, x), ty, bits)
                                    {
                                        unions.push((cid_e, ng));
                                    }
                                }
                            }
                            if ca == Some(1) {
                                for &bn in b_nodes.as_slice() {
                                    if let MachENode::Unary(Op::Not, x) = bn
                                        && let Some(ng) =
                                            eg.add(MachENode::Unary(Op::Neg, x), ty, bits)
                                    {
                                        unions.push((cid_e, ng));
                                    }
                                }
                            }
                            // `Add(x, Zext(c)) <-> Select(c, x + 1, x)` (exposes AArch64 `csinc`!).
                            for &(z_nodes, base_r) in &[(&b_nodes, ra), (&a_nodes, rb)] {
                                for &zn in z_nodes.as_slice() {
                                    if let MachENode::Zext(c_i1) = zn
                                        && eg.class_ty_bits(c_i1) == Some((Ty::I1, 1))
                                        && let Some(one) = eg.add(MachENode::Const(1), ty, bits)
                                        && let Some(inc) =
                                            eg.add(MachENode::Binary(Op::Add, base_r, one), ty, bits)
                                        && let Some(sel) =
                                            eg.add(MachENode::Select(c_i1, inc, base_r), ty, bits)
                                    {
                                        unions.push((cid_e, sel));
                                    }
                                }
                            }
                            // Cancellation: `(x - y) + y -> x`.
                            for &an in a_nodes.as_slice() {
                                if let MachENode::Binary(Op::Sub, x, y) = an
                                    && eg.uf.find(y) == rb
                                {
                                    unions.push((cid_e, x));
                                }
                            }
                            for &bn in b_nodes.as_slice() {
                                if let MachENode::Binary(Op::Sub, x, y) = bn
                                    && eg.uf.find(y) == ra
                                {
                                    unions.push((cid_e, x));
                                }
                            }
                            // Distributive factoring: `(x * c) + (y * c) -> (x + y) * c`, `(x << k) + (y << k) -> (x + y) << k`.
                            factor_add_sub(eg, cid_e, Op::Add, &a_nodes, &b_nodes, ty, bits, &mut unions);
                        }
                        Op::Sub => {
                            // Cancellation: `(x + y) - y -> x`, `x - (x - y) -> y`.
                            for &an in a_nodes.as_slice() {
                                if let MachENode::Binary(Op::Add, x, y) = an {
                                    if eg.uf.find(y) == rb {
                                        unions.push((cid_e, x));
                                    } else if eg.uf.find(x) == rb {
                                        unions.push((cid_e, y));
                                    }
                                }
                            }
                            for &bn in b_nodes.as_slice() {
                                if let MachENode::Binary(Op::Sub, x, y) = bn
                                    && eg.uf.find(x) == ra
                                {
                                    unions.push((cid_e, y));
                                }
                            }
                            factor_add_sub(eg, cid_e, Op::Sub, &a_nodes, &b_nodes, ty, bits, &mut unions);
                        }
                        Op::And => {
                            // De Morgan: `(!x) & (!y) -> !(x | y)`.
                            for &an in a_nodes.as_slice() {
                                if let MachENode::Unary(Op::Not, x) = an {
                                    let rx = eg.uf.find(x);
                                    if rx == rb
                                        && let Some(z) = eg.add(MachENode::Const(0), ty, bits)
                                    {
                                        unions.push((cid_e, z));
                                    }
                                    for &bn in b_nodes.as_slice() {
                                        if let MachENode::Unary(Op::Not, y) = bn
                                            && let Some(or_xy) =
                                                eg.add(MachENode::Binary(Op::Or, x, y), ty, bits)
                                            && let Some(not_or) =
                                                eg.add(MachENode::Unary(Op::Not, or_xy), ty, bits)
                                        {
                                            unions.push((cid_e, not_or));
                                        }
                                    }
                                }
                            }
                            for &bn in b_nodes.as_slice() {
                                if let MachENode::Unary(Op::Not, y) = bn
                                    && eg.uf.find(y) == ra
                                    && let Some(z) = eg.add(MachENode::Const(0), ty, bits)
                                {
                                    unions.push((cid_e, z));
                                }
                                // Absorption: `x & (x | y) -> x`.
                                if let MachENode::Binary(Op::Or, x, y) = bn
                                    && (eg.uf.find(x) == ra || eg.uf.find(y) == ra)
                                {
                                    unions.push((cid_e, ra));
                                }
                            }
                            for &an in a_nodes.as_slice() {
                                if let MachENode::Binary(Op::Or, x, y) = an
                                    && (eg.uf.find(x) == rb || eg.uf.find(y) == rb)
                                {
                                    unions.push((cid_e, rb));
                                }
                            }
                        }
                        Op::Or => {
                            // De Morgan: `(!x) | (!y) -> !(x & y)`.
                            for &an in a_nodes.as_slice() {
                                if let MachENode::Unary(Op::Not, x) = an {
                                    let rx = eg.uf.find(x);
                                    if rx == rb
                                        && let Some(m) = eg.add(MachENode::Const(mask), ty, bits)
                                    {
                                        unions.push((cid_e, m));
                                    }
                                    for &bn in b_nodes.as_slice() {
                                        if let MachENode::Unary(Op::Not, y) = bn
                                            && let Some(and_xy) =
                                                eg.add(MachENode::Binary(Op::And, x, y), ty, bits)
                                            && let Some(not_and) =
                                                eg.add(MachENode::Unary(Op::Not, and_xy), ty, bits)
                                        {
                                            unions.push((cid_e, not_and));
                                        }
                                    }
                                }
                            }
                            for &bn in b_nodes.as_slice() {
                                if let MachENode::Unary(Op::Not, y) = bn
                                    && eg.uf.find(y) == ra
                                    && let Some(m) = eg.add(MachENode::Const(mask), ty, bits)
                                {
                                    unions.push((cid_e, m));
                                }
                                // Absorption: `x | (x & y) -> x`.
                                if let MachENode::Binary(Op::And, x, y) = bn
                                    && (eg.uf.find(x) == ra || eg.uf.find(y) == ra)
                                {
                                    unions.push((cid_e, ra));
                                }
                            }
                            for &an in a_nodes.as_slice() {
                                if let MachENode::Binary(Op::And, x, y) = an
                                    && (eg.uf.find(x) == rb || eg.uf.find(y) == rb)
                                {
                                    unions.push((cid_e, rb));
                                }
                            }
                        }
                        Op::Xor => {
                            // Cancellation: `(x ^ y) ^ y -> x`.
                            for &an in a_nodes.as_slice() {
                                if let MachENode::Binary(Op::Xor, x, y) = an {
                                    if eg.uf.find(y) == rb {
                                        unions.push((cid_e, x));
                                    } else if eg.uf.find(x) == rb {
                                        unions.push((cid_e, y));
                                    }
                                }
                                // `(!x) ^ (!y) -> x ^ y` and `(!x) ^ x -> all_ones`.
                                if let MachENode::Unary(Op::Not, x) = an {
                                    if eg.uf.find(x) == rb
                                        && let Some(m) = eg.add(MachENode::Const(mask), ty, bits)
                                    {
                                        unions.push((cid_e, m));
                                    }
                                    for &bn in b_nodes.as_slice() {
                                        if let MachENode::Unary(Op::Not, y) = bn
                                            && let Some(xy) =
                                                eg.add(MachENode::Binary(Op::Xor, x, y), ty, bits)
                                        {
                                            unions.push((cid_e, xy));
                                        }
                                    }
                                }
                            }
                            for &bn in b_nodes.as_slice() {
                                if let MachENode::Binary(Op::Xor, x, y) = bn {
                                    if eg.uf.find(y) == ra {
                                        unions.push((cid_e, x));
                                    } else if eg.uf.find(x) == ra {
                                        unions.push((cid_e, y));
                                    }
                                }
                                if let MachENode::Unary(Op::Not, y) = bn
                                    && eg.uf.find(y) == ra
                                    && let Some(m) = eg.add(MachENode::Const(mask), ty, bits)
                                {
                                    unions.push((cid_e, m));
                                }
                            }
                        }
                        Op::Lshr => {
                            // `(x << k) >> k -> x & ((1 << (bits - k)) - 1)`.
                            if let Some(k2) = cb
                                && k2 > 0
                                && k2 < u64::from(bits)
                            {
                                for &an in a_nodes.as_slice() {
                                    if let MachENode::Binary(Op::Shl, x, k1) = an
                                        && eg.class_const(k1) == Some(k2)
                                    {
                                        let keep = bits.saturating_sub(trunc_u32(k2));
                                        let m = mask_of(keep);
                                        if let Some(mc) = eg.add(MachENode::Const(m), ty, bits)
                                            && let Some(and) =
                                                eg.add(MachENode::Binary(Op::And, x, mc), ty, bits)
                                        {
                                            unions.push((cid_e, and));
                                        }
                                    } else if let MachENode::Binary(Op::Lshr, x, k1) = an
                                        && let Some(v1) = eg.class_const(k1)
                                        && v1 < u64::from(bits)
                                    {
                                        let sum = v1.saturating_add(k2);
                                        if sum < u64::from(bits) {
                                            if let Some(sc) = eg.add(MachENode::Const(sum), ty, bits)
                                                && let Some(sh) = eg.add(
                                                    MachENode::Binary(Op::Lshr, x, sc),
                                                    ty,
                                                    bits,
                                                )
                                            {
                                                unions.push((cid_e, sh));
                                            }
                                        } else if let Some(z) = eg.add(MachENode::Const(0), ty, bits) {
                                            unions.push((cid_e, z));
                                        }
                                    }
                                }
                            }
                        }
                        Op::Shl => {
                            // `(x << k1) << k2 -> x << (k1 + k2)` (or `0` when `k1 + k2 >= bits`).
                            if let Some(k2) = cb
                                && k2 < u64::from(bits)
                            {
                                for &an in a_nodes.as_slice() {
                                    if let MachENode::Binary(Op::Shl, x, k1) = an
                                        && let Some(v1) = eg.class_const(k1)
                                        && v1 < u64::from(bits)
                                    {
                                        let sum = v1.saturating_add(k2);
                                        if sum < u64::from(bits) {
                                            if let Some(sc) = eg.add(MachENode::Const(sum), ty, bits)
                                                && let Some(sh) = eg.add(
                                                    MachENode::Binary(Op::Shl, x, sc),
                                                    ty,
                                                    bits,
                                                )
                                            {
                                                unions.push((cid_e, sh));
                                            }
                                        } else if let Some(z) = eg.add(MachENode::Const(0), ty, bits) {
                                            unions.push((cid_e, z));
                                        }
                                    }
                                }
                            }
                        }
                        Op::Rotr => {
                            // `rotr(rotr(x, k1), k2) -> rotr(x, (k1 + k2) % bits)`.
                            if let Some(k2) = cb {
                                for &an in a_nodes.as_slice() {
                                    if let MachENode::Binary(Op::Rotr, x, k1) = an
                                        && let Some(v1) = eg.class_const(k1)
                                    {
                                        let comb =
                                            (v1 % u64::from(bits) + k2 % u64::from(bits)) % u64::from(bits);
                                        if comb == 0 {
                                            unions.push((cid_e, x));
                                        } else if let Some(kc) =
                                            eg.add(MachENode::Const(comb), ty, bits)
                                            && let Some(rr) = eg.add(
                                                MachENode::Binary(Op::Rotr, x, kc),
                                                ty,
                                                bits,
                                            )
                                        {
                                            unions.push((cid_e, rr));
                                        }
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    let mut applied = 0u32;
    for (a, b) in unions {
        if eg.union(a, b) {
            applied = applied.saturating_add(1);
        }
    }
    if applied > 0 {
        eg.rebuild();
    }
    applied
}

#[inline]
fn has_const_shift_or_rot<A: Allocator + Copy>(eg: &MachEGraph<A>, cid: EId, outer: Op) -> bool {
    let root = eg.uf.find_ro(cid);
    let Some(cls) = eg.classes.get(to_idx(root)) else {
        return false;
    };
    let bits = u64::from(cls.bits);
    for &n in cls.nodes.as_slice() {
        let ok = match n {
            MachENode::Binary(Op::Shl | Op::Lshr | Op::Ashr, _, k) => {
                eg.class_const(k).is_some_and(|v| v < bits)
            }
            MachENode::Binary(Op::Rotr, _, k) if matches!(outer, Op::And | Op::Or | Op::Xor) => {
                eg.class_const(k).is_some_and(|v| v < bits)
            }
            _ => false,
        };
        if ok {
            return true;
        }
    }
    false
}

fn factor_add_sub<A: Allocator + Copy>(
    eg: &mut MachEGraph<A>,
    cid_e: EId,
    op: Op,
    a_nodes: &Vec<MachENode, A>,
    b_nodes: &Vec<MachENode, A>,
    ty: Ty,
    bits: u32,
    unions: &mut Vec<(EId, EId), A>,
) {
    for &an in a_nodes.as_slice() {
        if eg.at_budget() {
            break;
        }
        if let MachENode::Binary(Op::Mul, x1, x2) = an {
            for &bn in b_nodes.as_slice() {
                if let MachENode::Binary(Op::Mul, y1, y2) = bn {
                    let common = if x2 == y2 {
                        Some((x1, y1, x2))
                    } else if x1 == y1 {
                        Some((x2, y2, x1))
                    } else if x1 == y2 {
                        Some((x2, y1, x1))
                    } else if x2 == y1 {
                        Some((x1, y2, x2))
                    } else {
                        None
                    };
                    if let Some((lhs, rhs, factor)) = common
                        && let Some(comb) = eg.add(MachENode::Binary(op, lhs, rhs), ty, bits)
                        && let Some(prod) = eg.add(MachENode::Binary(Op::Mul, comb, factor), ty, bits)
                    {
                        unions.push((cid_e, prod));
                    }
                }
            }
        } else if let MachENode::Binary(Op::Shl, x, k1) = an {
            for &bn in b_nodes.as_slice() {
                if let MachENode::Binary(Op::Shl, y, k2) = bn
                    && k1 == k2
                    && let Some(comb) = eg.add(MachENode::Binary(op, x, y), ty, bits)
                    && let Some(sh) = eg.add(MachENode::Binary(Op::Shl, comb, k1), ty, bits)
                {
                    unions.push((cid_e, sh));
                }
            }
        }
    }
}

// ============================================================================
// Bounded Enumerative & Meet-in-the-Middle Bitvector Superoptimizer
// ============================================================================

/// Canonicalized small pure bitvector DAG (`<= 6` ops, `<= 3` external inputs) extracted from an
/// e-class for BLAKE3 cache lookup and bounded enumerative synthesis.
#[derive(Clone, Copy, Debug)]
struct CanonicalDag {
    bits: u32,
    num_inputs: u8,
    num_nodes: u8,
    /// Original e-class ids corresponding to canonical inputs `v0..v_{num_inputs-1}`.
    leaf_eids: [EId; MAX_DAG_INPUTS],
    /// Normalized topological nodes (`0..num_nodes`).
    nodes: [SynthInst; MAX_DAG_OPS],
    /// Small shift/constant hints extracted from the DAG to guide synthesis.
    const_hints: [u64; 4],
    num_hints: u8,
}

impl CanonicalDag {
    /// Computes the machine-independent canonical BLAKE3 digest of this DAG (Rule 25).
    fn digest(&self) -> [u8; 32] {
        let mut h = Blake3Hasher::new(DAG_HASH_DOMAIN);
        let bits_b = u8::try_from(self.bits).unwrap_or(64);
        h.update(&[bits_b, self.num_inputs, self.num_nodes]);
        let n = usize::from(self.num_nodes).min(MAX_DAG_OPS);
        for i in 0..n {
            if let Some(inst) = self.nodes.get(i) {
                h.update(&[inst.op.to_byte(), inst.a, inst.b]);
                h.update(&inst.imm.to_le_bytes());
            }
        }
        h.finish().0
    }

    /// Evaluates this DAG on canonical inputs `inputs[0..num_inputs]` at bit width `eval_bits`.
    #[inline]
    fn eval(&self, inputs: &[u64; MAX_DAG_INPUTS], eval_bits: u32) -> u64 {
        let mut vals = [0u64; MAX_DAG_INPUTS + MAX_DAG_OPS];
        let ni = usize::from(self.num_inputs).min(MAX_DAG_INPUTS);
        for i in 0..ni {
            if let (Some(dst), Some(&src)) = (vals.get_mut(i), inputs.get(i)) {
                *dst = src & mask_of(eval_bits);
            }
        }
        let n = usize::from(self.num_nodes).min(MAX_DAG_OPS);
        for i in 0..n {
            if let Some(inst) = self.nodes.get(i) {
                let va = vals.get(usize::from(inst.a)).copied().unwrap_or(0);
                let vb = vals.get(usize::from(inst.b)).copied().unwrap_or(0);
                let r = eval_synth_inst(*inst, va, vb, self.bits, eval_bits);
                if let Some(dst) = vals.get_mut(ni + i) {
                    *dst = r;
                }
            }
        }
        if n == 0 {
            0
        } else {
            vals.get(ni + n - 1).copied().unwrap_or(0)
        }
    }

    /// Returns `true` if every operation and constant in this DAG is bit-width-polymorphic
    /// (meaning exact 8-bit/4-bit verification is semantically valid for `bits` = 32/64).
    fn is_width_polymorphic(&self) -> bool {
        let orig_mask = mask_of(self.bits);
        let n = usize::from(self.num_nodes).min(MAX_DAG_OPS);
        for i in 0..n {
            let Some(inst) = self.nodes.get(i) else {
                return false;
            };
            match inst.op {
                SynthOp::Shl | SynthOp::Lshr | SynthOp::Ashr | SynthOp::Rotr => return false,
                SynthOp::Const => {
                    if inst.imm != 0 && inst.imm != 1 && inst.imm != orig_mask {
                        return false;
                    }
                }
                _ => {}
            }
        }
        true
    }
}

#[inline]
fn eval_synth_inst(inst: SynthInst, va: u64, vb: u64, orig_bits: u32, eval_bits: u32) -> u64 {
    let m = mask_of(eval_bits);
    let a = va & m;
    let b = vb & m;
    match inst.op {
        SynthOp::Add => a.wrapping_add(b) & m,
        SynthOp::Sub => a.wrapping_sub(b) & m,
        SynthOp::Mul => a.wrapping_mul(b) & m,
        SynthOp::And => (a & b) & m,
        SynthOp::Or => (a | b) & m,
        SynthOp::Xor => (a ^ b) & m,
        SynthOp::Shl => {
            let sh = trunc_u32(b % u64::from(eval_bits));
            a.wrapping_shl(sh) & m
        }
        SynthOp::Lshr => {
            let sh = trunc_u32(b % u64::from(eval_bits));
            a.wrapping_shr(sh) & m
        }
        SynthOp::Ashr => {
            let sh = trunc_u32(b % u64::from(eval_bits));
            let sa = sext(u128::from(a), eval_bits);
            trunc_u64((sa >> sh).cast_unsigned()) & m
        }
        SynthOp::Rotr => {
            let sh = trunc_u32(b % u64::from(eval_bits));
            if sh == 0 {
                a
            } else {
                let lsh = eval_bits.saturating_sub(sh);
                ((a >> sh) | (a.wrapping_shl(lsh))) & m
            }
        }
        SynthOp::Smin => {
            let sa = sext(u128::from(a), eval_bits);
            let sb = sext(u128::from(b), eval_bits);
            if sa <= sb { a } else { b }
        }
        SynthOp::Smax => {
            let sa = sext(u128::from(a), eval_bits);
            let sb = sext(u128::from(b), eval_bits);
            if sa >= sb { a } else { b }
        }
        SynthOp::Umin => a.min(b),
        SynthOp::Umax => a.max(b),
        SynthOp::Neg => 0u64.wrapping_sub(a) & m,
        SynthOp::Not => (!a) & m,
        SynthOp::Bic => (a & !b) & m,
        SynthOp::Orn => (a | !b) & m,
        SynthOp::Eon => (a ^ !b) & m,
        SynthOp::Copy => a,
        SynthOp::Const => {
            let orig_mask = mask_of(orig_bits);
            if inst.imm == orig_mask { m } else { inst.imm & m }
        }
    }
}

#[inline]
fn eval_synth_prog(prog: &SynthProg, inputs: &[u64; MAX_DAG_INPUTS], eval_bits: u32) -> u64 {
    let orig_bits = u32::from(prog.bits);
    let mut vals = [0u64; MAX_DAG_INPUTS + MAX_SYNTH_LEN];
    let ni = usize::from(prog.num_inputs).min(MAX_DAG_INPUTS);
    for i in 0..ni {
        if let (Some(dst), Some(&src)) = (vals.get_mut(i), inputs.get(i)) {
            *dst = src & mask_of(eval_bits);
        }
    }
    let n = usize::from(prog.len).min(MAX_SYNTH_LEN);
    for i in 0..n {
        if let Some(inst) = prog.insts.get(i) {
            let va = vals.get(usize::from(inst.a)).copied().unwrap_or(0);
            let vb = vals.get(usize::from(inst.b)).copied().unwrap_or(0);
            let r = eval_synth_inst(*inst, va, vb, orig_bits, eval_bits);
            if let Some(dst) = vals.get_mut(ni + i) {
                *dst = r;
            }
        }
    }
    if n == 0 {
        0
    } else {
        vals.get(ni + n - 1).copied().unwrap_or(0)
    }
}

#[inline]
fn prog_is_width_polymorphic(prog: &SynthProg) -> bool {
    let orig_mask = mask_of(u32::from(prog.bits));
    let n = usize::from(prog.len).min(MAX_SYNTH_LEN);
    for i in 0..n {
        let Some(inst) = prog.insts.get(i) else {
            return false;
        };
        match inst.op {
            SynthOp::Shl | SynthOp::Lshr | SynthOp::Ashr | SynthOp::Rotr => return false,
            SynthOp::Const => {
                if inst.imm != 0 && inst.imm != 1 && inst.imm != orig_mask {
                    return false;
                }
            }
            _ => {}
        }
    }
    true
}

#[inline]
fn lir_op_to_synth(op: Op) -> Option<SynthOp> {
    match op {
        Op::Add => Some(SynthOp::Add),
        Op::Sub => Some(SynthOp::Sub),
        Op::Mul => Some(SynthOp::Mul),
        Op::And => Some(SynthOp::And),
        Op::Or => Some(SynthOp::Or),
        Op::Xor => Some(SynthOp::Xor),
        Op::Shl => Some(SynthOp::Shl),
        Op::Lshr => Some(SynthOp::Lshr),
        Op::Ashr => Some(SynthOp::Ashr),
        Op::Rotr => Some(SynthOp::Rotr),
        Op::Smin => Some(SynthOp::Smin),
        Op::Smax => Some(SynthOp::Smax),
        Op::Umin => Some(SynthOp::Umin),
        Op::Umax => Some(SynthOp::Umax),
        Op::Neg => Some(SynthOp::Neg),
        Op::Not => Some(SynthOp::Not),
        _ => None,
    }
}

/// Iterative post-order extraction of a small canonical pure bitvector DAG rooted at `root_cid`.
/// Returns `None` if the DAG exceeds `MAX_DAG_OPS` ops or `MAX_DAG_INPUTS` inputs, or is trivial.
fn extract_canonical_dag<A: Allocator + Copy>(
    eg: &MachEGraph<A>,
    best: &[Option<BestChoice>],
    root_cid: EId,
    alloc: A,
) -> Option<CanonicalDag> {
    let (root_ty, bits) = eg.class_ty_bits(root_cid)?;
    if root_ty != Ty::I32 && root_ty != Ty::I64 {
        return None;
    }

    // Two-phase iterative traversal:
    // 1. Collect distinct leaves (`leaf_eids`) and internal nodes (`raw_nodes`) in post-order.
    //    Nodes reference either `Leaf(idx)` or `Internal(idx)` tagged in high bit (`0x80`).
    enum Frame {
        Enter(EId),
        FinishUnary { cid: EId, op: SynthOp },
        FinishBinary { cid: EId, op: SynthOp },
    }

    let mut stack: Vec<Frame, A> = Vec::with_capacity_in(16, alloc);
    let mut results: Vec<u8, A> = Vec::with_capacity_in(16, alloc);
    let mut visited_eids: [EId; MAX_DAG_INPUTS + MAX_DAG_OPS] = [NONE; MAX_DAG_INPUTS + MAX_DAG_OPS];
    let mut visited_refs: [u8; MAX_DAG_INPUTS + MAX_DAG_OPS] = [0; MAX_DAG_INPUTS + MAX_DAG_OPS];
    let mut visited_len = 0usize;

    let mut leaf_eids = [NONE; MAX_DAG_INPUTS];
    let mut num_leaves = 0usize;

    let mut raw_nodes = [SynthInst::NOP; MAX_DAG_OPS];
    let mut num_nodes = 0usize;

    let mut const_hints = [0u64; 4];
    let mut num_hints = 0usize;

    let mut active: [EId; MAX_DAG_OPS + 2] = [NONE; MAX_DAG_OPS + 2];
    let mut active_len = 0usize;

    stack.push(Frame::Enter(root_cid));

    while let Some(frame) = stack.pop() {
        match frame {
            Frame::Enter(cur) => {
                let r = eg.uf.find_ro(cur);
                // Check memo table first.
                let mut found = None;
                for i in 0..visited_len {
                    if visited_eids.get(i).copied() == Some(r) {
                        found = visited_refs.get(i).copied();
                        break;
                    }
                }
                if let Some(tag) = found {
                    results.push(tag);
                    continue;
                }
                // Guard against cycles.
                for i in 0..active_len {
                    if active.get(i).copied() == Some(r) {
                        return None;
                    }
                }

                let choice = best.get(to_idx(r)).copied().flatten()?;
                let (c_ty, _) = eg.class_ty_bits(r)?;
                if c_ty != root_ty {
                    return None;
                }

                match choice.node {
                    MachENode::Const(c) => {
                        if num_nodes >= MAX_DAG_OPS || visited_len >= MAX_DAG_INPUTS + MAX_DAG_OPS {
                            return None;
                        }
                        if num_hints < 4
                            && c > 0
                            && c < u64::from(bits)
                            && !const_hints.get(0..num_hints).is_some_and(|s| s.contains(&c))
                        {
                            set_at(&mut const_hints, num_hints, c);
                            num_hints += 1;
                        }
                        let idx_u8 = u8::try_from(num_nodes).ok()?;
                        set_at(&mut raw_nodes, num_nodes, SynthInst { op: SynthOp::Const, a: 0, b: 0, imm: c });
                        num_nodes += 1;
                        let tag = 0x80 | idx_u8;
                        set_at(&mut visited_eids, visited_len, r);
                        set_at(&mut visited_refs, visited_len, tag);
                        visited_len += 1;
                        results.push(tag);
                    }
                    MachENode::Unary(op, a) if lir_op_to_synth(op).is_some() => {
                        let sop = lir_op_to_synth(op)?;
                        if active_len >= active.len() {
                            return None;
                        }
                        set_at(&mut active, active_len, r);
                        active_len += 1;
                        stack.push(Frame::FinishUnary { cid: r, op: sop });
                        stack.push(Frame::Enter(a));
                    }
                    MachENode::Binary(op, a, b) if lir_op_to_synth(op).is_some() => {
                        let sop = lir_op_to_synth(op)?;
                        if active_len >= active.len() {
                            return None;
                        }
                        set_at(&mut active, active_len, r);
                        active_len += 1;
                        stack.push(Frame::FinishBinary { cid: r, op: sop });
                        stack.push(Frame::Enter(b));
                        stack.push(Frame::Enter(a));
                    }
                    // Treat `Var`, `Select`, `Zext`, or unsupported ops as opaque external leaf inputs.
                    _ => {
                        if num_leaves >= MAX_DAG_INPUTS || visited_len >= MAX_DAG_INPUTS + MAX_DAG_OPS
                        {
                            return None;
                        }
                        let leaf_idx = u8::try_from(num_leaves).ok()?;
                        set_at(&mut leaf_eids, num_leaves, r);
                        num_leaves += 1;
                        set_at(&mut visited_eids, visited_len, r);
                        set_at(&mut visited_refs, visited_len, leaf_idx);
                        visited_len += 1;
                        results.push(leaf_idx);
                    }
                }
            }
            Frame::FinishUnary { cid, op } => {
                let a_tag = results.pop()?;
                active_len = active_len.saturating_sub(1);
                if num_nodes >= MAX_DAG_OPS || visited_len >= MAX_DAG_INPUTS + MAX_DAG_OPS {
                    return None;
                }
                let idx_u8 = u8::try_from(num_nodes).ok()?;
                set_at(&mut raw_nodes, num_nodes, SynthInst { op, a: a_tag, b: 0, imm: 0 });
                num_nodes += 1;
                let tag = 0x80 | idx_u8;
                set_at(&mut visited_eids, visited_len, cid);
                set_at(&mut visited_refs, visited_len, tag);
                visited_len += 1;
                results.push(tag);
            }
            Frame::FinishBinary { cid, op } => {
                let b_tag = results.pop()?;
                let a_tag = results.pop()?;
                active_len = active_len.saturating_sub(1);
                if num_nodes >= MAX_DAG_OPS || visited_len >= MAX_DAG_INPUTS + MAX_DAG_OPS {
                    return None;
                }
                let idx_u8 = u8::try_from(num_nodes).ok()?;
                set_at(&mut raw_nodes, num_nodes, SynthInst { op, a: a_tag, b: b_tag, imm: 0 });
                num_nodes += 1;
                let tag = 0x80 | idx_u8;
                set_at(&mut visited_eids, visited_len, cid);
                set_at(&mut visited_refs, visited_len, tag);
                visited_len += 1;
                results.push(tag);
            }
        }
    }

    if num_nodes < 2 || num_leaves == 0 {
        return None;
    }

    // Resolve tagged operand indices: leaf `k` -> `k`, internal `0x80 | j` -> `num_leaves + j`.
    let ni_u8 = u8::try_from(num_leaves).ok()?;
    let nn_u8 = u8::try_from(num_nodes).ok()?;
    let mut resolved = [SynthInst::NOP; MAX_DAG_OPS];
    for i in 0..num_nodes {
        let raw = raw_nodes.get(i).copied()?;
        let resolve_op = |tag: u8| -> u8 {
            if (tag & 0x80) != 0 {
                ni_u8.saturating_add(tag & 0x7f)
            } else {
                tag
            }
        };
        let inst = match raw.op.arity() {
            0 => raw,
            1 => SynthInst { op: raw.op, a: resolve_op(raw.a), b: 0, imm: 0 },
            _ => SynthInst { op: raw.op, a: resolve_op(raw.a), b: resolve_op(raw.b), imm: 0 },
        };
        set_at(&mut resolved, i, inst);
    }

    Some(CanonicalDag {
        bits,
        num_inputs: ni_u8,
        num_nodes: nn_u8,
        leaf_eids,
        nodes: resolved,
        const_hints,
        num_hints: u8::try_from(num_hints).unwrap_or(0),
    })
}

/// Deterministic SplitMix64 step for bitvector oracle vectors (Rule 25: fixed seed).
#[inline]
const fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Builds the 64-vector deterministic bitvector oracle suite (edge cases + SplitMix64 vectors).
fn build_oracle_vectors(bits: u32) -> [[u64; MAX_DAG_INPUTS]; ORACLE_VECTORS] {
    let m = mask_of(bits);
    let sign = if bits >= 64 { 1u64 << 63 } else { 1u64 << (bits - 1) };
    let smax = sign.wrapping_sub(1);
    let alt5 = 0x5555_5555_5555_5555u64 & m;
    let alta = 0xAAAA_AAAA_AAAA_AAAAu64 & m;
    let lowf = 0x0F0F_0F0F_0F0F_0F0Fu64 & m;
    let hif = 0xF0F0_F0F0_F0F0_F0F0u64 & m;

    let mut vecs = [[0u64; MAX_DAG_INPUTS]; ORACLE_VECTORS];
    let fixed: [[u64; 3]; 16] = [
        [0, 0, 0],
        [1, m, alt5],
        [alta, sign, smax],
        [0x1234_5678_9ABC_DEF0 & m, 0xFEDC_BA98_7654_3210 & m, 0x00FF_00FF_00FF_00FF & m],
        [m, m, m],
        [sign, sign, 1],
        [smax, 1, m],
        [alt5, alta, lowf],
        [lowf, hif, alt5],
        [0, 1, 2],
        [2, 3, 4],
        [7, 13, 29],
        [m.wrapping_sub(1), 1, sign],
        [0x8000_0001u64 & m, 0x7FFF_FFFFu64 & m, 0xFFFF_0000u64 & m],
        [0x0123_4567_89AB_CDEF & m, 0x0123_4567_89AB_CDEF & m, 0],
        [1 << 4, 1 << 7, 1 << 15],
    ];
    for (i, &row) in fixed.iter().enumerate() {
        set_at(&mut vecs, i, row);
    }
    let mut rng = 0x243F_6A88_85A3_08D3u64 ^ u64::from(bits);
    for i in fixed.len()..ORACLE_VECTORS {
        let a = splitmix64(&mut rng) & m;
        let b = splitmix64(&mut rng) & m;
        let c = splitmix64(&mut rng) & m;
        set_at(&mut vecs, i, [a, b, c]);
    }
    vecs
}

/// Verifies a candidate [`SynthProg`] against `dag` using:
/// 1. Full 64-vector edge-case + SplitMix64 oracle at `dag.bits`.
/// 2. Exact exhaustive 8-bit (`1..=2` inputs) or 4-bit (`3` inputs) verification when both `dag`
///    and `cand` are bit-width-polymorphic, OR an extended 256-vector bit-boundary sweep otherwise.
fn verify_candidate_exact(
    dag: &CanonicalDag,
    cand: &SynthProg,
    vecs: &[[u64; MAX_DAG_INPUTS]; ORACLE_VECTORS],
    targets: &[u64; ORACLE_VECTORS],
) -> bool {
    // 1. Full 64-vector oracle at target width.
    for i in 0..ORACLE_VECTORS {
        let Some(inp) = vecs.get(i) else {
            return false;
        };
        let expected = targets.get(i).copied().unwrap_or(0);
        if eval_synth_prog(cand, inp, dag.bits) != expected {
            return false;
        }
    }

    // 2. Exact exhaustive small-width check for bit-width-polymorphic programs.
    if dag.is_width_polymorphic() && prog_is_width_polymorphic(cand) {
        match dag.num_inputs {
            1 => {
                for a in 0u64..256 {
                    let inp = [a, 0, 0];
                    if dag.eval(&inp, 8) != eval_synth_prog(cand, &inp, 8) {
                        return false;
                    }
                }
            }
            2 => {
                for a in 0u64..256 {
                    for b in 0u64..256 {
                        let inp = [a, b, 0];
                        if dag.eval(&inp, 8) != eval_synth_prog(cand, &inp, 8) {
                            return false;
                        }
                    }
                }
            }
            _ => {
                // 3 inputs: exhaustive 4-bit verification (4,096 triples).
                for a in 0u64..16 {
                    for b in 0u64..16 {
                        for c in 0u64..16 {
                            let inp = [a, b, c];
                            if dag.eval(&inp, 4) != eval_synth_prog(cand, &inp, 4) {
                                return false;
                            }
                        }
                    }
                }
            }
        }
        return true;
    }

    // 3. Non-polymorphic (e.g. wide constants or shifts): extended 256-vector bit-sweep + SplitMix64.
    let m = mask_of(dag.bits);
    for bit in 0..dag.bits {
        let p = (1u64 << bit) & m;
        let q = p.wrapping_sub(1) & m;
        for inp in &[[p, q, !p & m], [q, p, p], [p, p, q]] {
            if dag.eval(inp, dag.bits) != eval_synth_prog(cand, inp, dag.bits) {
                return false;
            }
        }
    }
    let mut rng = 0x1319_8A2E_0370_7344u64 ^ u64::from(dag.bits);
    for _ in 0..192 {
        let inp = [
            splitmix64(&mut rng) & m,
            splitmix64(&mut rng) & m,
            splitmix64(&mut rng) & m,
        ];
        if dag.eval(&inp, dag.bits) != eval_synth_prog(cand, &inp, dag.bits) {
            return false;
        }
    }
    true
}

/// 4-vector fast fingerprint for O(1) rejection and meet-in-the-middle lookup.
type FastSig = [u64; 4];

#[derive(Clone, Copy)]
struct MitmEntry {
    sig: FastSig,
    inst: SynthInst,
    cost: u32,
}

#[inline]
fn eval_sig_1op(
    inst: SynthInst,
    vecs: &[[u64; MAX_DAG_INPUTS]; ORACLE_VECTORS],
    bits: u32,
) -> FastSig {
    let mut out = [0u64; 4];
    for k in 0..4 {
        if let Some(inp) = vecs.get(k) {
            let va = inp.get(usize::from(inst.a)).copied().unwrap_or(0);
            let vb = inp.get(usize::from(inst.b)).copied().unwrap_or(0);
            let r = eval_synth_inst(inst, va, vb, bits, bits);
            set_at(&mut out, k, r);
        }
    }
    out
}

/// Bounded cost-ordered + meet-in-the-middle enumerative bitvector synthesizer.
///
/// Searches strictly lower-M5-cost programs (`cost < orig_cost`) within `budget` evaluations,
/// accepting a candidate ONLY when it passes 100% of [`verify_candidate_exact`].
fn synthesize_dag(
    dag: &CanonicalDag,
    orig_cost: u32,
    budget: u32,
    model: &AppleM5PortModel,
) -> Option<SynthProg> {
    if budget == 0 || orig_cost == 0 {
        return None;
    }

    let bits = dag.bits;
    let bits_u8 = u8::try_from(bits).ok()?;
    let mask = mask_of(bits);
    let ni = usize::from(dag.num_inputs).min(MAX_DAG_INPUTS);
    let ni_u8 = dag.num_inputs;
    let vecs = build_oracle_vectors(bits);

    let mut targets = [0u64; ORACLE_VECTORS];
    for i in 0..ORACLE_VECTORS {
        if let Some(inp) = vecs.get(i) {
            set_at(&mut targets, i, dag.eval(inp, bits));
        }
    }
    let target_sig: FastSig = [
        at(&targets, 0),
        at(&targets, 1),
        at(&targets, 2),
        at(&targets, 3),
    ];

    let mut rem = budget;

    let make_prog_1 = |inst: SynthInst, cost: u32| -> SynthProg {
        SynthProg {
            num_inputs: ni_u8,
            len: 1,
            bits: bits_u8,
            cost,
            insts: [inst, SynthInst::NOP, SynthInst::NOP],
        }
    };
    let make_prog_2 = |i0: SynthInst, i1: SynthInst, cost: u32| -> SynthProg {
        SynthProg {
            num_inputs: ni_u8,
            len: 2,
            bits: bits_u8,
            cost,
            insts: [i0, i1, SynthInst::NOP],
        }
    };

    // ---- Tier 0: 0-cost Identity (`Copy(v_i)`) and small constants (`0`, `1`, `-1`). ----
    for i in 0..ni {
        if rem == 0 {
            return None;
        }
        rem -= 1;
        let iu = u8::try_from(i).unwrap_or(0);
        let inst = SynthInst { op: SynthOp::Copy, a: iu, b: 0, imm: 0 };
        if eval_sig_1op(inst, &vecs, bits) == target_sig {
            let cand = make_prog_1(inst, 0);
            if verify_candidate_exact(dag, &cand, &vecs, &targets) {
                return Some(cand);
            }
        }
    }
    for &cval in &[0u64, 1u64, mask] {
        let ccost = model.const_cost(cval);
        if ccost >= orig_cost || rem == 0 {
            continue;
        }
        rem -= 1;
        let inst = SynthInst { op: SynthOp::Const, a: 0, b: 0, imm: cval };
        if eval_sig_1op(inst, &vecs, bits) == target_sig {
            let cand = make_prog_1(inst, ccost);
            if verify_candidate_exact(dag, &cand, &vecs, &targets) {
                return Some(cand);
            }
        }
    }

    // ---- Tier 1: Single-cycle 1-op candidates (cost = 100) + populate MITM table `S_1`. ----
    if model.simple_alu >= orig_cost {
        return None;
    }

    let mut mitm = [MitmEntry { sig: [0; 4], inst: SynthInst::NOP, cost: INF_COST }; MAX_MITM_STATES];
    let mut mitm_len = 0usize;

    let push_mitm = |mitm: &mut [MitmEntry; MAX_MITM_STATES],
                     mitm_len: &mut usize,
                     sig: FastSig,
                     inst: SynthInst,
                     cost: u32| {
        if *mitm_len >= MAX_MITM_STATES {
            return;
        }
        for k in 0..*mitm_len {
            if mitm.get(k).is_some_and(|e| e.sig == sig) {
                return;
            }
        }
        set_at(mitm, *mitm_len, MitmEntry { sig, inst, cost });
        *mitm_len += 1;
    };

    // 1a. Unary ops (`Neg`, `Not`) on inputs.
    for &uop in &[SynthOp::Not, SynthOp::Neg] {
        for i in 0..ni {
            if rem == 0 {
                return None;
            }
            rem -= 1;
            let iu = u8::try_from(i).unwrap_or(0);
            let inst = SynthInst { op: uop, a: iu, b: 0, imm: 0 };
            let sig = eval_sig_1op(inst, &vecs, bits);
            if sig == target_sig {
                let cand = make_prog_1(inst, model.simple_alu);
                if verify_candidate_exact(dag, &cand, &vecs, &targets) {
                    return Some(cand);
                }
            }
            push_mitm(&mut mitm, &mut mitm_len, sig, inst, model.simple_alu);
        }
    }

    // 1b. Commutative 1-op binary ops (`Xor`, `Add`, `And`, `Or`, `Smin`, `Smax`, `Umin`, `Umax`).
    let comm_ops: [SynthOp; 8] = [
        SynthOp::Xor,
        SynthOp::Add,
        SynthOp::And,
        SynthOp::Or,
        SynthOp::Smin,
        SynthOp::Smax,
        SynthOp::Umin,
        SynthOp::Umax,
    ];
    for &bop in &comm_ops {
        for i in 0..ni {
            for j in i..ni {
                if rem == 0 {
                    return None;
                }
                rem -= 1;
                let iu = u8::try_from(i).unwrap_or(0);
                let ju = u8::try_from(j).unwrap_or(0);
                let inst = SynthInst { op: bop, a: iu, b: ju, imm: 0 };
                let sig = eval_sig_1op(inst, &vecs, bits);
                if sig == target_sig {
                    let cand = make_prog_1(inst, model.simple_alu);
                    if verify_candidate_exact(dag, &cand, &vecs, &targets) {
                        return Some(cand);
                    }
                }
                push_mitm(&mut mitm, &mut mitm_len, sig, inst, model.simple_alu);
            }
        }
    }

    // 1c. Non-commutative 1-op binary ops (including fused AArch64 `bic`/`orn`/`eon` = 100!).
    let noncomm_ops: [SynthOp; 4] = [SynthOp::Sub, SynthOp::Bic, SynthOp::Orn, SynthOp::Eon];
    for &bop in &noncomm_ops {
        for i in 0..ni {
            for j in 0..ni {
                if i == j {
                    continue;
                }
                if rem == 0 {
                    return None;
                }
                rem -= 1;
                let iu = u8::try_from(i).unwrap_or(0);
                let ju = u8::try_from(j).unwrap_or(0);
                let inst = SynthInst { op: bop, a: iu, b: ju, imm: 0 };
                let sig = eval_sig_1op(inst, &vecs, bits);
                if sig == target_sig {
                    let cand = make_prog_1(inst, model.fused_alu);
                    if verify_candidate_exact(dag, &cand, &vecs, &targets) {
                        return Some(cand);
                    }
                }
                push_mitm(&mut mitm, &mut mitm_len, sig, inst, model.fused_alu);
            }
        }
    }

    // ---- Tier 2: 2-op Meet-in-the-Middle synthesis (cost = 200). ----
    let two_op_cost = model.simple_alu.saturating_add(model.simple_alu);
    if two_op_cost >= orig_cost {
        return None;
    }

    // 2a. Direct MITM inversion for reversible outer ops (`Xor`, `Add`, `Sub`) against `S_1`:
    //     Given target `T` and input `v_j`, compute exact required `S_1` signature in O(1)!
    for j in 0..ni {
        let ju = u8::try_from(j).unwrap_or(0);
        let mut vj_sig = [0u64; 4];
        for k in 0..4 {
            let v = vecs.get(k).and_then(|r| r.get(j).copied()).unwrap_or(0);
            set_at(&mut vj_sig, k, v);
        }
        // Check `s1 ^ vj == T` => `s1 == T ^ vj`,
        //       `s1 + vj == T` => `s1 == T - vj`,
        //       `s1 - vj == T` => `s1 == T + vj`,
        //       `vj - s1 == T` => `s1 == vj - T`.
        let mut need_xor = [0u64; 4];
        let mut need_add = [0u64; 4];
        let mut need_sub_l = [0u64; 4];
        let mut need_sub_r = [0u64; 4];
        for k in 0..4 {
            let t = at(&target_sig, k);
            let v = at(&vj_sig, k);
            set_at(&mut need_xor, k, (t ^ v) & mask);
            set_at(&mut need_add, k, t.wrapping_sub(v) & mask);
            set_at(&mut need_sub_l, k, t.wrapping_add(v) & mask);
            set_at(&mut need_sub_r, k, v.wrapping_sub(t) & mask);
        }

        let probes: [(FastSig, SynthOp, bool); 4] = [
            (need_xor, SynthOp::Xor, true),
            (need_add, SynthOp::Add, true),
            (need_sub_l, SynthOp::Sub, true),
            (need_sub_r, SynthOp::Sub, false),
        ];
        for &(need, outer_op, s1_is_lhs) in &probes {
            for m_idx in 0..mitm_len {
                if rem == 0 {
                    return None;
                }
                let Some(entry) = mitm.get(m_idx) else {
                    continue;
                };
                if entry.sig == need {
                    rem -= 1;
                    let (a, b) = if s1_is_lhs { (ni_u8, ju) } else { (ju, ni_u8) };
                    let i1 = SynthInst { op: outer_op, a, b, imm: 0 };
                    let cand = make_prog_2(entry.inst, i1, two_op_cost);
                    if verify_candidate_exact(dag, &cand, &vecs, &targets) {
                        return Some(cand);
                    }
                }
            }
        }
    }

    // 2b. Unary on `S_1` (`Not(s1)`, `Neg(s1)`) and constant-shift of `S_1` (`s1 << k`, `s1 >> k`).
    for m_idx in 0..mitm_len {
        let Some(entry) = mitm.get(m_idx) else {
            continue;
        };
        for &uop in &[SynthOp::Not, SynthOp::Neg] {
            if rem == 0 {
                return None;
            }
            rem -= 1;
            let mut sig = [0u64; 4];
            for k in 0..4 {
                let s = at(&entry.sig, k);
                let r = match uop {
                    SynthOp::Not => (!s) & mask,
                    _ => 0u64.wrapping_sub(s) & mask,
                };
                set_at(&mut sig, k, r);
            }
            if sig == target_sig {
                let i1 = SynthInst { op: uop, a: ni_u8, b: 0, imm: 0 };
                let cand = make_prog_2(entry.inst, i1, two_op_cost);
                if verify_candidate_exact(dag, &cand, &vecs, &targets) {
                    return Some(cand);
                }
            }
        }
    }

    // 2c. Non-reversible 2-op combinations `(s1 op v_j)` for `And`/`Or`/`Bic`/`Orn`/`Eon`/`Smin`/`Smax`/`Umin`/`Umax`.
    let nonrev_ops: [SynthOp; 9] = [
        SynthOp::And,
        SynthOp::Or,
        SynthOp::Bic,
        SynthOp::Orn,
        SynthOp::Eon,
        SynthOp::Smin,
        SynthOp::Smax,
        SynthOp::Umin,
        SynthOp::Umax,
    ];
    for &outer_op in &nonrev_ops {
        for m_idx in 0..mitm_len {
            let Some(entry) = mitm.get(m_idx) else {
                continue;
            };
            for j in 0..ni {
                if rem == 0 {
                    return None;
                }
                rem -= 1;
                let ju = u8::try_from(j).unwrap_or(0);
                let mut sig = [0u64; 4];
                let i1 = SynthInst { op: outer_op, a: ni_u8, b: ju, imm: 0 };
                for k in 0..4 {
                    let va = at(&entry.sig, k);
                    let vb = vecs.get(k).and_then(|r| r.get(j).copied()).unwrap_or(0);
                    set_at(&mut sig, k, eval_synth_inst(i1, va, vb, bits, bits));
                }
                if sig == target_sig {
                    let cand = make_prog_2(entry.inst, i1, two_op_cost);
                    if verify_candidate_exact(dag, &cand, &vecs, &targets) {
                        return Some(cand);
                    }
                }
            }
        }
    }

    // 2d. Shift by small constant (`1` or hint from DAG) on `S_1`: e.g. `(x & y) << 1`.
    let mut shifts = [1u64, 2, 3, 0];
    let mut n_shifts = 1usize;
    let nh = usize::from(dag.num_hints).min(4);
    for h_i in 0..nh {
        let hv = at(&dag.const_hints, h_i);
        if hv > 0 && hv < u64::from(bits) && n_shifts < 4 && !shifts.get(0..n_shifts).is_some_and(|s| s.contains(&hv)) {
            set_at(&mut shifts, n_shifts, hv);
            n_shifts += 1;
        }
    }
    if two_op_cost < orig_cost && MAX_SYNTH_LEN >= 3 {
        for s_i in 0..n_shifts {
            let sh = at(&shifts, s_i);
            let sh_u32 = trunc_u32(sh);
            for &sop in &[SynthOp::Shl, SynthOp::Lshr, SynthOp::Rotr] {
                for m_idx in 0..mitm_len {
                    if rem == 0 {
                        return None;
                    }
                    rem -= 1;
                    let Some(entry) = mitm.get(m_idx) else {
                        continue;
                    };
                    let mut sig = [0u64; 4];
                    for k in 0..4 {
                        let s = at(&entry.sig, k);
                        let r = match sop {
                            SynthOp::Shl => s.wrapping_shl(sh_u32) & mask,
                            SynthOp::Lshr => s.wrapping_shr(sh_u32) & mask,
                            _ => {
                                let lsh = bits.saturating_sub(sh_u32);
                                ((s >> sh_u32) | s.wrapping_shl(lsh)) & mask
                            }
                        };
                        set_at(&mut sig, k, r);
                    }
                    if sig == target_sig {
                        // Emit `[s1, Const(sh), sop(s1, c)]` (immediate shift folds const -> 200 cost!).
                        let c_inst = SynthInst { op: SynthOp::Const, a: 0, b: 0, imm: sh };
                        let s_inst = SynthInst {
                            op: sop,
                            a: ni_u8,
                            b: ni_u8.saturating_add(1),
                            imm: 0,
                        };
                        let cand = SynthProg {
                            num_inputs: ni_u8,
                            len: 3,
                            bits: bits_u8,
                            cost: two_op_cost,
                            insts: [entry.inst, c_inst, s_inst],
                        };
                        if verify_candidate_exact(dag, &cand, &vecs, &targets) {
                            return Some(cand);
                        }
                    }
                }
            }
        }
    }

    None
}

/// Injects a verified [`SynthProg`] into `eg`, binding `v0..` to `leaf_eids`, and returns the root
/// `EId` of the synthesized expression.
fn inject_synth_prog<A: Allocator + Copy>(
    eg: &mut MachEGraph<A>,
    prog: &SynthProg,
    leaf_eids: &[EId; MAX_DAG_INPUTS],
    ty: Ty,
    bits: u32,
) -> Option<EId> {
    let ni = usize::from(prog.num_inputs).min(MAX_DAG_INPUTS);
    let n = usize::from(prog.len).min(MAX_SYNTH_LEN);
    let mut vals = [NONE; MAX_DAG_INPUTS + MAX_SYNTH_LEN];
    for i in 0..ni {
        let leaf = leaf_eids.get(i).copied()?;
        if leaf == NONE {
            return None;
        }
        set_at(&mut vals, i, eg.uf.find(leaf));
    }

    for i in 0..n {
        let inst = prog.insts.get(i).copied()?;
        let get_val = |idx: u8, vals: &[EId]| -> Option<EId> {
            let v = vals.get(usize::from(idx)).copied()?;
            if v == NONE { None } else { Some(v) }
        };
        let eid = match inst.op {
            SynthOp::Copy => get_val(inst.a, &vals)?,
            SynthOp::Const => eg.add(MachENode::Const(inst.imm & mask_of(bits)), ty, bits)?,
            SynthOp::Neg => {
                let a = get_val(inst.a, &vals)?;
                eg.add(MachENode::Unary(Op::Neg, a), ty, bits)?
            }
            SynthOp::Not => {
                let a = get_val(inst.a, &vals)?;
                eg.add(MachENode::Unary(Op::Not, a), ty, bits)?
            }
            SynthOp::Bic => {
                let a = get_val(inst.a, &vals)?;
                let b = get_val(inst.b, &vals)?;
                let nb = eg.add(MachENode::Unary(Op::Not, b), ty, bits)?;
                eg.add(MachENode::Binary(Op::And, a, nb), ty, bits)?
            }
            SynthOp::Orn => {
                let a = get_val(inst.a, &vals)?;
                let b = get_val(inst.b, &vals)?;
                let nb = eg.add(MachENode::Unary(Op::Not, b), ty, bits)?;
                eg.add(MachENode::Binary(Op::Or, a, nb), ty, bits)?
            }
            SynthOp::Eon => {
                let a = get_val(inst.a, &vals)?;
                let b = get_val(inst.b, &vals)?;
                let nb = eg.add(MachENode::Unary(Op::Not, b), ty, bits)?;
                eg.add(MachENode::Binary(Op::Xor, a, nb), ty, bits)?
            }
            other => {
                let a = get_val(inst.a, &vals)?;
                let b = get_val(inst.b, &vals)?;
                let lir_op = match other {
                    SynthOp::Add => Op::Add,
                    SynthOp::Sub => Op::Sub,
                    SynthOp::Mul => Op::Mul,
                    SynthOp::And => Op::And,
                    SynthOp::Or => Op::Or,
                    SynthOp::Xor => Op::Xor,
                    SynthOp::Shl => Op::Shl,
                    SynthOp::Lshr => Op::Lshr,
                    SynthOp::Ashr => Op::Ashr,
                    SynthOp::Rotr => Op::Rotr,
                    SynthOp::Smin => Op::Smin,
                    SynthOp::Smax => Op::Smax,
                    SynthOp::Umin => Op::Umin,
                    SynthOp::Umax => Op::Umax,
                    _ => return None,
                };
                eg.add(MachENode::Binary(lir_op, a, b), ty, bits)?
            }
        };
        set_at(&mut vals, ni + i, eid);
    }

    if n == 0 {
        None
    } else {
        let out = vals.get(ni + n - 1).copied()?;
        if out == NONE { None } else { Some(out) }
    }
}

// ============================================================================
// LIR Classification, Copy & Iterative Materialization
// ============================================================================

#[inline]
fn is_egraph_scalar_ty(ty: Ty, ptr_bits: u32) -> Option<u32> {
    if !(ty.is_scalar() && ty.is_int()) {
        return None;
    }
    let bits = ty.bits(ptr_bits);
    if bits == 1 || bits == 32 || bits == 64 {
        Some(bits)
    } else {
        None
    }
}

#[inline]
fn is_egraph_op(op: Op, ty: Ty, flags: Flags, ptr_bits: u32) -> Option<u32> {
    if op.info().flags.meanings_only(flags).0 != 0 {
        return None;
    }
    let bits = is_egraph_scalar_ty(ty, ptr_bits)?;
    match op {
        Op::Iconst | Op::IconstWide => Some(bits),
        Op::Icmp if bits == 1 => Some(1),
        Op::Select if bits == 32 || bits == 64 => Some(bits),
        Op::Zext if bits == 32 || bits == 64 => Some(bits),
        Op::Neg
        | Op::Not
        | Op::Add
        | Op::Sub
        | Op::Mul
        | Op::And
        | Op::Or
        | Op::Xor
        | Op::Shl
        | Op::Lshr
        | Op::Ashr
        | Op::Rotl
        | Op::Rotr
        | Op::Smin
        | Op::Smax
        | Op::Umin
        | Op::Umax
        | Op::Umulhi
        | Op::Smulhi
        | Op::Udiv
        | Op::Urem
        | Op::Sdiv
        | Op::Srem
            if bits == 32 || bits == 64 =>
        {
            Some(bits)
        }
        Op::Not | Op::And | Op::Or | Op::Xor if bits == 1 => Some(1),
        _ => None,
    }
}

fn push_const<A: Allocator + Copy>(
    func: &mut Func<A>,
    ty: Ty,
    ptr_bits: u32,
    raw_value: u64,
    loc: u32,
) -> u32 {
    let bits = ty.bits(ptr_bits);
    let value = raw_value & mask_of(bits);
    let idx = to_u32(func.ops.len());
    if bits <= 32 {
        func.ops.push(op_byte(Op::Iconst));
        func.flags.push(0);
        func.tys.push(ty.0);
        func.data.push(trunc_u32(value));
        func.attrs.push(0);
        func.locs.push(loc);
        return idx;
    }
    match i32::try_from(sext(u128::from(value), bits)) {
        Ok(small) => {
            func.ops.push(op_byte(Op::Iconst));
            func.flags.push(0);
            func.tys.push(ty.0);
            func.data.push(small.cast_unsigned());
            func.attrs.push(0);
            func.locs.push(loc);
        }
        Err(_) => {
            let start = to_u32(func.extra.len());
            func.extra.push(trunc_u32(value));
            func.extra.push(trunc_u32(value >> 32));
            func.ops.push(op_byte(Op::IconstWide));
            func.flags.push(0);
            func.tys.push(ty.0);
            func.data.push(start);
            func.attrs.push(0);
            func.locs.push(loc);
        }
    }
    idx
}

fn copy_inst<A: Allocator + Copy>(
    view: &View<'_>,
    inst: usize,
    val_map: &[u32],
    func: &mut Func<A>,
) -> Result<u32, SimplifyError> {
    let op = view.op(inst);
    let flags = at(view.flags, inst);
    let ty = at(view.tys, inst);
    let attrs = at(view.attrs, inst);
    let loc = at(view.locs, inst);
    let map_val = |v: u32| -> Result<u32, SimplifyError> {
        let mapped = val_map.get(to_idx(v)).copied().unwrap_or(NONE);
        if mapped == NONE {
            Err(SimplifyError::Refused { inst, what: "unmapped operand in mach_egraph" })
        } else {
            Ok(mapped)
        }
    };
    let data = match op.info().layout {
        Layout::None => 0,
        Layout::Data(Field::Value) => map_val(at(view.data, inst))?,
        Layout::Data(_) => at(view.data, inst),
        Layout::Extra(fields) => {
            let start = to_u32(func.extra.len());
            let words = view.extra;
            let mut pos = to_idx(at(view.data, inst));
            let mut scrutinee = None;
            for &field in fields {
                match field {
                    Field::Value => {
                        let w = at(words, pos);
                        scrutinee = Some(w);
                        func.extra.push(map_val(w)?);
                        pos += 1;
                    }
                    Field::Block
                    | Field::Imm
                    | Field::Offset
                    | Field::IntCond
                    | Field::FloatCond
                    | Field::Rmw
                    | Field::Ref
                    | Field::Sig
                    | Field::Slot
                    | Field::Intrinsic
                    | Field::Asm => {
                        func.extra.push(at(words, pos));
                        pos += 1;
                    }
                    Field::Target => {
                        let block = at(words, pos);
                        let n_u32 = at(words, pos + 1);
                        let n = to_idx(n_u32);
                        func.extra.push(block);
                        func.extra.push(n_u32);
                        for k in 0..n {
                            func.extra.push(map_val(at(words, pos + 2 + k))?);
                        }
                        pos += 2 + n;
                    }
                    Field::Values => {
                        let n_u32 = at(words, pos);
                        let n = to_idx(n_u32);
                        func.extra.push(n_u32);
                        for k in 0..n {
                            func.extra.push(map_val(at(words, pos + 1 + k))?);
                        }
                        pos += 1 + n;
                    }
                    Field::Blocks | Field::Mask => {
                        let n = to_idx(at(words, pos));
                        func.extra.extend_from_slice(words.get(pos..pos + 1 + n).unwrap_or(&[]));
                        pos += 1 + n;
                    }
                    Field::Const => {
                        let n = to_idx(Ty(ty).const_words());
                        func.extra.extend_from_slice(words.get(pos..pos + n).unwrap_or(&[]));
                        pos += n;
                    }
                    Field::Cases => {
                        let width = to_idx(
                            scrutinee.map_or(0, |it: u32| Ty(at(view.tys, to_idx(it))).const_words()),
                        );
                        let n_u32 = at(words, pos);
                        let n = to_idx(n_u32);
                        func.extra.push(n_u32);
                        pos += 1;
                        for _ in 0..n {
                            func.extra.extend_from_slice(words.get(pos..pos + width).unwrap_or(&[]));
                            pos += width;
                            let block = at(words, pos);
                            let argc_u32 = at(words, pos + 1);
                            let argc = to_idx(argc_u32);
                            func.extra.push(block);
                            func.extra.push(argc_u32);
                            for k in 0..argc {
                                func.extra.push(map_val(at(words, pos + 2 + k))?);
                            }
                            pos += 2 + argc;
                        }
                    }
                }
            }
            start
        }
    };
    let out_idx = to_u32(func.ops.len());
    func.ops.push(at(view.ops, inst));
    func.flags.push(flags);
    func.tys.push(ty);
    func.data.push(data);
    func.attrs.push(attrs);
    func.locs.push(loc);
    Ok(out_idx)
}

enum MatFrame {
    EnterClass(EId),
    EnterInline { node: MachENode, ty: Ty },
    FinishUnary { memo: Option<EId>, op: Op, ty: Ty },
    FinishZext { memo: Option<EId>, ty: Ty },
    FinishBinary { memo: Option<EId>, op: Op, ty: Ty },
    FinishIcmp { memo: Option<EId>, cond: IntCond },
    FinishSelect { memo: Option<EId>, ty: Ty },
}

/// Iteratively materializes the optimal extracted [`BestChoice`] (including fused inline children)
/// for e-class `cid` into `func`, memoizing non-inline e-classes in `class_emitted`.
fn materialize_class<A: Allocator + Copy>(
    cid: EId,
    eg: &mut MachEGraph<A>,
    best: &[Option<BestChoice>],
    class_emitted: &mut [u32],
    visiting: &mut [bool],
    val_map: &[u32],
    func: &mut Func<A>,
    ptr_bits: u32,
    loc: u32,
    alloc: A,
) -> Option<u32> {
    let mut frames: Vec<MatFrame, A> = Vec::with_capacity_in(32, alloc);
    let mut results: Vec<u32, A> = Vec::with_capacity_in(32, alloc);
    let mut active: Vec<EId, A> = Vec::with_capacity_in(32, alloc);

    let cleanup = |visiting: &mut [bool], active: &[EId]| {
        for &v in active {
            set_at(visiting, to_idx(v), false);
        }
    };

    frames.push(MatFrame::EnterClass(cid));

    while let Some(frame) = frames.pop() {
        match frame {
            MatFrame::EnterClass(cur) => {
                let root = eg.uf.find(cur);
                let ri = to_idx(root);
                let existing = class_emitted.get(ri).copied().unwrap_or(NONE);
                if existing != NONE {
                    results.push(existing);
                    continue;
                }
                if at(visiting, ri) {
                    cleanup(visiting, active.as_slice());
                    return None;
                }
                let Some(choice) = best.get(ri).copied().flatten() else {
                    cleanup(visiting, active.as_slice());
                    return None;
                };
                let Some(ty) = eg.classes.get(ri).map(|c| c.ty) else {
                    cleanup(visiting, active.as_slice());
                    return None;
                };
                let push_child = |slot: u8,
                                  child_id: EId,
                                  fused: FusedPattern,
                                  eg: &MachEGraph<A>,
                                  frames: &mut Vec<MatFrame, A>| {
                    match fused {
                        FusedPattern::One { slot: fs, node: fnode } if fs == slot => {
                            let cty = eg.class_ty_bits(child_id).map_or(ty, |(t, _)| t);
                            frames.push(MatFrame::EnterInline { node: fnode, ty: cty });
                        }
                        FusedPattern::Two { left, right } if slot == 0 || slot == 1 => {
                            let cty = eg.class_ty_bits(child_id).map_or(ty, |(t, _)| t);
                            let fnode = if slot == 0 { left } else { right };
                            frames.push(MatFrame::EnterInline { node: fnode, ty: cty });
                        }
                        _ => frames.push(MatFrame::EnterClass(child_id)),
                    }
                };
                match choice.node {
                    MachENode::Var(orig_v) => {
                        let mapped = val_map.get(to_idx(orig_v)).copied().unwrap_or(NONE);
                        if mapped == NONE {
                            cleanup(visiting, active.as_slice());
                            return None;
                        }
                        set_at(class_emitted, ri, mapped);
                        results.push(mapped);
                    }
                    MachENode::Const(c) => {
                        let id = push_const(func, ty, ptr_bits, c, loc);
                        set_at(class_emitted, ri, id);
                        results.push(id);
                    }
                    MachENode::Unary(op, a) => {
                        set_at(visiting, ri, true);
                        active.push(root);
                        frames.push(MatFrame::FinishUnary { memo: Some(root), op, ty });
                        push_child(0, a, choice.fused, eg, &mut frames);
                    }
                    MachENode::Zext(a) => {
                        set_at(visiting, ri, true);
                        active.push(root);
                        frames.push(MatFrame::FinishZext { memo: Some(root), ty });
                        push_child(0, a, choice.fused, eg, &mut frames);
                    }
                    MachENode::Binary(op, a, b) => {
                        set_at(visiting, ri, true);
                        active.push(root);
                        frames.push(MatFrame::FinishBinary { memo: Some(root), op, ty });
                        push_child(1, b, choice.fused, eg, &mut frames);
                        push_child(0, a, choice.fused, eg, &mut frames);
                    }
                    MachENode::Icmp(cond, a, b) => {
                        set_at(visiting, ri, true);
                        active.push(root);
                        frames.push(MatFrame::FinishIcmp { memo: Some(root), cond });
                        push_child(1, b, choice.fused, eg, &mut frames);
                        push_child(0, a, choice.fused, eg, &mut frames);
                    }
                    MachENode::Select(c_id, t_id, f_id) => {
                        set_at(visiting, ri, true);
                        active.push(root);
                        frames.push(MatFrame::FinishSelect { memo: Some(root), ty });
                        push_child(2, f_id, choice.fused, eg, &mut frames);
                        push_child(1, t_id, choice.fused, eg, &mut frames);
                        push_child(0, c_id, choice.fused, eg, &mut frames);
                    }
                }
            }
            MatFrame::EnterInline { node, ty } => match node {
                MachENode::Var(orig_v) => {
                    let mapped = val_map.get(to_idx(orig_v)).copied().unwrap_or(NONE);
                    if mapped == NONE {
                        cleanup(visiting, active.as_slice());
                        return None;
                    }
                    results.push(mapped);
                }
                MachENode::Const(c) => {
                    let id = push_const(func, ty, ptr_bits, c, loc);
                    results.push(id);
                }
                MachENode::Unary(op, a) => {
                    frames.push(MatFrame::FinishUnary { memo: None, op, ty });
                    frames.push(MatFrame::EnterClass(a));
                }
                MachENode::Zext(a) => {
                    frames.push(MatFrame::FinishZext { memo: None, ty });
                    frames.push(MatFrame::EnterClass(a));
                }
                MachENode::Binary(op, a, b) => {
                    frames.push(MatFrame::FinishBinary { memo: None, op, ty });
                    frames.push(MatFrame::EnterClass(b));
                    frames.push(MatFrame::EnterClass(a));
                }
                MachENode::Icmp(cond, a, b) => {
                    frames.push(MatFrame::FinishIcmp { memo: None, cond });
                    frames.push(MatFrame::EnterClass(b));
                    frames.push(MatFrame::EnterClass(a));
                }
                MachENode::Select(c_id, t_id, f_id) => {
                    frames.push(MatFrame::FinishSelect { memo: None, ty });
                    frames.push(MatFrame::EnterClass(f_id));
                    frames.push(MatFrame::EnterClass(t_id));
                    frames.push(MatFrame::EnterClass(c_id));
                }
            },
            MatFrame::FinishUnary { memo, op, ty } => {
                let Some(ma) = results.pop() else {
                    cleanup(visiting, active.as_slice());
                    return None;
                };
                let idx = to_u32(func.ops.len());
                func.ops.push(op_byte(op));
                func.flags.push(0);
                func.tys.push(ty.0);
                func.data.push(ma);
                func.attrs.push(0);
                func.locs.push(loc);
                if let Some(root) = memo {
                    let ri = to_idx(root);
                    set_at(visiting, ri, false);
                    let _ = active.pop();
                    set_at(class_emitted, ri, idx);
                }
                results.push(idx);
            }
            MatFrame::FinishZext { memo, ty } => {
                let Some(ma) = results.pop() else {
                    cleanup(visiting, active.as_slice());
                    return None;
                };
                let idx = to_u32(func.ops.len());
                func.ops.push(op_byte(Op::Zext));
                func.flags.push(0);
                func.tys.push(ty.0);
                func.data.push(ma);
                func.attrs.push(0);
                func.locs.push(loc);
                if let Some(root) = memo {
                    let ri = to_idx(root);
                    set_at(visiting, ri, false);
                    let _ = active.pop();
                    set_at(class_emitted, ri, idx);
                }
                results.push(idx);
            }
            MatFrame::FinishBinary { memo, op, ty } => {
                let (Some(mb), Some(ma)) = (results.pop(), results.pop()) else {
                    cleanup(visiting, active.as_slice());
                    return None;
                };
                let start = to_u32(func.extra.len());
                func.extra.push(ma);
                func.extra.push(mb);
                let idx = to_u32(func.ops.len());
                func.ops.push(op_byte(op));
                func.flags.push(0);
                func.tys.push(ty.0);
                func.data.push(start);
                func.attrs.push(0);
                func.locs.push(loc);
                if let Some(root) = memo {
                    let ri = to_idx(root);
                    set_at(visiting, ri, false);
                    let _ = active.pop();
                    set_at(class_emitted, ri, idx);
                }
                results.push(idx);
            }
            MatFrame::FinishIcmp { memo, cond } => {
                let (Some(mb), Some(ma)) = (results.pop(), results.pop()) else {
                    cleanup(visiting, active.as_slice());
                    return None;
                };
                let start = to_u32(func.extra.len());
                func.extra.push(int_cond_word(cond));
                func.extra.push(ma);
                func.extra.push(mb);
                let idx = to_u32(func.ops.len());
                func.ops.push(op_byte(Op::Icmp));
                func.flags.push(0);
                func.tys.push(Ty::I1.0);
                func.data.push(start);
                func.attrs.push(0);
                func.locs.push(loc);
                if let Some(root) = memo {
                    let ri = to_idx(root);
                    set_at(visiting, ri, false);
                    let _ = active.pop();
                    set_at(class_emitted, ri, idx);
                }
                results.push(idx);
            }
            MatFrame::FinishSelect { memo, ty } => {
                let (Some(mf), Some(mt), Some(mc)) = (results.pop(), results.pop(), results.pop())
                else {
                    cleanup(visiting, active.as_slice());
                    return None;
                };
                let start = to_u32(func.extra.len());
                func.extra.push(mc);
                func.extra.push(mt);
                func.extra.push(mf);
                let idx = to_u32(func.ops.len());
                func.ops.push(op_byte(Op::Select));
                func.flags.push(0);
                func.tys.push(ty.0);
                func.data.push(start);
                func.attrs.push(0);
                func.locs.push(loc);
                if let Some(root) = memo {
                    let ri = to_idx(root);
                    set_at(visiting, ri, false);
                    let _ = active.pop();
                    set_at(class_emitted, ri, idx);
                }
                results.push(idx);
            }
        }
    }

    results.pop()
}

// ============================================================================
// Public Entry Point (`optimize_mach_egraph`)
// ============================================================================

/// Runs joint AArch64 machine-op equality saturation + bounded enumerative bitvector
/// superoptimization (with optional persistent BLAKE3 [`SuperoptCache`]) over `view`.
///
/// Rewrites are only emitted when they strictly lower execution-port cost under
/// [`AppleM5PortModel::apple_m5`], and the output is verified with [`verify`] and cleaned with
/// dead-code elimination before returning.
pub fn optimize_mach_egraph<A: Allocator + Copy>(
    view: View<'_>,
    opts: MachEGraphOpts,
    mut cache: Option<&mut SuperoptCache<A>>,
    alloc: A,
) -> Result<MachEGraphOutput<A>, SimplifyError> {
    let len = view.len();
    let ptr_bits = view.head.ptr_bits;
    if view.blocks() == 0 {
        return Ok(MachEGraphOutput {
            func: view.to_func_in(alloc),
            stats: MachEGraphStats::default(),
        });
    }

    let model = AppleM5PortModel::apple_m5();
    let mut func = view.to_func_in(alloc);
    func.ops.clear();
    func.flags.clear();
    func.tys.clear();
    func.data.clear();
    func.extra.clear();
    func.attrs.clear();
    func.locs.clear();
    func.block_starts.clear();
    func.block_kinds.clear();

    let mut val_map = filled(len, NONE, &alloc);
    let mut stats = MachEGraphStats::default();
    let mut any_rewritten = false;

    for b in 0..view.blocks() {
        let b_u32 = to_u32(b);
        func.block_starts.push(to_u32(func.ops.len()));
        func.block_kinds.push(at(view.block_kinds, b));
        let range = view.block(Block(b_u32));

        // Count pure non-constant integer/cmp/select ops in this block.
        let mut pure_ops = 0u32;
        for inst in range.start..range.end {
            let op = view.op(inst);
            let ty = Ty(at(view.tys, inst));
            if is_egraph_op(op, ty, Flags(at(view.flags, inst)), ptr_bits).is_some()
                && !matches!(op, Op::Iconst | Op::IconstWide)
            {
                pure_ops = pure_ops.saturating_add(1);
            }
        }
        if pure_ops == 0 {
            for inst in range.start..range.end {
                let copied = copy_inst(&view, inst, val_map.as_slice(), &mut func)?;
                set_at(&mut val_map, inst, copied);
            }
            continue;
        }

        stats.blocks_optimized = stats.blocks_optimized.saturating_add(1);

        // 1. Build bounded machine-aware E-Graph for all pure scalar integer/cmp/select ops.
        let mut eg = MachEGraph::new(opts.max_enodes.max(64), alloc);
        let mut inst_eid: Vec<u32, A> = filled(range.len(), NONE, &alloc);

        let resolve_operand =
            |eg: &mut MachEGraph<A>, inst_eid: &[u32], v: u32, expected_ty: Ty, expected_bits: u32| -> Option<EId> {
                let vi = to_idx(v);
                if vi >= range.start && vi < range.end {
                    let eid = inst_eid.get(vi - range.start).copied().unwrap_or(NONE);
                    if eid != NONE {
                        let root = eg.uf.find(eid);
                        if eg.class_ty_bits(root) == Some((expected_ty, expected_bits)) {
                            return Some(root);
                        }
                    }
                }
                eg.add(MachENode::Var(v), expected_ty, expected_bits)
            };

        for inst in range.start..range.end {
            let rel = inst - range.start;
            let op = view.op(inst);
            let ty = Ty(at(view.tys, inst));
            let Some(bits) = is_egraph_op(op, ty, Flags(at(view.flags, inst)), ptr_bits) else {
                continue;
            };
            let mask = mask_of(bits);

            let node_opt = match op {
                Op::Iconst => {
                    let v = trunc_u64(iconst_value(at(view.data, inst), bits)) & mask;
                    Some(MachENode::Const(v))
                }
                Op::IconstWide => {
                    let idx = to_idx(at(view.data, inst));
                    let w = to_idx(ty.const_words());
                    let v = trunc_u64(wide_value(view.extra.get(idx..idx + w).unwrap_or(&[]), bits))
                        & mask;
                    Some(MachENode::Const(v))
                }
                Op::Neg | Op::Not => {
                    let src = at(view.data, inst);
                    resolve_operand(&mut eg, inst_eid.as_slice(), src, ty, bits)
                        .map(|a| MachENode::Unary(op, a))
                }
                Op::Zext => {
                    let src = at(view.data, inst);
                    let src_ty = Ty(at(view.tys, to_idx(src)));
                    if src_ty == Ty::I1 {
                        resolve_operand(&mut eg, inst_eid.as_slice(), src, Ty::I1, 1)
                            .map(MachENode::Zext)
                    } else {
                        None
                    }
                }
                Op::Icmp => {
                    let idx = to_idx(at(view.data, inst));
                    let cond_w = at(view.extra, idx);
                    let lhs = at(view.extra, idx + 1);
                    let rhs = at(view.extra, idx + 2);
                    let op_ty = Ty(at(view.tys, to_idx(lhs)));
                    let op_bits = is_egraph_scalar_ty(op_ty, ptr_bits);
                    if let (Some(cond), Some(ob)) = (IntCond::from_word(cond_w), op_bits)
                        && (ob == 32 || ob == 64)
                        && Ty(at(view.tys, to_idx(rhs))) == op_ty
                    {
                        match (
                            resolve_operand(&mut eg, inst_eid.as_slice(), lhs, op_ty, ob),
                            resolve_operand(&mut eg, inst_eid.as_slice(), rhs, op_ty, ob),
                        ) {
                            (Some(a), Some(bv)) => Some(MachENode::Icmp(cond, a, bv)),
                            _ => None,
                        }
                    } else {
                        None
                    }
                }
                Op::Select => {
                    let idx = to_idx(at(view.data, inst));
                    let c_v = at(view.extra, idx);
                    let t_v = at(view.extra, idx + 1);
                    let f_v = at(view.extra, idx + 2);
                    if Ty(at(view.tys, to_idx(c_v))) == Ty::I1 {
                        match (
                            resolve_operand(&mut eg, inst_eid.as_slice(), c_v, Ty::I1, 1),
                            resolve_operand(&mut eg, inst_eid.as_slice(), t_v, ty, bits),
                            resolve_operand(&mut eg, inst_eid.as_slice(), f_v, ty, bits),
                        ) {
                            (Some(ce), Some(te), Some(fe)) => Some(MachENode::Select(ce, te, fe)),
                            _ => None,
                        }
                    } else {
                        None
                    }
                }
                Op::Udiv | Op::Urem | Op::Sdiv | Op::Srem => {
                    // Only enter div/rem into the e-graph when the divisor is a proven non-trapping
                    // constant so we NEVER eliminate a potentially trapping division!
                    let idx = to_idx(at(view.data, inst));
                    let lhs = at(view.extra, idx);
                    let rhs = at(view.extra, idx + 1);
                    match (
                        resolve_operand(&mut eg, inst_eid.as_slice(), lhs, ty, bits),
                        resolve_operand(&mut eg, inst_eid.as_slice(), rhs, ty, bits),
                    ) {
                        (Some(a), Some(bv)) => {
                            let safe_const = eg.class_const(bv).is_some_and(|c| {
                                c != 0 && (!matches!(op, Op::Sdiv | Op::Srem) || c != mask)
                            });
                            if safe_const {
                                Some(MachENode::Binary(op, a, bv))
                            } else {
                                None
                            }
                        }
                        _ => None,
                    }
                }
                Op::Add
                | Op::Sub
                | Op::Mul
                | Op::And
                | Op::Or
                | Op::Xor
                | Op::Shl
                | Op::Lshr
                | Op::Ashr
                | Op::Rotl
                | Op::Rotr
                | Op::Smin
                | Op::Smax
                | Op::Umin
                | Op::Umax
                | Op::Umulhi
                | Op::Smulhi => {
                    let idx = to_idx(at(view.data, inst));
                    let lhs = at(view.extra, idx);
                    let rhs = at(view.extra, idx + 1);
                    match (
                        resolve_operand(&mut eg, inst_eid.as_slice(), lhs, ty, bits),
                        resolve_operand(&mut eg, inst_eid.as_slice(), rhs, ty, bits),
                    ) {
                        (Some(a), Some(bv)) => Some(MachENode::Binary(op, a, bv)),
                        _ => None,
                    }
                }
                _ => None,
            };

            if let Some(n) = node_opt
                && let Some(eid) = eg.add(n, ty, bits)
            {
                set_at(&mut inst_eid, rel, eid);
            }
        }

        // Compute pre-saturation baseline M5 port cost for every initial e-class.
        let pre_classes = eg.classes.len();
        let orig_best = extract_best(&eg, pre_classes, &model, alloc);

        // 2. Bounded multi-round AArch64-aware equality saturation.
        for _ in 0..opts.max_iters {
            let applied = saturate_round(&mut eg);
            stats.rewrites_applied = stats.rewrites_applied.saturating_add(applied);
            if applied == 0 || eg.at_budget() {
                break;
            }
        }

        // 3. Initial M5-port-cost extraction.
        let mut best = extract_best(&eg, eg.classes.len(), &model, alloc);

        // 4. Bounded enumerative / MITM bitvector superoptimizer + persistent BLAKE3 cache lookup
        //    on multi-op pure integer sub-DAGs (`<= 6` ops, `<= 3` inputs, `i32`/`i64`).
        let mut synth_unions: Vec<(EId, EId), A> = Vec::new_in(alloc);
        let check_classes = eg.classes.len();
        for cid in 0..check_classes {
            if eg.at_budget() {
                break;
            }
            let cid_e = to_u32(cid);
            if eg.uf.find_ro(cid_e) != cid_e {
                continue;
            }
            let Some(cur_choice) = best.get(cid).copied().flatten() else {
                continue;
            };
            if cur_choice.cost <= model.simple_alu {
                continue;
            }
            let Some((ty, bits)) = eg.class_ty_bits(cid_e) else {
                continue;
            };
            let Some(dag) = extract_canonical_dag(&eg, best.as_slice(), cid_e, alloc) else {
                continue;
            };
            let digest = dag.digest();

            let cached = match cache.as_deref() {
                Some(c) => c.get(&digest),
                None => None,
            };
            match cached {
                Some(CachedRewrite::NoImprovement) => {
                    stats.cache_hits = stats.cache_hits.saturating_add(1);
                }
                Some(CachedRewrite::Better(prog)) => {
                    stats.cache_hits = stats.cache_hits.saturating_add(1);
                    if prog.cost < cur_choice.cost
                        && let Some(new_eid) =
                            inject_synth_prog(&mut eg, &prog, &dag.leaf_eids, ty, bits)
                    {
                        synth_unions.push((cid_e, new_eid));
                    }
                }
                None if opts.enable_synth => {
                    if let Some(prog) =
                        synthesize_dag(&dag, cur_choice.cost, opts.synth_budget, &model)
                    {
                        stats.synth_wins = stats.synth_wins.saturating_add(1);
                        stats.rewrites_applied = stats.rewrites_applied.saturating_add(1);
                        if let Some(c) = cache.as_deref_mut() {
                            c.insert(digest, CachedRewrite::Better(prog));
                        }
                        if let Some(new_eid) =
                            inject_synth_prog(&mut eg, &prog, &dag.leaf_eids, ty, bits)
                        {
                            synth_unions.push((cid_e, new_eid));
                        }
                    } else if let Some(c) = cache.as_deref_mut() {
                        c.insert(digest, CachedRewrite::NoImprovement);
                    }
                }
                None => {}
            }
        }

        if !synth_unions.is_empty() {
            let mut merged = false;
            for (a, b) in synth_unions {
                if eg.union(a, b) {
                    merged = true;
                }
            }
            if merged {
                eg.rebuild();
                best = extract_best(&eg, eg.classes.len(), &model, alloc);
            }
        }

        // 5. Materialize strictly lower-M5-cost roots; memoize per e-class so shared DAG nodes emit once.
        let n_classes = eg.classes.len();
        let mut class_emitted: Vec<u32, A> = filled(n_classes, NONE, &alloc);
        let mut visiting: Vec<bool, A> = filled(n_classes, false, &alloc);

        for inst in range.start..range.end {
            let rel = inst - range.start;
            let eid = inst_eid.get(rel).copied().unwrap_or(NONE);
            if eid != NONE && !matches!(view.op(inst), Op::Iconst | Op::IconstWide) {
                let root = eg.uf.find(eid);
                let eid_idx = to_idx(eid);
                let before = if eid_idx < pre_classes {
                    class_cost(orig_best.as_slice(), eid)
                } else {
                    INF_COST
                };
                let after = class_cost(best.as_slice(), root);
                if after < before
                    && let Some(new_val) = materialize_class(
                        root,
                        &mut eg,
                        best.as_slice(),
                        class_emitted.as_mut_slice(),
                        visiting.as_mut_slice(),
                        val_map.as_slice(),
                        &mut func,
                        ptr_bits,
                        at(view.locs, inst),
                        alloc,
                    )
                {
                    set_at(&mut val_map, inst, new_val);
                    stats.cost_before = stats.cost_before.saturating_add(u64::from(before));
                    stats.cost_after = stats.cost_after.saturating_add(u64::from(after));
                    any_rewritten = true;
                    continue;
                }
            }
            let copied = copy_inst(&view, inst, val_map.as_slice(), &mut func)?;
            set_at(&mut val_map, inst, copied);
            if eid != NONE {
                let root = eg.uf.find(eid);
                let ri = to_idx(root);
                if class_emitted.get(ri).copied().unwrap_or(NONE) == NONE {
                    set_at(class_emitted.as_mut_slice(), ri, copied);
                }
            }
        }
    }

    if !any_rewritten {
        return Ok(MachEGraphOutput { func: view.to_func_in(alloc), stats });
    }
    crate::verify::verify_in(func.view(), func.allocator()).map_err(SimplifyError::Verify)?;
    let cleaned = dead(func.view(), alloc)?;
    Ok(MachEGraphOutput { func: cleaned, stats })
}

// ============================================================================
// Unit Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use blaze_own::testing::TestHeap;

    use super::*;
    use crate::{
        builder::{Builder, Full, Param, Variadic},
        eval::{self, Outcome, Val},
        func::{BlockKind, Conv},
    };

    fn collect_ops(view: View<'_>, alloc: TestHeap) -> Vec<Op, TestHeap> {
        let mut ops = Vec::with_capacity_in(view.len(), alloc);
        for i in 0..view.len() {
            ops.push(view.op(i));
        }
        ops
    }

    #[test]
    fn mach_egraph_discovers_cssc_smin_smax_umin_umax() {
        let alloc = TestHeap;
        let cases: [(IntCond, bool, Op); 4] = [
            (IntCond::Slt, true, Op::Smin),
            (IntCond::Sge, true, Op::Smax),
            (IntCond::Ult, true, Op::Umin),
            (IntCond::Ugt, true, Op::Umax),
        ];

        for &(cond, direct, expect_op) in &cases {
            let mut b = Builder::<TestHeap, Full>::new_in(64, alloc);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::I64), Param::of(Ty::I64)],
                &[Param::of(Ty::I64)],
            );
            b.set_signature(sig);
            let entry = b.create_block(BlockKind::Normal);
            b.begin(entry);
            let a = b.param(Ty::I64);
            let bv = b.param(Ty::I64);
            let cmp = b.icmp(cond, a, bv);
            let sel = if direct {
                b.select(Flags::NONE, cmp, a, bv)
            } else {
                b.select(Flags::NONE, cmp, bv, a)
            };
            b.ret(&[sel]);

            let func = b.finish_checked().unwrap();
            let out =
                optimize_mach_egraph(func.view(), MachEGraphOpts::apple_m5(), None, alloc).unwrap();

            assert!(
                out.stats.cost_after < out.stats.cost_before,
                "expected strict M5 cost drop for {cond:?}: {:?}",
                out.stats
            );
            let ops = collect_ops(out.func.view(), alloc);
            assert!(
                ops.as_slice().contains(&expect_op),
                "expected {expect_op:?} in {ops:?}"
            );
            assert!(
                !ops.as_slice().contains(&Op::Select),
                "expected Select eliminated into {expect_op:?}: {ops:?}"
            );

            for &(x, y) in &[
                (0u64, 0u64),
                (1, 2),
                (2, 1),
                (u64::MAX, 0),
                (0, u64::MAX),
                (i64::MIN as u64, i64::MAX as u64),
                (i64::MAX as u64, i64::MIN as u64),
                (42, 42),
            ] {
                let args = [Val::Int(x.into()), Val::Int(y.into())];
                let r_before = eval::run_in(func.view(), &args, 10_000, alloc).unwrap();
                let r_after = eval::run_in(out.func.view(), &args, 10_000, alloc).unwrap();
                assert_eq!(r_before, r_after, "min/max diverged on ({x:#x}, {y:#x}) for {cond:?}");
            }
        }
    }

    #[test]
    fn mach_egraph_discovers_rotr_bic_and_shift_add_mul9() {
        let alloc = TestHeap;

        // 1. `(x >> 13) | (x << 51)` -> `Rotr(x, 13)`
        {
            let mut b = Builder::<TestHeap, Full>::new_in(64, alloc);
            let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
            b.set_signature(sig);
            let entry = b.create_block(BlockKind::Normal);
            b.begin(entry);
            let x = b.param(Ty::I64);
            let c13 = b.iconst(Ty::I64, 13);
            let c51 = b.iconst(Ty::I64, 51);
            let hi = b.binary(Op::Lshr, Flags::NONE, x, c13);
            let lo = b.binary(Op::Shl, Flags::NONE, x, c51);
            let rot = b.binary(Op::Or, Flags::NONE, hi, lo);
            b.ret(&[rot]);

            let func = b.finish_checked().unwrap();
            let out =
                optimize_mach_egraph(func.view(), MachEGraphOpts::apple_m5(), None, alloc).unwrap();
            assert!(out.stats.cost_after < out.stats.cost_before, "{:?}", out.stats);
            let ops = collect_ops(out.func.view(), alloc);
            assert!(ops.as_slice().contains(&Op::Rotr), "expected Op::Rotr in {ops:?}");

            for &xv in &[0u64, 1, u64::MAX, 0x0123_4567_89AB_CDEF, 0x8000_0000_0000_0001] {
                let args = [Val::Int(xv.into())];
                let r0 = eval::run_in(func.view(), &args, 10_000, alloc).unwrap();
                let r1 = eval::run_in(out.func.view(), &args, 10_000, alloc).unwrap();
                assert_eq!(r0, r1, "rotr diverged on {xv:#x}");
                match r1 {
                    Outcome::Returned(vals) => {
                        assert_eq!(vals.as_slice(), &[Val::Int(xv.rotate_right(13).into())]);
                    }
                    other => panic!("unexpected {other:?}"),
                }
            }
        }

        // 2. `a & (b ^ -1)` -> `And(a, Not(b))` (fused AArch64 `bic`)
        {
            let mut b = Builder::<TestHeap, Full>::new_in(64, alloc);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::I64), Param::of(Ty::I64)],
                &[Param::of(Ty::I64)],
            );
            b.set_signature(sig);
            let entry = b.create_block(BlockKind::Normal);
            b.begin(entry);
            let a = b.param(Ty::I64);
            let bv = b.param(Ty::I64);
            let all_ones = b.iconst(Ty::I64, u128::from(u64::MAX));
            let inv = b.binary(Op::Xor, Flags::NONE, bv, all_ones);
            let bic = b.binary(Op::And, Flags::NONE, a, inv);
            b.ret(&[bic]);

            let func = b.finish_checked().unwrap();
            let out =
                optimize_mach_egraph(func.view(), MachEGraphOpts::apple_m5(), None, alloc).unwrap();
            assert!(out.stats.cost_after < out.stats.cost_before, "{:?}", out.stats);
            let ops = collect_ops(out.func.view(), alloc);
            assert!(ops.as_slice().contains(&Op::Not), "expected Op::Not (bic) in {ops:?}");
            assert!(ops.as_slice().contains(&Op::And), "expected Op::And (bic) in {ops:?}");
            assert!(!ops.as_slice().contains(&Op::Xor), "expected Xor(-1) eliminated: {ops:?}");

            for &(av, bval) in &[(0u64, 0u64), (u64::MAX, 0x0F0F), (0xCAFE_BABE, 0xDEAD_BEEF)] {
                let args = [Val::Int(av.into()), Val::Int(bval.into())];
                let r0 = eval::run_in(func.view(), &args, 10_000, alloc).unwrap();
                let r1 = eval::run_in(out.func.view(), &args, 10_000, alloc).unwrap();
                assert_eq!(r0, r1, "bic diverged on ({av:#x}, {bval:#x})");
            }
        }

        // 3. `x * 9` -> `Add(x, Shl(x, 3))` (fused AArch64 `add x, x, x, lsl #3` = 100 units!)
        {
            let mut b = Builder::<TestHeap, Full>::new_in(64, alloc);
            let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
            b.set_signature(sig);
            let entry = b.create_block(BlockKind::Normal);
            b.begin(entry);
            let x = b.param(Ty::I64);
            let nine = b.iconst(Ty::I64, 9);
            let prod = b.binary(Op::Mul, Flags::NONE, x, nine);
            b.ret(&[prod]);

            let func = b.finish_checked().unwrap();
            let out =
                optimize_mach_egraph(func.view(), MachEGraphOpts::apple_m5(), None, alloc).unwrap();
            assert!(out.stats.cost_after < out.stats.cost_before, "{:?}", out.stats);
            let ops = collect_ops(out.func.view(), alloc);
            assert!(!ops.as_slice().contains(&Op::Mul), "expected Mul eliminated: {ops:?}");
            assert!(ops.as_slice().contains(&Op::Shl), "expected Shl in {ops:?}");
            assert!(ops.as_slice().contains(&Op::Add), "expected Add in {ops:?}");

            for &xv in &[0u64, 1, 7, 123_456_789, u64::MAX] {
                let args = [Val::Int(xv.into())];
                let r0 = eval::run_in(func.view(), &args, 10_000, alloc).unwrap();
                let r1 = eval::run_in(out.func.view(), &args, 10_000, alloc).unwrap();
                assert_eq!(r0, r1, "mul9 diverged on {xv}");
            }
        }
    }

    #[test]
    fn enumerative_synthesizer_discovers_bitvector_identities_and_populates_cache() {
        let alloc = TestHeap;
        let mut cache = SuperoptCache::new_in(alloc);

        // 1. `(x | y) - (x & y)` -> synthesized to `Xor(x, y)` (300 -> 100)!
        let build_or_minus_and = || {
            let mut b = Builder::<TestHeap, Full>::new_in(64, alloc);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::I64), Param::of(Ty::I64)],
                &[Param::of(Ty::I64)],
            );
            b.set_signature(sig);
            let entry = b.create_block(BlockKind::Normal);
            b.begin(entry);
            let x = b.param(Ty::I64);
            let y = b.param(Ty::I64);
            let o = b.binary(Op::Or, Flags::NONE, x, y);
            let a = b.binary(Op::And, Flags::NONE, x, y);
            let d = b.binary(Op::Sub, Flags::NONE, o, a);
            b.ret(&[d]);
            b.finish_checked().unwrap()
        };

        let f1 = build_or_minus_and();
        let out1 =
            optimize_mach_egraph(f1.view(), MachEGraphOpts::apple_m5(), Some(&mut cache), alloc)
                .unwrap();
        assert!(out1.stats.synth_wins >= 1, "expected synthesizer win: {:?}", out1.stats);
        assert_eq!(out1.stats.cache_hits, 0);
        assert!(out1.stats.cost_after < out1.stats.cost_before, "{:?}", out1.stats);
        let ops1 = collect_ops(out1.func.view(), alloc);
        assert!(ops1.as_slice().contains(&Op::Xor), "expected synthesized Xor in {ops1:?}");
        assert!(!ops1.as_slice().contains(&Op::Sub), "expected Sub eliminated: {ops1:?}");

        // Second run with the same cache MUST hit O(1) cache without re-running synthesis!
        let f1_again = build_or_minus_and();
        let out2 = optimize_mach_egraph(
            f1_again.view(),
            MachEGraphOpts::apple_m5(),
            Some(&mut cache),
            alloc,
        )
        .unwrap();
        assert!(out2.stats.cache_hits >= 1, "expected O(1) cache hit: {:?}", out2.stats);
        assert_eq!(out2.stats.synth_wins, 0, "should not re-synthesize on cache hit");

        for &(xv, yv) in &[
            (0u64, 0u64),
            (1, 2),
            (0xFFFF_0000, 0x00FF_FF00),
            (u64::MAX, 0x5555_5555_5555_5555),
            (0xDEAD_BEEF_CAFE_BABE, 0x0123_4567_89AB_CDEF),
        ] {
            let args = [Val::Int(xv.into()), Val::Int(yv.into())];
            let r0 = eval::run_in(f1.view(), &args, 10_000, alloc).unwrap();
            let r1 = eval::run_in(out1.func.view(), &args, 10_000, alloc).unwrap();
            let r2 = eval::run_in(out2.func.view(), &args, 10_000, alloc).unwrap();
            assert_eq!(r0, r1);
            assert_eq!(r0, r2);
        }

        // 2. `(x & y) + (x | y)` -> synthesized to `Add(x, y)` (300 -> 100)!
        let mut b = Builder::<TestHeap, Full>::new_in(64, alloc);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I32), Param::of(Ty::I32)],
            &[Param::of(Ty::I32)],
        );
        b.set_signature(sig);
        let entry = b.create_block(BlockKind::Normal);
        b.begin(entry);
        let x = b.param(Ty::I32);
        let y = b.param(Ty::I32);
        let a = b.binary(Op::And, Flags::NONE, x, y);
        let o = b.binary(Op::Or, Flags::NONE, x, y);
        let s = b.binary(Op::Add, Flags::NONE, a, o);
        b.ret(&[s]);
        let f_add = b.finish_checked().unwrap();

        let out_add = optimize_mach_egraph(
            f_add.view(),
            MachEGraphOpts::apple_m5(),
            Some(&mut cache),
            alloc,
        )
        .unwrap();
        assert!(out_add.stats.synth_wins >= 1, "expected Add synthesis win: {:?}", out_add.stats);
        let ops_add = collect_ops(out_add.func.view(), alloc);
        assert!(ops_add.as_slice().contains(&Op::Add), "expected Add in {ops_add:?}");
        assert!(!ops_add.as_slice().contains(&Op::And), "expected And eliminated: {ops_add:?}");
        assert!(!ops_add.as_slice().contains(&Op::Or), "expected Or eliminated: {ops_add:?}");

        for &(xv, yv) in &[(0u32, 0u32), (1, u32::MAX), (0x1234_5678, 0x9ABC_DEF0), (u32::MAX, u32::MAX)] {
            let args = [Val::Int(u128::from(xv)), Val::Int(u128::from(yv))];
            let r0 = eval::run_in(f_add.view(), &args, 10_000, alloc).unwrap();
            let r1 = eval::run_in(out_add.func.view(), &args, 10_000, alloc).unwrap();
            assert_eq!(r0, r1);
        }
    }

    #[test]
    fn superopt_cache_round_trips_byte_for_byte_and_rejects_corruption() {
        let alloc = TestHeap;
        let mut cache = SuperoptCache::new_in(alloc);

        // Populate cache via real synthesis + negative cache entry.
        let mut b = Builder::<TestHeap, Full>::new_in(64, alloc);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let entry = b.create_block(BlockKind::Normal);
        b.begin(entry);
        let x = b.param(Ty::I64);
        let y = b.param(Ty::I64);
        let o = b.binary(Op::Or, Flags::NONE, x, y);
        let a = b.binary(Op::And, Flags::NONE, x, y);
        let d = b.binary(Op::Sub, Flags::NONE, o, a);
        b.ret(&[d]);
        let func = b.finish_checked().unwrap();

        let _ = optimize_mach_egraph(
            func.view(),
            MachEGraphOpts::apple_m5(),
            Some(&mut cache),
            alloc,
        )
        .unwrap();
        cache.insert([0x42u8; 32], CachedRewrite::NoImprovement);
        assert!(cache.len() >= 2);

        // Encode -> Decode -> Re-encode must be byte-for-byte identical (Rule 26).
        let bytes1 = cache.encode_bytes(alloc);
        let mut decoded = SuperoptCache::decode_bytes(bytes1.as_slice(), alloc).unwrap();
        assert_eq!(decoded.len(), cache.len());
        let bytes2 = decoded.encode_bytes(alloc);
        assert_eq!(bytes1.as_slice(), bytes2.as_slice(), "cache bytes must round-trip deterministically");

        // Decoded cache must serve O(1) cache hit with synthesis disabled.
        let opts_cache_only = MachEGraphOpts { enable_synth: false, ..MachEGraphOpts::apple_m5() };
        let out = optimize_mach_egraph(func.view(), opts_cache_only, Some(&mut decoded), alloc)
            .unwrap();
        assert!(out.stats.cache_hits >= 1, "decoded cache should hit: {:?}", out.stats);
        assert!(out.stats.cost_after < out.stats.cost_before);

        // Reject corrupted magic.
        let mut bad_magic = Vec::with_capacity_in(bytes1.len(), alloc);
        bad_magic.extend_from_slice(bytes1.as_slice());
        bad_magic.as_mut_slice()[0] = b'X';
        assert_eq!(
            SuperoptCache::decode_bytes(bad_magic.as_slice(), alloc).unwrap_err(),
            CacheDecodeError::BadMagic
        );

        // Reject corrupted payload (BLAKE3 integrity seal mismatch).
        let mut bad_payload = Vec::with_capacity_in(bytes1.len(), alloc);
        bad_payload.extend_from_slice(bytes1.as_slice());
        let mid = bad_payload.len() / 2;
        bad_payload.as_mut_slice()[mid] ^= 0x01;
        assert_eq!(
            SuperoptCache::decode_bytes(bad_payload.as_slice(), alloc).unwrap_err(),
            CacheDecodeError::BadDigest
        );

        // Reject corrupted digest byte.
        let mut bad_digest = Vec::with_capacity_in(bytes1.len(), alloc);
        bad_digest.extend_from_slice(bytes1.as_slice());
        let last = bad_digest.len() - 1;
        bad_digest.as_mut_slice()[last] ^= 0x80;
        assert_eq!(
            SuperoptCache::decode_bytes(bad_digest.as_slice(), alloc).unwrap_err(),
            CacheDecodeError::BadDigest
        );

        // Reject truncated buffer.
        assert_eq!(
            SuperoptCache::decode_bytes(&bytes1.as_slice()[..16], alloc).unwrap_err(),
            CacheDecodeError::Truncated
        );
    }
}
