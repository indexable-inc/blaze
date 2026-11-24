//! 16KB Apple Silicon Page-Exact BOLT Function/Cold Packer (`pack_module_pages`) &
//! BPG2 Branch-Entropy / Value-Specialization Profile (`Bpg2Profile`).
//!
//! Two tightly coupled PGO / code-layout primitives for Apple Silicon (`hw.pagesize == 16384`):
//!
//! 1. **BPG2 Extended Profile Record (`BPG2`)** — backward-compatible companion to `BPG1`:
//!    - [`BranchEntropyEntry`]: per-branch `(taken, not_taken, mispredicts)` hardware PMU/profile
//!      counts driving [`BranchEntropyEntry::should_if_convert`] (convert unpredictable branches
//!      `>= 30‰` = 3% mispredict rate into branchless `csel`/SIMD masks to avoid 14-cycle M5
//!      flushes, while keeping `>= 97%`-predictable branches speculative).
//!    - [`ValueProfileEntry`]: per-parameter invariant value histogram driving
//!      [`ValueProfileEntry::should_specialize`] (`>= 800‰` = 80%+ invariant `stride == 1` /
//!      constant divisor -> guarded specialization).
//!    - [`Bpg2Profile`]: deterministically sorted & coalesced table with zero-trust BLAKE3-sealed
//!      [`Bpg2Profile::encode_bytes`] / [`Bpg2Profile::decode_bytes`] persistence (Rule 26).
//! 2. **16KB Page-Exact Function & Hot/Cold Section Packer (`pack_module_pages`)**:
//!    - Partitions module functions into **Hot/Warm** (`.text`) and **Cold** (`.text.cold`).
//!    - Runs **Ext-TSP / Pettis-Hansen greedy hot-chain merging** over weighted call-graph edges,
//!      preferring chains that stay inside one 16KB iTLB page and maximizing `weight / distance`.
//!    - Places hot chains with **16KB page-straddle avoidance** and packs cold functions tightly
//!      starting on a separate 16KB page boundary so cold code never pollutes hot iTLB entries.
//!
//! Pure sans-IO, zero `unsafe`, `#![no_std]` compatible, deterministic (Rule 25), bounded
//! (Rule 18), explicit allocator only.

use core::alloc::Allocator;
use core::fmt;

use blaze_blake3::Hasher;
use blaze_own::Vec;

/// Magic bytes for persisted `BPG2` branch-entropy & value-specialization profile records.
pub const BPG2_MAGIC: [u8; 4] = *b"BPG2";
/// Binary schema version of `BPG2`.
pub const BPG2_VERSION: u32 = 1;
/// BLAKE3 key-derivation domain for sealing and verifying `BPG2` records.
const BPG2_RECORD_DOMAIN: &str = "blaze v2 emit bpg2 record 1";

/// Default misprediction rate threshold (30‰ = 3.0%) above which if-conversion (`csel`/mask)
/// beats a 14-cycle Apple M5 branch mispredict flush.
pub const DEFAULT_IF_CONVERT_PERMILLE: u32 = 30;
/// Default dominant-value hit rate threshold (800‰ = 80.0%) for guarded value specialization.
pub const DEFAULT_VALUE_SPEC_PERMILLE: u32 = 800;

/// Exact virtual-memory / iTLB page size on Apple Silicon (`hw.pagesize == 16384`).
pub const APPLE_PAGE_BYTES: u32 = 16_384;

/// Maximum greedy chain-merge rounds in [`pack_module_pages`] (bounds compile time, Rule 18).
const MAX_MERGE_ROUNDS: usize = 4_096;

/// Header size of an encoded `BPG2` record (`magic(4) + version(4) + n_br(4) + n_val(4)`).
const BPG2_HEADER_BYTES: usize = 16;
/// Encoded byte width of one [`BranchEntropyEntry`] (`32 + 4 + 8 + 8 + 8`).
const BPG2_BRANCH_BYTES: usize = 60;
/// Encoded byte width of one [`ValueProfileEntry`] (`32 + 2 + 8 + 8 + 8`).
const BPG2_VALUE_BYTES: usize = 58;
/// BLAKE3 integrity digest trailer size in bytes.
const BPG2_TRAILER_BYTES: usize = 32;

#[inline]
fn to_idx(v: u32) -> usize {
    usize::try_from(v).unwrap_or(usize::MAX)
}

#[inline]
fn to_u32(v: usize) -> u32 {
    u32::try_from(v).unwrap_or(u32::MAX)
}

#[inline]
fn at_u64(s: &[u64], i: usize) -> u64 {
    s.get(i).copied().unwrap_or(0)
}

#[inline]
fn at_usize(s: &[usize], i: usize) -> usize {
    s.get(i).copied().unwrap_or(0)
}

#[inline]
fn at_bool(s: &[bool], i: usize) -> bool {
    s.get(i).copied().unwrap_or(false)
}

#[inline]
fn set_at<T>(s: &mut [T], i: usize, v: T) {
    if let Some(slot) = s.get_mut(i) {
        *slot = v;
    }
}

#[inline]
fn align_up(offset: u64, align: u64) -> u64 {
    let a = align.max(1);
    let rem = offset % a;
    if rem == 0 {
        offset
    } else {
        offset.saturating_add(a - rem)
    }
}

/// Error returned when decoding or verifying a persisted `BPG2` profile record.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Bpg2Error {
    TooShort,
    BadMagic,
    BadVersion(u32),
    LengthMismatch,
    BadDigest,
}

impl fmt::Display for Bpg2Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Bpg2Error::TooShort => write!(f, "BPG2 profile record truncated"),
            Bpg2Error::BadMagic => write!(f, "BPG2 profile record missing BPG2 magic"),
            Bpg2Error::BadVersion(v) => write!(f, "unsupported BPG2 profile record version {v}"),
            Bpg2Error::LengthMismatch => write!(f, "BPG2 profile record length does not match header counts"),
            Bpg2Error::BadDigest => write!(f, "BPG2 profile record BLAKE3 integrity check failed"),
        }
    }
}

