//! Whole-program all-crate bottom-up content-addressed ThinLTO for `blaze build --release`.
//!
//! Eliminates both the `Grain::Workspace`-only seeding restriction and the fixed `12,288`-body
//! BFS cap of `optimize_export_workspace_par_cached`:
//!
//! 1. **Zero-LIR-decode direct call graph**: extracts direct `SiteKind::Call` (and AArch64 tail
//!    `br x16`) edges across EVERY non-stub reached `RowKind::Instance` (`Workspace`, `Locked`
//!    third-party crates, and `Toolchain` `core`/`alloc`/`std`) from existing `HomeCodeStore`
//!    `CodeView` records, and runs deterministic iterative Tarjan SCC decomposition into
//!    reverse-topological (callees-first) wavefront levels.
//! 2. **Bottom-up Merkle SCC cache (`BLZOLT01` in `Namespace::LOCAL`)**: every SCC's key folds
//!    `(schema, producer, host machine, inline policy, size bounds, soft-float routines, SCC
//!    members' `(symbol, lir_key, is_workspace, persisted_pgo)`, sorted cross-SCC direct callees'
//!    `lto_key`)`. Unchanged `core`/`alloc`/`std`/third-party/workspace SCCs hit `BLZOLT01`
//!    directly with **zero** `BLZLIR02` decode, **zero** `simplify_body`, **zero** inlining,
//!    **zero** emission, and **zero** heap copy of emitted `CodeRecord`s!
//! 3. **Bounded streaming wavefront batches + caller-refcounted `InlineBody` frontier**: missed
//!    SCCs in each wavefront level are processed in bounded batches (`<= 4096` bodies), fetching
//!    missing `BuiltLir`s in parallel via [`ExportLirBuilder`], moving `Func` without cloning,
//!    inlining against self-contained lower-SCC `InlineBody`s, emitting with host-ISA SOTA +
//!    bottom-up IPRA, writing `BLZOLT01` straight to `Namespace::LOCAL`, and evicting finished
//!    `InlineBody`s once their callers complete (with a 128 MiB cap and lazy reload from mmap).

use blaze_code::datum::DatumView;
use blaze_code::record::{CodeView, SiteKind, Space, Symbol, Target as CodeTarget};
use blaze_emit::SotaRecordAbiOpts;
use blaze_hash::{FixedState, Map, MapOpts, Set, SetOpts};
use blaze_lir::{
    Block, BlockKind, Op,
    body::{Bodies, BodyId, DatumId, Target},
    func::{FnAttrs, Inline, RefId, RefKind, RefRec},
    lto::{LtoRunner, ModuleBody, OptimizedBody, lto_optimize_module_par, tarjan_sccs},
    pgo::{BodyWeight, PgoSiteTable, SiteEntry},
    verify,
};
use blaze_own::{AVec, Alloc, SharedAlloc, Vec};
use blaze_tier::{Arch, CallClobbers, Machine, RegSet};
use blaze_typeck::ItemKey;

use super::{
    BodyHeat, BuiltCallee, BuiltForeignStatic, BuiltLir, ByteReader, DeclaredRow, ExportLtoCache,
    ExportLtoError, ExportLtoOpts, ExportLtoTimes, HotCandidate, HotTier, LtoBodyOut, Maker,
    ParLtoErr, RecordsProgram, SymIndexMap, Typing,
    compute_loop_heat, decode_built_lir, encode_built_lir, find_body_idx,
    has_normal_return, is_zero_call_leaf, lower_ieee_libm_calls, push_len, push_u32, push_u64,
    push_u128, push_vtable_datum, put_at, should_expand_non_seed, should_keep_non_seed,
    EXPORT_PGO_PROFILE_DOMAIN, HOT_CALLER_HEAT_DIV, HOT_LEAF_THRESHOLD, MAX_IMPORT_HOPS,
    MAX_LOOP_DEPTH_EXP, PGO_DECAY_DEN, PGO_DECAY_NUM, STATIC_STRAIGHT_LINE_WEIGHT,
};
use crate::body_ids::{Grant, Walk};
use crate::code_records::{Callee, CodeRecordError, InstanceNames, emit_code_record_for_machine_with_abi};
use crate::front2::builtins_shim::{BoundRef, Routines};
use crate::run::make_one::func_needs_soft_float;
use crate::symbols::{static_symbol, tls_symbol};

/// Magic + format version for bottom-up Merkle-keyed per-SCC post-LTO records (`Namespace::LOCAL`).
const LTO_SCC_CACHE_MAGIC: &[u8; 8] = b"BLZOLT01";
/// Hash domain for deterministic bottom-up Tarjan SCC LTO keys (Rule 21, Rule 25).
const EXPORT_LTO_SCC_KEY_DOMAIN: &str = "blaze v2 export lto scc 1";
/// Hash domain for deriving one SCC member's `BLZOLT01` store key from its SCC key.
const EXPORT_LTO_MEMBER_KEY_DOMAIN: &str = "blaze v2 export lto member 1";

/// Maximum bodies processed together in one wavefront batch so peak RAM stays flat even on
/// 780k-function binaries (Rule 18, Rule 50).
const WAVEFRONT_BATCH_BODIES: usize = 4096;
/// Maximum in-memory bytes of cached `InlineBody`s held in the active wavefront frontier before
/// evicting already-persisted entries (which reload lazily from `BLZOLT01` if a higher caller needs
/// them).
const MAX_FRONTIER_INLINE_BYTES: usize = 128 << 20;

const HEAT_FLAG_ELIGIBLE: u8 = 1 << 0;
const HEAT_FLAG_CAN_CALLER_BONUS: u8 = 1 << 1;
const HEAT_FLAG_IS_HOT_LEAF: u8 = 1 << 2;
const HEAT_FLAG_HAS_OWN_HEAT: u8 = 1 << 3;
const HEAT_FLAG_HAS_INLINE_BODY: u8 = 1 << 4;

/// Demand-driven batch `BuiltLir` builder seam for whole-program export LTO (Rule 14, Rule 78).
///
/// Implementations may lower missing rows sequentially (via [`MakerLirBuilder`]) or in parallel
/// across epoch-resetting worker threads (`CliExportLirBuilder`), returning results in exact
/// `rows` order (Rule 25).
pub trait ExportLirBuilder {
    fn build_lir_batch(
        &mut self,
        rows: &[usize],
        machine: Machine,
        shared: SharedAlloc<'static>,
        alloc: Alloc<'_>,
    ) -> Result<
        Vec<(BuiltLir<SharedAlloc<'static>>, Vec<DeclaredRow, SharedAlloc<'static>>), SharedAlloc<'static>>,
        ExportLtoError,
    >;

    fn prepare_routines(&self) -> Result<Option<BoundRef<'_>>, CodeRecordError>;
}

/// Sequential [`ExportLirBuilder`] backed directly by a single-threaded [`Maker`].
pub struct MakerLirBuilder<'w, 'm, 'n, G: Grant, T: Typing<RecordsProgram<'n>>, F> {
    walk: &'w Walk<'w>,
    maker: &'m mut Maker<'n, G, T>,
    grant_for: F,
}

impl<'w, 'm, 'n, G: Grant, T: Typing<RecordsProgram<'n>>, F: FnMut(ItemKey) -> Option<G>>
    MakerLirBuilder<'w, 'm, 'n, G, T, F>
{
    pub fn new(walk: &'w Walk<'w>, maker: &'m mut Maker<'n, G, T>, grant_for: F) -> Self {
        Self { walk, maker, grant_for }
    }
}

impl<'w, 'm, 'n, G: Grant, T: Typing<RecordsProgram<'n>>, F: FnMut(ItemKey) -> Option<G>> ExportLirBuilder
    for MakerLirBuilder<'w, 'm, 'n, G, T, F>
{
    fn build_lir_batch(
        &mut self,
        rows: &[usize],
        machine: Machine,
        shared: SharedAlloc<'static>,
        alloc: Alloc<'_>,
    ) -> Result<
        Vec<(BuiltLir<SharedAlloc<'static>>, Vec<DeclaredRow, SharedAlloc<'static>>), SharedAlloc<'static>>,
        ExportLtoError,
    > {
        let mut out = Vec::with_capacity_in(rows.len(), shared);
        let mut lto_decl = crate::body_ids::DeclaredList::new_in(alloc);
        for &row in rows {
            let tabled = self.walk.tabled(row);
            let symbol = self.walk.symbol(row);
            let name = self.walk.name(row);
            let Some(grant) = (self.grant_for)(tabled) else {
                return Err(ExportLtoError::NotRunnable { symbol: symbol.0, tabled });
            };
            let enclosers = crate::body_ids::local_enclosers_of(self.walk, row, tabled);
            if !enclosers.locals().is_empty() {
                self.maker.prepare_local(enclosers.locals(), enclosers.enclosers());
            }
            lto_decl.clear();
            let mut built = self
                .maker
                .build_lir_with_reach_in(&grant, tabled, name, Some(&mut lto_decl), shared)
                .map_err(|error| ExportLtoError::BuildLir { symbol: symbol.0, error })?;
            let mut declared: Vec<DeclaredRow, SharedAlloc<'static>> =
                Vec::with_capacity_in(lto_decl.len(), shared);
            for idx in 0..lto_decl.len() {
                let (decl_tabled, decl_name) = lto_decl.row(idx);
                let mut copied = Vec::with_capacity_in(decl_name.len(), shared);
                copied.extend_from_slice(decl_name);
                declared.push((decl_tabled, copied));
            }
            if lower_ieee_libm_calls(&mut built, machine, alloc) {
                verify(built.func.view())
                    .map_err(|error| ExportLtoError::Verify { symbol: symbol.0, error })?;
            }
            out.push((built, declared));
        }
        Ok(out)
    }

    fn prepare_routines(&self) -> Result<Option<BoundRef<'_>>, CodeRecordError> {
        self.maker.prepare_routines()
    }
}

/// Checks whether the 4-byte AArch64 instruction at `off` in `code` is `br x16` (`0xd61f_0200`),
/// identifying GOT-slot direct tail calls (`SiteKind::Load` followed by `br x16`).
#[inline]
fn is_arm64_br_x16(code: &[u8], off: u32) -> bool {
    let Ok(at) = usize::try_from(off) else {
        return false;
    };
    let Some(bytes) = code.get(at..at.saturating_add(4)) else {
        return false;
    };
    let Ok(arr) = <[u8; 4]>::try_from(bytes) else {
        return false;
    };
    u32::from_le_bytes(arr) == 0xd61f_0200
}

/// Extracts sorted, deduplicated direct-call target node indices from a sealed `CodeRecord` slice
/// without decoding `BuiltLir` or introducing false vtable/`FuncAddr` cycles.
pub fn extract_direct_calls(
    code_record_bytes: &[u8],
    arch: Arch,
    node_by_sym: &Map<[u8; 32], usize, FixedState, SharedAlloc<'static>>,
    out: &mut AVec<'_, u32>,
) {
    out.clear();
    let Ok(view) = CodeView::read(code_record_bytes) else {
        return;
    };
    let code = view.code();
    let targets_len = view.targets();
    for i in 0..view.sites() {
        let site = view.site(i);
        let is_direct_call = match site.kind {
            SiteKind::Call => true,
            SiteKind::Load => arch != Arch::Arm64 || is_arm64_br_x16(code, site.at.saturating_add(8)),
            SiteKind::Relro => false,
        };
        if !is_direct_call {
            continue;
        }
        let Ok(t_idx) = usize::try_from(site.target) else {
            continue;
        };
        if t_idx >= targets_len {
            continue;
        }
        let target = view.target(t_idx);
        if target.space != Space::Func {
            continue;
        }
        if let Some(&node_idx) = node_by_sym.get(&target.symbol.0)
            && let Ok(node_u32) = u32::try_from(node_idx)
        {
            out.push(node_u32);
        }
    }
    out.sort_unstable();
    out.dedup();
}

/// Outcome of one optimized row returned by [`optimize_export_all_par_cached`].
pub enum LtoRowResult {
    /// Post-LTO `BLZOLT01` record is persisted in `Namespace::LOCAL` under `cache_key`; the
    /// caller borrows its sealed `CodeRecord` and `vtable_data` zero-copy at link time (Rule 51).
    Cached {
        row: usize,
        symbol: Symbol,
        cache_key: [u8; 32],
        inlined_bodies: usize,
    },
    /// Unkeyed row whose post-LTO record could not be cached; kept inline in heap memory.
    Inline(LtoBodyOut),
}

/// Output of [`optimize_export_all_par_cached`].
pub struct ExportAllLtoOut<'a> {
    /// Optimized rows replacing their baseline `HomeCodeStore` entries in the exported binary.
    pub rows: Vec<LtoRowResult, SharedAlloc<'static>>,
    /// Any `Space::Func` targets referenced by post-LTO records or vtables not yet in `Walk`.
    pub extra_walk_instances: Vec<(usize, ItemKey, Vec<u8, SharedAlloc<'static>>), SharedAlloc<'static>>,
    /// Any `Space::Static` targets referenced by post-LTO records or vtables not yet in `Walk`.
    pub extra_walk_statics: Vec<ItemKey, SharedAlloc<'static>>,
    /// Any `Space::Tls` targets referenced by post-LTO records not yet in `Walk`.
    pub extra_walk_tls: Vec<ItemKey, SharedAlloc<'static>>,
    /// Hot module symbols ranked hottest-first for `blaze_export::Profile::hot`.
    pub hot_symbols: AVec<'a, Symbol>,
    /// Deterministic BLAKE3 digest for `blaze_export::Profile::digest`.
    pub profile_digest: [u8; 32],
    /// Total eligible functions inspected across all crates.
    pub total_eligible: usize,
    /// Total non-skipped functions emitted through LTO.
    pub optimized_bodies: usize,
    /// Total callee bodies spliced across all optimized functions.
    pub total_inlined: usize,
    /// Fine-grained sub-phase timings and cache hit/miss counts.
    pub times: ExportLtoTimes,
}

/// Status byte of a `BLZOLT01` per-SCC post-LTO record.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CachedLtoStatus {
    /// Cold, noreturn, or oversized function kept at its baseline `HomeCodeStore` record.
    Skipped,
    /// Function optimized and emitted into a host-ISA sealed `CodeRecord`.
    Optimized,
}

/// Zero-copy borrowed view over a `BLZOLT01` record's header and payload slices (Rule 51).
pub struct CachedLtoView<'b> {
    pub status: CachedLtoStatus,
    pub clobbers: CallClobbers,
    pub inlined_bodies: u32,
    pub eligible: bool,
    pub can_receive_caller_bonus: bool,
    pub is_hot_leaf: bool,
    pub has_own_heat: bool,
    pub has_inline_body: bool,
    pub own_score: u64,
    pub pgo_body_weight: u64,
    pub record: &'b [u8],
    vtable_section: &'b [u8],
    vtable_count: usize,
    extra_declared_section: &'b [u8],
    extra_declared_count: usize,
    target_func_decls_section: &'b [u8],
    target_func_decls_count: usize,
    target_statics_section: &'b [u8],
    target_statics_count: usize,
    target_tls_section: &'b [u8],
    target_tls_count: usize,
    hot_callees_section: &'b [u8],
    hot_callees_count: usize,
    inline_body_bytes: Option<&'b [u8]>,
}

impl<'b> CachedLtoView<'b> {
    /// Iterates every `(vtable_symbol, datum_bytes)` slice carried by this record without allocating.
    pub fn each_vtable<E>(&self, mut f: impl FnMut(Symbol, &'b [u8]) -> Result<(), E>) -> Result<(), E> {
        let mut r = ByteReader::new(self.vtable_section);
        for _ in 0..self.vtable_count {
            let Some(sym) = r.sym() else { break };
            let Some(len) = r.len() else { break };
            let Some(bytes) = r.take(len) else { break };
            f(sym, bytes)?;
        }
        Ok(())
    }

    /// Iterates every soft-float `extra_declared` `(ItemKey, name_bytes)` pair without allocating.
    pub fn each_extra_declared(&self, mut f: impl FnMut(ItemKey, &'b [u8])) {
        let mut r = ByteReader::new(self.extra_declared_section);
        for _ in 0..self.extra_declared_count {
            let Some(item) = r.u128() else { break };
            let Some(len) = r.len() else { break };
            let Some(name) = r.take(len) else { break };
            f(ItemKey(item), name);
        }
    }

    /// Iterates every `Space::Func` declaration `(ItemKey, name_bytes)` needed by `record`/`vtable_data`.
    pub fn each_target_func_decl(&self, mut f: impl FnMut(ItemKey, &'b [u8])) {
        let mut r = ByteReader::new(self.target_func_decls_section);
        for _ in 0..self.target_func_decls_count {
            let Some(item) = r.u128() else { break };
            let Some(len) = r.len() else { break };
            let Some(name) = r.take(len) else { break };
            f(ItemKey(item), name);
        }
    }

    /// Iterates every `Space::Static` `ItemKey` referenced by `record`/`vtable_data`.
    pub fn each_target_static(&self, mut f: impl FnMut(ItemKey)) {
        let mut r = ByteReader::new(self.target_statics_section);
        for _ in 0..self.target_statics_count {
            let Some(item) = r.u128() else { break };
            f(ItemKey(item));
        }
    }

    /// Iterates every `Space::Tls` `ItemKey` referenced by `record`.
    pub fn each_target_tls(&self, mut f: impl FnMut(ItemKey)) {
        let mut r = ByteReader::new(self.target_tls_section);
        for _ in 0..self.target_tls_count {
            let Some(item) = r.u128() else { break };
            f(ItemKey(item));
        }
    }

    /// Iterates every post-LTO direct non-cold callee symbol recorded for 1-hop hot-caller ranking.
    pub fn each_hot_callee(&self, mut f: impl FnMut(Symbol)) {
        let mut r = ByteReader::new(self.hot_callees_section);
        for _ in 0..self.hot_callees_count {
            let Some(sym) = r.sym() else { break };
            f(sym);
        }
    }
}

fn skip_len_prefixed_sym_section<'b>(r: &mut ByteReader<'b>, count: usize) -> Option<&'b [u8]> {
    let start = r.at;
    for _ in 0..count {
        let _ = r.sym()?;
        let len = r.len()?;
        let _ = r.take(len)?;
    }
    r.bytes.get(start..r.at)
}

fn skip_len_prefixed_item_section<'b>(r: &mut ByteReader<'b>, count: usize) -> Option<&'b [u8]> {
    let start = r.at;
    for _ in 0..count {
        let _ = r.u128()?;
        let len = r.len()?;
        let _ = r.take(len)?;
    }
    r.bytes.get(start..r.at)
}

