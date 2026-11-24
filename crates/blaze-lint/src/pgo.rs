//! Profile-Guided Linting (PGL): re-ranks lint findings and emits targeted optimization advice
//! using historical runtime profile weights (e.g., from `blaze-profile::PgoProfile`).
//!
//! Pure, deterministic, `#![no_std]`, zero `unsafe`, explicit-allocator only.

use blaze_lex::{Cpu, FrontmatterAllowed, TokenKind, tokenize};
use blaze_own::{AVec, Alloc};

use crate::{Findings, Level, Levels, Rule, Span};

/// Canonical 64-bit hash of a function symbol or item path, deterministic across runs and hosts.
#[inline]
pub fn symbol_hash(symbol: &str) -> u64 {
    let mut h = blaze_blake3::Hasher::new_derive_key("blaze-lint pgo symbol 1");
    h.update(symbol.as_bytes());
    let out = h.finalize();
    u64::from_le_bytes([out[0], out[1], out[2], out[3], out[4], out[5], out[6], out[7]])
}

/// Historical execution heat tier derived from a function's share of total profile weight (ppm).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum Heat {
    /// `< 0.1%` of total runtime weight (`< 1_000 ppm`).
    Cold = 0,
    /// `0.1% .. 1%` (`1_000 .. 9_999 ppm`).
    Warm = 1,
    /// `1% .. 5%` (`10_000 .. 49_999 ppm`).
    Hot = 2,
    /// `>= 5%` (`>= 50_000 ppm`).
    Critical = 3,
}

impl Heat {
    #[inline]
    pub const fn from_ppm(share_ppm: u32) -> Heat {
        if share_ppm >= 50_000 {
            Heat::Critical
        } else if share_ppm >= 10_000 {
            Heat::Hot
        } else if share_ppm >= 1_000 {
            Heat::Warm
        } else {
            Heat::Cold
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Heat::Cold => "cold",
            Heat::Warm => "warm",
            Heat::Hot => "hot",
            Heat::Critical => "critical",
        }
    }
}

/// One profiled function's historical runtime weight.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct HotSite {
    pub symbol_hash: u64,
    pub weight: u64,
    pub share_ppm: u32,
}

impl HotSite {
    #[inline]
    pub fn for_symbol(symbol: &str, weight: u64, share_ppm: u32) -> HotSite {
        HotSite { symbol_hash: symbol_hash(symbol), weight, share_ppm }
    }

    #[inline]
    pub const fn heat(self) -> Heat {
        Heat::from_ppm(self.share_ppm)
    }
}

/// Sorted lookup table of profiled hot sites in an explicit allocator.
pub struct PgoHints<'a> {
    sites: AVec<'a, HotSite>,
    max_ppm: u32,
}

/// Resolved historical heat and ppm share for one function symbol.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct HeatInfo {
    pub heat: Heat,
    pub share_ppm: u32,
}

impl<'a> PgoHints<'a> {
    /// Builds a sorted, deduplicated [`PgoHints`] table from raw observations.
    /// Duplicate `symbol_hash` entries keep the larger `(share_ppm, weight)`.
    pub fn from_sites(raw: &[HotSite], alloc: Alloc<'a>) -> PgoHints<'a> {
        let mut sites = AVec::with_capacity_in(raw.len(), alloc);
        sites.extend_from_slice(raw);
        sites.sort_unstable_by(|a, b| {
            a.symbol_hash
                .cmp(&b.symbol_hash)
                .then_with(|| b.share_ppm.cmp(&a.share_ppm))
                .then_with(|| b.weight.cmp(&a.weight))
        });
        sites.dedup_by_key(|s| s.symbol_hash);
        let mut max_ppm = 0u32;
        for s in sites.iter() {
            if s.share_ppm > max_ppm {
                max_ppm = s.share_ppm;
            }
        }
        PgoHints { sites, max_ppm }
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.sites.is_empty()
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.sites.len()
    }

    /// Looks up the historical weight for `hash` in `O(log N)`.
    #[inline]
    pub fn lookup(&self, hash: u64) -> Option<HotSite> {
        let idx = self.sites.binary_search_by_key(&hash, |s| s.symbol_hash).ok()?;
        Some(self.sites[idx])
    }

    /// Looks up the historical weight for a function symbol name.
    #[inline]
    pub fn lookup_symbol(&self, symbol: &str) -> Option<HotSite> {
        self.lookup(symbol_hash(symbol))
    }

    #[inline]
    pub fn heat_of(&self, hash: u64) -> HeatInfo {
        match self.lookup(hash) {
            Some(site) => HeatInfo { heat: site.heat(), share_ppm: site.share_ppm },
            None => HeatInfo { heat: Heat::Cold, share_ppm: 0 },
        }
    }

    /// Highest `share_ppm` observed across all entries in this hint table.
    #[inline]
    pub const fn max_share_ppm(&self) -> u32 {
        self.max_ppm
    }
}

/// Effective severity after applying PGO promotion/suppression policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum EffectiveLevel {
    Suppressed = 0,
    Advice = 1,
    Error = 2,
}