impl core::error::Error for Bpg2Error {}

/// Per-branch runtime outcome and misprediction telemetry for entropy-guided if-conversion.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct BranchEntropyEntry {
    pub body: [u8; 32],
    pub br_inst: u32,
    pub taken: u64,
    pub not_taken: u64,
    pub mispredicts: u64,
}

impl BranchEntropyEntry {
    /// Total observed dynamic executions (`taken + not_taken`, saturating).
    #[inline]
    pub const fn total(&self) -> u64 {
        self.taken.saturating_add(self.not_taken)
    }

    /// Observed misprediction rate in permille (`0..=1000`: `mispredicts * 1000 / total`).
    #[inline]
    pub fn mispredict_permille(&self) -> u32 {
        let total = self.total();
        if total == 0 {
            return 0;
        }
        let clamped = self.mispredicts.min(total);
        let pm = (u128::from(clamped) * 1_000) / u128::from(total);
        u32::try_from(pm).unwrap_or(1_000)
    }

    /// Returns `true` when this branch should be if-converted (`csel` / SIMD mask) instead of
    /// kept as a speculative conditional branch (`mispredict_permille >= threshold_permille`).
    #[inline]
    pub fn should_if_convert(&self, threshold_permille: u32) -> bool {
        self.total() > 0 && self.mispredict_permille() >= threshold_permille
    }
}

/// Per-parameter hot-value observation driving guarded value specialization (e.g. `stride == 1`).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct ValueProfileEntry {
    pub body: [u8; 32],
    pub param_idx: u16,
    pub hot_value: u64,
    pub hits: u64,
    pub total: u64,
}

impl ValueProfileEntry {
    /// Observed hit rate of `hot_value` in permille (`0..=1000`: `hits * 1000 / total`).
    #[inline]
    pub fn hit_rate_permille(&self) -> u32 {
        if self.total == 0 {
            return 0;
        }
        let clamped = self.hits.min(self.total);
        let pm = (u128::from(clamped) * 1_000) / u128::from(self.total);
        u32::try_from(pm).unwrap_or(1_000)
    }

    /// Returns `true` when `hot_value` dominates parameter `param_idx` strongly enough to justify
    /// guarded specialization (`hit_rate_permille >= min_permille`, e.g. `>= 800‰`).
    #[inline]
    pub fn should_specialize(&self, min_permille: u32) -> bool {
        self.total > 0 && self.hit_rate_permille() >= min_permille
    }
}

/// Sorted, deterministic, BLAKE3-sealed `BPG2` branch-entropy & value-specialization profile.
#[derive(Debug)]
pub struct Bpg2Profile<A: Allocator> {
    branches: Vec<BranchEntropyEntry, A>,
    values: Vec<ValueProfileEntry, A>,
}

fn read_bytes32(bytes: &[u8], off: usize) -> Option<[u8; 32]> {
    bytes.get(off..off.checked_add(32)?)?.try_into().ok()
}

fn read_u16_le(bytes: &[u8], off: usize) -> Option<u16> {
    let buf: [u8; 2] = bytes.get(off..off.checked_add(2)?)?.try_into().ok()?;
    Some(u16::from_le_bytes(buf))
}

fn read_u32_le(bytes: &[u8], off: usize) -> Option<u32> {
    let buf: [u8; 4] = bytes.get(off..off.checked_add(4)?)?.try_into().ok()?;
    Some(u32::from_le_bytes(buf))
}

fn read_u64_le(bytes: &[u8], off: usize) -> Option<u64> {
    let buf: [u8; 8] = bytes.get(off..off.checked_add(8)?)?.try_into().ok()?;
    Some(u64::from_le_bytes(buf))
}

impl<A: Allocator + Copy> Bpg2Profile<A> {
    /// Builds a deterministically sorted and deduplicated `BPG2` profile table.
    ///
    /// Duplicate `(body, br_inst)` branch entries and `(body, param_idx, hot_value)` value
    /// entries are coalesced by saturating addition.
    pub fn new_in(
        raw_branches: &[BranchEntropyEntry],
        raw_values: &[ValueProfileEntry],
        alloc: A,
    ) -> Self {
        let mut branches = Vec::with_capacity_in(raw_branches.len(), alloc);
        branches.extend_from_slice(raw_branches);
        branches.sort_unstable_by(|a, b| {
            a.body.cmp(&b.body).then_with(|| a.br_inst.cmp(&b.br_inst))
        });
        let mut bw = 0usize;
        for br in 0..branches.len() {
            let Some(&entry) = branches.get(br) else {
                break;
            };
            let merged = if bw > 0
                && let Some(prev) = branches.get_mut(bw - 1)
                && prev.body == entry.body
                && prev.br_inst == entry.br_inst
            {
                prev.taken = prev.taken.saturating_add(entry.taken);
                prev.not_taken = prev.not_taken.saturating_add(entry.not_taken);
                prev.mispredicts = prev.mispredicts.saturating_add(entry.mispredicts);
                true
            } else {
                false
            };
            if !merged {
                set_at(&mut branches, bw, entry);
                bw += 1;
            }
        }
        branches.truncate(bw);

        let mut values = Vec::with_capacity_in(raw_values.len(), alloc);
        values.extend_from_slice(raw_values);
        values.sort_unstable_by(|a, b| {
            a.body
                .cmp(&b.body)
                .then_with(|| a.param_idx.cmp(&b.param_idx))
                .then_with(|| a.hot_value.cmp(&b.hot_value))
        });
        let mut vw = 0usize;
        for vr in 0..values.len() {
            let Some(&entry) = values.get(vr) else {
                break;
            };
            let merged = if vw > 0
                && let Some(prev) = values.get_mut(vw - 1)
                && prev.body == entry.body
                && prev.param_idx == entry.param_idx
                && prev.hot_value == entry.hot_value
            {
                prev.hits = prev.hits.saturating_add(entry.hits);
                prev.total = prev.total.saturating_add(entry.total);
                true
            } else {
                false
            };
            if !merged {
                set_at(&mut values, vw, entry);
                vw += 1;
            }
        }
        values.truncate(vw);

        Self { branches, values }
    }