/// Zero-copy validates and decodes a `BLZOLT01` record into a [`CachedLtoView`].
/// Returns `None` on any magic mismatch, truncation, or trailing garbage (Rule 3).
pub fn decode_lto_scc_header(bytes: &[u8]) -> Option<CachedLtoView<'_>> {
    let mut r = ByteReader::new(bytes);
    if r.take(8)? != LTO_SCC_CACHE_MAGIC {
        return None;
    }
    let status = match r.u8()? {
        0 => CachedLtoStatus::Skipped,
        1 => CachedLtoStatus::Optimized,
        _ => return None,
    };
    let clobbers = CallClobbers { clobbered: RegSet(r.u64()?) };
    let inlined_bodies = r.u32()?;
    let heat_flags = r.u8()?;
    let own_score = r.u64()?;
    let pgo_body_weight = r.u64()?;

    let eligible = (heat_flags & HEAT_FLAG_ELIGIBLE) != 0;
    let can_receive_caller_bonus = (heat_flags & HEAT_FLAG_CAN_CALLER_BONUS) != 0;
    let is_hot_leaf = (heat_flags & HEAT_FLAG_IS_HOT_LEAF) != 0;
    let has_own_heat = (heat_flags & HEAT_FLAG_HAS_OWN_HEAT) != 0;
    let has_inline_body = (heat_flags & HEAT_FLAG_HAS_INLINE_BODY) != 0;

    if status == CachedLtoStatus::Skipped {
        if !r.at_end() || has_inline_body {
            return None;
        }
        return Some(CachedLtoView {
            status,
            clobbers,
            inlined_bodies,
            eligible,
            can_receive_caller_bonus,
            is_hot_leaf,
            has_own_heat,
            has_inline_body: false,
            own_score,
            pgo_body_weight,
            record: &[],
            vtable_section: &[],
            vtable_count: 0,
            extra_declared_section: &[],
            extra_declared_count: 0,
            target_func_decls_section: &[],
            target_func_decls_count: 0,
            target_statics_section: &[],
            target_statics_count: 0,
            target_tls_section: &[],
            target_tls_count: 0,
            hot_callees_section: &[],
            hot_callees_count: 0,
            inline_body_bytes: None,
        });
    }

    let rec_len = r.len()?;
    let record = r.take(rec_len)?;

    let vtable_count = r.len()?;
    let vtable_section = skip_len_prefixed_sym_section(&mut r, vtable_count)?;

    let extra_declared_count = r.len()?;
    let extra_declared_section = skip_len_prefixed_item_section(&mut r, extra_declared_count)?;

    let target_func_decls_count = r.len()?;
    let target_func_decls_section = skip_len_prefixed_item_section(&mut r, target_func_decls_count)?;

    let target_statics_count = r.len()?;
    let target_statics_section = r.take(target_statics_count.checked_mul(16)?)?;

    let target_tls_count = r.len()?;
    let target_tls_section = r.take(target_tls_count.checked_mul(16)?)?;

    let hot_callees_count = r.len()?;
    let hot_callees_section = r.take(hot_callees_count.checked_mul(32)?)?;

    let inline_len = r.len()?;
    let inline_body_bytes = if has_inline_body {
        if inline_len == 0 {
            return None;
        }
        Some(r.take(inline_len)?)
    } else {
        if inline_len != 0 {
            return None;
        }
        None
    };

    if !r.at_end() {
        return None;
    }

    Some(CachedLtoView {
        status,
        clobbers,
        inlined_bodies,
        eligible,
        can_receive_caller_bonus,
        is_hot_leaf,
        has_own_heat,
        has_inline_body,
        own_score,
        pgo_body_weight,
        record,
        vtable_section,
        vtable_count,
        extra_declared_section,
        extra_declared_count,
        target_func_decls_section,
        target_func_decls_count,
        target_statics_section,
        target_statics_count,
        target_tls_section,
        target_tls_count,
        hot_callees_section,
        hot_callees_count,
        inline_body_bytes,
    })
}

/// Self-contained post-LTO inlineable body stored in the active wavefront frontier and optionally
/// embedded in `BLZOLT01` for incremental cache-hit inlining. All `imported_refs` are already
/// flattened into its own reference tables, so callers resolve imports in 1 hop with zero
/// transitive chains across dropped batches.
struct InlineBody {
    func: blaze_lir::Func<SharedAlloc<'static>>,
    funcs: Vec<BuiltCallee, SharedAlloc<'static>>,
    statics: Vec<ItemKey, SharedAlloc<'static>>,
    c_statics: Vec<BuiltForeignStatic, SharedAlloc<'static>>,
    vtables: Vec<Symbol, SharedAlloc<'static>>,
    vtable_data: Vec<(Symbol, Vec<u8, SharedAlloc<'static>>), SharedAlloc<'static>>,
    tls: Vec<ItemKey, SharedAlloc<'static>>,
    c_text: Vec<u8, SharedAlloc<'static>>,
    declared: Vec<DeclaredRow, SharedAlloc<'static>>,
    approx_bytes: usize,
}

impl InlineBody {
    fn callee_at(&self, place: u32) -> Option<Callee<'_>> {
        let idx = usize::try_from(place).ok()?;
        match *self.funcs.get(idx)? {
            BuiltCallee::Symbol(sym) => Some(Callee::Symbol(sym)),
            BuiltCallee::C { start, len } => self.c_str(start, len).map(Callee::C),
        }
    }

    fn c_str(&self, start: u32, len: u32) -> Option<&str> {
        let s = usize::try_from(start).ok()?;
        let l = usize::try_from(len).ok()?;
        let bytes = self.c_text.get(s..s.checked_add(l)?)?;
        core::str::from_utf8(bytes).ok()
    }

    fn foreign_static(&self, item: ItemKey) -> Option<(&str, bool)> {
        let cs = self.c_statics.iter().find(|cs| cs.item == item)?;
        let name = self.c_str(cs.start, cs.len)?;
        Some((name, cs.weak))
    }

    fn estimate_bytes(&self) -> usize {
        self.func
            .ops
            .len()
            .saturating_mul(16)
            .saturating_add(self.func.extra.len().saturating_mul(4))
            .saturating_add(self.funcs.len().saturating_mul(36))
            .saturating_add(self.vtable_data.iter().map(|(_, b)| b.len() + 40).sum::<usize>())
            .saturating_add(self.declared.iter().map(|(_, n)| n.len() + 32).sum::<usize>())
            .saturating_add(self.c_text.len())
            .saturating_add(256)
    }
}