/// How historical profile heat adjusts performance-sensitive lint severities.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PgoPolicy {
    /// Performance rules inside functions at or above this heat tier are auto-promoted to `Error`.
    pub promote_perf_at: Heat,
    /// When true, advice-level performance findings in `Heat::Cold` functions are suppressed.
    pub suppress_cold_perf: bool,
}

impl PgoPolicy {
    /// Promote perf findings in `Hot`/`Critical` functions (`>= 1%` runtime); keep cold advice.
    pub const DEFAULT: PgoPolicy = PgoPolicy { promote_perf_at: Heat::Hot, suppress_cold_perf: false };

    /// Focus exclusively on hot paths: promote `Hot`/`Critical` perf findings and suppress cold perf noise.
    pub const HOT_ONLY: PgoPolicy = PgoPolicy { promote_perf_at: Heat::Hot, suppress_cold_perf: true };
}

/// Whether `rule` is a performance-sensitive rule that benefits from PGO heat weighting.
#[inline]
pub const fn is_perf_rule(rule: Rule) -> bool {
    matches!(
        rule,
        Rule::Clones
            | Rule::Allocs
            | Rule::Recursion
            | Rule::AmbientHeap
            | Rule::UnboundedWait
            | Rule::AmbientHasher
            | Rule::SeqCstUnexplained
            | Rule::RawSync
            | Rule::BlockingOnPool
            | Rule::DoubleIndirectionArc
    )
}

/// Byte span of a function in an item/file, paired with its canonical [`symbol_hash`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FnSpan {
    pub range: Span,
    pub symbol_hash: u64,
}

impl FnSpan {
    #[inline]
    pub fn for_symbol(range: Span, symbol: &str) -> FnSpan {
        FnSpan { range, symbol_hash: symbol_hash(symbol) }
    }
}

/// One lint finding enriched with its enclosing function's historical runtime heat and effective level.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RankedFinding {
    pub finding_index: u32,
    pub rule: Rule,
    pub range: Span,
    pub heat: Heat,
    pub share_ppm: u32,
    pub level: EffectiveLevel,
}

/// Finds the innermost enclosing function span containing `pos`.
#[inline]
fn enclosing_fn(fn_spans: &[FnSpan], pos: u32) -> Option<FnSpan> {
    let mut best: Option<FnSpan> = None;
    for &sp in fn_spans {
        if sp.range.start <= pos && pos < sp.range.end {
            match best {
                Some(prev) if prev.range.len() <= sp.range.len() => {}
                _ => best = Some(sp),
            }
        }
    }
    best
}