    #[inline]
    pub fn branches(&self) -> &[BranchEntropyEntry] {
        self.branches.as_slice()
    }

    #[inline]
    pub fn values(&self) -> &[ValueProfileEntry] {
        self.values.as_slice()
    }

    /// Looks up the branch-entropy entry for instruction `br_inst` in `body`.
    pub fn branch_policy(&self, body: [u8; 32], br_inst: u32) -> Option<BranchEntropyEntry> {
        let idx = self
            .branches
            .binary_search_by(|e| e.body.cmp(&body).then_with(|| e.br_inst.cmp(&br_inst)))
            .ok()?;
        self.branches.get(idx).copied()
    }

    /// Returns the sorted slice of value-specialization hints recorded for `body`.
    pub fn hot_value_hints(&self, body: [u8; 32]) -> &[ValueProfileEntry] {
        let start = self.values.partition_point(|e| e.body < body);
        let end = self.values.partition_point(|e| e.body <= body);
        self.values.get(start..end).unwrap_or(&[])
    }

    /// Cross-run exponential moving decay merge (`old * decay_num / decay_den + fresh`),
    /// dropping entries whose decayed observation counts reach zero (Rule 86).
    pub fn merge_decayed(&self, fresh: &Self, decay_num: u64, decay_den: u64, alloc: A) -> Self {
        let den = decay_den.max(1);
        let mut merged_br = Vec::with_capacity_in(self.branches.len() + fresh.branches.len(), alloc);
        for e in self.branches.iter() {
            let taken = e.taken.saturating_mul(decay_num) / den;
            let not_taken = e.not_taken.saturating_mul(decay_num) / den;
            let mispredicts = e.mispredicts.saturating_mul(decay_num) / den;
            if taken > 0 || not_taken > 0 {
                merged_br.push(BranchEntropyEntry {
                    body: e.body,
                    br_inst: e.br_inst,
                    taken,
                    not_taken,
                    mispredicts,
                });
            }
        }
        merged_br.extend_from_slice(fresh.branches.as_slice());

        let mut merged_val = Vec::with_capacity_in(self.values.len() + fresh.values.len(), alloc);
        for e in self.values.iter() {
            let hits = e.hits.saturating_mul(decay_num) / den;
            let total = e.total.saturating_mul(decay_num) / den;
            if total > 0 {
                merged_val.push(ValueProfileEntry {
                    body: e.body,
                    param_idx: e.param_idx,
                    hot_value: e.hot_value,
                    hits,
                    total,
                });
            }
        }
        merged_val.extend_from_slice(fresh.values.as_slice());

        Self::new_in(&merged_br, &merged_val, alloc)
    }

    /// Encodes this profile into a deterministic `BPG2` byte record sealed with a 32-byte
    /// domain-separated BLAKE3 integrity digest (Rule 25 & Rule 26).
    pub fn encode_bytes(&self, alloc: A) -> Vec<u8, A> {
        let total = BPG2_HEADER_BYTES
            .saturating_add(self.branches.len().saturating_mul(BPG2_BRANCH_BYTES))
            .saturating_add(self.values.len().saturating_mul(BPG2_VALUE_BYTES))
            .saturating_add(BPG2_TRAILER_BYTES);
        let mut out = Vec::with_capacity_in(total, alloc);
        out.extend_from_slice(&BPG2_MAGIC);
        out.extend_from_slice(&BPG2_VERSION.to_le_bytes());
        out.extend_from_slice(&to_u32(self.branches.len()).to_le_bytes());
        out.extend_from_slice(&to_u32(self.values.len()).to_le_bytes());

        for e in self.branches.iter() {
            out.extend_from_slice(&e.body);
            out.extend_from_slice(&e.br_inst.to_le_bytes());
            out.extend_from_slice(&e.taken.to_le_bytes());
            out.extend_from_slice(&e.not_taken.to_le_bytes());
            out.extend_from_slice(&e.mispredicts.to_le_bytes());
        }
        for e in self.values.iter() {
            out.extend_from_slice(&e.body);
            out.extend_from_slice(&e.param_idx.to_le_bytes());
            out.extend_from_slice(&e.hot_value.to_le_bytes());
            out.extend_from_slice(&e.hits.to_le_bytes());
            out.extend_from_slice(&e.total.to_le_bytes());
        }

        let mut hasher = Hasher::new_derive_key(BPG2_RECORD_DOMAIN);
        hasher.update(out.as_slice());
        let digest = hasher.finalize();
        out.extend_from_slice(&digest);
        out
    }