fn encode_inline_body(body: &InlineBody, alloc: SharedAlloc<'static>) -> Option<Vec<u8, SharedAlloc<'static>>> {
    let func_bytes = blaze_lir::persist::to_bytes_in(&body.func.view(), alloc);
    let est = 64usize
        .saturating_add(func_bytes.len())
        .saturating_add(body.funcs.len().saturating_mul(33))
        .saturating_add(body.statics.len().saturating_mul(16))
        .saturating_add(body.c_statics.len().saturating_mul(25))
        .saturating_add(body.vtables.len().saturating_mul(32))
        .saturating_add(body.tls.len().saturating_mul(16))
        .saturating_add(body.c_text.len())
        .saturating_add(body.declared.len().saturating_mul(48));
    let mut out: Vec<u8, SharedAlloc<'static>> = Vec::with_capacity_in(est, alloc);
    push_len(&mut out, func_bytes.len())?;
    out.extend_from_slice(func_bytes.as_slice());

    push_len(&mut out, body.funcs.len())?;
    for f in body.funcs.iter() {
        match *f {
            BuiltCallee::Symbol(sym) => {
                out.push(0);
                out.extend_from_slice(&sym.0);
            }
            BuiltCallee::C { start, len } => {
                out.push(1);
                push_u32(&mut out, start);
                push_u32(&mut out, len);
            }
        }
    }
    push_len(&mut out, body.statics.len())?;
    for &item in body.statics.iter() {
        push_u128(&mut out, item.0);
    }
    push_len(&mut out, body.c_statics.len())?;
    for cs in body.c_statics.iter() {
        push_u128(&mut out, cs.item.0);
        push_u32(&mut out, cs.start);
        push_u32(&mut out, cs.len);
        out.push(u8::from(cs.weak));
    }
    push_len(&mut out, body.vtables.len())?;
    for &vsym in body.vtables.iter() {
        out.extend_from_slice(&vsym.0);
    }
    push_len(&mut out, body.vtable_data.len())?;
    for (vsym, bytes) in body.vtable_data.iter() {
        out.extend_from_slice(&vsym.0);
        push_len(&mut out, bytes.len())?;
        out.extend_from_slice(bytes.as_slice());
    }
    push_len(&mut out, body.tls.len())?;
    for &item in body.tls.iter() {
        push_u128(&mut out, item.0);
    }
    push_len(&mut out, body.c_text.len())?;
    out.extend_from_slice(body.c_text.as_slice());
    push_len(&mut out, body.declared.len())?;
    for (item, name) in body.declared.iter() {
        push_u128(&mut out, item.0);
        push_len(&mut out, name.len())?;
        out.extend_from_slice(name.as_slice());
    }
    Some(out)
}

fn decode_inline_body(bytes: &[u8], alloc: SharedAlloc<'static>) -> Option<InlineBody> {
    let mut r = ByteReader::new(bytes);
    let func_len = r.len()?;
    let func_slice = r.take(func_len)?;
    // Copy into a 16-byte-aligned `u128` backing buffer so `blaze_format::open_as` always sees
    // `blaze_format::ALIGN` alignment regardless of `inline_body_bytes` offset in `BLZOLT01`.
    let words = func_len.div_ceil(16);
    let mut aligned: Vec<u128, SharedAlloc<'static>> = Vec::with_capacity_in(words, alloc);
    aligned.resize(words, 0u128);
    // Safe byte copy into aligned `u128` storage without `unsafe`: pack 16-byte chunks via LE.
    for (wi, chunk) in func_slice.chunks(16).enumerate() {
        let mut buf = [0u8; 16];
        if let Some(dst) = buf.get_mut(..chunk.len()) {
            dst.copy_from_slice(chunk);
        }
        if let Some(slot) = aligned.get_mut(wi) {
            *slot = u128::from_le_bytes(buf);
        }
    }
    let func = {
        let aligned_bytes = aligned_u128_as_bytes(aligned.as_slice(), func_len)?;
        blaze_lir::persist::read_to_func_in(aligned_bytes, alloc).ok()?
    };

    let funcs_len = r.len()?;
    let mut funcs: Vec<BuiltCallee, SharedAlloc<'static>> = Vec::with_capacity_in(funcs_len, alloc);
    for _ in 0..funcs_len {
        match r.u8()? {
            0 => funcs.push(BuiltCallee::Symbol(r.sym()?)),
            1 => {
                let start = r.u32()?;
                let len = r.u32()?;
                funcs.push(BuiltCallee::C { start, len });
            }
            _ => return None,
        }
    }
    let statics_len = r.len()?;
    let mut statics: Vec<ItemKey, SharedAlloc<'static>> = Vec::with_capacity_in(statics_len, alloc);
    for _ in 0..statics_len {
        statics.push(ItemKey(r.u128()?));
    }
    let c_statics_len = r.len()?;
    let mut c_statics: Vec<BuiltForeignStatic, SharedAlloc<'static>> =
        Vec::with_capacity_in(c_statics_len, alloc);
    for _ in 0..c_statics_len {
        let item = ItemKey(r.u128()?);
        let start = r.u32()?;
        let len = r.u32()?;
        let weak = match r.u8()? {
            0 => false,
            1 => true,
            _ => return None,
        };
        c_statics.push(BuiltForeignStatic { item, start, len, weak });
    }
    let vtables_len = r.len()?;
    let mut vtables: Vec<Symbol, SharedAlloc<'static>> = Vec::with_capacity_in(vtables_len, alloc);
    for _ in 0..vtables_len {
        vtables.push(r.sym()?);
    }
    let vtable_data_len = r.len()?;
    let mut vtable_data: Vec<(Symbol, Vec<u8, SharedAlloc<'static>>), SharedAlloc<'static>> =
        Vec::with_capacity_in(vtable_data_len, alloc);
    for _ in 0..vtable_data_len {
        let vsym = r.sym()?;
        let dlen = r.len()?;
        let dbytes = r.take(dlen)?;
        let mut copy: Vec<u8, SharedAlloc<'static>> = Vec::with_capacity_in(dlen, alloc);
        copy.extend_from_slice(dbytes);
        vtable_data.push((vsym, copy));
    }
    let tls_len = r.len()?;
    let mut tls: Vec<ItemKey, SharedAlloc<'static>> = Vec::with_capacity_in(tls_len, alloc);
    for _ in 0..tls_len {
        tls.push(ItemKey(r.u128()?));
    }
    let c_text_len = r.len()?;
    let c_text_slice = r.take(c_text_len)?;
    let mut c_text: Vec<u8, SharedAlloc<'static>> = Vec::with_capacity_in(c_text_len, alloc);
    c_text.extend_from_slice(c_text_slice);

    let declared_len = r.len()?;
    let mut declared: Vec<DeclaredRow, SharedAlloc<'static>> = Vec::with_capacity_in(declared_len, alloc);
    for _ in 0..declared_len {
        let item = ItemKey(r.u128()?);
        let nlen = r.len()?;
        let nslice = r.take(nlen)?;
        let mut name: Vec<u8, SharedAlloc<'static>> = Vec::with_capacity_in(nlen, alloc);
        name.extend_from_slice(nslice);
        declared.push((item, name));
    }
    if !r.at_end() {
        return None;
    }

    let mut ib = InlineBody {
        func,
        funcs,
        statics,
        c_statics,
        vtables,
        vtable_data,
        tls,
        c_text,
        declared,
        approx_bytes: 0,
    };
    ib.approx_bytes = ib.estimate_bytes();
    Some(ib)
}

#[inline]
fn aligned_u128_as_bytes(words: &[u128], byte_len: usize) -> Option<&[u8]> {
    if byte_len > words.len().checked_mul(16)? {
        return None;
    }
    // SAFETY: `words` is a valid initialized `[u128]` slice, `u8` has alignment 1 and no invalid
    // bit patterns, and `byte_len <= words.len() * 16`.
    let raw = unsafe { core::slice::from_raw_parts(words.as_ptr().cast::<u8>(), byte_len) };
    Some(raw)
}

fn encode_lto_scc_skipped(alloc: SharedAlloc<'static>) -> Vec<u8, SharedAlloc<'static>> {
    let mut out: Vec<u8, SharedAlloc<'static>> = Vec::with_capacity_in(40, alloc);
    out.extend_from_slice(LTO_SCC_CACHE_MAGIC);
    out.push(0); // Skipped
    push_u64(&mut out, CallClobbers::C_ABI_ARM64.clobbered.0);
    push_u32(&mut out, 0); // inlined_bodies
    out.push(0); // heat_flags
    push_u64(&mut out, 0); // own_score
    push_u64(&mut out, 0); // pgo_body_weight
    out
}

#[expect(clippy::too_many_arguments, reason = "encodes every field of a BLZOLT01 post-LTO record directly without intermediate copies")]
fn encode_lto_scc_optimized(
    clobbers: CallClobbers,
    inlined_bodies: u32,
    heat: BodyHeat,
    pgo_body_weight: u64,
    record: &[u8],
    vtable_data: &[(Symbol, Vec<u8, SharedAlloc<'static>>)],
    extra_declared: &[(ItemKey, Vec<u8, SharedAlloc<'static>>)],
    target_func_decls: &[DeclaredRow],
    target_statics: &[ItemKey],
    target_tls: &[ItemKey],
    hot_callees: &[Symbol],
    inline_body: Option<&InlineBody>,
    alloc: SharedAlloc<'static>,
) -> Option<Vec<u8, SharedAlloc<'static>>> {
    let inline_bytes = match inline_body {
        Some(ib) => Some(encode_inline_body(ib, alloc)?),
        None => None,
    };
    let mut heat_flags = 0u8;
    if heat.eligible {
        heat_flags |= HEAT_FLAG_ELIGIBLE;
    }
    if heat.can_receive_caller_bonus {
        heat_flags |= HEAT_FLAG_CAN_CALLER_BONUS;
    }
    if heat.is_hot_leaf {
        heat_flags |= HEAT_FLAG_IS_HOT_LEAF;
    }
    if heat.has_own_heat {
        heat_flags |= HEAT_FLAG_HAS_OWN_HEAT;
    }
    if inline_bytes.is_some() {
        heat_flags |= HEAT_FLAG_HAS_INLINE_BODY;
    }

    let est = 64usize
        .saturating_add(record.len())
        .saturating_add(vtable_data.iter().map(|(_, b)| b.len() + 36).sum::<usize>())
        .saturating_add(extra_declared.iter().map(|(_, n)| n.len() + 20).sum::<usize>())
        .saturating_add(target_func_decls.iter().map(|(_, n)| n.len() + 20).sum::<usize>())
        .saturating_add(target_statics.len().saturating_mul(16))
        .saturating_add(target_tls.len().saturating_mul(16))
        .saturating_add(hot_callees.len().saturating_mul(32))
        .saturating_add(inline_bytes.as_ref().map_or(0, Vec::len));
    let mut out: Vec<u8, SharedAlloc<'static>> = Vec::with_capacity_in(est, alloc);
    out.extend_from_slice(LTO_SCC_CACHE_MAGIC);
    out.push(1); // Optimized
    push_u64(&mut out, clobbers.clobbered.0);
    push_u32(&mut out, inlined_bodies);
    out.push(heat_flags);
    push_u64(&mut out, heat.own_score);
    push_u64(&mut out, pgo_body_weight);

    push_len(&mut out, record.len())?;
    out.extend_from_slice(record);

    push_len(&mut out, vtable_data.len())?;
    for (vsym, bytes) in vtable_data.iter() {
        out.extend_from_slice(&vsym.0);
        push_len(&mut out, bytes.len())?;
        out.extend_from_slice(bytes.as_slice());
    }

    push_len(&mut out, extra_declared.len())?;
    for (item, name) in extra_declared.iter() {
        push_u128(&mut out, item.0);
        push_len(&mut out, name.len())?;
        out.extend_from_slice(name.as_slice());
    }

    push_len(&mut out, target_func_decls.len())?;
    for (item, name) in target_func_decls.iter() {
        push_u128(&mut out, item.0);
        push_len(&mut out, name.len())?;
        out.extend_from_slice(name.as_slice());
    }

    push_len(&mut out, target_statics.len())?;
    for &item in target_statics.iter() {
        push_u128(&mut out, item.0);
    }

    push_len(&mut out, target_tls.len())?;
    for &item in target_tls.iter() {
        push_u128(&mut out, item.0);
    }

    push_len(&mut out, hot_callees.len())?;
    for &sym in hot_callees.iter() {
        out.extend_from_slice(&sym.0);
    }

    match inline_bytes {
        Some(ref ib) => {
            push_len(&mut out, ib.len())?;
            out.extend_from_slice(ib.as_slice());
        }
        None => {
            push_u32(&mut out, 0);
        }
    }
    Some(out)
}

fn hash_policy_and_opts(
    h: &mut blaze_ids::Hasher,
    producer: [u8; 32],
    opts: &ExportLtoOpts<'_>,
    bound_routines: Option<BoundRef<'_>>,
) {
    h.update(LTO_SCC_CACHE_MAGIC);
    h.update(&blaze_lir::persist::SCHEMA.0.0);
    h.update(&producer);
    let arch_byte: u8 = match opts.machine.arch {
        Arch::Arm64 => 1,
        Arch::X86_64 => 2,
    };
    let os_byte: u8 = match opts.machine.os {
        blaze_tier::Os::MacOs => 1,
        blaze_tier::Os::Linux => 2,
    };
    h.update(&[arch_byte, os_byte]);
    h.update(&opts.machine.features.to_le_bytes());
    h.update(&u64::try_from(opts.max_callee_insts).unwrap_or(u64::MAX).to_le_bytes());
    h.update(&u64::try_from(opts.max_always_inline_insts).unwrap_or(u64::MAX).to_le_bytes());
    h.update(&u64::try_from(opts.max_caller_insts).unwrap_or(u64::MAX).to_le_bytes());
    let p = opts.policy;
    h.update(&p.cost_threshold.to_le_bytes());
    h.update(&p.max_callee_insts.to_le_bytes());
    h.update(&p.hot_multiplier.to_le_bytes());
    h.update(&p.hot_weight_threshold.to_le_bytes());
    h.update(&p.max_depth.to_le_bytes());
    h.update(&p.max_growth.to_le_bytes());
    h.update(&[u8::from(p.allow_external_refs), u8::from(p.allow_loops)]);
    match bound_routines {
        Some(bound) => {
            h.update(&[1]);
            bound.hash_into(h);
        }
        None => {
            h.update(&[0]);
        }
    }
}

/// Computes deterministic bottom-up Merkle `lto_key`s for every SCC in `sccs` (Rule 21, Rule 25).
#[expect(clippy::too_many_arguments, reason = "bottom-up Merkle key computation over SCC graph, lir keys, options, routines, and persisted PGO")]
fn compute_scc_lto_keys<'a>(
    num_nodes: usize,
    node_symbols: &[Symbol],
    node_is_workspace: &[bool],
    node_lir_keys: &[Option<[u8; 32]>],
    adj_offsets: &[u32],
    adj: &[u32],
    sccs: &blaze_lir::lto::SccOrder<Alloc<'a>>,
    node_scc: &[u32],
    producer: Option<[u8; 32]>,
    opts: &ExportLtoOpts<'_>,
    bound_routines: Option<BoundRef<'_>>,
    persisted_pgo: Option<&PgoSiteTable<SharedAlloc<'static>>>,
    alloc: Alloc<'a>,
) -> AVec<'a, Option<[u8; 32]>> {
    let mut lto_keys: AVec<'a, Option<[u8; 32]>> = Vec::with_capacity_in(num_nodes, alloc);
    lto_keys.resize(num_nodes, None);
    let Some(producer) = producer else {
        return lto_keys;
    };

    let mut base_hasher = blaze_ids::Hasher::new(EXPORT_LTO_SCC_KEY_DOMAIN);
    hash_policy_and_opts(&mut base_hasher, producer, opts, bound_routines);
    let base_digest = base_hasher.finish().0;

    let mut members: AVec<'a, (Symbol, [u8; 32], bool, usize)> = Vec::new_in(alloc);
    let mut ext_callees: AVec<'a, (Symbol, [u8; 32])> = Vec::new_in(alloc);

    for scc_idx in 0..sccs.len() {
        let comp = sccs.component(scc_idx);
        members.clear();
        ext_callees.clear();
        let mut all_keyed = true;

        for &node_u32 in comp {
            let Ok(u) = usize::try_from(node_u32) else {
                all_keyed = false;
                break;
            };
            let Some(Some(lk)) = node_lir_keys.get(u).copied() else {
                all_keyed = false;
                break;
            };
            let sym = node_symbols.get(u).copied().unwrap_or(Symbol([0u8; 32]));
            let is_ws = node_is_workspace.get(u).copied().unwrap_or(false);
            members.push((sym, lk, is_ws, u));

            let start = usize::try_from(adj_offsets.get(u).copied().unwrap_or(0)).unwrap_or(0);
            let end = usize::try_from(adj_offsets.get(u.saturating_add(1)).copied().unwrap_or(0))
                .unwrap_or(start);
            for &callee_u32 in adj.get(start..end).unwrap_or(&[]) {
                let Ok(v) = usize::try_from(callee_u32) else {
                    all_keyed = false;
                    break;
                };
                if usize::try_from(node_scc.get(v).copied().unwrap_or(0)).unwrap_or(0) == scc_idx {
                    continue;
                }
                let Some(Some(ck)) = lto_keys.get(v).copied() else {
                    all_keyed = false;
                    break;
                };
                let csym = node_symbols.get(v).copied().unwrap_or(Symbol([0u8; 32]));
                ext_callees.push((csym, ck));
            }
            if !all_keyed {
                break;
            }
        }
        if !all_keyed {
            continue;
        }

        members.sort_unstable_by(|a, b| a.0.0.cmp(&b.0.0));
        ext_callees.sort_unstable_by(|a, b| a.0.0.cmp(&b.0.0));
        ext_callees.dedup_by(|a, b| a.0 == b.0);

        let mut h = blaze_ids::Hasher::new(EXPORT_LTO_SCC_KEY_DOMAIN);
        h.update(&base_digest);
        h.update(&u64::try_from(members.len()).unwrap_or(u64::MAX).to_le_bytes());
        for &(sym, lk, is_ws, _) in members.iter() {
            h.update(&sym.0);
            h.update(&lk);
            h.update(&[u8::from(is_ws)]);
            let (bw, sites_slice) = match persisted_pgo {
                Some(pgo) => {
                    let bid = BodyId(sym.0);
                    let bw = pgo.body_weight(bid);
                    let sites = pgo.site_entries();
                    let start = sites.partition_point(|s| s.caller < bid);
                    let mut end = start;
                    while sites.get(end).is_some_and(|s| s.caller == bid) {
                        end += 1;
                    }
                    (bw, sites.get(start..end).unwrap_or(&[]))
                }
                None => (0u64, &[][..]),
            };
            h.update(&bw.to_le_bytes());
            h.update(&u64::try_from(sites_slice.len()).unwrap_or(u64::MAX).to_le_bytes());
            for s in sites_slice {
                h.update(&s.site_inst.to_le_bytes());
                h.update(&s.weight.to_le_bytes());
            }
        }
        h.update(&u64::try_from(ext_callees.len()).unwrap_or(u64::MAX).to_le_bytes());
        for &(csym, ck) in ext_callees.iter() {
            h.update(&csym.0);
            h.update(&ck);
        }
        let scc_digest = h.finish().0;

        for &(sym, _, _, u) in members.iter() {
            let mut mh = blaze_ids::Hasher::new(EXPORT_LTO_MEMBER_KEY_DOMAIN);
            mh.update(&scc_digest);
            mh.update(&sym.0);
            if let Some(slot) = lto_keys.get_mut(u) {
                *slot = Some(mh.finish().0);
            }
        }
    }

    lto_keys
}

struct BatchOrigin {
    node_idx: usize,
    row: usize,
    symbol: Symbol,
    is_workspace: bool,
    is_toolchain: bool,
    lto_key: Option<[u8; 32]>,
    orig_unreachables: usize,
    orig_refs: Vec<RefRec, SharedAlloc<'static>>,
    funcs: Vec<BuiltCallee, SharedAlloc<'static>>,
    statics: Vec<ItemKey, SharedAlloc<'static>>,
    c_statics: Vec<BuiltForeignStatic, SharedAlloc<'static>>,
    vtables: Vec<Symbol, SharedAlloc<'static>>,
    vtable_data: Vec<(Symbol, Vec<u8, SharedAlloc<'static>>), SharedAlloc<'static>>,
    tls: Vec<ItemKey, SharedAlloc<'static>>,
    traps: Vec<blaze_code::record::TrapSource, SharedAlloc<'static>>,
    c_text: Vec<u8, SharedAlloc<'static>>,
    declared: Vec<DeclaredRow, SharedAlloc<'static>>,
}

impl BatchOrigin {
    fn callee_at(&self, place: u32) -> Option<Callee<'_>> {
        let idx = usize::try_from(place).ok()?;
        match *self.funcs.get(idx)? {
            BuiltCallee::Symbol(sym) => Some(Callee::Symbol(sym)),
            BuiltCallee::C { start, len } => self.c_str(start, len).map(Callee::C),
        }
    }

    fn c_str(&self, start: u32, len: u32) -> Option<&str> {
        let s = usize::try_from(start).ok()?;
        let l = usize::try_from(len).ok()?;
        let bytes = self.c_text.get(s..s.checked_add(l)?)?;
        core::str::from_utf8(bytes).ok()
    }

    fn foreign_static(&self, item: ItemKey) -> Option<(&str, bool)> {
        let cs = self.c_statics.iter().find(|cs| cs.item == item)?;
        let name = self.c_str(cs.start, cs.len)?;
        Some((name, cs.weak))
    }
}

/// `Bodies + Sync` overlay combining the current wavefront batch's pre-LTO ref tables with the
/// already-optimized self-contained lower-SCC `InlineBody` frontier.
struct LtoFrontierBodies<'b> {
    batch_origins: &'b [BatchOrigin],
    batch_by_sym: &'b SymIndexMap,
    frontier: &'b Map<[u8; 32], InlineBody, FixedState, SharedAlloc<'static>>,
    empty_func: &'b blaze_lir::Func<SharedAlloc<'static>>,
}

impl Bodies for LtoFrontierBodies<'_> {
    fn callee(&self, from: BodyId, reference: RefId) -> Option<(BodyId, blaze_lir::View<'_>)> {
        if let Some(caller_idx) = find_body_idx(self.batch_by_sym, from.0) {
            let caller = self.batch_origins.get(caller_idx)?;
            let rec = caller.orig_refs.get(reference.index())?;
            if RefKind::from_word(rec.kind)? != RefKind::Func {
                return None;
            }
            let place = usize::try_from(rec.place).ok()?;
            let BuiltCallee::Symbol(callee_sym) = *caller.funcs.get(place)? else {
                return None;
            };
            if find_body_idx(self.batch_by_sym, callee_sym.0).is_some() {
                // `ParModuleBodies` / `ModuleBodies` / `SccLocalBodies` override this view with
                // `module[idx].func.view()`, and `build_module_call_graph` only checks `id_to_index`.
                return Some((BodyId(callee_sym.0), self.empty_func.view()));
            }
            let ib = self.frontier.get(&callee_sym.0)?;
            return Some((BodyId(callee_sym.0), ib.func.view()));
        }
        let caller = self.frontier.get(&from.0)?;
        let rec = caller.func.view().refs.get(reference.index())?;
        if RefKind::from_word(rec.kind)? != RefKind::Func {
            return None;
        }
        let place = usize::try_from(rec.place).ok()?;
        let BuiltCallee::Symbol(callee_sym) = *caller.funcs.get(place)? else {
            return None;
        };
        let ib = self.frontier.get(&callee_sym.0)?;
        Some((BodyId(callee_sym.0), ib.func.view()))
    }

    fn target(&self, from: BodyId, reference: RefId) -> Option<Target> {
        if let Some(caller_idx) = find_body_idx(self.batch_by_sym, from.0) {
            let caller = self.batch_origins.get(caller_idx)?;
            let rec = caller.orig_refs.get(reference.index())?;
            let kind = RefKind::from_word(rec.kind)?;
            let place = usize::try_from(rec.place).ok()?;
            return match kind {
                RefKind::Func => match *caller.funcs.get(place)? {
                    BuiltCallee::Symbol(sym) => Some(Target::Func(BodyId(sym.0))),
                    BuiltCallee::C { .. } => None,
                },
                RefKind::Static => {
                    let &item = caller.statics.get(place)?;
                    if caller.foreign_static(item).is_some() {
                        None
                    } else {
                        Some(Target::Datum(DatumId(static_symbol(item).0)))
                    }
                }
                RefKind::Vtable => {
                    let &sym = caller.vtables.get(place)?;
                    Some(Target::Datum(DatumId(sym.0)))
                }
                RefKind::Tls => {
                    let &item = caller.tls.get(place)?;
                    Some(Target::Datum(DatumId(tls_symbol(item).0)))
                }
                RefKind::Data => None,
            };
        }
        let caller = self.frontier.get(&from.0)?;
        let rec = caller.func.view().refs.get(reference.index())?;
        let kind = RefKind::from_word(rec.kind)?;
        let place = usize::try_from(rec.place).ok()?;
        match kind {
            RefKind::Func => match *caller.funcs.get(place)? {
                BuiltCallee::Symbol(sym) => Some(Target::Func(BodyId(sym.0))),
                BuiltCallee::C { .. } => None,
            },
            RefKind::Static => {
                let &item = caller.statics.get(place)?;
                if caller.foreign_static(item).is_some() {
                    None
                } else {
                    Some(Target::Datum(DatumId(static_symbol(item).0)))
                }
            }
            RefKind::Vtable => {
                let &sym = caller.vtables.get(place)?;
                Some(Target::Datum(DatumId(sym.0)))
            }
            RefKind::Tls => {
                let &item = caller.tls.get(place)?;
                Some(Target::Datum(DatumId(tls_symbol(item).0)))
            }
            RefKind::Data => None,
        }
    }

    fn body(&self, id: BodyId) -> Option<blaze_lir::View<'_>> {
        self.frontier.get(&id.0).map(|ib| ib.func.view())
    }
}

enum RefOriginSource<'b> {
    Batch(&'b BatchOrigin, RefRec),
    Frontier(&'b InlineBody, RefRec),
}

/// Resolves an `ImportedRef` from a batch body through intra-batch SCC layers to either a batch
/// origin or a self-contained lower-SCC `InlineBody`.
fn resolve_batch_or_frontier_ref<'b>(
    start_idx: usize,
    start_ref: RefId,
    lto_bodies: &[OptimizedBody<SharedAlloc<'static>>],
    batch_origins: &'b [BatchOrigin],
    batch_by_sym: &SymIndexMap,
    frontier: &'b Map<[u8; 32], InlineBody, FixedState, SharedAlloc<'static>>,
) -> Result<RefOriginSource<'b>, ParLtoErr> {
    let start_sym = batch_origins.get(start_idx).map_or([0u8; 32], |o| o.symbol.0);
    let mut cur_batch: Option<usize> = Some(start_idx);
    let mut cur_frontier: Option<&'b InlineBody> = None;
    let mut cur_ref = start_ref;

    for _ in 0..=MAX_IMPORT_HOPS {
        if let Some(ib) = cur_frontier {
            let Some(&rec) = ib.func.view().refs.get(cur_ref.index()) else {
                return Err(ParLtoErr::BadRef { symbol: start_sym, what: "frontier RefId", ref_id: cur_ref.0 });
            };
            return Ok(RefOriginSource::Frontier(ib, rec));
        }
        let Some(cur_idx) = cur_batch else {
            return Err(ParLtoErr::BadRef { symbol: start_sym, what: "empty ref origin", ref_id: cur_ref.0 });
        };
        let Some(opt) = lto_bodies.get(cur_idx) else {
            return Err(ParLtoErr::BadRef { symbol: start_sym, what: "LTO body index", ref_id: cur_ref.0 });
        };
        if let Ok(pos) = opt.imported_refs.binary_search_by_key(&cur_ref, |r| r.caller_ref)
            && let Some(imp) = opt.imported_refs.get(pos)
        {
            if let Some(next_idx) = find_body_idx(batch_by_sym, imp.source_body.0) {
                cur_batch = Some(next_idx);
                cur_ref = imp.source_ref;
                continue;
            }
            if let Some(ib) = frontier.get(&imp.source_body.0) {
                cur_batch = None;
                cur_frontier = Some(ib);
                cur_ref = imp.source_ref;
                continue;
            }
            return Err(ParLtoErr::UnresolvedImport { symbol: start_sym, source: imp.source_body.0 });
        }
        let Some(orig) = batch_origins.get(cur_idx) else {
            return Err(ParLtoErr::BadRef { symbol: start_sym, what: "origin body index", ref_id: cur_ref.0 });
        };
        let Some(&rec) = orig.orig_refs.get(cur_ref.index()) else {
            return Err(ParLtoErr::BadRef { symbol: start_sym, what: "origin RefId", ref_id: cur_ref.0 });
        };
        return Ok(RefOriginSource::Batch(orig, rec));
    }
    Err(ParLtoErr::ImportCycle { symbol: start_sym })
}

fn cache_get_slice_or_copy<C: ExportLtoCache>(
    cache: &C,
    key: [u8; 32],
    shared: SharedAlloc<'static>,
    mut on_bytes: impl FnMut(&[u8]),
) -> bool {
    if let Some(hit) = cache.get_slice_with(key, &mut on_bytes) {
        return hit;
    }
    if let Some(owned) = cache.get(key, shared) {
        on_bytes(owned.as_slice());
        true
    } else {
        false
    }
}

/// Loads the transitive closure (bounded by `max_depth`) of lower-SCC `InlineBody`s reachable from
/// `seed_syms`, decoding lazily from `BLZOLT01` in `cache` whenever an inlineable callee was a
/// cache hit or evicted from RAM.
#[expect(clippy::too_many_arguments, reason = "bounded lazy frontier loader threading cache, symbol index, frontier map, and byte cap")]
fn ensure_frontier_inline_bodies<C: ExportLtoCache>(
    seed_syms: &[Symbol],
    max_depth: u32,
    node_by_sym: &SymIndexMap,
    node_has_inline_body: &[bool],
    node_lto_keys: &[Option<[u8; 32]>],
    node_is_toolchain: &[bool],
    frontier: &mut Map<[u8; 32], InlineBody, FixedState, SharedAlloc<'static>>,
    frontier_bytes: &mut usize,
    cache: &C,
    shared: SharedAlloc<'static>,
    alloc: Alloc<'_>,
) -> Result<(), ExportLtoError> {
    let mut queue: AVec<'_, (Symbol, u32)> = Vec::with_capacity_in(seed_syms.len(), alloc);
    let mut seen: Set<[u8; 32], FixedState, Alloc<'_>> =
        Set::new(SetOpts { hasher: FixedState, alloc, capacity: Some(seed_syms.len().saturating_mul(2)) });
    for &sym in seed_syms {
        if seen.insert(sym.0) {
            queue.push((sym, 0));
        }
    }

    let mut head = 0usize;
    while let Some(&(sym, depth)) = queue.get(head) {
        head += 1;
        let Some(node_idx) = find_body_idx(node_by_sym, sym.0) else {
            continue;
        };
        if !node_has_inline_body.get(node_idx).copied().unwrap_or(false) {
            continue;
        }
        if !frontier.contains_key(&sym.0) {
            let Some(Some(lto_key)) = node_lto_keys.get(node_idx).copied() else {
                continue;
            };
            let is_tc = node_is_toolchain.get(node_idx).copied().unwrap_or(false);
            let mut decoded: Option<Result<InlineBody, ExportLtoError>> = None;
            let hit = cache_get_slice_or_copy(cache, lto_key, shared, |bytes| {
                let res = decode_lto_scc_header(bytes)
                    .and_then(|hdr| hdr.inline_body_bytes.and_then(|ib| decode_inline_body(ib, shared)))
                    .ok_or(ExportLtoError::CorruptCache { what: "BLZOLT01 InlineBody", key: lto_key });
                decoded = Some(res);
            });
            if hit {
                cache.touch_with_class(is_tc, lto_key);
                if let Some(res) = decoded {
                    let ib = res?;
                    *frontier_bytes = frontier_bytes.saturating_add(ib.approx_bytes);
                    frontier.insert(sym.0, ib);
                }
            }
        }
        if depth < max_depth
            && let Some(ib) = frontier.get(&sym.0)
        {
            for callee in ib.funcs.iter() {
                if let BuiltCallee::Symbol(next_sym) = *callee
                    && seen.insert(next_sym.0)
                {
                    queue.push((next_sym, depth.saturating_add(1)));
                }
            }
        }
    }
    Ok(())
}

/// Checks if a post-LTO body is eligible to be inlined into higher-SCC callers.
#[inline]
fn is_inlineable_post_lto(view: blaze_lir::View<'_>, opts: &ExportLtoOpts<'_>) -> bool {
    if !should_expand_non_seed(view.head.attrs) {
        return false;
    }
    let inline_attr = FnAttrs(view.head.attrs).inline();
    if inline_attr == Inline::Never {
        return false;
    }
    let max_hot_inline = usize::try_from(opts.policy.max_callee_insts)
        .unwrap_or(256)
        .saturating_mul(usize::try_from(opts.policy.hot_multiplier).unwrap_or(4))
        .max(opts.max_always_inline_insts);
    view.len() <= max_hot_inline
}

/// Builds a batch-local `PgoSiteTable` from static per-body estimates merged with any persisted
/// `BPG1` weights for the bodies in `mod_bodies`.
fn build_batch_pgo_par<R: LtoRunner>(
    mod_bodies: &[ModuleBody<SharedAlloc<'static>>],
    persisted_pgo: Option<&PgoSiteTable<SharedAlloc<'static>>>,
    shared: SharedAlloc<'static>,
    runner: &R,
) -> Result<PgoSiteTable<SharedAlloc<'static>>, ExportLtoError> {
    let static_pgo = PgoSiteTable::<SharedAlloc<'static>>::estimate_static_par(mod_bodies, shared, runner)
        .map_err(ExportLtoError::Optimize)?;
    let Some(persisted) = persisted_pgo else {
        return Ok(static_pgo);
    };
    let mut sub_sites: Vec<SiteEntry, SharedAlloc<'static>> = Vec::new_in(shared);
    let mut sub_bodies: Vec<BodyWeight, SharedAlloc<'static>> = Vec::new_in(shared);
    let all_sites = persisted.site_entries();
    let all_bodies = persisted.body_entries();
    for mb in mod_bodies.iter() {
        if let Ok(bi) = all_bodies.binary_search_by_key(&mb.id, |b| b.body)
            && let Some(&entry) = all_bodies.get(bi)
        {
            sub_bodies.push(entry);
        }
        let start = all_sites.partition_point(|s| s.caller < mb.id);
        let mut i = start;
        while let Some(&entry) = all_sites.get(i).filter(|s| s.caller == mb.id) {
            sub_sites.push(entry);
            i += 1;
        }
    }
    if sub_sites.is_empty() && sub_bodies.is_empty() {
        return Ok(static_pgo);
    }
    let sub_persisted = PgoSiteTable::new_in(sub_sites.as_slice(), sub_bodies.as_slice(), 1, shared);
    Ok(sub_persisted.merge_decayed(&static_pgo, PGO_DECAY_NUM, PGO_DECAY_DEN, shared))
}

/// Ranks hot symbols across all optimized bodies from precomputed [`BodyHeat`]s and post-LTO
/// direct call edges (deterministic, matching `rank_hot_symbols_with_heats`).
fn rank_hot_symbols_from_heats<'a, 's>(
    body_heats: &[BodyHeat],
    call_edges: &[(Symbol, Symbol)],
    alloc: Alloc<'a>,
    scratch: Alloc<'s>,
) -> AVec<'a, Symbol> {
    let n = body_heats.len();
    let mut sym_index: Map<[u8; 32], usize, FixedState, Alloc<'s>> =
        Map::new(MapOpts { hasher: FixedState, alloc: scratch, capacity: Some(n) });
    for (idx, info) in body_heats.iter().enumerate() {
        sym_index.insert(info.symbol.0, idx);
    }

    let mut hot_edges: AVec<'s, (usize, usize)> = Vec::new_in(scratch);
    for &(caller_sym, callee_sym) in call_edges.iter() {
        if caller_sym == callee_sym {
            continue;
        }
        let Some(&caller_idx) = sym_index.get(&caller_sym.0) else {
            continue;
        };
        let Some(&callee_idx) = sym_index.get(&callee_sym.0) else {
            continue;
        };
        let caller_ok = body_heats.get(caller_idx).is_some_and(|b| b.can_receive_caller_bonus);
        let callee_hot = body_heats.get(callee_idx).is_some_and(|b| b.is_hot_leaf);
        if caller_ok && callee_hot {
            hot_edges.push((caller_idx, callee_idx));
        }
    }
    hot_edges.sort_unstable();

    let mut caller_bonus: AVec<'s, u64> = Vec::with_capacity_in(n, scratch);
    caller_bonus.resize(n, 0);
    let mut caller_max_callee: AVec<'s, u64> = Vec::with_capacity_in(n, scratch);
    caller_max_callee.resize(n, 0);

    let mut prev_edge: Option<(usize, usize)> = None;
    for &(caller_idx, callee_idx) in hot_edges.iter() {
        if prev_edge == Some((caller_idx, callee_idx)) {
            continue;
        }
        prev_edge = Some((caller_idx, callee_idx));
        let Some(callee_heat) = body_heats.get(callee_idx).map(|b| b.own_score) else {
            continue;
        };
        let delta = callee_heat / HOT_CALLER_HEAT_DIV;
        if delta == 0 {
            continue;
        }
        if let Some(slot) = caller_bonus.get_mut(caller_idx) {
            *slot = slot.saturating_add(delta);
        }
        if let Some(max_slot) = caller_max_callee.get_mut(caller_idx)
            && callee_heat > *max_slot
        {
            *max_slot = callee_heat;
        }
    }

    let mut ranked: AVec<'s, HotCandidate> = Vec::with_capacity_in(n, scratch);
    for (idx, &info) in body_heats.iter().enumerate() {
        if !info.eligible {
            continue;
        }
        let raw_bonus = caller_bonus.get(idx).copied().unwrap_or(0);
        let max_callee = caller_max_callee.get(idx).copied().unwrap_or(0);
        let bonus_cap = max_callee.saturating_sub(max_callee / HOT_CALLER_HEAT_DIV);
        let bonus = raw_bonus.min(bonus_cap);
        if !info.has_own_heat && bonus == 0 {
            continue;
        }
        let tier = if info.is_hot_leaf {
            HotTier::HotLeaf
        } else if bonus > 0 {
            HotTier::HotLeafCaller
        } else {
            HotTier::Other
        };
        let score = info.own_score.saturating_add(bonus);
        ranked.push(HotCandidate { symbol: info.symbol, tier, score, is_seed: info.is_seed });
    }

    ranked.sort_unstable_by(|a, b| {
        b.tier
            .cmp(&a.tier)
            .then_with(|| b.score.cmp(&a.score))
            .then_with(|| b.is_seed.cmp(&a.is_seed))
            .then_with(|| a.symbol.0.cmp(&b.symbol.0))
    });
    let mut hot_symbols: AVec<'a, Symbol> = Vec::with_capacity_in(ranked.len(), alloc);
    for item in ranked.iter() {
        hot_symbols.push(item.symbol);
    }
    hot_symbols
}