/// Maps every finding in `findings` to its historical profile heat, applies `policy`, drops
/// suppressed findings, and returns them sorted by priority:
/// `(Error before Advice, heat DESC, is_perf DESC, share_ppm DESC, range.start ASC, index ASC)`.
pub fn rank_findings<'a>(
    findings: &Findings<'_>,
    base_levels: &Levels,
    fn_spans: &[FnSpan],
    hints: &PgoHints<'_>,
    policy: PgoPolicy,
    alloc: Alloc<'a>,
) -> AVec<'a, RankedFinding> {
    let mut out = AVec::with_capacity_in(findings.list.len(), alloc);
    for (idx, f) in findings.list.iter().enumerate() {
        let rule = f.rule();
        let base = base_levels.of(rule);
        if base == Level::Off {
            continue;
        }
        let HeatInfo { heat, share_ppm } = match enclosing_fn(fn_spans, f.range.start) {
            Some(sp) => hints.heat_of(sp.symbol_hash),
            None => HeatInfo { heat: Heat::Cold, share_ppm: 0 },
        };
        let perf = is_perf_rule(rule);
        let level = if perf && heat >= policy.promote_perf_at {
            EffectiveLevel::Error
        } else if perf && policy.suppress_cold_perf && heat == Heat::Cold && base != Level::Error {
            EffectiveLevel::Suppressed
        } else {
            match base {
                Level::Error => EffectiveLevel::Error,
                Level::Advice => EffectiveLevel::Advice,
                Level::Off => EffectiveLevel::Suppressed,
            }
        };
        if level == EffectiveLevel::Suppressed {
            continue;
        }
        out.push(RankedFinding {
            finding_index: idx as u32,
            rule,
            range: f.range,
            heat,
            share_ppm,
            level,
        });
    }
    out.sort_unstable_by(|a, b| {
        b.level
            .cmp(&a.level)
            .then_with(|| b.heat.cmp(&a.heat))
            .then_with(|| is_perf_rule(b.rule).cmp(&is_perf_rule(a.rule)))
            .then_with(|| b.share_ppm.cmp(&a.share_ppm))
            .then_with(|| a.range.start.cmp(&b.range.start))
            .then_with(|| a.finding_index.cmp(&b.finding_index))
    });
    out
}

/// Targeted automatic optimization suggestion triggered by combining static token patterns or
/// live JIT/LIR telemetry with PGO hot-function profiles.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum AdviceKind {
    /// `.clear()` inside a loop in a hot function: `bstd::mem::GenMap` gives O(1) clear by bumping an epoch.
    UseGenMapForHotClear = 1,
    /// `Vec::new` / `Vec::with_capacity` in a hot function: `bstd::mem::SmallVec<T, N, A>` avoids heap churn.
    UseSmallVecForHotLocal = 2,
    /// Bare `Atomic*` field in a struct without `CachePadded`: risks 128-byte false sharing on 128-core machines.
    UseCachePadded = 3,
    /// `.binary_search*` in a hot function: on uniform/digest keys, `bstd::mem::interp_search_by_prefix` is O(log log N).
    UseInterpSearch = 4,
    /// Hot loop accesses `< 40%` of a large `AoS` struct (`>= 32 B` stride): switch to `SoA` for unit-stride SIMD.
    UseSoaLayout = 5,
    /// Hot branch with `>= 99%` JIT skew: mark the cold path `#[cold]` or use `Conv::BlazeResult` carry-flag `?`.
    UseColdErrorPath = 6,
    /// Non-escaping heap allocation inside a hot loop: promote to stack, `SmallVec`, or task frame.
    PromoteHotHeapAlloc = 7,
    /// Hot async coroutine with large disjoint cross-suspend locals: split/graph-color frame slots.
    ShrinkAsyncFrame = 8,
    /// Hot integer division with a JIT-skewed or loop-invariant divisor: specialize into shift-multiply.
    SpecializeHotDivisor = 9,
    /// Hot indirect gather loop with high cache miss pressure: insert software prefetch (`blaze.prefetch.r1`).
    PrefetchHotGather = 10,
}