    /// Decodes and verifies a persisted `BPG2` profile record, checking magic, version,
    /// exact byte length (overflow-checked), and BLAKE3 integrity digest before trusting bytes.
    pub fn decode_bytes(bytes: &[u8], alloc: A) -> Result<Self, Bpg2Error> {
        if bytes.len() < BPG2_HEADER_BYTES + BPG2_TRAILER_BYTES {
            return Err(Bpg2Error::TooShort);
        }
        let magic = bytes.get(0..4).ok_or(Bpg2Error::TooShort)?;
        if magic != BPG2_MAGIC {
            return Err(Bpg2Error::BadMagic);
        }
        let version = read_u32_le(bytes, 4).ok_or(Bpg2Error::TooShort)?;
        if version != BPG2_VERSION {
            return Err(Bpg2Error::BadVersion(version));
        }
        let n_br = usize::try_from(read_u32_le(bytes, 8).ok_or(Bpg2Error::TooShort)?)
            .map_err(|_| Bpg2Error::LengthMismatch)?;
        let n_val = usize::try_from(read_u32_le(bytes, 12).ok_or(Bpg2Error::TooShort)?)
            .map_err(|_| Bpg2Error::LengthMismatch)?;

        let br_bytes = n_br.checked_mul(BPG2_BRANCH_BYTES).ok_or(Bpg2Error::LengthMismatch)?;
        let val_bytes = n_val.checked_mul(BPG2_VALUE_BYTES).ok_or(Bpg2Error::LengthMismatch)?;
        let expected = BPG2_HEADER_BYTES
            .checked_add(br_bytes)
            .and_then(|n| n.checked_add(val_bytes))
            .and_then(|n| n.checked_add(BPG2_TRAILER_BYTES))
            .ok_or(Bpg2Error::LengthMismatch)?;
        if bytes.len() != expected {
            return Err(Bpg2Error::LengthMismatch);
        }

        let payload_len = bytes.len() - BPG2_TRAILER_BYTES;
        let payload = bytes.get(..payload_len).ok_or(Bpg2Error::LengthMismatch)?;
        let trailer = bytes.get(payload_len..).ok_or(Bpg2Error::LengthMismatch)?;
        let mut hasher = Hasher::new_derive_key(BPG2_RECORD_DOMAIN);
        hasher.update(payload);
        let expected_digest = hasher.finalize();
        if expected_digest.as_slice() != trailer {
            return Err(Bpg2Error::BadDigest);
        }

        let mut cur = BPG2_HEADER_BYTES;
        let mut raw_branches = Vec::with_capacity_in(n_br, alloc);
        for _ in 0..n_br {
            let body = read_bytes32(bytes, cur).ok_or(Bpg2Error::LengthMismatch)?;
            let br_inst = read_u32_le(bytes, cur + 32).ok_or(Bpg2Error::LengthMismatch)?;
            let taken = read_u64_le(bytes, cur + 36).ok_or(Bpg2Error::LengthMismatch)?;
            let not_taken = read_u64_le(bytes, cur + 44).ok_or(Bpg2Error::LengthMismatch)?;
            let mispredicts = read_u64_le(bytes, cur + 52).ok_or(Bpg2Error::LengthMismatch)?;
            cur += BPG2_BRANCH_BYTES;
            raw_branches.push(BranchEntropyEntry {
                body,
                br_inst,
                taken,
                not_taken,
                mispredicts,
            });
        }

        let mut raw_values = Vec::with_capacity_in(n_val, alloc);
        for _ in 0..n_val {
            let body = read_bytes32(bytes, cur).ok_or(Bpg2Error::LengthMismatch)?;
            let param_idx = read_u16_le(bytes, cur + 32).ok_or(Bpg2Error::LengthMismatch)?;
            let hot_value = read_u64_le(bytes, cur + 34).ok_or(Bpg2Error::LengthMismatch)?;
            let hits = read_u64_le(bytes, cur + 42).ok_or(Bpg2Error::LengthMismatch)?;
            let total = read_u64_le(bytes, cur + 50).ok_or(Bpg2Error::LengthMismatch)?;
            cur += BPG2_VALUE_BYTES;
            raw_values.push(ValueProfileEntry {
                body,
                param_idx,
                hot_value,
                hits,
                total,
            });
        }

        Ok(Self::new_in(&raw_branches, &raw_values, alloc))
    }
}

/// Configuration for [`pack_module_pages`] (Rule 83: one options struct).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BoltOpts {
    /// Target hardware iTLB / virtual-memory page size in bytes (`16_384` on Apple Silicon).
    pub page_bytes: u32,
    /// Minimum byte alignment for hot functions (`16` on AArch64 for fetch-group alignment).
    pub align_hot_bytes: u32,
    /// Functions with `body_weight <= cold_threshold_weight` (or `is_cold == true`) are moved
    /// into `.text.cold` when `split_cold_section` is enabled.
    pub cold_threshold_weight: u64,
    /// Whether to segregate cold functions into `.text.cold` starting on a fresh page boundary.
    pub split_cold_section: bool,
}

impl BoltOpts {
    /// Apple Silicon defaults: 16KB pages, 16-byte hot alignment, hot/cold section split enabled.
    pub const fn apple_silicon() -> Self {
        Self {
            page_bytes: APPLE_PAGE_BYTES,
            align_hot_bytes: 16,
            cold_threshold_weight: 0,
            split_cold_section: true,
        }
    }
}

impl Default for BoltOpts {
    fn default() -> Self {
        Self::apple_silicon()
    }
}

/// One module function handed to [`pack_module_pages`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BoltFn {
    pub id: u32,
    pub code_bytes: u32,
    pub body_weight: u64,
    pub is_cold: bool,
}

/// One weighted directed call edge `(caller_idx -> callee_idx)` in the module call graph.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BoltCallEdge {
    pub caller_idx: u32,
    pub callee_idx: u32,
    pub call_weight: u64,
}

/// One placed function in the output [`PageLayoutPlan`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PlacedFn {
    /// Original index into the input `fns` slice.
    pub fn_idx: u32,
    /// Function identifier copied from [`BoltFn::id`].
    pub id: u32,
    /// Exact byte offset from the start of the packed module text image.
    pub byte_offset: u64,
    /// Page index (`byte_offset / page_bytes`) containing the start of this function.
    pub page_index: u32,
    /// Alignment / page-straddle-avoidance padding inserted immediately before this function.
    pub align_padding: u32,
    /// Function machine-code size in bytes.
    pub code_bytes: u32,
}

/// Summary statistics from [`pack_module_pages`].
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct BoltLayoutStats {
    pub hot_pages: u32,
    pub cold_pages: u32,
    pub cross_page_hot_calls_before: u64,
    pub cross_page_hot_calls_after: u64,
    pub hot_bytes: u64,
    pub cold_bytes: u64,
}

/// Deterministic 16KB page-exact function layout plan returned by [`pack_module_pages`].
#[derive(Debug)]
pub struct PageLayoutPlan<A: Allocator> {
    pub hot_order: Vec<PlacedFn, A>,
    pub cold_order: Vec<PlacedFn, A>,
    pub stats: BoltLayoutStats,
}

struct HotChain<A: Allocator> {
    nodes: Vec<usize, A>,
    total_bytes: u64,
    total_weight: u64,
    alive: bool,
}