fn compute_streaming_profile_digest(
    pgo_weights: &mut [(Symbol, u64)],
    hot_symbols: &[Symbol],
) -> [u8; 32] {
    pgo_weights.sort_unstable_by(|a, b| a.0.0.cmp(&b.0.0));
    let mut h = blaze_ids::Hasher::new(EXPORT_PGO_PROFILE_DOMAIN);
    h.update(&u64::try_from(pgo_weights.len()).unwrap_or(u64::MAX).to_le_bytes());
    for &(sym, w) in pgo_weights.iter() {
        h.update(&sym.0);
        h.update(&w.to_le_bytes());
    }
    h.update(&u64::try_from(hot_symbols.len()).unwrap_or(u64::MAX).to_le_bytes());
    for sym in hot_symbols.iter() {
        h.update(&sym.0);
    }
    h.finish().0
}

/// Whole-program all-crate bottom-up content-addressed ThinLTO + host-ISA SOTA emission.
///
/// Optimizes EVERY non-cold, non-noreturn reachable function across `Workspace`, `Locked`
/// third-party dependencies, and `Toolchain` (`core`/`alloc`/`std`) in reverse-topological
/// Tarjan SCC wavefronts with Merkle-keyed `BLZOLT01` caching and bounded streaming memory.
#[expect(clippy::too_many_arguments, reason = "whole-program all-crate ThinLTO over walk, eligible rows, class predicates, zero-copy records, LIR builder, options, allocators, runner, and cache")]
pub fn optimize_export_all_par_cached<'a, 'r, 'b, B, R, C>(
    walk: &Walk<'_>,
    eligible_rows: &[usize],
    is_workspace: impl Fn(usize) -> bool,
    is_toolchain: impl Fn(usize) -> bool,
    code_record_of: impl Fn(usize) -> Option<&'r [u8]> + Sync,
    lir_builder: &mut B,
    opts: ExportLtoOpts<'b>,
    shared: SharedAlloc<'static>,
    alloc: Alloc<'a>,
    runner: &R,
    cache: &C,
) -> Result<ExportAllLtoOut<'a>, ExportLtoError>
where
    B: ExportLirBuilder,
    R: LtoRunner,
    C: ExportLtoCache,
{
    let mut times = ExportLtoTimes::default();
    let num_nodes = eligible_rows.len();
    if num_nodes == 0 {
        return Ok(ExportAllLtoOut {
            rows: Vec::new_in(shared),
            extra_walk_instances: Vec::new_in(shared),
            extra_walk_statics: Vec::new_in(shared),
            extra_walk_tls: Vec::new_in(shared),
            hot_symbols: Vec::new_in(alloc),
            profile_digest: [0u8; 32],
            total_eligible: 0,
            optimized_bodies: 0,
            total_inlined: 0,
            times,
        });
    }

    let t_graph = cache.now_ns();
    // 1. Index eligible rows, extract zero-copy direct-call graph in parallel, run Tarjan SCCs,
    //    and partition SCCs into reverse-topological wavefront levels.
    let mut node_symbols: AVec<'a, Symbol> = Vec::with_capacity_in(num_nodes, alloc);
    let mut node_is_workspace: AVec<'a, bool> = Vec::with_capacity_in(num_nodes, alloc);
    let mut node_is_toolchain: AVec<'a, bool> = Vec::with_capacity_in(num_nodes, alloc);
    let mut node_lir_keys: AVec<'a, Option<[u8; 32]>> = Vec::with_capacity_in(num_nodes, alloc);
    let mut node_by_sym: SymIndexMap =
        Map::new(MapOpts { hasher: FixedState, alloc: shared, capacity: Some(num_nodes) });

    for (u, &row) in eligible_rows.iter().enumerate() {
        let sym = walk.symbol(row);
        node_by_sym.insert(sym.0, u);
        node_symbols.push(sym);
        node_is_workspace.push(is_workspace(row));
        node_is_toolchain.push(is_toolchain(row));
        node_lir_keys.push(cache.lir_key(row, opts.machine));
    }

    let node_by_sym_ref = &node_by_sym;
    let arch = opts.machine.arch;
    let per_node_edges: Vec<Vec<u32, SharedAlloc<'static>>, SharedAlloc<'static>> = runner
        .par_map(num_nodes, shared, |u, scratch| {
            let mut local: AVec<'_, u32> = Vec::new_in(scratch);
            if let Some(&row) = eligible_rows.get(u)
                && let Some(rec_bytes) = code_record_of(row)
            {
                extract_direct_calls(rec_bytes, arch, node_by_sym_ref, &mut local);
            }
            let mut out = Vec::with_capacity_in(local.len(), shared);
            out.extend_from_slice(local.as_slice());
            out
        })
        .map_err(ExportLtoError::Optimize)?;

    let total_edges: usize = per_node_edges.iter().map(Vec::len).sum();
    let mut adj_offsets: AVec<'a, u32> = Vec::with_capacity_in(num_nodes.saturating_add(1), alloc);
    let mut adj: AVec<'a, u32> = Vec::with_capacity_in(total_edges, alloc);
    adj_offsets.push(0);
    for edges in per_node_edges.iter() {
        adj.extend_from_slice(edges.as_slice());
        let off = u32::try_from(adj.len()).map_err(|_| ExportLtoError::BadRef {
            symbol: [0u8; 32],
            what: "all-crate call graph CSR offset",
            ref_id: 0,
        })?;
        adj_offsets.push(off);
    }
    drop(per_node_edges);

    let sccs = tarjan_sccs(num_nodes, adj_offsets.as_slice(), adj.as_slice(), alloc);
    let num_sccs = sccs.len();
    let mut node_scc: AVec<'a, u32> = Vec::with_capacity_in(num_nodes, alloc);
    node_scc.resize(num_nodes, 0);
    for scc_idx in 0..num_sccs {
        let scc_u32 = u32::try_from(scc_idx).unwrap_or(u32::MAX);
        for &node_u32 in sccs.component(scc_idx) {
            if let Ok(u) = usize::try_from(node_u32)
                && let Some(slot) = node_scc.get_mut(u)
            {
                *slot = scc_u32;
            }
        }
    }

    // Compute wavefront level per SCC and cross-SCC caller refcounts for frontier eviction.
    let mut scc_level: AVec<'a, u32> = Vec::with_capacity_in(num_sccs, alloc);
    scc_level.resize(num_sccs, 0);
    let mut remaining_callers: AVec<'a, u32> = Vec::with_capacity_in(num_nodes, alloc);
    remaining_callers.resize(num_nodes, 0);
    let mut max_level = 0u32;
    for scc_idx in 0..num_sccs {
        let mut lvl = 0u32;
        for &node_u32 in sccs.component(scc_idx) {
            let Ok(u) = usize::try_from(node_u32) else { continue };
            let start = usize::try_from(adj_offsets.get(u).copied().unwrap_or(0)).unwrap_or(0);
            let end = usize::try_from(adj_offsets.get(u.saturating_add(1)).copied().unwrap_or(0))
                .unwrap_or(start);
            for &callee_u32 in adj.get(start..end).unwrap_or(&[]) {
                let Ok(v) = usize::try_from(callee_u32) else { continue };
                let cs = usize::try_from(node_scc.get(v).copied().unwrap_or(0)).unwrap_or(0);
                if cs != scc_idx {
                    if let Some(rc) = remaining_callers.get_mut(v) {
                        *rc = rc.saturating_add(1);
                    }
                    let next = scc_level.get(cs).copied().unwrap_or(0).saturating_add(1);
                    if next > lvl {
                        lvl = next;
                    }
                }
            }
        }
        if let Some(slot) = scc_level.get_mut(scc_idx) {
            *slot = lvl;
        }
        if lvl > max_level {
            max_level = lvl;
        }
    }

    let num_levels = usize::try_from(max_level).unwrap_or(0).saturating_add(1);
    let mut level_counts: AVec<'a, usize> = Vec::with_capacity_in(num_levels, alloc);
    level_counts.resize(num_levels, 0);
    for &lvl in scc_level.iter() {
        if let Some(c) = level_counts.get_mut(usize::try_from(lvl).unwrap_or(0)) {
            *c = c.saturating_add(1);
        }
    }
    let mut level_offsets: AVec<'a, usize> = Vec::with_capacity_in(num_levels.saturating_add(1), alloc);
    let mut acc = 0usize;
    for &c in level_counts.iter() {
        level_offsets.push(acc);
        acc = acc.saturating_add(c);
    }
    level_offsets.push(acc);
    let mut level_cursor: AVec<'a, usize> = Vec::with_capacity_in(num_levels, alloc);
    if let Some(prefix) = level_offsets.get(..num_levels) {
        level_cursor.extend_from_slice(prefix);
    }
    let mut level_sccs: AVec<'a, usize> = Vec::with_capacity_in(num_sccs, alloc);
    level_sccs.resize(num_sccs, 0);
    for (scc_idx, &lvl) in scc_level.iter().enumerate() {
        let l = usize::try_from(lvl).unwrap_or(0);
        if let Some(cur) = level_cursor.get_mut(l) {
            if let Some(dst) = level_sccs.get_mut(*cur) {
                *dst = scc_idx;
            }
            *cur = cur.saturating_add(1);
        }
    }

    // 2. Decode persisted PGO once, prepare soft-float routines once, and compute deterministic
    //    bottom-up Merkle `BLZOLT01` keys for every SCC (zero LIR decoding!).
    let t_pgo_dec = cache.now_ns();
    let persisted_pgo = match opts.pgo_bytes {
        Some(bytes) => Some(PgoSiteTable::decode(bytes, shared).map_err(ExportLtoError::PgoRecord)?),
        None => None,
    };
    times.pgo_ns = times.pgo_ns.saturating_add(cache.now_ns().saturating_sub(t_pgo_dec));

    let producer_digest = cache.producer_digest();
    let node_lto_keys = {
        let bound_routines = lir_builder
            .prepare_routines()
            .map_err(|error| ExportLtoError::Record { symbol: [0u8; 32], error })?;
        compute_scc_lto_keys(
            num_nodes,
            node_symbols.as_slice(),
            node_is_workspace.as_slice(),
            node_lir_keys.as_slice(),
            adj_offsets.as_slice(),
            adj.as_slice(),
            &sccs,
            node_scc.as_slice(),
            producer_digest,
            &opts,
            bound_routines,
            persisted_pgo.as_ref(),
            alloc,
        )
    };
    times.bfs_ns = times.bfs_ns.saturating_add(cache.now_ns().saturating_sub(t_graph));

    // 3. Wavefront-by-wavefront streaming execution: zero-copy `BLZOLT01` cache hits first, then
    //    bounded parallel batches for cache misses.
    let mut known_clobbers: AVec<'a, CallClobbers> = Vec::with_capacity_in(num_nodes, alloc);
    known_clobbers.resize(num_nodes, CallClobbers::C_ABI_ARM64);
    let mut node_has_inline_body: AVec<'a, bool> = Vec::with_capacity_in(num_nodes, alloc);
    node_has_inline_body.resize(num_nodes, false);

    let mut frontier: Map<[u8; 32], InlineBody, FixedState, SharedAlloc<'static>> =
        Map::new(MapOpts { hasher: FixedState, alloc: shared, capacity: Some(1024) });
    let mut frontier_bytes = 0usize;

    let mut out_rows: Vec<LtoRowResult, SharedAlloc<'static>> = Vec::with_capacity_in(num_nodes, shared);
    let mut body_heats: AVec<'a, BodyHeat> = Vec::with_capacity_in(num_nodes, alloc);
    let mut hot_call_edges: AVec<'a, (Symbol, Symbol)> = Vec::new_in(alloc);
    let mut pgo_weight_rows: AVec<'a, (Symbol, u64)> = Vec::with_capacity_in(num_nodes, alloc);
    let mut total_inlined = 0usize;
    let mut optimized_bodies = 0usize;

    // Missing walk-target recovery state (preserving Rule 3 zero-hole invariant across cache hits).
    let tls_getter = crate::symbols::tls_getter_symbol();
    let tls_desc = crate::symbols::tls_desc_symbol();
    let mut seen_extra_inst: Set<Symbol, FixedState, Alloc<'a>> =
        Set::new(SetOpts { hasher: FixedState, alloc, capacity: Some(64) });
    let mut seen_extra_static: Set<Symbol, FixedState, Alloc<'a>> =
        Set::new(SetOpts { hasher: FixedState, alloc, capacity: Some(32) });
    let mut seen_extra_tls: Set<Symbol, FixedState, Alloc<'a>> =
        Set::new(SetOpts { hasher: FixedState, alloc, capacity: Some(16) });
    let mut extra_walk_instances: Vec<(usize, ItemKey, Vec<u8, SharedAlloc<'static>>), SharedAlloc<'static>> =
        Vec::new_in(shared);
    let mut extra_walk_statics: Vec<ItemKey, SharedAlloc<'static>> = Vec::new_in(shared);
    let mut extra_walk_tls: Vec<ItemKey, SharedAlloc<'static>> = Vec::new_in(shared);

    let empty_func = blaze_lir::Func::new_in(
        blaze_lir::func::Head { ptr_bits: 64, sig: 0, attrs: 0 },
        shared,
    );
    let is_arm64 = opts.machine.arch == Arch::Arm64;
    let machine = opts.machine;
    let mut missed_sccs: AVec<'a, usize> = Vec::new_in(alloc);
    let mut batch_nodes: AVec<'a, usize> = Vec::with_capacity_in(WAVEFRONT_BATCH_BODIES, alloc);
    let mut batch_sccs: AVec<'a, usize> = Vec::with_capacity_in(WAVEFRONT_BATCH_BODIES, alloc);

    for lvl in 0..num_levels {
        let l_start = level_offsets.get(lvl).copied().unwrap_or(0);
        let l_end = level_offsets.get(lvl.saturating_add(1)).copied().unwrap_or(l_start);
        let sccs_in_level = level_sccs.get(l_start..l_end).unwrap_or(&[]);
        missed_sccs.clear();

        // Phase A: zero-copy probe `BLZOLT01` for every SCC in this level.
        for &scc_idx in sccs_in_level {
            let comp = sccs.component(scc_idx);
            let mut scc_all_hit = true;
            for &node_u32 in comp {
                let Ok(u) = usize::try_from(node_u32) else {
                    scc_all_hit = false;
                    break;
                };
                let Some(Some(lto_key)) = node_lto_keys.get(u).copied() else {
                    scc_all_hit = false;
                    break;
                };
                let mut valid = false;
                let hit = cache_get_slice_or_copy(cache, lto_key, shared, |bytes| {
                    valid = decode_lto_scc_header(bytes).is_some();
                });
                if !hit {
                    scc_all_hit = false;
                    break;
                }
                if !valid {
                    return Err(ExportLtoError::CorruptCache { what: "BLZOLT01", key: lto_key });
                }
            }
            if !scc_all_hit {
                missed_sccs.push(scc_idx);
                continue;
            }
            // All members of this SCC hit `BLZOLT01`: absorb headers zero-copy!
            for &node_u32 in comp {
                let Ok(u) = usize::try_from(node_u32) else { continue };
                let Some(&row) = eligible_rows.get(u) else { continue };
                let Some(&sym) = node_symbols.get(u) else { continue };
                let is_ws = node_is_workspace.get(u).copied().unwrap_or(false);
                let is_tc = node_is_toolchain.get(u).copied().unwrap_or(false);
                let Some(Some(lto_key)) = node_lto_keys.get(u).copied() else { continue };
                cache.touch_with_class(is_tc, lto_key);
                times.emit_hits = times.emit_hits.saturating_add(1);

                let mut hit_err: Option<ExportLtoError> = None;
                let _ = cache_get_slice_or_copy(cache, lto_key, shared, |bytes| {
                    let Some(hdr) = decode_lto_scc_header(bytes) else {
                        hit_err = Some(ExportLtoError::CorruptCache { what: "BLZOLT01", key: lto_key });
                        return;
                    };
                    if let Some(slot) = known_clobbers.get_mut(u) {
                        *slot = hdr.clobbers;
                    }
                    if let Some(slot) = node_has_inline_body.get_mut(u) {
                        *slot = hdr.has_inline_body;
                    }
                    if hdr.status == CachedLtoStatus::Skipped {
                        return;
                    }
                    optimized_bodies = optimized_bodies.saturating_add(1);
                    let inlined = usize::try_from(hdr.inlined_bodies).unwrap_or(0);
                    total_inlined = total_inlined.saturating_add(inlined);
                    body_heats.push(BodyHeat {
                        symbol: sym,
                        is_seed: is_ws,
                        eligible: hdr.eligible,
                        can_receive_caller_bonus: hdr.can_receive_caller_bonus,
                        is_hot_leaf: hdr.is_hot_leaf,
                        has_own_heat: hdr.has_own_heat,
                        own_score: hdr.own_score,
                    });
                    pgo_weight_rows.push((sym, hdr.pgo_body_weight));
                    hdr.each_hot_callee(|callee_sym| {
                        hot_call_edges.push((sym, callee_sym));
                    });
                    // Check extra_declared (soft-float) and target_* against `walk`.
                    hdr.each_extra_declared(|tabled, name_slice| {
                        let fsym = crate::symbols::fn_symbol_of_name(name_slice);
                        if walk.row_of(&fsym).is_none()
                            && fsym != tls_getter
                            && seen_extra_inst.insert(fsym)
                        {
                            let mut copied = Vec::with_capacity_in(name_slice.len(), shared);
                            copied.extend_from_slice(name_slice);
                            extra_walk_instances.push((row, tabled, copied));
                        }
                    });
                    hdr.each_target_func_decl(|tabled, name_slice| {
                        let fsym = crate::symbols::fn_symbol_of_name(name_slice);
                        if walk.row_of(&fsym).is_none()
                            && fsym != tls_getter
                            && seen_extra_inst.insert(fsym)
                        {
                            let mut copied = Vec::with_capacity_in(name_slice.len(), shared);
                            copied.extend_from_slice(name_slice);
                            extra_walk_instances.push((row, tabled, copied));
                        }
                    });
                    hdr.each_target_static(|item| {
                        let ssym = static_symbol(item);
                        if walk.row_of(&ssym).is_none()
                            && ssym != tls_desc
                            && seen_extra_static.insert(ssym)
                        {
                            extra_walk_statics.push(item);
                        }
                    });
                    hdr.each_target_tls(|item| {
                        let tsym = tls_symbol(item);
                        if walk.row_of(&tsym).is_none() && seen_extra_tls.insert(tsym) {
                            extra_walk_tls.push(item);
                        }
                    });
                    out_rows.push(LtoRowResult::Cached {
                        row,
                        symbol: sym,
                        cache_key: lto_key,
                        inlined_bodies: inlined,
                    });
                });
                if let Some(err) = hit_err {
                    return Err(err);
                }
            }
        }

        // Phase B: process cache-missed SCCs in bounded streaming batches (`<= WAVEFRONT_BATCH_BODIES`).
        let mut ms_cursor = 0usize;
        while ms_cursor < missed_sccs.len() {
            batch_nodes.clear();
            batch_sccs.clear();
            while let Some(&scc_idx) = missed_sccs.get(ms_cursor) {
                let comp = sccs.component(scc_idx);
                if !batch_nodes.is_empty()
                    && batch_nodes.len().saturating_add(comp.len()) > WAVEFRONT_BATCH_BODIES
                {
                    break;
                }
                batch_sccs.push(scc_idx);
                for &node_u32 in comp {
                    if let Ok(u) = usize::try_from(node_u32) {
                        batch_nodes.push(u);
                    }
                }
                ms_cursor += 1;
            }
            if batch_nodes.is_empty() {
                continue;
            }

            // B1. Fetch `BLZLIR02` hits and batch-build misses in parallel via `lir_builder`.
            let t_lir = cache.now_ns();
            let b_total = batch_nodes.len();
            let mut fetched: Vec<
                Option<(BuiltLir<SharedAlloc<'static>>, Vec<DeclaredRow, SharedAlloc<'static>>)>,
                SharedAlloc<'static>,
            > = Vec::with_capacity_in(b_total, shared);
            let mut missing_pos: AVec<'a, usize> = Vec::new_in(alloc);
            let mut missing_rows: AVec<'a, usize> = Vec::new_in(alloc);

            for (pos, &u) in batch_nodes.iter().enumerate() {
                let Some(&row) = eligible_rows.get(u) else {
                    fetched.push(None);
                    continue;
                };
                let is_tc = node_is_toolchain.get(u).copied().unwrap_or(false);
                let lir_key = node_lir_keys.get(u).copied().flatten();
                let mut loaded = None;
                if let Some(key) = lir_key {
                    let mut dec_res = None;
                    let hit = cache_get_slice_or_copy(cache, key, shared, |bytes| {
                        dec_res = Some(
                            decode_built_lir(bytes, shared)
                                .ok_or(ExportLtoError::CorruptCache { what: "BuiltLir", key }),
                        );
                    });
                    if hit {
                        cache.touch_with_class(is_tc, key);
                        times.lir_hits = times.lir_hits.saturating_add(1);
                        if let Some(res) = dec_res {
                            loaded = Some(res?);
                        }
                    }
                }
                if loaded.is_some() {
                    fetched.push(loaded);
                } else {
                    fetched.push(None);
                    missing_pos.push(pos);
                    missing_rows.push(row);
                }
            }

            if !missing_rows.is_empty() {
                times.lir_misses = times.lir_misses.saturating_add(missing_rows.len());
                let built_batch =
                    lir_builder.build_lir_batch(missing_rows.as_slice(), machine, shared, alloc)?;
                for (&pos, (built, declared)) in missing_pos.iter().zip(built_batch.into_iter()) {
                    if let Some(&u) = batch_nodes.get(pos)
                        && let Some(key) = node_lir_keys.get(u).copied().flatten()
                        && let Some(encoded) = encode_built_lir(&built, declared.as_slice(), shared)
                    {
                        let is_tc = node_is_toolchain.get(u).copied().unwrap_or(false);
                        cache.put_with_class(is_tc, key, encoded.as_slice());
                    }
                    if let Some(slot) = fetched.get_mut(pos) {
                        *slot = Some((built, declared));
                    }
                }
            }
            times.bfs_ns = times.bfs_ns.saturating_add(cache.now_ns().saturating_sub(t_lir));

            // B2. Filter out cold/noreturn/oversized bodies (writing 38-byte `Skipped` tombstones)
            //     and move surviving `Func`s directly into `mod_bodies` (ZERO `Func` clones!).
            let mut batch_origins: Vec<BatchOrigin, SharedAlloc<'static>> =
                Vec::with_capacity_in(b_total, shared);
            let mut mod_bodies: Vec<ModuleBody<SharedAlloc<'static>>, SharedAlloc<'static>> =
                Vec::with_capacity_in(b_total, shared);
            let mut batch_by_sym: SymIndexMap =
                Map::new(MapOpts { hasher: FixedState, alloc: shared, capacity: Some(b_total) });
            let mut lower_callee_seeds: AVec<'a, Symbol> = Vec::new_in(alloc);

            for (pos, pair_opt) in fetched.into_iter().enumerate() {
                let Some((built, declared)) = pair_opt else { continue };
                let Some(&u) = batch_nodes.get(pos) else { continue };
                let Some(&row) = eligible_rows.get(u) else { continue };
                let Some(&sym) = node_symbols.get(u) else { continue };
                let is_ws = node_is_workspace.get(u).copied().unwrap_or(false);
                let is_tc = node_is_toolchain.get(u).copied().unwrap_or(false);
                let lto_key = node_lto_keys.get(u).copied().flatten();

                let view = built.func.view();
                let keep = if is_ws {
                    // Keep workspace functions unless strictly `#[noreturn]` or mega-sized past SOTA cap.
                    let is_noreturn = (view.head.attrs & FnAttrs::NORETURN) != 0;
                    !is_noreturn && view.len() <= opts.max_caller_insts.max(65_536)
                } else {
                    should_keep_non_seed(view.head.attrs, view.len(), false, 24, &opts)
                };
                if !keep {
                    if let Some(slot) = known_clobbers.get_mut(u) {
                        *slot = CallClobbers::C_ABI_ARM64;
                    }
                    if let Some(slot) = node_has_inline_body.get_mut(u) {
                        *slot = false;
                    }
                    times.emit_misses = times.emit_misses.saturating_add(1);
                    if let Some(key) = lto_key {
                        let tomb = encode_lto_scc_skipped(shared);
                        cache.put_with_class(is_tc, key, tomb.as_slice());
                    }
                    continue;
                }

                let orig_unreachables = view
                    .ops
                    .iter()
                    .filter(|&&op| Op::from_byte(op) == Some(Op::Unreachable))
                    .count();
                let mut orig_refs: Vec<RefRec, SharedAlloc<'static>> =
                    Vec::with_capacity_in(built.func.refs.len(), shared);
                orig_refs.extend_from_slice(built.func.refs.as_slice());

                for callee in built.funcs.iter() {
                    if let BuiltCallee::Symbol(csym) = *callee {
                        lower_callee_seeds.push(csym);
                    }
                }

                let b_idx = batch_origins.len();
                batch_by_sym.insert(sym.0, b_idx);
                mod_bodies.push(ModuleBody { id: BodyId(sym.0), func: built.func });
                batch_origins.push(BatchOrigin {
                    node_idx: u,
                    row,
                    symbol: sym,
                    is_workspace: is_ws,
                    is_toolchain: is_tc,
                    lto_key,
                    orig_unreachables,
                    orig_refs,
                    funcs: built.funcs,
                    statics: built.statics,
                    c_statics: built.c_statics,
                    vtables: built.vtables,
                    vtable_data: built.vtable_data,
                    tls: built.tls,
                    traps: built.traps,
                    c_text: built.c_text,
                    declared,
                });
            }

            if batch_origins.is_empty() {
                continue;
            }

            // B3. Ensure lower-SCC `InlineBody`s needed by this batch are loaded in `frontier`.
            ensure_frontier_inline_bodies(
                lower_callee_seeds.as_slice(),
                u32::from(opts.policy.max_depth),
                &node_by_sym,
                node_has_inline_body.as_slice(),
                node_lto_keys.as_slice(),
                node_is_toolchain.as_slice(),
                &mut frontier,
                &mut frontier_bytes,
                cache,
                shared,
                alloc,
            )?;

            // B4. Build batch PGO table and run parallel bottom-up SCC ThinLTO over the batch!
            let t_pgo = cache.now_ns();
            let batch_pgo = build_batch_pgo_par(mod_bodies.as_slice(), persisted_pgo.as_ref(), shared, runner)?;
            times.pgo_ns = times.pgo_ns.saturating_add(cache.now_ns().saturating_sub(t_pgo));

            let t_inline = cache.now_ns();
            let bodies_overlay = LtoFrontierBodies {
                batch_origins: batch_origins.as_slice(),
                batch_by_sym: &batch_by_sym,
                frontier: &frontier,
                empty_func: &empty_func,
            };
            let lto = lto_optimize_module_par(
                mod_bodies,
                &bodies_overlay,
                &batch_pgo,
                opts.policy,
                shared,
                runner,
            )
            .map_err(ExportLtoError::Optimize)?;
            times.inline_ns = times.inline_ns.saturating_add(cache.now_ns().saturating_sub(t_inline));

            // B5. Parallel flatten imports + verify + host-ISA SOTA emit with bottom-up IPRA +
            //     self-contained `InlineBody` & `BLZOLT01` assembly!
            let t_emit = cache.now_ns();
            let bound_routines = lir_builder
                .prepare_routines()
                .map_err(|error| ExportLtoError::Record { symbol: [0u8; 32], error })?;
            let bn = batch_origins.len();
            let origins_slice = batch_origins.as_slice();
            let lto_slice = lto.bodies.as_slice();
            let batch_by_sym_ref = &batch_by_sym;
            let frontier_ref = &frontier;
            let batch_pgo_ref = &batch_pgo;
            let node_by_sym_ref = &node_by_sym;
            let known_clobbers_slice = known_clobbers.as_slice();
            let node_scc_slice = node_scc.as_slice();

            struct BatchEmitItem {
                b_idx: usize,
                node_idx: usize,
                row: usize,
                symbol: Symbol,
                is_workspace: bool,
                is_toolchain: bool,
                lto_key: Option<[u8; 32]>,
                inlined_bodies: usize,
                clobbers: CallClobbers,
                heat: BodyHeat,
                pgo_body_weight: u64,
                record: Vec<u8, SharedAlloc<'static>>,
                vtable_data: Vec<(Symbol, Vec<u8, SharedAlloc<'static>>), SharedAlloc<'static>>,
                extra_declared: Vec<(ItemKey, Vec<u8, SharedAlloc<'static>>), SharedAlloc<'static>>,
                target_func_decls: Vec<DeclaredRow, SharedAlloc<'static>>,
                target_statics: Vec<ItemKey, SharedAlloc<'static>>,
                target_tls: Vec<ItemKey, SharedAlloc<'static>>,
                hot_callees: Vec<Symbol, SharedAlloc<'static>>,
                inline_body: Option<InlineBody>,
            }

            let emit_results = runner
                .par_map(bn, shared, |b_idx, fn_alloc| -> Result<BatchEmitItem, ParLtoErr> {
                    let Some(orig) = origins_slice.get(b_idx) else {
                        return Err(ParLtoErr::BadRef { symbol: [0u8; 32], what: "LTO batch origin index", ref_id: u32::try_from(b_idx).unwrap_or(u32::MAX) });
                    };
                    let Some(opt) = lto_slice.get(b_idx) else {
                        return Err(ParLtoErr::BadRef { symbol: orig.symbol.0, what: "LTO batch body index", ref_id: u32::try_from(b_idx).unwrap_or(u32::MAX) });
                    };
                    let my_scc = node_scc_slice.get(orig.node_idx).copied().unwrap_or(u32::MAX);

                    let mut funcs_owned: Vec<BuiltCallee, SharedAlloc<'static>> =
                        Vec::with_capacity_in(orig.funcs.len() + opt.imported_refs.len(), shared);
                    funcs_owned.extend_from_slice(orig.funcs.as_slice());
                    let mut c_text: Vec<u8, SharedAlloc<'static>> =
                        Vec::with_capacity_in(orig.c_text.len(), shared);
                    c_text.extend_from_slice(orig.c_text.as_slice());
                    let mut statics: Vec<ItemKey, SharedAlloc<'static>> =
                        Vec::with_capacity_in(orig.statics.len(), shared);
                    statics.extend_from_slice(orig.statics.as_slice());
                    let mut c_statics: Vec<BuiltForeignStatic, SharedAlloc<'static>> =
                        Vec::with_capacity_in(orig.c_statics.len(), shared);
                    c_statics.extend_from_slice(orig.c_statics.as_slice());
                    let mut vtables: Vec<Symbol, SharedAlloc<'static>> =
                        Vec::with_capacity_in(orig.vtables.len(), shared);
                    vtables.extend_from_slice(orig.vtables.as_slice());
                    let mut vtable_data: Vec<(Symbol, Vec<u8, SharedAlloc<'static>>), SharedAlloc<'static>> =
                        Vec::with_capacity_in(orig.vtable_data.len(), shared);
                    for (vsym, bytes) in orig.vtable_data.iter() {
                        push_vtable_datum(&mut vtable_data, *vsym, bytes.as_slice(), shared);
                    }
                    let mut tls: Vec<ItemKey, SharedAlloc<'static>> =
                        Vec::with_capacity_in(orig.tls.len(), shared);
                    tls.extend_from_slice(orig.tls.as_slice());

                    // Collect declaration lookup from own origin + any imported origin/frontier.
                    let mut decl_map: Map<Symbol, (ItemKey, &[u8]), FixedState, Alloc<'_>> =
                        Map::new(MapOpts {
                            hasher: FixedState,
                            alloc: fn_alloc,
                            capacity: Some(orig.declared.len().saturating_mul(2).max(8)),
                        });
                    for (t, n) in orig.declared.iter() {
                        decl_map.entry(crate::symbols::fn_symbol_of_name(n.as_slice())).or_insert((*t, n.as_slice()));
                    }

                    let mut append_c_str = |s: &str, ref_id: u32| -> Result<(u32, u32), ParLtoErr> {
                        let start = u32::try_from(c_text.len()).map_err(|_| ParLtoErr::BadRef {
                            symbol: orig.symbol.0,
                            what: "imported C symbol offset",
                            ref_id,
                        })?;
                        let len = u32::try_from(s.len()).map_err(|_| ParLtoErr::BadRef {
                            symbol: orig.symbol.0,
                            what: "imported C symbol len",
                            ref_id,
                        })?;
                        c_text.extend_from_slice(s.as_bytes());
                        Ok((start, len))
                    };

                    for imp in opt.imported_refs.iter() {
                        let Some(kind) = RefKind::from_word(imp.rec.kind) else {
                            return Err(ParLtoErr::BadRef {
                                symbol: orig.symbol.0,
                                what: "imported ref kind",
                                ref_id: imp.caller_ref.0,
                            });
                        };
                        let place = usize::try_from(imp.rec.place).map_err(|_| ParLtoErr::BadRef {
                            symbol: orig.symbol.0,
                            what: "imported ref place",
                            ref_id: imp.caller_ref.0,
                        })?;
                        let resolved = resolve_batch_or_frontier_ref(
                            b_idx,
                            imp.caller_ref,
                            lto_slice,
                            origins_slice,
                            batch_by_sym_ref,
                            frontier_ref,
                        )?;
                        let (src_rec, src_decls): (RefRec, &[DeclaredRow]) = match resolved {
                            RefOriginSource::Batch(src, rec) => (rec, src.declared.as_slice()),
                            RefOriginSource::Frontier(src, rec) => (rec, src.declared.as_slice()),
                        };
                        if RefKind::from_word(src_rec.kind) != Some(kind) {
                            return Err(ParLtoErr::RefKindMismatch {
                                symbol: orig.symbol.0,
                                ref_id: imp.caller_ref.0,
                            });
                        }
                        for (t, n) in src_decls.iter() {
                            decl_map.entry(crate::symbols::fn_symbol_of_name(n.as_slice())).or_insert((*t, n.as_slice()));
                        }
                        let src_place = usize::try_from(src_rec.place).map_err(|_| ParLtoErr::BadRef {
                            symbol: orig.symbol.0,
                            what: "origin ref place",
                            ref_id: imp.caller_ref.0,
                        })?;
                        match kind {
                            RefKind::Func => {
                                let callee = match resolved {
                                    RefOriginSource::Batch(src, _) => src.callee_at(src_rec.place),
                                    RefOriginSource::Frontier(src, _) => src.callee_at(src_rec.place),
                                };
                                let Some(callee) = callee else {
                                    return Err(ParLtoErr::BadRef {
                                        symbol: orig.symbol.0,
                                        what: "origin func callee",
                                        ref_id: imp.caller_ref.0,
                                    });
                                };
                                let owned_callee = match callee {
                                    Callee::Symbol(s) => BuiltCallee::Symbol(s),
                                    Callee::C(cname) => {
                                        let (start, len) = append_c_str(cname, imp.caller_ref.0)?;
                                        BuiltCallee::C { start, len }
                                    }
                                };
                                put_at(&mut funcs_owned, place, owned_callee, orig.symbol.0, "imported func place", imp.caller_ref.0)?;
                            }
                            RefKind::Static => {
                                let (item, foreign) = match resolved {
                                    RefOriginSource::Batch(src, _) => {
                                        let Some(&it) = src.statics.get(src_place) else {
                                            return Err(ParLtoErr::BadRef {
                                                symbol: orig.symbol.0,
                                                what: "origin static item",
                                                ref_id: imp.caller_ref.0,
                                            });
                                        };
                                        (it, src.foreign_static(it))
                                    }
                                    RefOriginSource::Frontier(src, _) => {
                                        let Some(&it) = src.statics.get(src_place) else {
                                            return Err(ParLtoErr::BadRef {
                                                symbol: orig.symbol.0,
                                                what: "origin static item",
                                                ref_id: imp.caller_ref.0,
                                            });
                                        };
                                        (it, src.foreign_static(it))
                                    }
                                };
                                put_at(&mut statics, place, item, orig.symbol.0, "imported static place", imp.caller_ref.0)?;
                                if let Some((cname, weak)) = foreign
                                    && !c_statics.iter().any(|cs| cs.item == item)
                                {
                                    let (start, len) = append_c_str(cname, imp.caller_ref.0)?;
                                    c_statics.push(BuiltForeignStatic { item, start, len, weak });
                                }
                            }
                            RefKind::Vtable => {
                                let (vsym, src_vt_data) = match resolved {
                                    RefOriginSource::Batch(src, _) => {
                                        let Some(&vs) = src.vtables.get(src_place) else {
                                            return Err(ParLtoErr::BadRef {
                                                symbol: orig.symbol.0,
                                                what: "origin vtable symbol",
                                                ref_id: imp.caller_ref.0,
                                            });
                                        };
                                        (vs, src.vtable_data.as_slice())
                                    }
                                    RefOriginSource::Frontier(src, _) => {
                                        let Some(&vs) = src.vtables.get(src_place) else {
                                            return Err(ParLtoErr::BadRef {
                                                symbol: orig.symbol.0,
                                                what: "origin vtable symbol",
                                                ref_id: imp.caller_ref.0,
                                            });
                                        };
                                        (vs, src.vtable_data.as_slice())
                                    }
                                };
                                put_at(&mut vtables, place, vsym, orig.symbol.0, "imported vtable place", imp.caller_ref.0)?;
                                for (sym, bytes) in src_vt_data.iter() {
                                    push_vtable_datum(&mut vtable_data, *sym, bytes.as_slice(), shared);
                                }
                            }
                            RefKind::Tls => {
                                let item = match resolved {
                                    RefOriginSource::Batch(src, _) => src.tls.get(src_place).copied(),
                                    RefOriginSource::Frontier(src, _) => src.tls.get(src_place).copied(),
                                };
                                let Some(item) = item else {
                                    return Err(ParLtoErr::BadRef {
                                        symbol: orig.symbol.0,
                                        what: "origin tls item",
                                        ref_id: imp.caller_ref.0,
                                    });
                                };
                                put_at(&mut tls, place, item, orig.symbol.0, "imported tls place", imp.caller_ref.0)?;
                            }
                            RefKind::Data => {
                                return Err(ParLtoErr::BadRef {
                                    symbol: orig.symbol.0,
                                    what: "unexpected imported Data ref",
                                    ref_id: imp.caller_ref.0,
                                });
                            }
                        }
                    }

                    // Build borrowed `Callee` / `ForeignStatic` tables pointing into `c_text`.
                    let mut funcs_ref: AVec<'_, Callee<'_>> =
                        Vec::with_capacity_in(funcs_owned.len().saturating_add(8), fn_alloc);
                    for f in funcs_owned.iter() {
                        match *f {
                            BuiltCallee::Symbol(s) => funcs_ref.push(Callee::Symbol(s)),
                            BuiltCallee::C { start, len } => {
                                let s = usize::try_from(start).ok().and_then(|st| {
                                    let l = usize::try_from(len).ok()?;
                                    let b = c_text.get(st..st.checked_add(l)?)?;
                                    core::str::from_utf8(b).ok()
                                });
                                let Some(cname) = s else {
                                    return Err(ParLtoErr::BadRef {
                                        symbol: orig.symbol.0,
                                        what: "C callee text",
                                        ref_id: 0,
                                    });
                                };
                                funcs_ref.push(Callee::C(cname));
                            }
                        }
                    }
                    let mut c_statics_ref: AVec<'_, crate::code_records::ForeignStatic<'_>> =
                        Vec::with_capacity_in(c_statics.len(), fn_alloc);
                    for cs in c_statics.iter() {
                        let s = usize::try_from(cs.start).ok().and_then(|st| {
                            let l = usize::try_from(cs.len).ok()?;
                            let b = c_text.get(st..st.checked_add(l)?)?;
                            core::str::from_utf8(b).ok()
                        });
                        let Some(cname) = s else {
                            return Err(ParLtoErr::BadRef {
                                symbol: orig.symbol.0,
                                what: "foreign static text",
                                ref_id: 0,
                            });
                        };
                        c_statics_ref.push(crate::code_records::ForeignStatic {
                            item: cs.item,
                            name: cname,
                            weak: cs.weak,
                        });
                    }

                    let routines = if func_needs_soft_float(&opt.func.view()) {
                        match bound_routines {
                            Some(bound) => bound
                                .bind_in(&mut funcs_ref)
                                .map_err(|error| ParLtoErr::Record { symbol: orig.symbol.0, error })?,
                            None => Routines::NONE,
                        }
                    } else {
                        Routines::NONE
                    };

                    let verified = verify(opt.func.view())
                        .map_err(|error| ParLtoErr::Verify { symbol: orig.symbol.0, error })?;
                    let view = verified.view();
                    let post_unreachables = view
                        .ops
                        .iter()
                        .filter(|&&op| Op::from_byte(op) == Some(Op::Unreachable))
                        .count();
                    let traps: &[blaze_code::record::TrapSource] =
                        if opt.read.is_empty() && orig.orig_unreachables == post_unreachables {
                            orig.traps.as_slice()
                        } else {
                            &[]
                        };

                    let is_cold = (view.head.attrs & FnAttrs::COLD) != 0;
                    let is_noreturn = (view.head.attrs & FnAttrs::NORETURN) != 0;
                    let loop_heat = if is_cold || is_noreturn {
                        super::LoopHeat { max_depth: 0, loop_blocks: 0 }
                    } else {
                        compute_loop_heat(view, fn_alloc)
                    };
                    // In `--release` AOT export, every non-cold, non-noreturn function (including
                    // straight-line helpers/closures called inside outer loops, e.g.
                    // `core::str::pattern::simd_contains`'s closure) compiles with full
                    // `EmitProfile::Sota` (SROA + GVN + regalloc + mopt + msched); mega-functions
                    // (> 4096 blocks / > 65536 insts) stay linear via `is_mega_fn` in `blaze-emit`.
                    // Ext-TSP 16KB hot-page layout ranking stays driven by `BodyHeat` below.
                    let is_hot = !is_cold && !is_noreturn;
                    let pgo_w = batch_pgo_ref.body_weight(BodyId(orig.symbol.0));
                    let heat = if is_cold || is_noreturn {
                        BodyHeat {
                            symbol: orig.symbol,
                            is_seed: orig.is_workspace,
                            eligible: false,
                            can_receive_caller_bonus: false,
                            is_hot_leaf: false,
                            has_own_heat: false,
                            own_score: 0,
                        }
                    } else {
                        let loop_score = if loop_heat.max_depth > 0 {
                            10u64
                                .saturating_pow(loop_heat.max_depth.min(MAX_LOOP_DEPTH_EXP))
                                .saturating_mul(loop_heat.loop_blocks)
                        } else {
                            0
                        };
                        let own_score = loop_score.saturating_add(pgo_w);
                        let has_own_heat = loop_heat.max_depth > 0 || pgo_w > STATIC_STRAIGHT_LINE_WEIGHT;
                        let zero_call = is_zero_call_leaf(view);
                        let is_hot_leaf = zero_call && has_own_heat && own_score >= HOT_LEAF_THRESHOLD;
                        let can_receive_caller_bonus = !zero_call && has_normal_return(view);
                        BodyHeat {
                            symbol: orig.symbol,
                            is_seed: orig.is_workspace,
                            eligible: true,
                            can_receive_caller_bonus,
                            is_hot_leaf,
                            has_own_heat,
                            own_score,
                        }
                    };

                    // Collect direct post-LTO non-cold callee symbols for 1-hop hot-caller bonus.
                    let mut hot_callees: Vec<Symbol, SharedAlloc<'static>> = Vec::new_in(shared);
                    if heat.can_receive_caller_bonus {
                        for b in 0..view.blocks() {
                            let Ok(b_u32) = u32::try_from(b) else { break };
                            let block = Block(b_u32);
                            if view.block_kind(block) != BlockKind::Normal {
                                continue;
                            }
                            let range = view.block(block);
                            if let Some(term) = range.end.checked_sub(1)
                                && term >= range.start
                                && matches!(view.op(term), Op::Trap | Op::Unreachable | Op::Resume)
                            {
                                continue;
                            }
                            for at in range {
                                if !matches!(view.op(at), Op::Call | Op::Invoke | Op::TailCall)
                                    || view.flags(at).contains(blaze_lir::Flags::COLD)
                                {
                                    continue;
                                }
                                let Some(&data_at) = view.data.get(at) else { continue };
                                let Ok(extra_idx) = usize::try_from(data_at) else { continue };
                                let Some(&ref_word) = view.extra.get(extra_idx) else { continue };
                                let Some(rec) = view.refs.get(RefId(ref_word).index()) else { continue };
                                if RefKind::from_word(rec.kind) != Some(RefKind::Func) {
                                    continue;
                                }
                                let Ok(place) = usize::try_from(rec.place) else { continue };
                                if let Some(&Callee::Symbol(csym)) = funcs_ref.get(place)
                                    && csym != orig.symbol
                                    && find_body_idx(node_by_sym_ref, csym.0).is_some()
                                {
                                    hot_callees.push(csym);
                                }
                            }
                        }
                        hot_callees.sort_unstable_by(|a, b| a.0.cmp(&b.0));
                        hot_callees.dedup();
                    }

                    // Bottom-up IPRA: gather finalized `CallClobbers` strictly from lower SCCs.
                    let mut callee_clobbers_buf: AVec<'_, (u32, CallClobbers)> = Vec::new_in(fn_alloc);
                    if is_arm64 {
                        for (place, callee) in funcs_ref.iter().enumerate() {
                            if let &Callee::Symbol(csym) = callee
                                && let Some(c_node) = find_body_idx(node_by_sym_ref, csym.0)
                                && node_scc_slice.get(c_node).copied() != Some(my_scc)
                                && let Some(&clob) = known_clobbers_slice.get(c_node)
                                && let Ok(place_u32) = u32::try_from(place)
                            {
                                callee_clobbers_buf.push((place_u32, clob));
                            }
                        }
                    }

                    let abi_opts = SotaRecordAbiOpts {
                        machine,
                        is_hot,
                        scratch_order: None,
                        preserve_all_cold: false,
                        callee_clobbers: callee_clobbers_buf.as_slice(),
                        internal_fast: false,
                        internal_fast_callees: &[],
                    };
                    let names = InstanceNames {
                        funcs: funcs_ref.as_slice(),
                        statics: statics.as_slice(),
                        c_statics: c_statics_ref.as_slice(),
                        vtables: vtables.as_slice(),
                        tls: tls.as_slice(),
                    };

                    let (record_tmp, called_declared, clobbers) = emit_code_record_for_machine_with_abi(
                        verified,
                        &names,
                        routines,
                        traps,
                        abi_opts,
                        fn_alloc,
                    )
                    .map_err(|error| ParLtoErr::Record { symbol: orig.symbol.0, error })?;

                    let mut record: Vec<u8, SharedAlloc<'static>> =
                        Vec::with_capacity_in(record_tmp.len(), shared);
                    record.extend_from_slice(record_tmp.as_slice());

                    let mut extra_declared: Vec<(ItemKey, Vec<u8, SharedAlloc<'static>>), SharedAlloc<'static>> =
                        Vec::with_capacity_in(called_declared.len(), shared);
                    let mut soft_float_set: Set<Symbol, FixedState, Alloc<'_>> =
                        Set::new(SetOpts { hasher: FixedState, alloc: fn_alloc, capacity: Some(4) });
                    for d in called_declared.iter() {
                        let mut name_buf: Vec<u8, SharedAlloc<'static>> =
                            Vec::with_capacity_in(d.name.len(), shared);
                        name_buf.extend_from_slice(d.name);
                        soft_float_set.insert(crate::symbols::fn_symbol_of_name(d.name));
                        extra_declared.push((d.tabled, name_buf));
                    }

                    // Extract minimal binary-independent target declarations for `record` + `vtable_data`.
                    let mut static_by_sym: Map<Symbol, ItemKey, FixedState, Alloc<'_>> = Map::new(MapOpts {
                        hasher: FixedState,
                        alloc: fn_alloc,
                        capacity: Some(statics.len()),
                    });
                    for &item in statics.iter() {
                        if !c_statics.iter().any(|cs| cs.item == item) {
                            static_by_sym.entry(static_symbol(item)).or_insert(item);
                        }
                    }
                    let mut tls_by_sym: Map<Symbol, ItemKey, FixedState, Alloc<'_>> = Map::new(MapOpts {
                        hasher: FixedState,
                        alloc: fn_alloc,
                        capacity: Some(tls.len()),
                    });
                    for &item in tls.iter() {
                        tls_by_sym.entry(tls_symbol(item)).or_insert(item);
                    }

                    let mut target_func_decls: Vec<DeclaredRow, SharedAlloc<'static>> = Vec::new_in(shared);
                    let mut seen_tf: Set<Symbol, FixedState, Alloc<'_>> =
                        Set::new(SetOpts { hasher: FixedState, alloc: fn_alloc, capacity: Some(16) });
                    let mut target_statics: Vec<ItemKey, SharedAlloc<'static>> = Vec::new_in(shared);
                    let mut seen_ts: Set<Symbol, FixedState, Alloc<'_>> =
                        Set::new(SetOpts { hasher: FixedState, alloc: fn_alloc, capacity: Some(8) });
                    let mut target_tls: Vec<ItemKey, SharedAlloc<'static>> = Vec::new_in(shared);
                    let mut seen_tt: Set<Symbol, FixedState, Alloc<'_>> =
                        Set::new(SetOpts { hasher: FixedState, alloc: fn_alloc, capacity: Some(4) });

                    let mut record_target = |t: CodeTarget| {
                        match t.space {
                            Space::Func => {
                                if t.symbol == tls_getter
                                    || soft_float_set.contains(&t.symbol)
                                    || !seen_tf.insert(t.symbol)
                                {
                                    return;
                                }
                                if let Some(&(tabled, name_slice)) = decl_map.get(&t.symbol) {
                                    let mut copied = Vec::with_capacity_in(name_slice.len(), shared);
                                    copied.extend_from_slice(name_slice);
                                    target_func_decls.push((tabled, copied));
                                }
                            }
                            Space::Static => {
                                if t.symbol != tls_desc
                                    && seen_ts.insert(t.symbol)
                                    && let Some(&item) = static_by_sym.get(&t.symbol)
                                {
                                    target_statics.push(item);
                                }
                            }
                            Space::Tls => {
                                if seen_tt.insert(t.symbol)
                                    && let Some(&item) = tls_by_sym.get(&t.symbol)
                                {
                                    target_tls.push(item);
                                }
                            }
                            Space::Vtable | Space::Outside | Space::WeakOutside => {}
                        }
                    };

                    if let Ok(cv) = CodeView::read(record.as_slice()) {
                        for ti in 0..cv.targets() {
                            record_target(cv.target(ti));
                        }
                    }
                    for (_, vt_bytes) in vtable_data.iter() {
                        if let Ok(dv) = DatumView::read(vt_bytes.as_slice()) {
                            for ti in 0..dv.targets() {
                                record_target(dv.target(ti));
                            }
                        }
                    }
                    target_func_decls.sort_unstable_by(|a, b| (a.0, a.1.as_slice()).cmp(&(b.0, b.1.as_slice())));
                    target_statics.sort_unstable();
                    target_tls.sort_unstable();

                    // Build self-contained `InlineBody` iff this function is inlineable into callers.
                    let inline_body = if is_inlineable_post_lto(view, &opts) {
                        let mut ib_vt: Vec<(Symbol, Vec<u8, SharedAlloc<'static>>), SharedAlloc<'static>> =
                            Vec::with_capacity_in(vtable_data.len(), shared);
                        for (vsym, bytes) in vtable_data.iter() {
                            push_vtable_datum(&mut ib_vt, *vsym, bytes.as_slice(), shared);
                        }
                        let mut ib_decls: Vec<DeclaredRow, SharedAlloc<'static>> =
                            Vec::with_capacity_in(target_func_decls.len(), shared);
                        for (t, n) in target_func_decls.iter() {
                            let mut copy = Vec::with_capacity_in(n.len(), shared);
                            copy.extend_from_slice(n.as_slice());
                            ib_decls.push((*t, copy));
                        }
                        // Also retain declarations for any `funcs_owned` symbol (in case a caller
                        // inlines us and references a callee from `funcs_owned` directly).
                        for f in funcs_owned.iter() {
                            if let BuiltCallee::Symbol(fsym) = *f
                                && !seen_tf.contains(&fsym)
                                && seen_tf.insert(fsym)
                            {
                                if let Some(&(t, ns)) = decl_map.get(&fsym) {
                                    let mut copy = Vec::with_capacity_in(ns.len(), shared);
                                    copy.extend_from_slice(ns);
                                    ib_decls.push((t, copy));
                                }
                            }
                        }
                        ib_decls.sort_unstable_by(|a, b| (a.0, a.1.as_slice()).cmp(&(b.0, b.1.as_slice())));
                        let mut ib = InlineBody {
                            func: view.to_func_in(shared),
                            funcs: funcs_owned,
                            statics,
                            c_statics,
                            vtables,
                            vtable_data: ib_vt,
                            tls,
                            c_text,
                            declared: ib_decls,
                            approx_bytes: 0,
                        };
                        ib.approx_bytes = ib.estimate_bytes();
                        Some(ib)
                    } else {
                        None
                    };

                    Ok(BatchEmitItem {
                        b_idx,
                        node_idx: orig.node_idx,
                        row: orig.row,
                        symbol: orig.symbol,
                        is_workspace: orig.is_workspace,
                        is_toolchain: orig.is_toolchain,
                        lto_key: orig.lto_key,
                        inlined_bodies: opt.read.len(),
                        clobbers,
                        heat,
                        pgo_body_weight: pgo_w,
                        record,
                        vtable_data,
                        extra_declared,
                        target_func_decls,
                        target_statics,
                        target_tls,
                        hot_callees,
                        inline_body,
                    })
                })
                .map_err(ExportLtoError::Optimize)?;

            // Unwrap worker results in deterministic order and compute conservative SCC-union
            // `CallClobbers` for multi-node recursive SCCs BEFORE persisting `BLZOLT01`.
            let mut items: Vec<BatchEmitItem, SharedAlloc<'static>> = Vec::with_capacity_in(bn, shared);
            for res in emit_results {
                items.push(res.map_err(ExportLtoError::from)?);
            }
            for &scc_idx in batch_sccs.iter() {
                let comp = sccs.component(scc_idx);
                if comp.len() > 1 {
                    let mut scc_union = CallClobbers::EMPTY;
                    for item in items.iter() {
                        if node_scc.get(item.node_idx).and_then(|&s| usize::try_from(s).ok()) == Some(scc_idx) {
                            scc_union.clobbered = scc_union.clobbered.union(item.clobbers.clobbered);
                        }
                    }
                    for item in items.iter_mut() {
                        if node_scc.get(item.node_idx).and_then(|&s| usize::try_from(s).ok()) == Some(scc_idx) {
                            item.clobbers = scc_union;
                        }
                    }
                }
            }

            // Persist `BLZOLT01` records to `Namespace::LOCAL`, update frontier, and check walk targets.
            for mut item in items {
                let _ = item.b_idx;
                let u = item.node_idx;
                times.emit_misses = times.emit_misses.saturating_add(1);
                optimized_bodies = optimized_bodies.saturating_add(1);
                total_inlined = total_inlined.saturating_add(item.inlined_bodies);
                if let Some(slot) = known_clobbers.get_mut(u) {
                    *slot = item.clobbers;
                }
                let has_ib = item.inline_body.is_some();
                if let Some(slot) = node_has_inline_body.get_mut(u) {
                    *slot = has_ib;
                }
                body_heats.push(item.heat);
                pgo_weight_rows.push((item.symbol, item.pgo_body_weight));
                for &csym in item.hot_callees.iter() {
                    hot_call_edges.push((item.symbol, csym));
                }

                // Verify every emitted target has a declaration in `walk` or `target_*` (Rule 3),
                // and backfill any already-walked target missing from `target_func_decls` so
                // `BLZOLT01` stays 100% binary-independent across different workspaces/binaries.
                let mut decl_idx: Map<Symbol, (ItemKey, usize), FixedState, Alloc<'a>> = Map::new(MapOpts {
                    hasher: FixedState,
                    alloc,
                    capacity: Some(item.target_func_decls.len()),
                });
                for (di, (t, n)) in item.target_func_decls.iter().enumerate() {
                    decl_idx.entry(crate::symbols::fn_symbol_of_name(n.as_slice())).or_insert((*t, di));
                }
                let mut sf_set: Set<Symbol, FixedState, Alloc<'a>> =
                    Set::new(SetOpts { hasher: FixedState, alloc, capacity: Some(item.extra_declared.len()) });
                for (t, n) in item.extra_declared.iter() {
                    let fsym = crate::symbols::fn_symbol_of_name(n.as_slice());
                    sf_set.insert(fsym);
                    if walk.row_of(&fsym).is_none()
                        && fsym != tls_getter
                        && seen_extra_inst.insert(fsym)
                    {
                        let mut copied = Vec::with_capacity_in(n.len(), shared);
                        copied.extend_from_slice(n.as_slice());
                        extra_walk_instances.push((item.row, *t, copied));
                    }
                }
                let mut st_idx: Map<Symbol, ItemKey, FixedState, Alloc<'a>> = Map::new(MapOpts {
                    hasher: FixedState,
                    alloc,
                    capacity: Some(item.target_statics.len()),
                });
                for &it in item.target_statics.iter() {
                    st_idx.insert(static_symbol(it), it);
                }
                let mut tl_idx: Map<Symbol, ItemKey, FixedState, Alloc<'a>> = Map::new(MapOpts {
                    hasher: FixedState,
                    alloc,
                    capacity: Some(item.target_tls.len()),
                });
                for &it in item.target_tls.iter() {
                    tl_idx.insert(tls_symbol(it), it);
                }

                let mut backfilled_decls = false;
                let mut verify_target = |t: CodeTarget| -> Result<(), ExportLtoError> {
                    match t.space {
                        Space::Func => {
                            if t.symbol == tls_getter || sf_set.contains(&t.symbol) {
                                return Ok(());
                            }
                            if let Some(wrow) = walk.row_of(&t.symbol) {
                                if !decl_idx.contains_key(&t.symbol) {
                                    let ns = walk.name(wrow);
                                    let mut copy = Vec::with_capacity_in(ns.len(), shared);
                                    copy.extend_from_slice(ns);
                                    let di = item.target_func_decls.len();
                                    let tabled = walk.tabled(wrow);
                                    item.target_func_decls.push((tabled, copy));
                                    decl_idx.insert(t.symbol, (tabled, di));
                                    backfilled_decls = true;
                                }
                                return Ok(());
                            }
                            if seen_extra_inst.contains(&t.symbol) {
                                return Ok(());
                            }
                            let Some(&(tabled, di)) = decl_idx.get(&t.symbol) else {
                                return Err(ExportLtoError::MissingDeclared { from: item.symbol.0, target: t });
                            };
                            seen_extra_inst.insert(t.symbol);
                            if let Some((_, name_vec)) = item.target_func_decls.get(di) {
                                let name_slice = name_vec.as_slice();
                                let mut copied = Vec::with_capacity_in(name_slice.len(), shared);
                                copied.extend_from_slice(name_slice);
                                extra_walk_instances.push((item.row, tabled, copied));
                            }
                            Ok(())
                        }
                        Space::Static => {
                            if walk.row_of(&t.symbol).is_some()
                                || t.symbol == tls_desc
                                || seen_extra_static.contains(&t.symbol)
                            {
                                return Ok(());
                            }
                            let Some(&s_item) = st_idx.get(&t.symbol) else {
                                return Err(ExportLtoError::MissingDeclared { from: item.symbol.0, target: t });
                            };
                            seen_extra_static.insert(t.symbol);
                            extra_walk_statics.push(s_item);
                            Ok(())
                        }
                        Space::Tls => {
                            if walk.row_of(&t.symbol).is_some() || seen_extra_tls.contains(&t.symbol) {
                                return Ok(());
                            }
                            let Some(&t_item) = tl_idx.get(&t.symbol) else {
                                return Err(ExportLtoError::MissingDeclared { from: item.symbol.0, target: t });
                            };
                            seen_extra_tls.insert(t.symbol);
                            extra_walk_tls.push(t_item);
                            Ok(())
                        }
                        Space::Vtable | Space::Outside | Space::WeakOutside => Ok(()),
                    }
                };
                let cv = CodeView::read(item.record.as_slice())
                    .map_err(|_| ExportLtoError::DecodeRecord { symbol: item.symbol.0, what: "code record" })?;
                for ti in 0..cv.targets() {
                    verify_target(cv.target(ti))?;
                }
                for (_, vt_bytes) in item.vtable_data.iter() {
                    let dv = DatumView::read(vt_bytes.as_slice())
                        .map_err(|_| ExportLtoError::DecodeRecord { symbol: item.symbol.0, what: "vtable datum" })?;
                    for ti in 0..dv.targets() {
                        verify_target(dv.target(ti))?;
                    }
                }
                if backfilled_decls {
                    item.target_func_decls
                        .sort_unstable_by(|a, b| (a.0, a.1.as_slice()).cmp(&(b.0, b.1.as_slice())));
                }

                let mut cached_ok = false;
                if let Some(key) = item.lto_key
                    && let Some(encoded) = encode_lto_scc_optimized(
                        item.clobbers,
                        u32::try_from(item.inlined_bodies).unwrap_or(u32::MAX),
                        item.heat,
                        item.pgo_body_weight,
                        item.record.as_slice(),
                        item.vtable_data.as_slice(),
                        item.extra_declared.as_slice(),
                        item.target_func_decls.as_slice(),
                        item.target_statics.as_slice(),
                        item.target_tls.as_slice(),
                        item.hot_callees.as_slice(),
                        item.inline_body.as_ref(),
                        shared,
                    )
                {
                    cache.put_with_class(item.is_toolchain, key, encoded.as_slice());
                    out_rows.push(LtoRowResult::Cached {
                        row: item.row,
                        symbol: item.symbol,
                        cache_key: key,
                        inlined_bodies: item.inlined_bodies,
                    });
                    cached_ok = true;
                }
                if !cached_ok {
                    out_rows.push(LtoRowResult::Inline(LtoBodyOut {
                        row: item.row,
                        symbol: item.symbol,
                        record: item.record,
                        vtable_data: item.vtable_data,
                        extra_declared: item.extra_declared,
                        inlined_bodies: item.inlined_bodies,
                    }));
                }

                if let Some(ib) = item.inline_body
                    && remaining_callers.get(u).copied().unwrap_or(0) > 0
                {
                    let bytes = ib.approx_bytes;
                    if frontier_bytes.saturating_add(bytes) <= MAX_FRONTIER_INLINE_BYTES {
                        frontier_bytes = frontier_bytes.saturating_add(bytes);
                        frontier.insert(item.symbol.0, ib);
                    }
                }
            }
            times.emit_ns = times.emit_ns.saturating_add(cache.now_ns().saturating_sub(t_emit));
        }

        // Phase C: decrement caller refcounts for cross-SCC callees of this completed level and
        // evict finished `InlineBody`s so peak frontier RAM stays bounded.
        for &scc_idx in sccs_in_level {
            for &node_u32 in sccs.component(scc_idx) {
                let Ok(u) = usize::try_from(node_u32) else { continue };
                let start = usize::try_from(adj_offsets.get(u).copied().unwrap_or(0)).unwrap_or(0);
                let end = usize::try_from(adj_offsets.get(u.saturating_add(1)).copied().unwrap_or(0))
                    .unwrap_or(start);
                for &callee_u32 in adj.get(start..end).unwrap_or(&[]) {
                    let Ok(v) = usize::try_from(callee_u32) else { continue };
                    if node_scc.get(v).and_then(|&s| usize::try_from(s).ok()) != Some(scc_idx)
                        && let Some(rc) = remaining_callers.get_mut(v)
                    {
                        *rc = rc.saturating_sub(1);
                        if *rc == 0
                            && let Some(sym) = node_symbols.get(v)
                            && let Some(evicted) = frontier.remove(&sym.0)
                        {
                            frontier_bytes = frontier_bytes.saturating_sub(evicted.approx_bytes);
                        }
                    }
                }
            }
        }
        // If lazy reloads pushed frontier past cap, drop cached entries (all are persisted in
        // `BLZOLT01` when keyed and reload on demand).
        if frontier_bytes > MAX_FRONTIER_INLINE_BYTES {
            frontier.clear();
            frontier_bytes = 0;
        }
    }

    let t_rank = cache.now_ns();
    let hot_symbols = rank_hot_symbols_from_heats(
        body_heats.as_slice(),
        hot_call_edges.as_slice(),
        alloc,
        alloc,
    );
    let profile_digest = compute_streaming_profile_digest(pgo_weight_rows.as_mut_slice(), hot_symbols.as_slice());
    times.prep_ns = times.prep_ns.saturating_add(cache.now_ns().saturating_sub(t_rank));

    Ok(ExportAllLtoOut {
        rows: out_rows,
        extra_walk_instances,
        extra_walk_statics,
        extra_walk_tls,
        hot_symbols,
        profile_digest,
        total_eligible: num_nodes,
        optimized_bodies,
        total_inlined,
        times,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use blaze_code::record::{CodeRecord, Site};
    use blaze_lir::{
        Builder, Full,
        builder::{Param, Variadic},
        func::Conv,
        inline::InlinePolicy,
    };
    use blaze_tier::Os as TierOs;

    fn arm64_machine() -> Machine {
        Machine {
            arch: Arch::Arm64,
            os: TierOs::MacOs,
            features: Machine::LSE | Machine::DOTPROD | Machine::CRC32 | Machine::SME | Machine::SME2,
        }
    }

    #[test]
    fn blzolt01_codec_round_trips_skipped_and_optimized_with_inline_body_and_rejects_corruption() {
        let shared = blaze_own::testing::heap().shared();
        let alloc = blaze_own::testing::heap().alloc();

        // 1. Skipped tombstone round-trip.
        let skipped = encode_lto_scc_skipped(shared);
        let sh = decode_lto_scc_header(skipped.as_slice()).expect("decode skipped");
        assert_eq!(sh.status, CachedLtoStatus::Skipped);
        assert_eq!(sh.clobbers, CallClobbers::C_ABI_ARM64);
        assert!(!sh.has_inline_body);
        assert!(decode_lto_scc_header(&skipped[..skipped.len() - 1]).is_none());

        // 2. Optimized body with self-contained InlineBody, vtable, extra_declared, and target decls.
        let func = {
            let mut b: Builder<SharedAlloc<'static>, Full> = Builder::new_in(64, shared);
            let sig = b.sig(Conv::C, Variadic::No, &[Param::of(blaze_lir::Ty::I64)], &[Param::of(blaze_lir::Ty::I64)]);
            b.set_signature(sig);
            let entry = b.create_block(BlockKind::Normal);
            b.begin(entry);
            let x = b.param(blaze_lir::Ty::I64);
            let one = b.iconst(blaze_lir::Ty::I64, 1);
            let y = b.binary(Op::Add, blaze_lir::Flags::NONE, x, one);
            b.ret(&[y]);
            b.finish_checked().expect("valid leaf func")
        };
        let mut funcs_v: Vec<BuiltCallee, SharedAlloc<'static>> = Vec::new_in(shared);
        funcs_v.push(BuiltCallee::Symbol(Symbol::of_name("callee_x")));
        let mut decl_name: Vec<u8, SharedAlloc<'static>> = Vec::new_in(shared);
        decl_name.extend_from_slice(b"callee_x_name");
        let mut decls: Vec<DeclaredRow, SharedAlloc<'static>> = Vec::new_in(shared);
        decls.push((ItemKey(0x1234), decl_name));
        let ib = InlineBody {
            func: func.view().to_func_in(shared),
            funcs: funcs_v,
            statics: Vec::new_in(shared),
            c_statics: Vec::new_in(shared),
            vtables: Vec::new_in(shared),
            vtable_data: Vec::new_in(shared),
            tls: Vec::new_in(shared),
            c_text: Vec::new_in(shared),
            declared: Vec::new_in(shared),
            approx_bytes: 128,
        };
        let heat = BodyHeat {
            symbol: Symbol::of_name("fn_a"),
            is_seed: false,
            eligible: true,
            can_receive_caller_bonus: true,
            is_hot_leaf: false,
            has_own_heat: true,
            own_score: 420,
        };
        let mut vt_payload: Vec<u8, SharedAlloc<'static>> = Vec::new_in(shared);
        vt_payload.extend_from_slice(&[9, 8, 7]);
        let mut vts: Vec<(Symbol, Vec<u8, SharedAlloc<'static>>), SharedAlloc<'static>> = Vec::new_in(shared);
        vts.push((Symbol::of_name("vt_a"), vt_payload));
        let clob = CallClobbers { clobbered: RegSet(0x00FF) };
        let hot_callees = [Symbol::of_name("hot_leaf")];
        let statics = [ItemKey(0xCAFE)];
        let tls = [ItemKey(0xBEEF)];

        let enc = encode_lto_scc_optimized(
            clob,
            3,
            heat,
            100,
            b"sealed-code-rec",
            vts.as_slice(),
            &[],
            decls.as_slice(),
            &statics,
            &tls,
            &hot_callees,
            Some(&ib),
            shared,
        )
        .expect("encode BLZOLT01");

        let view = decode_lto_scc_header(enc.as_slice()).expect("decode BLZOLT01");
        assert_eq!(view.status, CachedLtoStatus::Optimized);
        assert_eq!(view.clobbers, clob);
        assert_eq!(view.inlined_bodies, 3);
        assert_eq!(view.own_score, 420);
        assert_eq!(view.pgo_body_weight, 100);
        assert!(view.eligible && view.can_receive_caller_bonus && !view.is_hot_leaf && view.has_own_heat && view.has_inline_body);
        assert_eq!(view.record, b"sealed-code-rec");

        let mut got_vt = 0usize;
        view.each_vtable(|s, b| -> Result<(), ()> {
            assert_eq!(s, Symbol::of_name("vt_a"));
            assert_eq!(b, &[9, 8, 7]);
            got_vt += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(got_vt, 1);

        let mut got_decls = 0usize;
        view.each_target_func_decl(|k, n| {
            assert_eq!(k, ItemKey(0x1234));
            assert_eq!(n, b"callee_x_name");
            got_decls += 1;
        });
        assert_eq!(got_decls, 1);

        let dec_ib = decode_inline_body(view.inline_body_bytes.expect("inline body present"), shared)
            .expect("decode inline body");
        assert_eq!(
            blaze_lir::persist::to_bytes_in(&func.view(), alloc).as_slice(),
            blaze_lir::persist::to_bytes_in(&dec_ib.func.view(), alloc).as_slice(),
        );
        assert_eq!(dec_ib.funcs.as_slice(), &[BuiltCallee::Symbol(Symbol::of_name("callee_x"))]);

        // Corruption/truncation rejection (Rule 3).
        assert!(decode_lto_scc_header(&enc[..enc.len() - 1]).is_none());
        let mut bad = enc.as_slice().to_vec();
        bad[0] ^= 0x55;
        assert!(decode_lto_scc_header(&bad).is_none());
    }

    #[test]
    fn extract_direct_calls_captures_calls_and_arm64_tail_calls_but_ignores_func_addr() {
        let alloc = blaze_own::testing::heap().alloc();
        let mut walk = Walk::new_in(alloc);
        let r_call = walk.root::<()>( (ItemKey(1), b"callee_direct"), 16 ).unwrap();
        let r_tail = walk.root::<()>( (ItemKey(2), b"callee_tail"), 16 ).unwrap();
        let r_addr = walk.root::<()>( (ItemKey(3), b"callee_fnptr"), 16 ).unwrap();

        let mut rec = CodeRecord::new_in(alloc);
        let t_call = rec.target(CodeTarget { space: Space::Func, symbol: walk.symbol(r_call) });
        let t_tail = rec.target(CodeTarget { space: Space::Func, symbol: walk.symbol(r_tail) });
        let t_addr = rec.target(CodeTarget { space: Space::Func, symbol: walk.symbol(r_addr) });
        // 0: direct call (adrp x16; ldr x16, [x16]; blr x16)
        rec.code.extend_from_slice(&0x9000_0010u32.to_le_bytes());
        rec.code.extend_from_slice(&0xF940_0210u32.to_le_bytes());
        rec.code.extend_from_slice(&0xD63F_0200u32.to_le_bytes());
        // 12: tail call (adrp x16; ldr x16, [x16]; br x16)
        rec.code.extend_from_slice(&0x9000_0010u32.to_le_bytes());
        rec.code.extend_from_slice(&0xF940_0210u32.to_le_bytes());
        rec.code.extend_from_slice(&0xD61F_0200u32.to_le_bytes());
        // 24: FuncAddr load into x0 followed by ret (NOT a direct call!)
        rec.code.extend_from_slice(&0x9000_0000u32.to_le_bytes());
        rec.code.extend_from_slice(&0xF940_0000u32.to_le_bytes());
        rec.code.extend_from_slice(&0xD65F_03C0u32.to_le_bytes());
        rec.sites.push(Site { at: 0, kind: SiteKind::Call, target: t_call, addend: 0 });
        rec.sites.push(Site { at: 12, kind: SiteKind::Load, target: t_tail, addend: 0 });
        rec.sites.push(Site { at: 24, kind: SiteKind::Load, target: t_addr, addend: 0 });
        let sealed = rec.seal().expect("seal");
        let mut bytes: AVec<'_, u8> = Vec::new_in(alloc);
        sealed.encode(&mut bytes);

        let shared = blaze_own::testing::heap().shared();
        let mut node_by_sym: Map<[u8; 32], usize, FixedState, SharedAlloc<'static>> =
            Map::new(MapOpts { hasher: FixedState, alloc: shared, capacity: Some(4) });
        node_by_sym.insert(walk.symbol(r_call).0, 0);
        node_by_sym.insert(walk.symbol(r_tail).0, 1);
        node_by_sym.insert(walk.symbol(r_addr).0, 2);
        let mut out: AVec<'_, u32> = Vec::new_in(alloc);
        extract_direct_calls(bytes.as_slice(), Arch::Arm64, &node_by_sym, &mut out);
        assert_eq!(out.as_slice(), &[0, 1], "must capture Call and br x16 TailCall, excluding FuncAddr");
    }

    #[test]
    fn merkle_scc_keys_invalidate_on_callee_edit_policy_machine_and_pgo_planted_controls() {
        let alloc = blaze_own::testing::heap().alloc();
        let shared = blaze_own::testing::heap().shared();
        // Call graph: caller(0) -> callee(1); unrelated(2) has no edges.
        let syms = [Symbol([0x01; 32]), Symbol([0x02; 32]), Symbol([0x03; 32])];
        let is_ws = [true, false, false];
        let base_lir = [Some([0xA1; 32]), Some([0xB2; 32]), Some([0xC3; 32])];
        let adj_offsets = [0u32, 1, 1, 1];
        let adj = [1u32];
        let sccs = tarjan_sccs(3, &adj_offsets, &adj, alloc);
        let mut node_scc = [0u32; 3];
        for scc_idx in 0..sccs.len() {
            for &n in sccs.component(scc_idx) {
                node_scc[n as usize] = scc_idx as u32;
            }
        }
        let mut base_opts = ExportLtoOpts::for_export(blaze_emit::Isa { lse: true, os: blaze_emit::Os::MacOs });
        base_opts.machine = arm64_machine();
        let prod = Some([0x77; 32]);

        let k0 = compute_scc_lto_keys(3, &syms, &is_ws, &base_lir, &adj_offsets, &adj, &sccs, &node_scc, prod, &base_opts, None, None, alloc);
        assert!(k0.iter().all(Option::is_some));

        // 1. Editing callee(1)'s LIR invalidates callee(1) AND caller(0), NOT unrelated(2)!
        let mut edited_callee = base_lir;
        edited_callee[1] = Some([0xFF; 32]);
        let k_cal = compute_scc_lto_keys(3, &syms, &is_ws, &edited_callee, &adj_offsets, &adj, &sccs, &node_scc, prod, &base_opts, None, None, alloc);
        assert_ne!(k0[1], k_cal[1], "edited callee key must move");
        assert_ne!(k0[0], k_cal[0], "caller of edited callee must invalidate transitively");
        assert_eq!(k0[2], k_cal[2], "unrelated library function must stay cached!");

        // 2. Editing caller(0) alone leaves callee(1) and unrelated(2) cached!
        let mut edited_caller = base_lir;
        edited_caller[0] = Some([0xEE; 32]);
        let k_clr = compute_scc_lto_keys(3, &syms, &is_ws, &edited_caller, &adj_offsets, &adj, &sccs, &node_scc, prod, &base_opts, None, None, alloc);
        assert_ne!(k0[0], k_clr[0]);
        assert_eq!(k0[1], k_clr[1], "callee of edited caller must stay cached!");
        assert_eq!(k0[2], k_clr[2]);

        // 3. Changing inline policy or host machine features invalidates all keys.
        let mut opts_pol = base_opts;
        opts_pol.policy = InlinePolicy { cost_threshold: base_opts.policy.cost_threshold + 1, ..base_opts.policy };
        let k_pol = compute_scc_lto_keys(3, &syms, &is_ws, &base_lir, &adj_offsets, &adj, &sccs, &node_scc, prod, &opts_pol, None, None, alloc);
        assert_ne!(k0[0], k_pol[0]);
        assert_ne!(k0[1], k_pol[1]);

        let mut opts_mach = base_opts;
        opts_mach.machine.features ^= Machine::SME2;
        let k_mach = compute_scc_lto_keys(3, &syms, &is_ws, &base_lir, &adj_offsets, &adj, &sccs, &node_scc, prod, &opts_mach, None, None, alloc);
        assert_ne!(k0[0], k_mach[0]);

        // 4. Persisted PGO on callee(1) invalidates callee(1) + caller(0), NOT unrelated(2)!
        let pgo = PgoSiteTable::new_in(&[], &[BodyWeight { body: BodyId(syms[1].0), weight: 5000 }], 1, shared);
        let k_pgo = compute_scc_lto_keys(3, &syms, &is_ws, &base_lir, &adj_offsets, &adj, &sccs, &node_scc, prod, &base_opts, None, Some(&pgo), alloc);
        assert_ne!(k0[1], k_pgo[1]);
        assert_ne!(k0[0], k_pgo[0]);
        assert_eq!(k0[2], k_pgo[2], "unrelated fn must not be invalidated by another fn's PGO");
    }

    #[test]
    fn optimize_export_all_inlines_non_workspace_crates_and_hits_blzolt01_on_warm_and_partial_edits() {
        use core::cell::RefCell;
        use blaze_lir::lto::SequentialLtoRunner;

        let shared = blaze_own::testing::heap().shared();
        let alloc = blaze_own::testing::heap().alloc();

        // Build a 4-row walk:
        //   0: ws_main   (Workspace)  -> calls lib_outer
        //   1: lib_outer (Locked)     -> calls lib_leaf
        //   2: lib_leaf  (Toolchain)  -> returns x + addend
        //   3: lib_other (Toolchain)  -> unrelated leaf x * 3
        let mut walk = Walk::new_in(alloc);
        let r_main = walk.root::<()>((ItemKey(10), b"ws_main"), 16).unwrap();
        let r_outer = walk.root::<()>((ItemKey(20), b"lib_outer"), 16).unwrap();
        let r_leaf = walk.root::<()>((ItemKey(30), b"lib_leaf"), 16).unwrap();
        let r_other = walk.root::<()>((ItemKey(40), b"lib_other"), 16).unwrap();
        let rows = [r_main, r_outer, r_leaf, r_other];

        let sym_main = walk.symbol(r_main);
        let sym_outer = walk.symbol(r_outer);
        let sym_leaf = walk.symbol(r_leaf);

        let make_code_rec = |callee: Option<Symbol>| -> Vec<u8, SharedAlloc<'static>> {
            let mut rec = CodeRecord::new_in(alloc);
            if let Some(c) = callee {
                let t = rec.target(CodeTarget { space: Space::Func, symbol: c });
                rec.code.extend_from_slice(&0x9000_0010u32.to_le_bytes());
                rec.code.extend_from_slice(&0xF940_0210u32.to_le_bytes());
                rec.code.extend_from_slice(&0xD63F_0200u32.to_le_bytes());
                rec.sites.push(Site { at: 0, kind: SiteKind::Call, target: t, addend: 0 });
            }
            rec.code.extend_from_slice(&0xD65F_03C0u32.to_le_bytes());
            let sealed = rec.seal().expect("seal");
            let mut tmp = Vec::new_in(alloc);
            sealed.encode(&mut tmp);
            let mut bytes = Vec::with_capacity_in(tmp.len(), shared);
            bytes.extend_from_slice(tmp.as_slice());
            bytes
        };
        let code_recs = [
            make_code_rec(Some(sym_outer)),
            make_code_rec(Some(sym_leaf)),
            make_code_rec(None),
            make_code_rec(None),
        ];

        let build_caller_lir = |callee_sym: Symbol, callee_item: ItemKey, callee_name: &[u8], bump: u128| -> (BuiltLir<SharedAlloc<'static>>, Vec<DeclaredRow, SharedAlloc<'static>>) {
            let mut b: Builder<SharedAlloc<'static>, Full> = Builder::new_in(64, shared);
            let sig = b.sig(Conv::C, Variadic::No, &[Param::of(blaze_lir::Ty::I64)], &[Param::of(blaze_lir::Ty::I64)]);
            b.set_signature(sig);
            let fref = b.reference(RefKind::Func, 0);
            let entry = b.create_block(BlockKind::Normal);
            b.begin(entry);
            let x = b.param(blaze_lir::Ty::I64);
            let c = b.iconst(blaze_lir::Ty::I64, bump);
            let x1 = b.binary(Op::Add, blaze_lir::Flags::NONE, x, c);
            let call_res = b.call(blaze_lir::Flags::NONE, fref, sig, &[x1]);
            b.ret(&[call_res]);
            let func = b.finish_checked().expect("valid caller");
            let mut funcs = Vec::new_in(shared);
            funcs.push(BuiltCallee::Symbol(callee_sym));
            let mut cname = Vec::new_in(shared);
            cname.extend_from_slice(callee_name);
            let mut decl = Vec::new_in(shared);
            decl.push((callee_item, cname));
            (
                BuiltLir {
                    func,
                    funcs,
                    statics: Vec::new_in(shared),
                    c_statics: Vec::new_in(shared),
                    vtables: Vec::new_in(shared),
                    vtable_data: Vec::new_in(shared),
                    tls: Vec::new_in(shared),
                    traps: Vec::new_in(shared),
                    c_text: Vec::new_in(shared),
                },
                decl,
            )
        };
        let build_leaf_lir = |addend: u128| -> (BuiltLir<SharedAlloc<'static>>, Vec<DeclaredRow, SharedAlloc<'static>>) {
            let mut b: Builder<SharedAlloc<'static>, Full> = Builder::new_in(64, shared);
            let sig = b.sig(Conv::C, Variadic::No, &[Param::of(blaze_lir::Ty::I64)], &[Param::of(blaze_lir::Ty::I64)]);
            b.set_signature(sig);
            let entry = b.create_block(BlockKind::Normal);
            b.begin(entry);
            let x = b.param(blaze_lir::Ty::I64);
            let k = b.iconst(blaze_lir::Ty::I64, addend);
            let y = b.binary(Op::Add, blaze_lir::Flags::NONE, x, k);
            b.ret(&[y]);
            let func = b.finish_checked().expect("valid leaf");
            (
                BuiltLir {
                    func,
                    funcs: Vec::new_in(shared),
                    statics: Vec::new_in(shared),
                    c_statics: Vec::new_in(shared),
                    vtables: Vec::new_in(shared),
                    vtable_data: Vec::new_in(shared),
                    tls: Vec::new_in(shared),
                    traps: Vec::new_in(shared),
                    c_text: Vec::new_in(shared),
                },
                Vec::new_in(shared),
            )
        };

        struct MockBuilder<'a> {
            sym_outer: Symbol,
            sym_leaf: Symbol,
            main_bump: u128,
            leaf_addend: u128,
            built_rows: AVec<'a, usize>,
            build_caller: &'a dyn Fn(Symbol, ItemKey, &[u8], u128) -> (BuiltLir<SharedAlloc<'static>>, Vec<DeclaredRow, SharedAlloc<'static>>),
            build_leaf: &'a dyn Fn(u128) -> (BuiltLir<SharedAlloc<'static>>, Vec<DeclaredRow, SharedAlloc<'static>>),
        }
        impl ExportLirBuilder for MockBuilder<'_> {
            fn build_lir_batch(
                &mut self,
                rows: &[usize],
                _machine: Machine,
                shared: SharedAlloc<'static>,
                _alloc: Alloc<'_>,
            ) -> Result<Vec<(BuiltLir<SharedAlloc<'static>>, Vec<DeclaredRow, SharedAlloc<'static>>), SharedAlloc<'static>>, ExportLtoError> {
                let mut out = Vec::with_capacity_in(rows.len(), shared);
                for &r in rows {
                    self.built_rows.push(r);
                    let pair = match r {
                        0 => (self.build_caller)(self.sym_outer, ItemKey(20), b"lib_outer", self.main_bump),
                        1 => (self.build_caller)(self.sym_leaf, ItemKey(30), b"lib_leaf", 2),
                        2 => (self.build_leaf)(self.leaf_addend),
                        _ => (self.build_leaf)(99),
                    };
                    out.push(pair);
                }
                Ok(out)
            }
            fn prepare_routines(&self) -> Result<Option<BoundRef<'_>>, CodeRecordError> {
                Ok(None)
            }
        }

        struct MemCache {
            lir_keys: RefCell<[Option<[u8; 32]>; 4]>,
            store: RefCell<Map<[u8; 32], Vec<u8, SharedAlloc<'static>>, FixedState, SharedAlloc<'static>>>,
            shared: SharedAlloc<'static>,
        }
        impl ExportLtoCache for MemCache {
            fn producer_digest(&self) -> Option<[u8; 32]> { Some([0x55; 32]) }
            fn lir_key(&self, row: usize, _machine: Machine) -> Option<[u8; 32]> { self.lir_keys.borrow()[row] }
            fn get(&self, key: [u8; 32], alloc: SharedAlloc<'static>) -> Option<Vec<u8, SharedAlloc<'static>>> {
                let map = self.store.borrow();
                let bytes = map.get(&key)?;
                let mut out = Vec::with_capacity_in(bytes.len(), alloc);
                out.extend_from_slice(bytes);
                Some(out)
            }
            fn put(&self, key: [u8; 32], bytes: &[u8]) {
                let mut v = Vec::with_capacity_in(bytes.len(), self.shared);
                v.extend_from_slice(bytes);
                self.store.borrow_mut().insert(key, v);
            }
            fn now_ns(&self) -> u64 { 0 }
        }

        let row_inlined = |rows: &[LtoRowResult], target_row: usize| -> Option<usize> {
            rows.iter().find_map(|r| match r {
                LtoRowResult::Cached { row, inlined_bodies, .. } if *row == target_row => Some(*inlined_bodies),
                LtoRowResult::Inline(b) if b.row == target_row => Some(b.inlined_bodies),
                _ => None,
            })
        };

        let cache = MemCache {
            lir_keys: RefCell::new([Some([1; 32]), Some([2; 32]), Some([3; 32]), Some([4; 32])]),
            store: RefCell::new(Map::new(MapOpts { hasher: FixedState, alloc: shared, capacity: None })),
            shared,
        };
        let mut opts = ExportLtoOpts::for_export(blaze_emit::Isa { lse: true, os: blaze_emit::Os::MacOs });
        opts.machine = arm64_machine();

        // 1. Cold run: all 4 rows optimize; non-workspace `lib_outer` inlines `lib_leaf`!
        let mut mb1 = MockBuilder {
            sym_outer,
            sym_leaf,
            main_bump: 1,
            leaf_addend: 7,
            built_rows: Vec::new_in(alloc),
            build_caller: &build_caller_lir,
            build_leaf: &build_leaf_lir,
        };
        let out1 = optimize_export_all_par_cached(
            &walk,
            &rows,
            |r| r == r_main,
            |r| r == r_leaf || r == r_other,
            |r| Some(code_recs[r].as_slice()),
            &mut mb1,
            opts,
            shared,
            alloc,
            &SequentialLtoRunner,
            &cache,
        )
        .expect("cold run");
        assert_eq!(out1.optimized_bodies, 4);
        assert_eq!(out1.times.emit_hits, 0);
        assert_eq!(out1.times.emit_misses, 4);
        let outer_inlined = row_inlined(out1.rows.as_slice(), r_outer).expect("lib_outer optimized");
        assert!(outer_inlined >= 1, "non-workspace lib_outer must inline lib_leaf (got {outer_inlined})");
        let main_inlined = row_inlined(out1.rows.as_slice(), r_main).expect("ws_main optimized");
        assert!(main_inlined >= 1, "ws_main must inline lib_outer (got {main_inlined})");

        // 2. Warm run: 100% BLZOLT01 cache hits, ZERO LIR loads/builds!
        let mut mb2 = MockBuilder {
            sym_outer,
            sym_leaf,
            main_bump: 1,
            leaf_addend: 7,
            built_rows: Vec::new_in(alloc),
            build_caller: &build_caller_lir,
            build_leaf: &build_leaf_lir,
        };
        let out2 = optimize_export_all_par_cached(
            &walk,
            &rows,
            |r| r == r_main,
            |r| r == r_leaf || r == r_other,
            |r| Some(code_recs[r].as_slice()),
            &mut mb2,
            opts,
            shared,
            alloc,
            &SequentialLtoRunner,
            &cache,
        )
        .expect("warm run");
        assert_eq!(out2.times.emit_hits, 4, "warm run must hit BLZOLT01 for all 4 bodies");
        assert_eq!(out2.times.emit_misses, 0);
        assert_eq!(out2.times.lir_hits, 0, "warm run must not decode BLZLIR02");
        assert_eq!(out2.times.lir_misses, 0);
        assert!(mb2.built_rows.is_empty(), "warm run must never invoke ExportLirBuilder");
        assert!(out2.rows.iter().all(|r| matches!(r, LtoRowResult::Cached { .. })), "warm hits must be zero-copy LtoRowResult::Cached");

        // 3. Edit ONLY ws_main: 3 library rows hit BLZOLT01, ONLY ws_main re-optimizes (reloading lib_outer's InlineBody from BLZOLT01)!
        cache.lir_keys.borrow_mut()[r_main] = Some([0xAA; 32]);
        let mut mb3 = MockBuilder {
            sym_outer,
            sym_leaf,
            main_bump: 42,
            leaf_addend: 7,
            built_rows: Vec::new_in(alloc),
            build_caller: &build_caller_lir,
            build_leaf: &build_leaf_lir,
        };
        let out3 = optimize_export_all_par_cached(
            &walk,
            &rows,
            |r| r == r_main,
            |r| r == r_leaf || r == r_other,
            |r| Some(code_recs[r].as_slice()),
            &mut mb3,
            opts,
            shared,
            alloc,
            &SequentialLtoRunner,
            &cache,
        )
        .expect("partial edit caller");
        assert_eq!(out3.times.emit_hits, 3, "only edited ws_main should miss BLZOLT01");
        assert_eq!(out3.times.emit_misses, 1);
        assert_eq!(mb3.built_rows.as_slice(), &[r_main], "only ws_main should build LIR");
        let main_inlined_3 = row_inlined(out3.rows.as_slice(), r_main).expect("edited ws_main re-optimized");
        assert!(main_inlined_3 >= 1, "edited ws_main must still inline cached lib_outer via BLZOLT01 InlineBody reload!");

        // 4. Edit lib_leaf: lib_leaf + lib_outer + ws_main invalidate, lib_other stays cached!
        cache.lir_keys.borrow_mut()[r_leaf] = Some([0xBB; 32]);
        let mut mb4 = MockBuilder {
            sym_outer,
            sym_leaf,
            main_bump: 42,
            leaf_addend: 13,
            built_rows: Vec::new_in(alloc),
            build_caller: &build_caller_lir,
            build_leaf: &build_leaf_lir,
        };
        let out4 = optimize_export_all_par_cached(
            &walk,
            &rows,
            |r| r == r_main,
            |r| r == r_leaf || r == r_other,
            |r| Some(code_recs[r].as_slice()),
            &mut mb4,
            opts,
            shared,
            alloc,
            &SequentialLtoRunner,
            &cache,
        )
        .expect("partial edit leaf");
        assert_eq!(out4.times.emit_hits, 1, "unrelated lib_other must stay cached on lib_leaf edit");
        assert_eq!(out4.times.emit_misses, 3);
    }
}