impl AdviceKind {
    pub const fn message(self) -> &'static str {
        match self {
            AdviceKind::UseGenMapForHotClear => {
                "`.clear()` inside a hot loop: consider `bstd::mem::GenMap` for O(1) epoch-bump clears"
            }
            AdviceKind::UseSmallVecForHotLocal => {
                "`Vec` constructed in a hot function: consider `bstd::mem::SmallVec<T, N, A>` for zero-heap inline storage"
            }
            AdviceKind::UseCachePadded => {
                "bare `Atomic*` field in a struct: wrap in `bstd::sync::CachePadded<T>` (128-byte aligned) to prevent false sharing across cores"
            }
            AdviceKind::UseInterpSearch => {
                "`.binary_search` in a hot function: for uniform hash/digest keys, `bstd::mem::interp_search_by_prefix` runs in O(log log N)"
            }
            AdviceKind::UseSoaLayout => {
                "hot loop touches <40% of a large struct's cache line (`AoS`): convert to Struct-of-Arrays (`SoA`) for unit-stride NEON/AVX SIMD"
            }
            AdviceKind::UseColdErrorPath => {
                "hot branch is >=99% predictable in JIT profile: mark cold path `#[cold]` or use `Conv::BlazeResult` carry-flag `?`"
            }
            AdviceKind::PromoteHotHeapAlloc => {
                "non-escaping heap allocation inside a hot loop: promote to stack, `bstd::mem::SmallVec`, or task frame"
            }
            AdviceKind::ShrinkAsyncFrame => {
                "hot async state machine holds reducible cross-suspend locals: split/graph-color state slots to shrink future size"
            }
            AdviceKind::SpecializeHotDivisor => {
                "hot integer division has a dominant runtime divisor in JIT profile: specialize into magic shift-multiply"
            }
            AdviceKind::PrefetchHotGather => {
                "hot indirect gather loop has high cache-miss pressure: insert software prefetch (`blaze.prefetch.r1`)"
            }
        }
    }
}

/// Live JIT / LIR-analysis observation fed into Blaze's profiler-driven linter.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum JitTelemetryKind {
    /// Hot strided loop over an Array-of-Structs (`stride_bytes >= 32`) touching only `accessed_bytes`.
    AosStrideLoop { stride_bytes: u32, accessed_bytes: u32, trip_count: u64 },
    /// Hot branch whose JIT profile shows extreme skew (`taken_ppm` in `0..=1_000_000`).
    SkewedBranch { taken_ppm: u32, executions: u64 },
    /// Non-escaping heap allocation detected inside a hot loop by escape analysis.
    NonEscapingHeapAlloc { alloc_bytes: u32, executions: u64 },
    /// Hot async coroutine whose raw frame (`frame_bytes`) can be graph-colored down to `colored_bytes`.
    OversizedAsyncFrame { frame_bytes: u32, colored_bytes: u32, polls: u64 },
    /// Hot integer division whose runtime divisor equals `dominant_divisor` for `dominant_ppm` of calls.
    SkewedDivisor { dominant_divisor: u64, dominant_ppm: u32, executions: u64 },
    /// Hot indirect gather loop (`data[idx[i]]`) with elevated L1/L2 miss rate (`miss_rate_ppm`).
    IndirectGatherLoop { miss_rate_ppm: u32, trip_count: u64 },
}

/// One source-located JIT telemetry record for an item/function.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct JitObservation {
    pub symbol_hash: u64,
    pub range: Span,
    pub kind: JitTelemetryKind,
}

impl JitObservation {
    #[inline]
    pub fn for_symbol(symbol: &str, range: Span, kind: JitTelemetryKind) -> JitObservation {
        JitObservation { symbol_hash: symbol_hash(symbol), range, kind }
    }
}

/// Actionable, impact-ranked performance lint derived from live JIT telemetry + PGO heat.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct JitAdvice {
    pub kind: AdviceKind,
    pub range: Span,
    pub heat: Heat,
    pub share_ppm: u32,
    /// Priority score combining PGO share, execution frequency, and wasted work.
    pub impact_score: u64,
    /// Estimated resource waste percentage (`0..=100`, e.g. cache-line waste for `AoS`, frame bloat for async).
    pub waste_pct: u8,
    /// Estimated speedup in tenths (`10 = 1.0x`, `40 = 4.0x`, `80 = 8.0x`).
    pub est_speedup_x10: u16,
}