#[derive(Clone, Copy)]
struct HotEdge {
    u_slot: usize,
    v_slot: usize,
    weight: u64,
}

#[inline]
fn end_page_of(start_off: u64, code_bytes: u64, page_bytes: u64) -> u64 {
    if code_bytes == 0 {
        start_off / page_bytes
    } else {
        start_off.saturating_add(code_bytes - 1) / page_bytes
    }
}

/// Packs module functions into 16KB Apple Silicon pages using Ext-TSP / Pettis-Hansen greedy
/// hot-chain merging, 16KB page-straddle avoidance, and hot/cold `.text.cold` segregation.
pub fn pack_module_pages<A: Allocator + Copy>(
    fns: &[BoltFn],
    edges: &[BoltCallEdge],
    opts: BoltOpts,
    alloc: A,
) -> PageLayoutPlan<A> {
    let n = fns.len();
    if n == 0 {
        return PageLayoutPlan {
            hot_order: Vec::new_in(alloc),
            cold_order: Vec::new_in(alloc),
            stats: BoltLayoutStats::default(),
        };
    }

    let page_bytes = u64::from(opts.page_bytes.max(64));
    let align_hot = u64::from(opts.align_hot_bytes.max(1));

    // Partition into Hot/Warm vs Cold.
    let mut is_cold_fn = Vec::from_elem_in(false, n, alloc);
    let mut hot_slot = Vec::from_elem_in(usize::MAX, n, alloc);
    let mut hot_fn_indices = Vec::new_in(alloc);
    let mut cold_fn_indices = Vec::new_in(alloc);
    for (i, f) in fns.iter().enumerate() {
        let cold = opts.split_cold_section && (f.is_cold || f.body_weight <= opts.cold_threshold_weight);
        set_at(&mut is_cold_fn, i, cold);
        if cold {
            cold_fn_indices.push(i);
        } else {
            let slot = hot_fn_indices.len();
            set_at(&mut hot_slot, i, slot);
            hot_fn_indices.push(i);
        }
    }

    // Measure baseline (`before`) cross-page hot call weight in original input order.
    let mut before_start_page = Vec::from_elem_in(0u64, n, alloc);
    let mut before_end_page = Vec::from_elem_in(0u64, n, alloc);
    let mut cur_before = 0u64;
    for (i, f) in fns.iter().enumerate() {
        let off = align_up(cur_before, align_hot);
        let sz = u64::from(f.code_bytes);
        set_at(&mut before_start_page, i, off / page_bytes);
        set_at(&mut before_end_page, i, end_page_of(off, sz, page_bytes));
        cur_before = off.saturating_add(sz);
    }

    let mut cross_before = 0u64;
    let mut raw_hot_edges = Vec::new_in(alloc);
    for e in edges {
        if e.call_weight == 0 {
            continue;
        }
        let u = to_idx(e.caller_idx);
        let v = to_idx(e.callee_idx);
        if u >= n || v >= n || u == v {
            continue;
        }
        let both_hot = !at_bool(&is_cold_fn, u) && !at_bool(&is_cold_fn, v);
        if both_hot {
            let same_page = at_u64(&before_start_page, u) == at_u64(&before_start_page, v)
                && at_u64(&before_end_page, u) == at_u64(&before_end_page, v)
                && at_u64(&before_start_page, u) == at_u64(&before_end_page, u);
            if !same_page {
                cross_before = cross_before.saturating_add(e.call_weight);
            }
            let us = at_usize(&hot_slot, u);
            let vs = at_usize(&hot_slot, v);
            if us < hot_fn_indices.len() && vs < hot_fn_indices.len() {
                raw_hot_edges.push(HotEdge {
                    u_slot: us,
                    v_slot: vs,
                    weight: e.call_weight,
                });
            }
        }
    }

    // Coalesce duplicate directed hot edges deterministically.
    raw_hot_edges.sort_unstable_by(|a, b| {
        a.u_slot.cmp(&b.u_slot).then_with(|| a.v_slot.cmp(&b.v_slot))
    });
    let mut hw = 0usize;
    for hr in 0..raw_hot_edges.len() {
        let Some(&edge) = raw_hot_edges.get(hr) else {
            break;
        };
        let merged = if hw > 0
            && let Some(prev) = raw_hot_edges.get_mut(hw - 1)
            && prev.u_slot == edge.u_slot
            && prev.v_slot == edge.v_slot
        {
            prev.weight = prev.weight.saturating_add(edge.weight);
            true
        } else {
            false
        };
        if !merged {
            set_at(&mut raw_hot_edges, hw, edge);
            hw += 1;
        }
    }
    raw_hot_edges.truncate(hw);

    // Initialize one Ext-TSP chain per hot function.
    let hot_count = hot_fn_indices.len();
    let mut chains: Vec<HotChain<A>, A> = Vec::with_capacity_in(hot_count, alloc);
    let mut chain_of = Vec::with_capacity_in(hot_count, alloc);
    let mut offset_in_chain = Vec::from_elem_in(0u64, hot_count, alloc);
    for (slot, &fn_idx) in hot_fn_indices.iter().enumerate() {
        let f = fns.get(fn_idx).copied().unwrap_or(BoltFn {
            id: 0,
            code_bytes: 0,
            body_weight: 0,
            is_cold: false,
        });
        let mut nodes = Vec::with_capacity_in(1, alloc);
        nodes.push(slot);
        chains.push(HotChain {
            nodes,
            total_bytes: align_up(u64::from(f.code_bytes), align_hot),
            total_weight: f.body_weight,
            alive: true,
        });
        chain_of.push(slot);
    }

    // Greedy Ext-TSP / Pettis-Hansen chain merging maximizing `weight / distance` with a 2x
    // locality bonus when the merged chain fits inside a single 16KB page.
    let max_rounds = hot_count.min(MAX_MERGE_ROUNDS);
    for _ in 0..max_rounds {
        let mut best_score = 0u128;
        let mut best_weight = 0u64;
        let mut best_pair: Option<(usize, usize)> = None;

        for e in raw_hot_edges.iter() {
            let ca = at_usize(&chain_of, e.u_slot);
            let cb = at_usize(&chain_of, e.v_slot);
            if ca == cb {
                continue;
            }
            let (Some(chain_a), Some(chain_b)) = (chains.get(ca), chains.get(cb)) else {
                continue;
            };
            if !chain_a.alive || !chain_b.alive {
                continue;
            }
            let combined_bytes = chain_a.total_bytes.saturating_add(chain_b.total_bytes);
            let page_bonus = if combined_bytes <= page_bytes { 2u128 } else { 1u128 };
            let off_u = at_u64(&offset_in_chain, e.u_slot);
            let off_v = at_u64(&offset_in_chain, e.v_slot);

            // Evaluate `ca ++ cb` (caller chain before callee chain; +1% tie-break preference for
            // forward call fallthrough) and `cb ++ ca`.
            let dist_ab = chain_a.total_bytes.saturating_sub(off_u).saturating_add(off_v);
            let score_ab = u128::from(e.weight)
                .saturating_mul(page_bonus)
                .saturating_mul(u128::from(page_bytes))
                .saturating_mul(101)
                / u128::from(dist_ab.saturating_add(64)).saturating_mul(100);

            let dist_ba = chain_b.total_bytes.saturating_sub(off_v).saturating_add(off_u);
            let score_ba = u128::from(e.weight)
                .saturating_mul(page_bonus)
                .saturating_mul(u128::from(page_bytes))
                / u128::from(dist_ba.saturating_add(64));

            let (cand_score, cand_pair) = if score_ab >= score_ba {
                (score_ab, (ca, cb))
            } else {
                (score_ba, (cb, ca))
            };
            let better = match best_pair {
                None => cand_score > 0,
                Some(prev) => {
                    cand_score > best_score
                        || (cand_score == best_score && e.weight > best_weight)
                        || (cand_score == best_score && e.weight == best_weight && cand_pair < prev)
                }
            };
            if better {
                best_score = cand_score;
                best_weight = e.weight;
                best_pair = Some(cand_pair);
            }
        }

        let Some((dst_c, src_c)) = best_pair else {
            break;
        };

        // Splice `src_c` onto the end of `dst_c` without cloning or overlapping borrows.
        let (src_nodes, src_bytes, src_weight) = if let Some(sc) = chains.get_mut(src_c) {
            sc.alive = false;
            let moved = core::mem::replace(&mut sc.nodes, Vec::new_in(alloc));
            (moved, sc.total_bytes, sc.total_weight)
        } else {
            break;
        };
        if let Some(dc) = chains.get_mut(dst_c) {
            let base_off = dc.total_bytes;
            for &node_slot in src_nodes.iter() {
                set_at(&mut chain_of, node_slot, dst_c);
                let prev_off = at_u64(&offset_in_chain, node_slot);
                set_at(&mut offset_in_chain, node_slot, base_off.saturating_add(prev_off));
                dc.nodes.push(node_slot);
            }
            dc.total_bytes = dc.total_bytes.saturating_add(src_bytes);
            dc.total_weight = dc.total_weight.saturating_add(src_weight);
        }
    }

    // Order surviving hot chains by descending weight density (weight per KB), then descending
    // total weight, then ascending original first function index (deterministic, Rule 25).
    let mut alive_chains: Vec<usize, A> = Vec::with_capacity_in(hot_count, alloc);
    for (c_idx, ch) in chains.iter().enumerate() {
        if ch.alive && !ch.nodes.is_empty() {
            alive_chains.push(c_idx);
        }
    }
    alive_chains.sort_unstable_by(|&a, &b| {
        let (Some(ca), Some(cb)) = (chains.get(a), chains.get(b)) else {
            return a.cmp(&b);
        };
        let da = u128::from(ca.total_weight).saturating_mul(1_024) / u128::from(ca.total_bytes.max(1));
        let db = u128::from(cb.total_weight).saturating_mul(1_024) / u128::from(cb.total_bytes.max(1));
        db.cmp(&da)
            .then_with(|| cb.total_weight.cmp(&ca.total_weight))
            .then_with(|| {
                let fa = ca.nodes.first().copied().map_or(usize::MAX, |s| at_usize(&hot_fn_indices, s));
                let fb = cb.nodes.first().copied().map_or(usize::MAX, |s| at_usize(&hot_fn_indices, s));
                fa.cmp(&fb)
            })
    });

    // Place hot functions with 16KB page-straddle avoidance.
    let mut after_start_page = Vec::from_elem_in(0u64, n, alloc);
    let mut after_end_page = Vec::from_elem_in(0u64, n, alloc);
    let mut hot_order = Vec::with_capacity_in(hot_count, alloc);
    let mut cur_offset = 0u64;
    for &c_idx in alive_chains.iter() {
        let Some(ch) = chains.get(c_idx) else {
            continue;
        };
        for &slot in ch.nodes.iter() {
            let fn_idx = at_usize(&hot_fn_indices, slot);
            let Some(&f) = fns.get(fn_idx) else {
                continue;
            };
            let sz = u64::from(f.code_bytes);
            let mut placed_off = align_up(cur_offset, align_hot);
            let page_off = placed_off % page_bytes;
            // Avoid splitting a hot function (<= 1 page) across two 16KB iTLB pages when the
            // pad to the next page boundary is at most half a page.
            if sz <= page_bytes
                && page_off > 0
                && page_off.saturating_add(sz) > page_bytes
                && (page_bytes - page_off) <= (page_bytes / 2)
            {
                placed_off = placed_off.saturating_add(page_bytes - page_off);
            }
            let pad = u32::try_from(placed_off.saturating_sub(cur_offset)).unwrap_or(u32::MAX);
            let start_pg = placed_off / page_bytes;
            let end_pg = end_page_of(placed_off, sz, page_bytes);
            set_at(&mut after_start_page, fn_idx, start_pg);
            set_at(&mut after_end_page, fn_idx, end_pg);
            hot_order.push(PlacedFn {
                fn_idx: to_u32(fn_idx),
                id: f.id,
                byte_offset: placed_off,
                page_index: u32::try_from(start_pg).unwrap_or(u32::MAX),
                align_padding: pad,
                code_bytes: f.code_bytes,
            });
            cur_offset = placed_off.saturating_add(sz);
        }
    }

    let hot_bytes = cur_offset;
    let hot_pages = if hot_order.is_empty() {
        0
    } else {
        u32::try_from(hot_bytes.div_ceil(page_bytes)).unwrap_or(u32::MAX)
    };

    // Pack cold functions tightly into `.text.cold` starting on the next 16KB page boundary so
    // cold code never shares an iTLB page with hot code.
    cold_fn_indices.sort_unstable_by(|&a, &b| {
        let wa = fns.get(a).map_or(0, |f| f.body_weight);
        let wb = fns.get(b).map_or(0, |f| f.body_weight);
        wb.cmp(&wa).then_with(|| a.cmp(&b))
    });
    let mut cold_order = Vec::with_capacity_in(cold_fn_indices.len(), alloc);
    let cold_start = if !hot_order.is_empty() && !cold_fn_indices.is_empty() {
        align_up(cur_offset, page_bytes)
    } else {
        cur_offset
    };
    let mut cur_cold = cold_start;
    for &fn_idx in cold_fn_indices.iter() {
        let Some(&f) = fns.get(fn_idx) else {
            continue;
        };
        let sz = u64::from(f.code_bytes);
        let placed_off = align_up(cur_cold, 4);
        let pad = u32::try_from(placed_off.saturating_sub(cur_cold)).unwrap_or(u32::MAX);
        let start_pg = placed_off / page_bytes;
        let end_pg = end_page_of(placed_off, sz, page_bytes);
        set_at(&mut after_start_page, fn_idx, start_pg);
        set_at(&mut after_end_page, fn_idx, end_pg);
        cold_order.push(PlacedFn {
            fn_idx: to_u32(fn_idx),
            id: f.id,
            byte_offset: placed_off,
            page_index: u32::try_from(start_pg).unwrap_or(u32::MAX),
            align_padding: pad,
            code_bytes: f.code_bytes,
        });
        cur_cold = placed_off.saturating_add(sz);
    }
    let cold_bytes = cur_cold.saturating_sub(cold_start);
    let cold_pages = if cold_order.is_empty() {
        0
    } else {
        u32::try_from(cold_bytes.div_ceil(page_bytes)).unwrap_or(u32::MAX)
    };

    // Measure `after` cross-page hot call weight using the exact same metric as `before`.
    let mut cross_after = 0u64;
    for e in edges {
        if e.call_weight == 0 {
            continue;
        }
        let u = to_idx(e.caller_idx);
        let v = to_idx(e.callee_idx);
        if u >= n || v >= n || u == v {
            continue;
        }
        if !at_bool(&is_cold_fn, u) && !at_bool(&is_cold_fn, v) {
            let same_page = at_u64(&after_start_page, u) == at_u64(&after_start_page, v)
                && at_u64(&after_end_page, u) == at_u64(&after_end_page, v)
                && at_u64(&after_start_page, u) == at_u64(&after_end_page, u);
            if !same_page {
                cross_after = cross_after.saturating_add(e.call_weight);
            }
        }
    }

    PageLayoutPlan {
        hot_order,
        cold_order,
        stats: BoltLayoutStats {
            hot_pages,
            cold_pages,
            cross_page_hot_calls_before: cross_before,
            cross_page_hot_calls_after: cross_after,
            hot_bytes,
            cold_bytes,
        },
    }
}

#[cfg(test)]
mod tests {
    use blaze_own::testing::TestHeap;

    use super::*;

    fn body_id(tag: u8) -> [u8; 32] {
        let mut b = [0u8; 32];
        b[0] = tag;
        b[31] = tag.wrapping_mul(7);
        b
    }

    #[test]
    fn bpg2_roundtrip_corruption_and_policy_thresholds() {
        let b1 = body_id(1);
        let b2 = body_id(2);

        // 50% mispredict (500‰) -> if-convert; 0.5% mispredict (5‰) -> keep speculative.
        let unpredictable = BranchEntropyEntry {
            body: b1,
            br_inst: 12,
            taken: 500,
            not_taken: 500,
            mispredicts: 500,
        };
        let predictable = BranchEntropyEntry {
            body: b1,
            br_inst: 24,
            taken: 995,
            not_taken: 5,
            mispredicts: 5,
        };
        assert_eq!(unpredictable.mispredict_permille(), 500);
        assert!(unpredictable.should_if_convert(DEFAULT_IF_CONVERT_PERMILLE));
        assert_eq!(predictable.mispredict_permille(), 5);
        assert!(!predictable.should_if_convert(DEFAULT_IF_CONVERT_PERMILLE));

        // 95% invariant `stride == 1` -> specialize; 40% -> do not specialize.
        let hot_stride = ValueProfileEntry {
            body: b2,
            param_idx: 1,
            hot_value: 1,
            hits: 950,
            total: 1_000,
        };
        let cold_stride = ValueProfileEntry {
            body: b2,
            param_idx: 2,
            hot_value: 8,
            hits: 400,
            total: 1_000,
        };
        assert_eq!(hot_stride.hit_rate_permille(), 950);
        assert!(hot_stride.should_specialize(DEFAULT_VALUE_SPEC_PERMILLE));
        assert_eq!(cold_stride.hit_rate_permille(), 400);
        assert!(!cold_stride.should_specialize(DEFAULT_VALUE_SPEC_PERMILLE));

        // Build profile in unsorted order and verify deterministic round-trip encoding.
        let prof = Bpg2Profile::new_in(
            &[predictable, unpredictable],
            &[cold_stride, hot_stride],
            TestHeap,
        );
        let bytes = prof.encode_bytes(TestHeap);
        let decoded = Bpg2Profile::decode_bytes(&bytes, TestHeap).expect("valid BPG2 decodes");
        assert_eq!(decoded.branches(), prof.branches());
        assert_eq!(decoded.values(), prof.values());
        assert_eq!(decoded.encode_bytes(TestHeap).as_slice(), bytes.as_slice());

        // Lookup methods.
        let p_unpred = decoded.branch_policy(b1, 12).expect("branch 12 present");
        assert!(p_unpred.should_if_convert(DEFAULT_IF_CONVERT_PERMILLE));
        let p_pred = decoded.branch_policy(b1, 24).expect("branch 24 present");
        assert!(!p_pred.should_if_convert(DEFAULT_IF_CONVERT_PERMILLE));
        assert!(decoded.branch_policy(b1, 999).is_none());

        let hints = decoded.hot_value_hints(b2);
        assert_eq!(hints.len(), 2);
        assert!(hints[0].should_specialize(DEFAULT_VALUE_SPEC_PERMILLE));
        assert!(!hints[1].should_specialize(DEFAULT_VALUE_SPEC_PERMILLE));

        // Corruption rejection: flipping any payload or digest byte fails BLAKE3 verification.
        let mut tampered = Vec::with_capacity_in(bytes.len(), TestHeap);
        tampered.extend_from_slice(&bytes);
        tampered[20] ^= 0x01;
        assert_eq!(
            Bpg2Profile::decode_bytes(&tampered, TestHeap).unwrap_err(),
            Bpg2Error::BadDigest
        );

        // Bad magic & truncation checks.
        let mut bad_magic = Vec::with_capacity_in(bytes.len(), TestHeap);
        bad_magic.extend_from_slice(&bytes);
        bad_magic[3] = b'1';
        assert_eq!(
            Bpg2Profile::decode_bytes(&bad_magic, TestHeap).unwrap_err(),
            Bpg2Error::BadMagic
        );
        assert_eq!(
            Bpg2Profile::decode_bytes(&bytes[..10], TestHeap).unwrap_err(),
            Bpg2Error::TooShort
        );
    }