/// Evaluates live JIT & LIR-analysis observations against PGO heat hints to produce deterministic,
/// impact-ranked performance lints (e.g., `AoS -> SoA`, cold error splitting, heap promotion,
/// async frame coloring, divisor specialization, and gather prefetching).
pub fn suggest_from_jit<'a>(
    observations: &[JitObservation],
    hints: &PgoHints<'_>,
    min_heat: Heat,
    alloc: Alloc<'a>,
) -> AVec<'a, JitAdvice> {
    let mut out = AVec::new_in(alloc);
    for &obs in observations {
        let HeatInfo { heat, share_ppm } = hints.heat_of(obs.symbol_hash);
        if heat < min_heat {
            continue;
        }
        let heat_mult = (u64::from(share_ppm)).max(1);
        let emitted = match obs.kind {
            JitTelemetryKind::AosStrideLoop { stride_bytes, accessed_bytes, trip_count } => {
                if stride_bytes >= 32
                    && accessed_bytes > 0
                    && accessed_bytes < stride_bytes
                    && u64::from(accessed_bytes) * 100 < u64::from(stride_bytes) * 40
                    && trip_count >= 128
                {
                    let wasted = stride_bytes - accessed_bytes;
                    let waste_pct = u8::try_from(((u64::from(wasted) * 100) / u64::from(stride_bytes)).min(99)).unwrap_or(99);
                    let est_speedup_x10 = u16::try_from(
                        ((u64::from(stride_bytes) * 10) / u64::from(accessed_bytes)).clamp(15, 80),
                    )
                    .unwrap_or(80);
                    let impact_score = heat_mult
                        .saturating_mul(trip_count.min(1_000_000))
                        .saturating_mul(u64::from(wasted));
                    Some((AdviceKind::UseSoaLayout, impact_score, waste_pct, est_speedup_x10))
                } else {
                    None
                }
            }
            JitTelemetryKind::SkewedBranch { taken_ppm, executions } => {
                let skewed = taken_ppm >= 990_000 || taken_ppm <= 10_000;
                if skewed && executions >= 1_000 {
                    let skew_pct = if taken_ppm >= 500_000 {
                        u8::try_from((taken_ppm / 10_000).min(100)).unwrap_or(100)
                    } else {
                        u8::try_from(((1_000_000u32.saturating_sub(taken_ppm)) / 10_000).min(100)).unwrap_or(100)
                    };
                    let impact_score = heat_mult.saturating_mul(executions.min(1_000_000));
                    Some((AdviceKind::UseColdErrorPath, impact_score, skew_pct, 15))
                } else {
                    None
                }
            }
            JitTelemetryKind::NonEscapingHeapAlloc { alloc_bytes, executions } => {
                if alloc_bytes > 0 && executions >= 100 {
                    let impact_score = heat_mult
                        .saturating_mul(executions.min(1_000_000))
                        .saturating_mul(u64::from(alloc_bytes).max(16));
                    Some((AdviceKind::PromoteHotHeapAlloc, impact_score, 100, 30))
                } else {
                    None
                }
            }
            JitTelemetryKind::OversizedAsyncFrame { frame_bytes, colored_bytes, polls } => {
                if frame_bytes >= 128
                    && colored_bytes < frame_bytes
                    && u64::from(colored_bytes) * 100 <= u64::from(frame_bytes) * 75
                    && polls >= 100
                {
                    let saved = frame_bytes - colored_bytes;
                    let waste_pct = u8::try_from(((u64::from(saved) * 100) / u64::from(frame_bytes)).min(99)).unwrap_or(99);
                    let est_speedup_x10 = u16::try_from(
                        ((u64::from(frame_bytes) * 10) / u64::from(colored_bytes.max(1))).clamp(12, 40),
                    )
                    .unwrap_or(40);
                    let impact_score = heat_mult
                        .saturating_mul(polls.min(1_000_000))
                        .saturating_mul(u64::from(saved));
                    Some((AdviceKind::ShrinkAsyncFrame, impact_score, waste_pct, est_speedup_x10))
                } else {
                    None
                }
            }
            JitTelemetryKind::SkewedDivisor { dominant_divisor, dominant_ppm, executions } => {
                if dominant_divisor > 1 && dominant_ppm >= 800_000 && executions >= 1_000 {
                    let dom_pct = u8::try_from((dominant_ppm / 10_000).min(100)).unwrap_or(100);
                    let impact_score = heat_mult.saturating_mul(executions.min(1_000_000));
                    Some((AdviceKind::SpecializeHotDivisor, impact_score, dom_pct, 35))
                } else {
                    None
                }
            }
            JitTelemetryKind::IndirectGatherLoop { miss_rate_ppm, trip_count } => {
                if miss_rate_ppm >= 50_000 && trip_count >= 1_000 {
                    let miss_pct = u8::try_from((miss_rate_ppm / 10_000).min(100)).unwrap_or(100);
                    let impact_score = heat_mult
                        .saturating_mul(trip_count.min(1_000_000))
                        .saturating_mul(u64::from(miss_rate_ppm) / 1_000);
                    Some((AdviceKind::PrefetchHotGather, impact_score, miss_pct, 25))
                } else {
                    None
                }
            }
        };
        if let Some((kind, impact_score, waste_pct, est_speedup_x10)) = emitted {
            out.push(JitAdvice {
                kind,
                range: obs.range,
                heat,
                share_ppm,
                impact_score,
                waste_pct,
                est_speedup_x10,
            });
        }
    }
    out.sort_unstable_by(|a, b| {
        b.heat
            .cmp(&a.heat)
            .then_with(|| b.impact_score.cmp(&a.impact_score))
            .then_with(|| b.share_ppm.cmp(&a.share_ppm))
            .then_with(|| a.range.start.cmp(&b.range.start))
            .then_with(|| a.kind.cmp(&b.kind))
    });
    out
}

/// One PGO-triggered automatic optimization recommendation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PgoAdvice {
    pub kind: AdviceKind,
    pub range: Span,
    pub heat: Heat,
    pub share_ppm: u32,
}

#[inline]
fn is_atomic_type(word: &str) -> bool {
    matches!(
        word,
        "AtomicBool"
            | "AtomicU8"
            | "AtomicU16"
            | "AtomicU32"
            | "AtomicU64"
            | "AtomicUsize"
            | "AtomicI8"
            | "AtomicI16"
            | "AtomicI32"
            | "AtomicI64"
            | "AtomicIsize"
            | "AtomicPtr"
    )
}

/// Single-pass token scanner that combines PGO hotness (`fn_spans` + `hints`) with lightweight
/// structural patterns to suggest Blaze's high-performance stdlib primitives automatically.
pub fn suggest_from_pgo<'a>(
    text: &str,
    fn_spans: &[FnSpan],
    hints: &PgoHints<'_>,
    min_heat: Heat,
    alloc: Alloc<'a>,
) -> AVec<'a, PgoAdvice> {
    let mut out = AVec::new_in(alloc);
    let mut pos = 0u32;
    let mut brace_depth = 0u32;
    let mut loop_depths = AVec::<u32>::new_in(alloc);
    let mut pending_loop = false;
    let mut in_struct = false;
    let mut struct_brace_depth = 0u32;
    let mut saw_struct_kw = false;

    // Small sliding window of previous non-trivia tokens (kind tag + byte range).
    let mut prev2_word: Option<&str> = None;
    let mut prev1_word: Option<&str> = None;
    let mut prev1_punct: u8 = 0;
    let mut prev2_punct: u8 = 0;
    let mut prev_was_cache_padded = false;

    let file_max_ppm = hints.max_share_ppm();
    let file_max_heat = Heat::from_ppm(file_max_ppm);

    for tok in tokenize(text, FrontmatterAllowed::No, Cpu::BASELINE) {
        let start = pos;
        let end = pos + tok.len;
        pos = end;

        match tok.kind {
            TokenKind::Whitespace | TokenKind::LineComment { .. } | TokenKind::BlockComment { .. } => {
                continue;
            }
            TokenKind::OpenBrace => {
                brace_depth += 1;
                if pending_loop {
                    loop_depths.push(brace_depth);
                    pending_loop = false;
                }
                if saw_struct_kw {
                    in_struct = true;
                    struct_brace_depth = brace_depth;
                    saw_struct_kw = false;
                }
                prev2_word = None;
                prev1_word = None;
                prev2_punct = 0;
                prev1_punct = b'{';
                prev_was_cache_padded = false;
            }
            TokenKind::CloseBrace => {
                if loop_depths.last().copied() == Some(brace_depth) {
                    loop_depths.pop();
                }
                if in_struct && brace_depth == struct_brace_depth {
                    in_struct = false;
                }
                brace_depth = brace_depth.saturating_sub(1);
                prev2_word = None;
                prev1_word = None;
                prev2_punct = 0;
                prev1_punct = b'}';
                prev_was_cache_padded = false;
            }
            TokenKind::Semi => {
                saw_struct_kw = false;
                pending_loop = false;
                prev2_word = None;
                prev1_word = None;
                prev2_punct = prev1_punct;
                prev1_punct = b';';
                prev_was_cache_padded = false;
            }
            TokenKind::Dot => {
                prev2_punct = prev1_punct;
                prev1_punct = b'.';
            }
            TokenKind::Colon => {
                prev2_punct = prev1_punct;
                prev1_punct = b':';
            }
            TokenKind::Lt => {
                prev2_punct = prev1_punct;
                prev1_punct = b'<';
            }
            TokenKind::OpenParen => {
                // Check `Vec::new(` or `Vec::with_capacity(` or `Vec::new_in(` or `.clear()` or `.binary_search*(`
                if let Some(w1) = prev1_word {
                    if let Some(fn_sp) = enclosing_fn(fn_spans, start) {
                        let HeatInfo { heat, share_ppm } = hints.heat_of(fn_sp.symbol_hash);
                        if heat >= min_heat {
                            if w1 == "clear" && prev1_punct == b'.' && !loop_depths.is_empty() {
                                out.push(PgoAdvice {
                                    kind: AdviceKind::UseGenMapForHotClear,
                                    range: Span::new(start.saturating_sub(w1.len() as u32), end),
                                    heat,
                                    share_ppm,
                                });
                            } else if matches!(w1, "new" | "with_capacity" | "new_in" | "with_capacity_in")
                                && prev2_word == Some("Vec")
                                && prev2_punct == b':'
                                && prev1_punct == b':'
                            {
                                out.push(PgoAdvice {
                                    kind: AdviceKind::UseSmallVecForHotLocal,
                                    range: Span::new(start.saturating_sub(w1.len() as u32), end),
                                    heat,
                                    share_ppm,
                                });
                            } else if (w1 == "binary_search"
                                || w1 == "binary_search_by"
                                || w1 == "binary_search_by_key")
                                && prev1_punct == b'.'
                            {
                                out.push(PgoAdvice {
                                    kind: AdviceKind::UseInterpSearch,
                                    range: Span::new(start.saturating_sub(w1.len() as u32), end),
                                    heat,
                                    share_ppm,
                                });
                            }
                        }
                    }
                }
                prev2_punct = prev1_punct;
                prev1_punct = b'(';
                prev_was_cache_padded = false;
            }
            TokenKind::Ident => {
                let word = &text[start as usize..end as usize];
                if matches!(word, "for" | "while" | "loop") {
                    pending_loop = true;
                } else if word == "struct" {
                    saw_struct_kw = true;
                } else if word == "CachePadded" {
                    prev_was_cache_padded = true;
                } else if in_struct
                    && brace_depth == struct_brace_depth
                    && is_atomic_type(word)
                    && !prev_was_cache_padded
                    && file_max_heat >= min_heat
                {
                    out.push(PgoAdvice {
                        kind: AdviceKind::UseCachePadded,
                        range: Span::new(start, end),
                        heat: file_max_heat,
                        share_ppm: file_max_ppm,
                    });
                } else if prev1_punct != b'<' {
                    prev_was_cache_padded = false;
                }
                prev2_word = prev1_word;
                prev1_word = Some(word);
                if prev1_punct != b'.' && prev1_punct != b':' {
                    prev2_punct = prev1_punct;
                    prev1_punct = 0;
                }
            }
            _ => {
                prev2_punct = prev1_punct;
                prev1_punct = 0;
                prev_was_cache_padded = false;
            }
        }
    }

    out.sort_unstable_by(|a, b| {
        b.heat
            .cmp(&a.heat)
            .then_with(|| b.share_ppm.cmp(&a.share_ppm))
            .then_with(|| a.range.start.cmp(&b.range.start))
            .then_with(|| a.kind.cmp(&b.kind))
    });
    out
}