    #[test]
    fn pack_module_pages_packs_hot_chain_into_single_16kb_page() {
        // Interleave a 5-function hot call chain (each 2 KiB -> 10 KiB total <= 16 KiB) with
        // 20 cold functions (each 4 KiB -> 4 cold fns = 16 KiB between every hot pair!).
        let mut fns = Vec::new_in(TestHeap);
        let mut hot_indices = [0u32; 5];
        let mut next_id = 100u32;
        for h in 0..5usize {
            let idx = u32::try_from(fns.len()).unwrap();
            hot_indices[h] = idx;
            fns.push(BoltFn {
                id: next_id,
                code_bytes: 2_048,
                body_weight: 50_000,
                is_cold: false,
            });
            next_id += 1;
            for _ in 0..4 {
                fns.push(BoltFn {
                    id: next_id,
                    code_bytes: 4_096,
                    body_weight: 0,
                    is_cold: true,
                });
                next_id += 1;
            }
        }
        assert_eq!(fns.len(), 25);

        // Weighted call chain `h0 -> h1 -> h2 -> h3 -> h4`.
        let mut edges = Vec::new_in(TestHeap);
        for h in 0..4usize {
            edges.push(BoltCallEdge {
                caller_idx: hot_indices[h],
                callee_idx: hot_indices[h + 1],
                call_weight: 10_000,
            });
        }

        let plan = pack_module_pages(&fns, &edges, BoltOpts::apple_silicon(), TestHeap);
        assert_eq!(plan.hot_order.len(), 5);
        assert_eq!(plan.cold_order.len(), 20);
        assert_eq!(plan.stats.hot_pages, 1, "all 5 hot fns must fit on 1 Apple Silicon 16KB page");
        assert!(plan.stats.cold_pages >= 5);
        assert_eq!(plan.stats.cross_page_hot_calls_before, 40_000);
        assert_eq!(plan.stats.cross_page_hot_calls_after, 0);
        assert!(
            plan.stats.cross_page_hot_calls_after < plan.stats.cross_page_hot_calls_before,
            "BOLT 16KB packing must strictly reduce cross-page hot calls"
        );

        // Every hot function resides on page 0 in call-chain order; every cold function starts on
        // page >= 1 (strict `.text.cold` separation).
        for (pos, placed) in plan.hot_order.iter().enumerate() {
            assert_eq!(placed.page_index, 0);
            assert_eq!(placed.fn_idx, hot_indices[pos]);
        }
        for placed in plan.cold_order.iter() {
            assert!(placed.page_index >= 1, "cold function must not share hot 16KB page 0");
            assert!(placed.byte_offset >= u64::from(APPLE_PAGE_BYTES));
        }
    }

    #[test]
    fn pack_module_pages_avoids_16kb_page_straddle_for_hot_fn() {
        // Two hot functions: 12 KiB followed by 6 KiB. Without straddle avoidance, fn 1 starts at
        // 12 KiB and straddles the 16 KiB boundary (12 KiB..18 KiB = 2 iTLB pages). With 16KB
        // straddle avoidance, fn 1 is padded by 4 KiB to start cleanly at 16 KiB (page 1).
        let fns = [
            BoltFn { id: 1, code_bytes: 12_288, body_weight: 20_000, is_cold: false },
            BoltFn { id: 2, code_bytes: 6_144, body_weight: 9_000, is_cold: false },
        ];
        let plan = pack_module_pages(&fns, &[], BoltOpts::apple_silicon(), TestHeap);
        assert_eq!(plan.hot_order.len(), 2);
        assert_eq!(plan.hot_order[0].byte_offset, 0);
        assert_eq!(plan.hot_order[0].page_index, 0);
        assert_eq!(plan.hot_order[1].byte_offset, u64::from(APPLE_PAGE_BYTES));
        assert_eq!(plan.hot_order[1].page_index, 1);
        assert_eq!(plan.hot_order[1].align_padding, 4_096);
    }
}
