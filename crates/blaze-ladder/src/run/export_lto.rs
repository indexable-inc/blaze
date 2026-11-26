//! Whole-program demand-driven cross-function LIR inlining and LTO for `blaze export`.
//!
//! 1. Starts from workspace seed instances already reached by `Walk` and performs a bounded BFS
//!    (depth <= 12, up to 1536 bodies) following normal-block non-cold `Op::Call` edges first
//!    (and `Op::Invoke` edges only with remaining budget) into inlineable callees (`core`/`std`/
//!    dependency helpers with `<= 384` raw LIR instructions, or `#[inline]` / `#[inline(always)]`
//!    up to `640` raw instructions; `#[cold]`, `-> !` noreturn, and `#[inline(never)]` callees
//!    are skipped).
//! 2. Runs deterministic bottom-up Tarjan SCC ThinLTO (`blaze_lir::lto::lto_optimize_module`) over
//!    the gathered module so iterator loops (`Range::into_iter`/`next`, `slice::Iter::next`),
//!    slice indexing (`Index::index`), wrapping/checked arithmetic, and workspace helpers inline
//!    completely across function boundaries before SOTA loop optimization.
//! 3. Transitively resolves every [`blaze_lir::inline::ImportedRef`] across all [`RefKind`]s
//!    (`Func`, `Static`, `Tls`, `Vtable`, and C externs) back to its originating body's reference
//!    tables, binds soft-float routines at collision-free tail places, and emits each optimized
//!    body into an ephemeral sealed `CodeRecord` via `emit_sota_record_for_machine` targeting the
//!    probed host [`Machine`] (`export_machine`: `LSE | DOTPROD | CRC32 | SME | SME2 | CSSC | SHA3`).
//! 4. Keeps host-specific LTO records strictly in the caller's export allocator — never writing
//!    them back to portable `CodeStore` (Rule 25).

use core::fmt;

use blaze_code::record::{Symbol, TrapSource};
use blaze_emit::Isa;
use blaze_lir::{
    Block, BlockKind, Field, Flags, Layout, Op, Ty, VerifyError,
    body::{Bodies, BodyId, DatumId, Target},
    func::{Conv, FnAttrs, Inline, ParamKind, RefId, RefKind, RefRec, SigId, SigRec},
    inline::InlinePolicy,
    lto::{ModuleBody, OptimizedBody, lto_optimize_module},
    op::{Word, walk},
    pgo::{PgoRecordError, PgoSiteTable},
    simplify::SimplifyError,
    verify,
};
use blaze_own::{AVec, Alloc, Vec};
use blaze_tier::{Arch, Machine};
use blaze_typeck::ItemKey;

use super::{BuiltCallee, BuiltLir, Maker, RecordsProgram, Typing};
use crate::body_ids::{Grant, Walk};
pub use crate::code_records::export_machine;
use crate::code_records::{Callee, CodeRecordError, InstanceNames, emit_code_record_for_machine};
use crate::run::RunError;
use crate::symbols::{static_symbol, tls_symbol};

/// Upper bound on transitive `ImportedRef` resolution hops (Rule 18).
const MAX_IMPORT_HOPS: usize = 32;
/// Exponential decay ratio (`3/4` persisted + fresh static baseline) when merging a persisted
/// cross-worktree `BPG1` PGO record into export LTO (Rule 86).
const PGO_DECAY_NUM: u64 = 3;
const PGO_DECAY_DEN: u64 = 4;
/// Baseline static weight assigned by [`PgoSiteTable::estimate_static`] to non-cold straight-line
/// bodies without loops. Only bodies strictly hotter than this baseline (either containing loops,
/// observed hot in a persisted `BPG1` profile, or directly calling a hot leaf kernel) enter
/// [`ExportLtoOut::hot_symbols`].
const STATIC_STRAIGHT_LINE_WEIGHT: u64 = 10;
/// Minimum own ranking heat (`loop_score + pgo_weight`) for a 0-call leaf body to qualify as a
/// hot leaf kernel that propagates 1-hop heat to its direct callers. A depth-1 leaf with `>= 2`
/// loop blocks (`loop_score >= 20`) or hot persisted PGO weight qualifies, while 0-loop
/// straight-line wrappers (`own_score == STATIC_STRAIGHT_LINE_WEIGHT`) stay out.
const HOT_LEAF_THRESHOLD: u64 = 20;
/// Divisor for 1-hop hot-caller heat propagation (`callee_heat / HOT_CALLER_HEAT_DIV`).
const HOT_CALLER_HEAT_DIV: u64 = 4;
/// Cap loop nesting depth exponent so `10u64.saturating_pow(depth)` stays well within `u64`.
const MAX_LOOP_DEPTH_EXP: u32 = 6;
/// Maximum distinct loop headers analyzed per function when scoring static loop heat (Rule 18).
const MAX_LOOP_HEADERS: usize = 256;
/// Hash domain for deterministic export PGO profile digests (`blaze_export::Profile::digest`).
const EXPORT_PGO_PROFILE_DOMAIN: &str = "blaze v2 export pgo profile 1";

/// Configuration for [`optimize_export_workspace`] (Rule 83: one options struct).
#[derive(Clone, Copy, Debug)]
pub struct ExportLtoOpts<'b> {
    /// Target host machine features (typically from [`export_machine`]).
    pub machine: Machine,
    /// Maximum BFS hop depth from workspace seed functions into inlineable callees.
    pub max_depth: u8,
    /// Maximum total function bodies gathered into the LTO module.
    pub max_bodies: usize,
    /// Maximum LIR instruction count for keeping a non-workspace callee in the LTO module.
    pub max_callee_insts: usize,
    /// Maximum LIR instruction count for keeping a `#[inline(always)]` non-workspace callee.
    pub max_always_inline_insts: usize,
    /// Cross-function inlining policy handed to `blaze_lir::lto::lto_optimize_module`.
    pub policy: InlinePolicy,
    /// Optional persisted `BPG1` PGO binary record merged with static cold-start estimates
    /// via exponential decay before LTO inlining and hot-function layout ranking.
    pub pgo_bytes: Option<&'b [u8]>,
}

impl ExportLtoOpts<'static> {
    /// Release export defaults: bounded demand-driven BFS (`depth <= 16`, `<= 4096` bodies,
    /// raw non-workspace callees `<= 384` LIR insts / `#[inline]` & `#[inline(always)]` `<= 640`)
    /// and aggressive cross-function inlining.
    pub fn for_export(isa: Isa) -> Self {
        Self {
            machine: export_machine(isa),
            max_depth: 16,
            max_bodies: 4096,
            max_callee_insts: 384,
            max_always_inline_insts: 640,
            policy: InlinePolicy {
                cost_threshold: 120,
                max_callee_insts: 256,
                hot_multiplier: 4,
                hot_weight_threshold: 10,
                max_depth: 16,
                max_growth: 4096,
                allow_external_refs: true,
                allow_loops: true,
            },
            pgo_bytes: None,
        }
    }
}

/// Why whole-program export LTO stopped, naming the symbol and rule (Rule 19, Rule 3).
#[derive(Debug)]
pub enum ExportLtoError {
    /// A reached instance row lacked a runnable grant when rebuilding LIR.
    NotRunnable { symbol: [u8; 32], tabled: ItemKey },
    /// Building LIR for one reached instance failed.
    BuildLir { symbol: [u8; 32], error: RunError },
    /// Decoding an explicit persisted `BPG1` PGO record failed.
    PgoRecord(PgoRecordError),
    /// `blaze_lir::lto::lto_optimize_module` refused the module.
    Optimize(SimplifyError),
    /// A post-LTO function failed LIR verification.
    Verify { symbol: [u8; 32], error: VerifyError },
    /// An `ImportedRef` pointed to a body not present in the LTO module.
    UnresolvedImport { symbol: [u8; 32], source: [u8; 32] },
    /// A transitive `ImportedRef` chain exceeded [`MAX_IMPORT_HOPS`].
    ImportCycle { symbol: [u8; 32] },
    /// An `ImportedRef` named a reference id or place outside its originating body's table.
    BadRef { symbol: [u8; 32], what: &'static str, ref_id: u32 },
    /// An `ImportedRef` kind disagreed with its originating body's `RefRec` kind.
    RefKindMismatch { symbol: [u8; 32], ref_id: u32 },
    /// Emitting or sealing the post-LTO `CodeRecord` failed.
    Record { symbol: [u8; 32], error: CodeRecordError },
}

struct Hex<'a>(&'a [u8; 32]);

impl fmt::Display for Hex<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0.iter() {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Display for ExportLtoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExportLtoError::NotRunnable { symbol, tabled } => write!(
                f,
                "export LTO: reached fn {:#x} (symbol {}) has no runnable proof (export_lto.rs)",
                tabled.0,
                Hex(symbol)
            ),
            ExportLtoError::BuildLir { symbol, error } => {
                write!(f, "export LTO: building LIR for {}: {error} (export_lto.rs)", Hex(symbol))
            }
            ExportLtoError::PgoRecord(error) => {
                write!(f, "export LTO: persisted BPG1 PGO record refused: {error} (export_lto.rs)")
            }
            ExportLtoError::Optimize(error) => {
                write!(f, "export LTO: module inlining failed: {error} (export_lto.rs)")
            }
            ExportLtoError::Verify { symbol, error } => {
                write!(f, "export LTO: post-LTO LIR failed verify for {}: {error} (export_lto.rs)", Hex(symbol))
            }
            ExportLtoError::UnresolvedImport { symbol, source } => write!(
                f,
                "export LTO: imported ref in {} names unknown source body {} (export_lto.rs)",
                Hex(symbol),
                Hex(source)
            ),
            ExportLtoError::ImportCycle { symbol } => write!(
                f,
                "export LTO: transitive imported ref chain in {} exceeded {MAX_IMPORT_HOPS} hops (export_lto.rs)",
                Hex(symbol)
            ),
            ExportLtoError::BadRef { symbol, what, ref_id } => write!(
                f,
                "export LTO: bad {what} (ref {ref_id}) while resolving imports in {} (export_lto.rs)",
                Hex(symbol)
            ),
            ExportLtoError::RefKindMismatch { symbol, ref_id } => write!(
                f,
                "export LTO: imported ref {ref_id} in {} disagreed with originating RefRec kind (export_lto.rs)",
                Hex(symbol)
            ),
            ExportLtoError::Record { symbol, error } => {
                write!(f, "export LTO: emitting code record for {}: {error} (export_lto.rs)", Hex(symbol))
            }
        }
    }
}

impl core::error::Error for ExportLtoError {}

/// One LTO-optimized and re-emitted function record for `blaze export`.
pub struct LtoBodyOut<'a> {
    /// Row index in `Walk`.
    pub row: usize,
    /// Instance symbol.
    pub symbol: Symbol,
    /// Encoded sealed canonical `CodeRecord` bytes (ready for `CodeView::read` and
    /// `materialize_positionless`).
    pub record: AVec<'a, u8>,
    /// Encoded vtable datums referenced by this body or any of its inlined callees.
    pub vtable_data: AVec<'a, (Symbol, AVec<'a, u8>)>,
    /// Any `SoftFloat` routines that legalization called during emission, as `(tabled, name_bytes)`.
    pub extra_declared: AVec<'a, (ItemKey, AVec<'a, u8>)>,
    /// Count of callee bodies spliced into this function.
    pub inlined_bodies: usize,
}

/// Output of [`optimize_export_workspace`].
pub struct ExportLtoOut<'a> {
    /// Optimized functions replacing their walk rows in the exported binary.
    pub bodies: AVec<'a, LtoBodyOut<'a>>,
    /// Hot module symbols ranked hottest-first for `blaze_export::Profile::hot`
    /// (16 KB iTLB-page-aware Ext-TSP code packing).
    pub hot_symbols: AVec<'a, Symbol>,
    /// Deterministic BLAKE3 digest of the PGO profile and hot-symbol order for
    /// `blaze_export::Profile::digest`.
    pub profile_digest: [u8; 32],
    /// Total bodies gathered into the LTO module.
    pub module_bodies: usize,
    /// Total callee bodies spliced across the module.
    pub total_inlined: usize,
}

struct LtoOrigin<'a> {
    row: usize,
    symbol: Symbol,
    is_seed: bool,
    lowered_libm: bool,
    orig_unreachables: usize,
    built: BuiltLir<'a>,
}

struct LtoBodies<'a, 'b> {
    origins: &'b [LtoOrigin<'a>],
    by_sym: &'b [([u8; 32], usize)],
}

fn find_body_idx(by_sym: &[([u8; 32], usize)], sym: [u8; 32]) -> Option<usize> {
    let pos = by_sym.binary_search_by(|row| row.0.cmp(&sym)).ok()?;
    by_sym.get(pos).map(|row| row.1)
}

impl Bodies for LtoBodies<'_, '_> {
    fn callee(&self, from: BodyId, reference: RefId) -> Option<(BodyId, blaze_lir::View<'_>)> {
        let caller_idx = find_body_idx(self.by_sym, from.0)?;
        let caller = self.origins.get(caller_idx)?;
        let rec = caller.built.func.view().refs.get(reference.index())?;
        if RefKind::from_word(rec.kind)? != RefKind::Func {
            return None;
        }
        let place = usize::try_from(rec.place).ok()?;
        let BuiltCallee::Symbol(callee_sym) = *caller.built.funcs.get(place)? else {
            return None;
        };
        let callee_idx = find_body_idx(self.by_sym, callee_sym.0)?;
        let callee = self.origins.get(callee_idx)?;
        Some((BodyId(callee_sym.0), callee.built.func.view()))
    }

    fn target(&self, from: BodyId, reference: RefId) -> Option<Target> {
        let caller_idx = find_body_idx(self.by_sym, from.0)?;
        let caller = self.origins.get(caller_idx)?;
        let rec = caller.built.func.view().refs.get(reference.index())?;
        let kind = RefKind::from_word(rec.kind)?;
        let place = usize::try_from(rec.place).ok()?;
        match kind {
            RefKind::Func => match *caller.built.funcs.get(place)? {
                BuiltCallee::Symbol(sym) => Some(Target::Func(BodyId(sym.0))),
                BuiltCallee::C { .. } => None,
            },
            RefKind::Static => {
                let &item = caller.built.statics.get(place)?;
                if caller.built.foreign_static_name(item).is_some() {
                    None
                } else {
                    Some(Target::Datum(DatumId(static_symbol(item).0)))
                }
            }
            RefKind::Vtable => {
                let &sym = caller.built.vtables.get(place)?;
                Some(Target::Datum(DatumId(sym.0)))
            }
            RefKind::Tls => {
                let &item = caller.built.tls.get(place)?;
                Some(Target::Datum(DatumId(tls_symbol(item).0)))
            }
            RefKind::Data => None,
        }
    }

    fn body(&self, id: BodyId) -> Option<blaze_lir::View<'_>> {
        let idx = find_body_idx(self.by_sym, id.0)?;
        self.origins.get(idx).map(|o| o.built.func.view())
    }
}

/// Transitively resolves an `ImportedRef` (`start_idx`, `start_ref`) back through bottom-up SCC
/// inlining layers to its originating body index and pre-LTO `RefRec`.
fn resolve_ref_origin<'a>(
    start_idx: usize,
    start_ref: RefId,
    lto_bodies: &[OptimizedBody<Alloc<'a>>],
    origins: &[LtoOrigin<'a>],
    by_sym: &[([u8; 32], usize)],
) -> Result<(usize, RefRec), ExportLtoError> {
    let start_sym = origins.get(start_idx).map_or([0u8; 32], |o| o.symbol.0);
    let mut cur_idx = start_idx;
    let mut cur_ref = start_ref;
    for _ in 0..=MAX_IMPORT_HOPS {
        let Some(opt) = lto_bodies.get(cur_idx) else {
            return Err(ExportLtoError::BadRef { symbol: start_sym, what: "LTO body index", ref_id: cur_ref.0 });
        };
        if let Ok(pos) = opt.imported_refs.binary_search_by_key(&cur_ref, |r| r.caller_ref)
            && let Some(imp) = opt.imported_refs.get(pos)
        {
            let Some(next_idx) = find_body_idx(by_sym, imp.source_body.0) else {
                return Err(ExportLtoError::UnresolvedImport { symbol: start_sym, source: imp.source_body.0 });
            };
            cur_idx = next_idx;
            cur_ref = imp.source_ref;
            continue;
        }
        let Some(orig) = origins.get(cur_idx) else {
            return Err(ExportLtoError::BadRef { symbol: start_sym, what: "origin body index", ref_id: cur_ref.0 });
        };
        let Some(&rec) = orig.built.func.view().refs.get(cur_ref.index()) else {
            return Err(ExportLtoError::BadRef { symbol: start_sym, what: "origin RefId", ref_id: cur_ref.0 });
        };
        return Ok((cur_idx, rec));
    }
    Err(ExportLtoError::ImportCycle { symbol: start_sym })
}

/// Writes `val` at `place` in `table` (overwriting an unused slot `< len` or appending at `len`).
fn put_at<'a, T>(
    table: &mut AVec<'a, T>,
    place: usize,
    val: T,
    symbol: [u8; 32],
    what: &'static str,
    ref_id: u32,
) -> Result<(), ExportLtoError> {
    if let Some(slot) = table.get_mut(place) {
        *slot = val;
        return Ok(());
    }
    if place == table.len() {
        table.push(val);
        return Ok(());
    }
    Err(ExportLtoError::BadRef { symbol, what, ref_id })
}

/// Appends `(symbol, bytes)` to `out` if `symbol` is not already present.
fn push_vtable_datum<'a>(out: &mut AVec<'a, (Symbol, AVec<'a, u8>)>, symbol: Symbol, bytes: &[u8], alloc: Alloc<'a>) {
    if out.iter().any(|(s, _)| *s == symbol) {
        return;
    }
    let mut copied: AVec<'a, u8> = Vec::with_capacity_in(bytes.len(), alloc);
    copied.extend_from_slice(bytes);
    out.push((symbol, copied));
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct LibmOp {
    op: Op,
    ty: Ty,
    arity: u8,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct LoweredLibmCall {
    op: Op,
    ty: Ty,
    arity: u8,
    args: [u32; 3],
    /// `Some(normal_blk)` when lowering an `Op::Invoke` terminator (emits `Op::Jump(normal_blk)`
    /// right after the native float op); `None` when lowering an `Op::Call`.
    invoke_normal: Option<u32>,
}

fn op_byte(op: Op) -> Option<u8> {
    let idx = Op::ALL.iter().position(|&o| o == op)?;
    u8::try_from(idx).ok()
}

/// Maps an IEEE-754 C libm symbol name (with or without a leading `_`) to its exact hardware LIR
/// op, gated strictly to ops supported by `blaze-emit`'s `sota` backend for `machine`.
///
/// (`Op::Fcopysign` is intentionally omitted because neither `sota/isel.rs` nor `sota/x86_64.rs`
/// supports it yet.)
fn match_ieee_libm(raw_name: &str, machine: Machine) -> Option<LibmOp> {
    let name = raw_name.strip_prefix('_').unwrap_or(raw_name);
    let candidate = match name {
        "fmaf" => LibmOp { op: Op::Fma, ty: Ty::F32, arity: 3 },
        "fma" => LibmOp { op: Op::Fma, ty: Ty::F64, arity: 3 },
        "sqrtf" => LibmOp { op: Op::Sqrt, ty: Ty::F32, arity: 1 },
        "sqrt" => LibmOp { op: Op::Sqrt, ty: Ty::F64, arity: 1 },
        "fabsf" => LibmOp { op: Op::Fabs, ty: Ty::F32, arity: 1 },
        "fabs" => LibmOp { op: Op::Fabs, ty: Ty::F64, arity: 1 },
        "floorf" => LibmOp { op: Op::Floor, ty: Ty::F32, arity: 1 },
        "floor" => LibmOp { op: Op::Floor, ty: Ty::F64, arity: 1 },
        "ceilf" => LibmOp { op: Op::Ceil, ty: Ty::F32, arity: 1 },
        "ceil" => LibmOp { op: Op::Ceil, ty: Ty::F64, arity: 1 },
        "truncf" => LibmOp { op: Op::Ftrunc, ty: Ty::F32, arity: 1 },
        "trunc" => LibmOp { op: Op::Ftrunc, ty: Ty::F64, arity: 1 },
        "rintf" | "nearbyintf" | "roundevenf" => LibmOp { op: Op::Nearest, ty: Ty::F32, arity: 1 },
        "rint" | "nearbyint" | "roundeven" => LibmOp { op: Op::Nearest, ty: Ty::F64, arity: 1 },
        "roundf" => LibmOp { op: Op::Fround, ty: Ty::F32, arity: 1 },
        "round" => LibmOp { op: Op::Fround, ty: Ty::F64, arity: 1 },
        _ => return None,
    };
    let supported = match machine.arch {
        Arch::Arm64 => true,
        Arch::X86_64 => candidate.op == Op::Fma && (machine.features & Machine::FMA) != 0,
    };
    if supported { Some(candidate) } else { None }
}

/// Checks whether instruction `inst` is a non-variadic `Conv::C` `Op::Call` or `Op::Invoke` to an
/// exact IEEE-754 libm function supported by `sota` on `machine` with matching signature and types.
fn detect_libm_call(built: &BuiltLir<'_>, inst: usize, machine: Machine) -> Option<LoweredLibmCall> {
    let view = built.func.view();
    let op = Op::from_byte(*view.ops.get(inst)?)?;
    let is_invoke = match op {
        Op::Call => false,
        Op::Invoke => true,
        _ => return None,
    };
    let extra_start = usize::try_from(*view.data.get(inst)?).ok()?;
    let ref_word = *view.extra.get(extra_start)?;
    let sig_word = *view.extra.get(extra_start.checked_add(1)?)?;
    let (invoke_normal, args_len_pos) = if is_invoke {
        let normal_blk = *view.extra.get(extra_start.checked_add(2)?)?;
        let normal_idx = usize::try_from(normal_blk).ok()?;
        if normal_blk == 0 || normal_idx >= view.blocks() || normal_idx >= view.block_kinds.len() {
            return None;
        }
        let normal_block = Block(normal_blk);
        if view.block_kind(normal_block).is_landing() || !view.params(normal_block).is_empty() {
            return None;
        }
        (Some(normal_blk), extra_start.checked_add(4)?)
    } else {
        (None, extra_start.checked_add(2)?)
    };

    let ref_rec = view.refs.get(RefId(ref_word).index())?;
    if RefKind::from_word(ref_rec.kind) != Some(RefKind::Func) {
        return None;
    }
    let place_idx = usize::try_from(ref_rec.place).ok()?;
    let BuiltCallee::C { start, len } = *built.funcs.get(place_idx)? else {
        return None;
    };
    let c_name = built.c_str(start, len)?;
    let libm = match_ieee_libm(c_name, machine)?;
    let arity = usize::from(libm.arity);

    let sig_rec = view.sigs.get(SigId(sig_word).index())?;
    if Conv::from_byte(sig_rec.conv) != Some(Conv::C)
        || (sig_rec.bits & SigRec::VARIADIC) != 0
        || usize::from(sig_rec.params) != arity
        || sig_rec.rets != 1
    {
        return None;
    }
    let sig_first = usize::try_from(sig_rec.first).ok()?;
    let sig_ret_pos = sig_first.checked_add(arity)?;
    let sig_params = view.sig_params.get(sig_first..sig_ret_pos)?;
    let sig_ret = view.sig_params.get(sig_ret_pos)?;
    for p in sig_params {
        if Ty(p.ty) != libm.ty || ParamKind::from_byte(p.kind) != Some(ParamKind::Value) {
            return None;
        }
    }
    if Ty(sig_ret.ty) != libm.ty || ParamKind::from_byte(sig_ret.kind) != Some(ParamKind::Value) {
        return None;
    }

    if Ty(*view.tys.get(inst)?) != libm.ty {
        return None;
    }
    let argc = usize::try_from(*view.extra.get(args_len_pos)?).ok()?;
    if argc != arity {
        return None;
    }
    let args_start = args_len_pos.checked_add(1)?;
    let arg_words = view.extra.get(args_start..args_start.checked_add(arity)?)?;
    let mut args = [0u32; 3];
    for (k, &v) in arg_words.iter().enumerate() {
        let v_idx = usize::try_from(v).ok()?;
        if Ty(*view.tys.get(v_idx)?) != libm.ty {
            return None;
        }
        if let Some(slot) = args.get_mut(k) {
            *slot = v;
        }
    }

    Some(LoweredLibmCall {
        op: libm.op,
        ty: libm.ty,
        arity: libm.arity,
        args,
        invoke_normal,
    })
}

fn remap_val(val_map: &[u32], old: u32) -> Option<u32> {
    let idx = usize::try_from(old).ok()?;
    val_map.get(idx).copied()
}

fn copy_remapped_target<'a>(
    old_extra: &[u32],
    at: &mut usize,
    val_map: &[u32],
    out_extra: &mut AVec<'a, u32>,
) -> Option<()> {
    let block = *old_extra.get(*at)?;
    *at = at.checked_add(1)?;
    out_extra.push(block);
    let n_word = *old_extra.get(*at)?;
    *at = at.checked_add(1)?;
    out_extra.push(n_word);
    let n = usize::try_from(n_word).ok()?;
    for _ in 0..n {
        let v = *old_extra.get(*at)?;
        *at = at.checked_add(1)?;
        out_extra.push(remap_val(val_map, v)?);
    }
    Some(())
}

fn copy_remapped_extra<'a>(
    old_extra: &[u32],
    start: usize,
    fields: &[Field],
    ty: Ty,
    old_tys: &[u16],
    val_map: &[u32],
    out_extra: &mut AVec<'a, u32>,
) -> Option<()> {
    let mut at = start;
    let mut last_value: Option<u32> = None;
    for &field in fields {
        match field {
            Field::Value => {
                let v = *old_extra.get(at)?;
                at = at.checked_add(1)?;
                last_value = Some(v);
                out_extra.push(remap_val(val_map, v)?);
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
                let w = *old_extra.get(at)?;
                at = at.checked_add(1)?;
                out_extra.push(w);
            }
            Field::Target => {
                copy_remapped_target(old_extra, &mut at, val_map, out_extra)?;
            }
            Field::Values => {
                let n_word = *old_extra.get(at)?;
                at = at.checked_add(1)?;
                out_extra.push(n_word);
                let n = usize::try_from(n_word).ok()?;
                for _ in 0..n {
                    let v = *old_extra.get(at)?;
                    at = at.checked_add(1)?;
                    out_extra.push(remap_val(val_map, v)?);
                }
            }
            Field::Blocks | Field::Mask => {
                let n_word = *old_extra.get(at)?;
                at = at.checked_add(1)?;
                out_extra.push(n_word);
                let n = usize::try_from(n_word).ok()?;
                for _ in 0..n {
                    let w = *old_extra.get(at)?;
                    at = at.checked_add(1)?;
                    out_extra.push(w);
                }
            }
            Field::Const => {
                let words = usize::try_from(ty.const_words()).ok()?;
                for _ in 0..words {
                    let w = *old_extra.get(at)?;
                    at = at.checked_add(1)?;
                    out_extra.push(w);
                }
            }
            Field::Cases => {
                let scrutinee = last_value?;
                let scrut_idx = usize::try_from(scrutinee).ok()?;
                let scrut_ty = Ty(*old_tys.get(scrut_idx)?);
                let width = usize::try_from(scrut_ty.const_words()).ok()?;
                let n_word = *old_extra.get(at)?;
                at = at.checked_add(1)?;
                out_extra.push(n_word);
                let n = usize::try_from(n_word).ok()?;
                for _ in 0..n {
                    for _ in 0..width {
                        let w = *old_extra.get(at)?;
                        at = at.checked_add(1)?;
                        out_extra.push(w);
                    }
                    copy_remapped_target(old_extra, &mut at, val_map, out_extra)?;
                }
            }
        }
    }
    Some(())
}

/// Lowers hardware-exact IEEE-754 libm `Op::Call` / `Op::Invoke` instructions in `built` to native
/// LIR float ops supported by `sota` on `machine`, rebuilding `extra` contiguously and preserving
/// SSA numbering across `Op::Invoke -> [Op::Fma/Sqrt/..., Op::Jump(normal)]` expansions.
///
/// Running this immediately after `maker.build_lir` turns `std::f32::mul_add` / `std::f64::mul_add`
/// / `sqrt` wrappers into tiny unwind-free pure leaves that `lto_optimize_module` inlines directly
/// into caller loops (exposing `Op::Fma` to `gemm.rs` SME2 `FMOPA.S`/`FMOPA.D`).
/// Returns `true` iff at least one call/invoke was lowered.
fn lower_ieee_libm_calls<'a>(built: &mut BuiltLir<'a>, machine: Machine, alloc: Alloc<'a>) -> bool {
    let n = built.func.ops.len();
    if n == 0 || built.funcs.is_empty() || built.c_text.is_empty() {
        return false;
    }

    // Fast path: scan before allocating any rewrite buffers.
    let mut first_match: Option<(usize, LoweredLibmCall)> = None;
    for i in 0..n {
        if let Some(hit) = detect_libm_call(built, i, machine) {
            first_match = Some((i, hit));
            break;
        }
    }
    let Some((first_idx, first_hit)) = first_match else {
        return false;
    };

    let mut rewrites: AVec<'a, Option<LoweredLibmCall>> = Vec::with_capacity_in(n, alloc);
    rewrites.resize(n, None);
    if let Some(slot) = rewrites.get_mut(first_idx) {
        *slot = Some(first_hit);
    }
    let mut extra_insts = usize::from(first_hit.invoke_normal.is_some());
    for i in (first_idx + 1)..n {
        if let Some(hit) = detect_libm_call(built, i, machine) {
            if hit.invoke_normal.is_some() {
                extra_insts = extra_insts.saturating_add(1);
            }
            if let Some(slot) = rewrites.get_mut(i) {
                *slot = Some(hit);
            }
        }
    }

    let mut val_map: AVec<'a, u32> = Vec::with_capacity_in(n, alloc);
    let mut next_inst = 0u32;
    for i in 0..n {
        val_map.push(next_inst);
        let Some(step1) = next_inst.checked_add(1) else {
            return false;
        };
        next_inst = step1;
        if matches!(rewrites.get(i), Some(Some(r)) if r.invoke_normal.is_some()) {
            let Some(step2) = next_inst.checked_add(1) else {
                return false;
            };
            next_inst = step2;
        }
    }

    let Some(jump_byte) = op_byte(Op::Jump) else {
        return false;
    };
    let new_cap = n.saturating_add(extra_insts);
    let mut new_ops: AVec<'a, u8> = Vec::with_capacity_in(new_cap, alloc);
    let mut new_flags: AVec<'a, u8> = Vec::with_capacity_in(new_cap, alloc);
    let mut new_tys: AVec<'a, u16> = Vec::with_capacity_in(new_cap, alloc);
    let mut new_data: AVec<'a, u32> = Vec::with_capacity_in(new_cap, alloc);
    let mut new_extra: AVec<'a, u32> = Vec::with_capacity_in(built.func.extra.len(), alloc);
    let mut new_attrs: AVec<'a, u32> = Vec::with_capacity_in(new_cap, alloc);
    let mut new_locs: AVec<'a, u32> = Vec::with_capacity_in(new_cap, alloc);

    {
        let view = built.func.view();
        for i in 0..n {
            let Some(&old_loc) = view.locs.get(i) else {
                return false;
            };
            if let Some(Some(hit)) = rewrites.get(i).copied() {
                let Some(byte) = op_byte(hit.op) else {
                    return false;
                };
                let data_word = if hit.arity == 1 {
                    let [a0_old, ..] = hit.args;
                    let Some(a0) = remap_val(val_map.as_slice(), a0_old) else {
                        return false;
                    };
                    a0
                } else {
                    let Ok(pos) = u32::try_from(new_extra.len()) else {
                        return false;
                    };
                    for k in 0..usize::from(hit.arity) {
                        let Some(&arg) = hit.args.get(k) else {
                            return false;
                        };
                        let Some(mapped) = remap_val(val_map.as_slice(), arg) else {
                            return false;
                        };
                        new_extra.push(mapped);
                    }
                    pos
                };
                new_ops.push(byte);
                new_flags.push(Flags::NONE.0);
                new_tys.push(hit.ty.0);
                new_data.push(data_word);
                new_attrs.push(0);
                new_locs.push(old_loc);

                if let Some(normal_blk) = hit.invoke_normal {
                    let Ok(jump_pos) = u32::try_from(new_extra.len()) else {
                        return false;
                    };
                    new_ops.push(jump_byte);
                    new_flags.push(Flags::NONE.0);
                    new_tys.push(Ty::NONE.0);
                    new_data.push(jump_pos);
                    new_attrs.push(0);
                    new_locs.push(old_loc);
                    new_extra.push(normal_blk);
                    new_extra.push(0);
                }
                continue;
            }

            let Some(&old_op_byte) = view.ops.get(i) else {
                return false;
            };
            let Some(old_op) = Op::from_byte(old_op_byte) else {
                return false;
            };
            let Some(&old_flag) = view.flags.get(i) else {
                return false;
            };
            let Some(&old_ty_raw) = view.tys.get(i) else {
                return false;
            };
            let Some(&old_data) = view.data.get(i) else {
                return false;
            };
            let Some(&old_attr) = view.attrs.get(i) else {
                return false;
            };

            let new_data_word = match old_op.info().layout {
                Layout::None => 0,
                Layout::Data(Field::Value) => {
                    let Some(mapped) = remap_val(val_map.as_slice(), old_data) else {
                        return false;
                    };
                    mapped
                }
                Layout::Data(_) => old_data,
                Layout::Extra(fields) => {
                    let Ok(pos) = u32::try_from(new_extra.len()) else {
                        return false;
                    };
                    let Ok(old_start) = usize::try_from(old_data) else {
                        return false;
                    };
                    if copy_remapped_extra(
                        view.extra,
                        old_start,
                        fields,
                        Ty(old_ty_raw),
                        view.tys,
                        val_map.as_slice(),
                        &mut new_extra,
                    )
                    .is_none()
                    {
                        return false;
                    }
                    pos
                }
            };

            new_ops.push(old_op_byte);
            new_flags.push(old_flag);
            new_tys.push(old_ty_raw);
            new_data.push(new_data_word);
            new_attrs.push(old_attr);
            new_locs.push(old_loc);
        }
    }

    let mut new_block_starts: AVec<'a, u32> = Vec::with_capacity_in(built.func.block_starts.len(), alloc);
    for &old_start in built.func.block_starts.iter() {
        let Some(mapped) = remap_val(val_map.as_slice(), old_start) else {
            return false;
        };
        new_block_starts.push(mapped);
    }

    built.func.ops.clear();
    built.func.ops.extend_from_slice(new_ops.as_slice());
    built.func.flags.clear();
    built.func.flags.extend_from_slice(new_flags.as_slice());
    built.func.tys.clear();
    built.func.tys.extend_from_slice(new_tys.as_slice());
    built.func.data.clear();
    built.func.data.extend_from_slice(new_data.as_slice());
    built.func.extra.clear();
    built.func.extra.extend_from_slice(new_extra.as_slice());
    built.func.attrs.clear();
    built.func.attrs.extend_from_slice(new_attrs.as_slice());
    built.func.locs.clear();
    built.func.locs.extend_from_slice(new_locs.as_slice());
    built.func.block_starts.clear();
    built.func.block_starts.extend_from_slice(new_block_starts.as_slice());
    true
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct LoopHeat {
    max_depth: u32,
    loop_blocks: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum HotTier {
    /// Other hot functions (loops or persisted PGO weight above baseline).
    Other = 0,
    /// Non-cold normal-returning direct callers of at least one hot 0-call leaf kernel.
    HotLeafCaller = 1,
    /// Hot 0-call leaf kernels (packed first into pages 0..2).
    HotLeaf = 2,
}

#[derive(Clone, Copy)]
struct BodyHeat {
    symbol: Symbol,
    is_seed: bool,
    eligible: bool,
    can_receive_caller_bonus: bool,
    is_hot_leaf: bool,
    has_own_heat: bool,
    own_score: u64,
}

#[derive(Clone, Copy)]
struct HotCandidate {
    symbol: Symbol,
    tier: HotTier,
    score: u64,
    is_seed: bool,
}

/// Builds the module's PGO weight table from static cold-start estimates, exponentially
/// decay-merged with an explicit persisted `BPG1` record when provided (Rule 86).
fn build_pgo_table<'s>(
    mod_bodies: &[ModuleBody<Alloc<'s>>],
    pgo_bytes: Option<&[u8]>,
    scratch: Alloc<'s>,
) -> Result<PgoSiteTable<Alloc<'s>>, ExportLtoError> {
    let static_pgo = PgoSiteTable::estimate_static(mod_bodies, scratch);
    match pgo_bytes {
        Some(bytes) => {
            let persisted = PgoSiteTable::decode(bytes, scratch).map_err(ExportLtoError::PgoRecord)?;
            Ok(persisted.merge_decayed(&static_pgo, PGO_DECAY_NUM, PGO_DECAY_DEN, scratch))
        }
        None => Ok(static_pgo),
    }
}

fn for_each_normal_succ(view: blaze_lir::View<'_>, b: usize, mut visit: impl FnMut(usize)) {
    let Ok(b_u32) = u32::try_from(b) else {
        return;
    };
    let block = Block(b_u32);
    if view.block_kind(block) != BlockKind::Normal {
        return;
    }
    let range = view.block(block);
    let Some(at) = range.end.checked_sub(1) else {
        return;
    };
    if at < range.start {
        return;
    }
    let op = view.op(at);
    let Layout::Extra(fields) = op.info().layout else {
        return;
    };
    let Some(&data_at) = view.data.get(at) else {
        return;
    };
    let Ok(extra_start) = usize::try_from(data_at) else {
        return;
    };
    let Some(words) = view.extra.get(extra_start..) else {
        return;
    };
    let Some(&ty_raw) = view.tys.get(at) else {
        return;
    };
    let mut on_word = |word: Word| -> Result<(), core::convert::Infallible> {
        if let Word::Block(to) = word
            && let Ok(to_idx) = usize::try_from(to)
            && to_idx < view.blocks()
            && view.block_kind(Block(to)) == BlockKind::Normal
        {
            visit(to_idx);
        }
        Ok(())
    };
    let _ = walk(words, fields, Ty(ty_raw), view.tys, &mut on_word);
}

#[inline]
fn is_dfs_descendant(dfs_in: &[u32], dfs_out: &[u32], ancestor: usize, node: usize) -> bool {
    let (Some(&a_in), Some(&a_out), Some(&n_in), Some(&n_out)) = (
        dfs_in.get(ancestor),
        dfs_out.get(ancestor),
        dfs_in.get(node),
        dfs_out.get(node),
    ) else {
        return false;
    };
    a_in != u32::MAX && n_in != u32::MAX && a_in <= n_in && n_out <= a_out
}

/// Computes static loop nesting depth and loop block count over reachable `BlockKind::Normal`
/// blocks of `view` using iterative DFS back-edge detection and bounded natural-loop reverse BFS.
fn compute_loop_heat<'s>(view: blaze_lir::View<'_>, scratch: Alloc<'s>) -> LoopHeat {
    let blocks = view.blocks();
    if blocks == 0 || view.block_kind(Block(0)) != BlockKind::Normal {
        return LoopHeat { max_depth: 0, loop_blocks: 0 };
    }

    // 1. Build deduplicated forward CSR adjacency over normal blocks.
    let mut succ_head: AVec<'s, usize> = Vec::with_capacity_in(blocks.saturating_add(1), scratch);
    let mut succ_edges: AVec<'s, usize> = Vec::with_capacity_in(blocks.saturating_mul(2), scratch);
    succ_head.push(0);
    for b in 0..blocks {
        let edge_start = succ_edges.len();
        for_each_normal_succ(view, b, |to| succ_edges.push(to));
        if let Some(slice) = succ_edges.get_mut(edge_start..) {
            slice.sort_unstable();
        }
        let mut write = edge_start;
        for read in edge_start..succ_edges.len() {
            let Some(&edge) = succ_edges.get(read) else {
                break;
            };
            let is_dup = write > edge_start
                && write.checked_sub(1).and_then(|prev| succ_edges.get(prev)).copied() == Some(edge);
            if !is_dup {
                if let Some(slot) = succ_edges.get_mut(write) {
                    *slot = edge;
                }
                write = write.saturating_add(1);
            }
        }
        succ_edges.truncate(write);
        succ_head.push(succ_edges.len());
    }

    // 2. Iterative DFS from entry block `0` to compute `(dfs_in, dfs_out)` intervals.
    let mut dfs_in: AVec<'s, u32> = Vec::with_capacity_in(blocks, scratch);
    dfs_in.resize(blocks, u32::MAX);
    let mut dfs_out: AVec<'s, u32> = Vec::with_capacity_in(blocks, scratch);
    dfs_out.resize(blocks, 0);
    let mut stack: AVec<'s, (usize, usize)> = Vec::with_capacity_in(blocks, scratch);
    let mut time = 0u32;
    if let Some(slot) = dfs_in.get_mut(0) {
        *slot = time;
        time = time.saturating_add(1);
    }
    stack.push((0usize, succ_head.first().copied().unwrap_or(0)));
    while let Some((u, edge_idx)) = stack.last().copied() {
        let end = succ_head.get(u.saturating_add(1)).copied().unwrap_or(0);
        if edge_idx < end {
            if let Some(top) = stack.last_mut() {
                top.1 = edge_idx.saturating_add(1);
            }
            if let Some(&v) = succ_edges.get(edge_idx)
                && dfs_in.get(v).copied() == Some(u32::MAX)
            {
                if let Some(slot) = dfs_in.get_mut(v) {
                    *slot = time;
                    time = time.saturating_add(1);
                }
                let v_start = succ_head.get(v).copied().unwrap_or(0);
                stack.push((v, v_start));
            }
        } else {
            if let Some(slot) = dfs_out.get_mut(u) {
                *slot = time;
                time = time.saturating_add(1);
            }
            stack.pop();
        }
    }

    // 3. Gather DFS back-edges `(header, latch)` sorted by header for deterministic merging.
    let mut back_edges: AVec<'s, (usize, usize)> = Vec::new_in(scratch);
    for u in 0..blocks {
        if dfs_in.get(u).copied().unwrap_or(u32::MAX) == u32::MAX {
            continue;
        }
        let start = succ_head.get(u).copied().unwrap_or(0);
        let end = succ_head.get(u.saturating_add(1)).copied().unwrap_or(start);
        if let Some(slice) = succ_edges.get(start..end) {
            for &h in slice {
                if is_dfs_descendant(dfs_in.as_slice(), dfs_out.as_slice(), h, u) {
                    back_edges.push((h, u));
                }
            }
        }
    }
    if back_edges.is_empty() {
        return LoopHeat { max_depth: 0, loop_blocks: 0 };
    }
    back_edges.sort_unstable();

    // 4. Build reverse CSR predecessor table for bounded natural-loop discovery.
    let mut pred_count: AVec<'s, usize> = Vec::with_capacity_in(blocks, scratch);
    pred_count.resize(blocks, 0);
    for &v in succ_edges.iter() {
        if let Some(c) = pred_count.get_mut(v) {
            *c = c.saturating_add(1);
        }
    }
    let mut pred_head: AVec<'s, usize> = Vec::with_capacity_in(blocks.saturating_add(1), scratch);
    let mut running = 0usize;
    pred_head.push(0);
    for &c in pred_count.iter() {
        running = running.saturating_add(c);
        pred_head.push(running);
    }
    let mut pred_cursor: AVec<'s, usize> = Vec::with_capacity_in(blocks, scratch);
    if let Some(starts) = pred_head.get(..blocks) {
        pred_cursor.extend_from_slice(starts);
    }
    let mut pred_edges: AVec<'s, usize> = Vec::with_capacity_in(succ_edges.len(), scratch);
    pred_edges.resize(succ_edges.len(), 0);
    for u in 0..blocks {
        let start = succ_head.get(u).copied().unwrap_or(0);
        let end = succ_head.get(u.saturating_add(1)).copied().unwrap_or(start);
        if let Some(slice) = succ_edges.get(start..end) {
            for &v in slice {
                if let Some(cur) = pred_cursor.get_mut(v) {
                    let pos = *cur;
                    *cur = pos.saturating_add(1);
                    if let Some(slot) = pred_edges.get_mut(pos) {
                        *slot = u;
                    }
                }
            }
        }
    }

    // 5. Accumulate per-block natural loop nesting depth across distinct loop headers.
    let mut depth: AVec<'s, u32> = Vec::with_capacity_in(blocks, scratch);
    depth.resize(blocks, 0);
    let mut seen_stamp: AVec<'s, u32> = Vec::with_capacity_in(blocks, scratch);
    seen_stamp.resize(blocks, 0);
    let mut worklist: AVec<'s, usize> = Vec::with_capacity_in(blocks, scratch);

    let mut stamp = 0u32;
    let mut idx = 0usize;
    let mut headers_done = 0usize;
    while idx < back_edges.len() && headers_done < MAX_LOOP_HEADERS {
        let Some((h, _)) = back_edges.get(idx).copied() else {
            break;
        };
        stamp = stamp.saturating_add(1);
        headers_done = headers_done.saturating_add(1);
        worklist.clear();

        if let Some(s) = seen_stamp.get_mut(h) {
            *s = stamp;
        }
        if let Some(d) = depth.get_mut(h) {
            *d = d.saturating_add(1);
        }

        while let Some((bh, latch)) = back_edges.get(idx).copied() {
            if bh != h {
                break;
            }
            idx = idx.saturating_add(1);
            if seen_stamp.get(latch).copied() != Some(stamp) {
                if let Some(s) = seen_stamp.get_mut(latch) {
                    *s = stamp;
                }
                if let Some(d) = depth.get_mut(latch) {
                    *d = d.saturating_add(1);
                }
                worklist.push(latch);
            }
        }

        let mut wl_head = 0usize;
        while let Some(cur) = worklist.get(wl_head).copied() {
            wl_head = wl_head.saturating_add(1);
            let p_start = pred_head.get(cur).copied().unwrap_or(0);
            let p_end = pred_head.get(cur.saturating_add(1)).copied().unwrap_or(p_start);
            if let Some(preds) = pred_edges.get(p_start..p_end) {
                for &p in preds {
                    if seen_stamp.get(p).copied() != Some(stamp)
                        && is_dfs_descendant(dfs_in.as_slice(), dfs_out.as_slice(), h, p)
                    {
                        if let Some(s) = seen_stamp.get_mut(p) {
                            *s = stamp;
                        }
                        if let Some(d) = depth.get_mut(p) {
                            *d = d.saturating_add(1);
                        }
                        worklist.push(p);
                    }
                }
            }
        }
    }

    let mut max_depth = 0u32;
    let mut loop_blocks = 0u64;
    for &d in depth.iter() {
        if d > 0 {
            loop_blocks = loop_blocks.saturating_add(1);
            if d > max_depth {
                max_depth = d;
            }
        }
    }
    LoopHeat { max_depth, loop_blocks }
}

/// Returns `true` iff `view` has at least one `BlockKind::Normal` block that returns normally
/// (or tail-calls), excluding panic/abort-only wrappers whose normal blocks only terminate in
/// `Op::Trap`, `Op::Unreachable`, or `Op::Resume`.
fn has_normal_return(view: blaze_lir::View<'_>) -> bool {
    for b in 0..view.blocks() {
        let Ok(b_u32) = u32::try_from(b) else {
            break;
        };
        let block = Block(b_u32);
        if view.block_kind(block) != BlockKind::Normal {
            continue;
        }
        let range = view.block(block);
        if let Some(term) = range.end.checked_sub(1)
            && term >= range.start
            && matches!(
                view.op(term),
                Op::Return | Op::TailCall | Op::TailCallIndirect
            )
        {
            return true;
        }
    }
    false
}

/// Returns `true` iff `view` has no non-cold calls/invokes/tail-calls in normal non-panic blocks.
fn is_zero_call_leaf(view: blaze_lir::View<'_>) -> bool {
    for b in 0..view.blocks() {
        let Ok(b_u32) = u32::try_from(b) else {
            break;
        };
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
            if matches!(
                view.op(at),
                Op::Call
                    | Op::CallIndirect
                    | Op::Invoke
                    | Op::InvokeIndirect
                    | Op::TailCall
                    | Op::TailCallIndirect
            ) && !view.flags(at).contains(Flags::COLD)
            {
                return false;
            }
        }
    }
    true
}

/// Collects direct non-cold post-LTO call edges `(caller_sym, callee_sym)` within the LTO module,
/// resolving both original and inliner-imported `RefId`s back to their originating `BuiltCallee`.
fn collect_post_lto_call_edges<'s>(
    origins: &[LtoOrigin<'s>],
    lto_bodies: &[OptimizedBody<Alloc<'s>>],
    by_sym: &[([u8; 32], usize)],
    scratch: Alloc<'s>,
) -> AVec<'s, (Symbol, Symbol)> {
    let mut edges: AVec<'s, (Symbol, Symbol)> = Vec::new_in(scratch);
    for (idx, (orig, opt)) in origins.iter().zip(lto_bodies.iter()).enumerate() {
        let view = opt.func.view();
        for b in 0..view.blocks() {
            let Ok(b_u32) = u32::try_from(b) else {
                break;
            };
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
                let op = view.op(at);
                if !matches!(op, Op::Call | Op::Invoke | Op::TailCall) {
                    continue;
                }
                if view.flags(at).contains(Flags::COLD) {
                    continue;
                }
                let Some(&data_at) = view.data.get(at) else {
                    continue;
                };
                let Ok(extra_idx) = usize::try_from(data_at) else {
                    continue;
                };
                let Some(&ref_word) = view.extra.get(extra_idx) else {
                    continue;
                };
                let ref_id = RefId(ref_word);
                let Some(rec) = view.refs.get(ref_id.index()) else {
                    continue;
                };
                if RefKind::from_word(rec.kind) != Some(RefKind::Func) {
                    continue;
                }
                let Ok((origin_idx, origin_rec)) = resolve_ref_origin(
                    idx,
                    ref_id,
                    lto_bodies,
                    origins,
                    by_sym,
                ) else {
                    continue;
                };
                if RefKind::from_word(origin_rec.kind) != Some(RefKind::Func) {
                    continue;
                }
                let Ok(src_place) = usize::try_from(origin_rec.place) else {
                    continue;
                };
                let Some(src_origin) = origins.get(origin_idx) else {
                    continue;
                };
                let Some(&BuiltCallee::Symbol(callee_sym)) = src_origin.built.funcs.get(src_place) else {
                    continue;
                };
                if callee_sym == orig.symbol {
                    continue;
                }
                if find_body_idx(by_sym, callee_sym.0).is_some() {
                    edges.push((orig.symbol, callee_sym));
                }
            }
        }
    }
    edges
}

/// Ranks hot module bodies deterministically (Rule 25) for `blaze_export::Profile::hot`.
///
/// Cold and noreturn bodies are excluded. Pass 1 computes each body's own static LIR loop
/// nesting heat (`10^min(max_depth, 6) * loop_blocks`) plus `pgo.body_weight(id)`. Pass 2
/// performs deterministic 1-hop hot-caller heat propagation over `call_edges`: any non-cold
/// normal-returning function directly calling a hot 0-call leaf kernel (`own_score >=
/// HOT_LEAF_THRESHOLD`) receives `callee_heat / 4` (deduplicated per callee and capped safely
/// below its hottest called leaf). Ordering packs `HotLeaf` kernels first (pages 0..2), direct
/// `HotLeafCaller`s immediately after (pages 1..4), then remaining hot bodies, breaking ties by
/// `score` descending, `is_seed` descending, then `Symbol` bytes ascending.
fn rank_hot_symbols<'a, 's>(
    bodies: &[(Symbol, bool, blaze_lir::View<'_>)],
    call_edges: &[(Symbol, Symbol)],
    pgo: &PgoSiteTable<Alloc<'s>>,
    alloc: Alloc<'a>,
    scratch: Alloc<'s>,
) -> AVec<'a, Symbol> {
    let n = bodies.len();
    let mut sym_index: AVec<'s, ([u8; 32], usize)> = Vec::with_capacity_in(n, scratch);
    let mut body_heats: AVec<'s, BodyHeat> = Vec::with_capacity_in(n, scratch);

    for (idx, &(symbol, is_seed, view)) in bodies.iter().enumerate() {
        sym_index.push((symbol.0, idx));
        let is_cold = (view.head.attrs & FnAttrs::COLD) != 0;
        let is_noreturn = (view.head.attrs & FnAttrs::NORETURN) != 0;
        if is_cold || is_noreturn {
            body_heats.push(BodyHeat {
                symbol,
                is_seed,
                eligible: false,
                can_receive_caller_bonus: false,
                is_hot_leaf: false,
                has_own_heat: false,
                own_score: 0,
            });
            continue;
        }
        let pgo_w = pgo.body_weight(BodyId(symbol.0));
        let heat = compute_loop_heat(view, scratch);
        let loop_score = if heat.max_depth > 0 {
            10u64
                .saturating_pow(heat.max_depth.min(MAX_LOOP_DEPTH_EXP))
                .saturating_mul(heat.loop_blocks)
        } else {
            0
        };
        let own_score = loop_score.saturating_add(pgo_w);
        let has_own_heat = heat.max_depth > 0 || pgo_w > STATIC_STRAIGHT_LINE_WEIGHT;
        let zero_call = is_zero_call_leaf(view);
        let is_hot_leaf = zero_call && has_own_heat && own_score >= HOT_LEAF_THRESHOLD;
        let can_receive_caller_bonus = !zero_call && has_normal_return(view);
        body_heats.push(BodyHeat {
            symbol,
            is_seed,
            eligible: true,
            can_receive_caller_bonus,
            is_hot_leaf,
            has_own_heat,
            own_score,
        });
    }
    sym_index.sort_unstable();

    let mut hot_edges: AVec<'s, (usize, usize)> = Vec::new_in(scratch);
    for &(caller_sym, callee_sym) in call_edges.iter() {
        if caller_sym == callee_sym {
            continue;
        }
        let Some(caller_idx) = find_body_idx(sym_index.as_slice(), caller_sym.0) else {
            continue;
        };
        let Some(callee_idx) = find_body_idx(sym_index.as_slice(), callee_sym.0) else {
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
    for idx in 0..n {
        let Some(&info) = body_heats.get(idx) else {
            continue;
        };
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

/// Computes a deterministic BLAKE3 digest over the active `PgoSiteTable` and ranked `hot_symbols`
/// for `blaze_export::Profile::digest` (Rule 21, Rule 25).
fn compute_profile_digest<'s>(
    pgo: &PgoSiteTable<Alloc<'s>>,
    hot_symbols: &[Symbol],
    scratch: Alloc<'s>,
) -> [u8; 32] {
    let encoded = pgo.encode(scratch);
    let mut hasher = blaze_ids::Hasher::new(EXPORT_PGO_PROFILE_DOMAIN);
    hasher.update(encoded.as_slice());
    let count = u64::try_from(hot_symbols.len()).unwrap_or(u64::MAX);
    hasher.update(&count.to_le_bytes());
    for sym in hot_symbols.iter() {
        hasher.update(&sym.0);
    }
    hasher.finish().0
}

/// Runs demand-driven whole-program cross-function LIR inlining and host-ISA SOTA emission over
/// workspace seed rows and their small inlineable callees.
#[expect(clippy::too_many_arguments, reason = "whole-program export LTO over walk, seeds, gate, maker, options, output allocator, and scratch allocator")]
pub fn optimize_export_workspace<'a, 's, 'n, 'b, G, T>(
    walk: &Walk<'_>,
    seeds: &[usize],
    is_eligible_callee: impl Fn(usize) -> bool,
    mut grant_for: impl FnMut(ItemKey) -> Option<G>,
    maker: &mut Maker<'n, G, T>,
    opts: ExportLtoOpts<'b>,
    alloc: Alloc<'a>,
    scratch: Alloc<'s>,
) -> Result<ExportLtoOut<'a>, ExportLtoError>
where
    G: Grant,
    T: Typing<RecordsProgram<'n>>,
{
    if seeds.is_empty() || opts.max_bodies == 0 {
        return Ok(ExportLtoOut {
            bodies: Vec::new_in(alloc),
            hot_symbols: Vec::new_in(alloc),
            profile_digest: [0u8; 32],
            module_bodies: 0,
            total_inlined: 0,
        });
    }

    // Visited states: prioritize direct `Op::Call` callees (which `inline_guided` can splice)
    // ahead of `Op::Invoke` callees so unwind/formatting subtrees never evict small leaf helpers
    // (`PartialOrd::lt`, `slice::iter`, `IntoIterator::into_iter`) from the LTO module budget.
    const UNSEEN: u8 = 0;
    const QUEUED_INVOKE: u8 = 1;
    const QUEUED_CALL: u8 = 2;
    const PROCESSED: u8 = 3;

    let mut visited: AVec<'s, u8> = Vec::with_capacity_in(walk.len(), scratch);
    visited.resize(walk.len(), UNSEEN);
    let mut call_queue: AVec<'s, (usize, u8, bool)> = Vec::with_capacity_in(seeds.len().min(opts.max_bodies), scratch);
    let mut invoke_queue: AVec<'s, (usize, u8, bool)> = Vec::new_in(scratch);
    for &row in seeds {
        if let Some(state) = visited.get_mut(row)
            && *state == UNSEEN
            && is_eligible_callee(row)
        {
            *state = QUEUED_CALL;
            call_queue.push((row, 0, true));
        }
    }

    // 1. Bounded demand-driven BFS: gather workspace seeds + small inlineable callees.
    // Reserve the final eighth of large LTO budgets for tiny `<= 24`-inst `#[inline]` /
    // `#[inline(always)]` leaf helpers (e.g. `slice::get -> ptr::add -> ptr::read -> ub_checks`)
    // so medium callees in huge workspaces never crowd out deep leaf chains.
    const TINY_INLINE_LEAF_INSTS: usize = 24;
    const MAX_RESERVE_SCANS: usize = 2048;
    let tiny_reserve = if opts.max_bodies >= 512 { opts.max_bodies / 8 } else { 0 };
    let soft_limit = opts.max_bodies.saturating_sub(tiny_reserve);
    let mut reserve_scans = 0usize;
    let mut origins: AVec<'s, LtoOrigin<'s>> = Vec::with_capacity_in(call_queue.len(), scratch);
    let mut call_head = 0usize;
    let mut invoke_head = 0usize;
    while origins.len() < opts.max_bodies {
        let in_tiny_reserve = tiny_reserve > 0 && origins.len() >= soft_limit;
        if in_tiny_reserve && reserve_scans >= MAX_RESERVE_SCANS {
            break;
        }
        let next = if let Some(&item) = call_queue.get(call_head) {
            call_head += 1;
            item
        } else if !in_tiny_reserve
            && let Some(&item) = invoke_queue.get(invoke_head)
        {
            invoke_head += 1;
            item
        } else {
            break;
        };
        let (row, depth, is_seed) = next;
        let Some(state) = visited.get_mut(row) else {
            continue;
        };
        if *state == PROCESSED {
            continue;
        }
        *state = PROCESSED;
        if in_tiny_reserve && !is_seed {
            reserve_scans += 1;
        }
        let tabled = walk.tabled(row);
        let symbol = walk.symbol(row);
        let name = walk.name(row);
        let Some(grant) = grant_for(tabled) else {
            return Err(ExportLtoError::NotRunnable { symbol: symbol.0, tabled });
        };
        let mut built = maker
            .build_lir(&grant, tabled, name, scratch)
            .map_err(|error| ExportLtoError::BuildLir { symbol: symbol.0, error })?;
        let lowered_libm = lower_ieee_libm_calls(&mut built, opts.machine, scratch);
        if lowered_libm {
            verify(built.func.view())
                .map_err(|error| ExportLtoError::Verify { symbol: symbol.0, error })?;
        }

        let view = built.func.view();
        if !is_seed {
            let is_cold = (view.head.attrs & FnAttrs::COLD) != 0;
            let is_noreturn = (view.head.attrs & FnAttrs::NORETURN) != 0;
            let inline_attr = FnAttrs(view.head.attrs).inline();
            let insts = view.len();
            let keep = if is_noreturn
                || inline_attr == Inline::Never
                || (is_cold && inline_attr != Inline::Always)
            {
                false
            } else if in_tiny_reserve {
                matches!(inline_attr, Inline::Always | Inline::Hint) && insts <= TINY_INLINE_LEAF_INSTS
            } else if inline_attr == Inline::Always || inline_attr == Inline::Hint {
                insts <= opts.max_always_inline_insts
            } else {
                insts <= opts.max_callee_insts
            };
            if !keep {
                continue;
            }
        }

        if depth < opts.max_depth {
            let next_depth = depth.saturating_add(1);
            for b in 0..view.blocks() {
                let Ok(b_u32) = u32::try_from(b) else { break };
                let block = Block(b_u32);
                if view.block_kind(block) != BlockKind::Normal {
                    continue;
                }
                for at in view.block(block) {
                    let op = view.op(at);
                    if !matches!(op, Op::Call | Op::Invoke) {
                        continue;
                    }
                    if view.flags(at).contains(Flags::COLD) {
                        continue;
                    }
                    let Some(&data_at) = view.data.get(at) else { continue };
                    let Ok(extra_idx) = usize::try_from(data_at) else { continue };
                    let Some(&ref_word) = view.extra.get(extra_idx) else { continue };
                    let Some(rec) = view.refs.get(RefId(ref_word).index()) else { continue };
                    if RefKind::from_word(rec.kind) != Some(RefKind::Func) {
                        continue;
                    }
                    let Ok(place_idx) = usize::try_from(rec.place) else { continue };
                    let Some(&BuiltCallee::Symbol(callee_sym)) = built.funcs.get(place_idx) else { continue };
                    let Some(callee_row) = walk.row_of(&callee_sym) else { continue };
                    if !is_eligible_callee(callee_row) {
                        continue;
                    }
                    if let Some(state) = visited.get_mut(callee_row) {
                        if op == Op::Call && *state < QUEUED_CALL {
                            *state = QUEUED_CALL;
                            call_queue.push((callee_row, next_depth, false));
                        } else if op == Op::Invoke && *state == UNSEEN {
                            *state = QUEUED_INVOKE;
                            invoke_queue.push((callee_row, next_depth, false));
                        }
                    }
                }
            }
        }

        let orig_unreachables = view.ops.iter().filter(|&&op| Op::from_byte(op) == Some(Op::Unreachable)).count();
        origins.push(LtoOrigin {
            row,
            symbol,
            is_seed,
            lowered_libm,
            orig_unreachables,
            built,
        });
    }

    if origins.is_empty() {
        return Ok(ExportLtoOut {
            bodies: Vec::new_in(alloc),
            hot_symbols: Vec::new_in(alloc),
            profile_digest: [0u8; 32],
            module_bodies: 0,
            total_inlined: 0,
        });
    }

    // 2. Bottom-up Tarjan SCC cross-function LIR inlining + SROA/propagate/DCE/CFG-coalesce,
    //    guided by static PGO estimates merged with any persisted cross-worktree `BPG1` record.
    let n = origins.len();
    let mut by_sym: AVec<'s, ([u8; 32], usize)> = Vec::with_capacity_in(n, scratch);
    let mut mod_bodies: AVec<'s, ModuleBody<Alloc<'s>>> = Vec::with_capacity_in(n, scratch);
    for (idx, orig) in origins.iter().enumerate() {
        by_sym.push((orig.symbol.0, idx));
        mod_bodies.push(ModuleBody {
            id: BodyId(orig.symbol.0),
            func: orig.built.func.view().to_func_in(scratch),
        });
    }
    by_sym.sort_unstable();

    let pgo = build_pgo_table(mod_bodies.as_slice(), opts.pgo_bytes, scratch)?;
    let bodies_view = LtoBodies { origins: origins.as_slice(), by_sym: by_sym.as_slice() };
    let lto = lto_optimize_module(mod_bodies, &bodies_view, &pgo, opts.policy, scratch)
        .map_err(ExportLtoError::Optimize)?;

    let mut hot_inputs: AVec<'s, (Symbol, bool, blaze_lir::View<'_>)> = Vec::with_capacity_in(n, scratch);
    for (orig, opt) in origins.iter().zip(lto.bodies.iter()) {
        hot_inputs.push((orig.symbol, orig.is_seed, opt.func.view()));
    }
    let call_edges = collect_post_lto_call_edges(
        origins.as_slice(),
        lto.bodies.as_slice(),
        by_sym.as_slice(),
        scratch,
    );
    let hot_symbols = rank_hot_symbols(hot_inputs.as_slice(), call_edges.as_slice(), &pgo, alloc, scratch);
    let profile_digest = compute_profile_digest(&pgo, hot_symbols.as_slice(), scratch);

    // 3. Expand reference tables transitively for every `ImportedRef` and emit host-ISA records.
    let mut out_bodies: AVec<'a, LtoBodyOut<'a>> = Vec::with_capacity_in(n, alloc);
    for (idx, (orig, opt)) in origins.iter().zip(lto.bodies.iter()).enumerate() {
        if !orig.is_seed && !orig.lowered_libm && opt.read.is_empty() {
            continue;
        }
        let mut funcs: AVec<'s, Callee<'_>> = Vec::with_capacity_in(orig.built.funcs.len() + opt.imported_refs.len(), scratch);
        for p in 0..orig.built.funcs.len() {
            let p_u32 = u32::try_from(p).map_err(|_| ExportLtoError::BadRef {
                symbol: orig.symbol.0,
                what: "original func place",
                ref_id: u32::MAX,
            })?;
            let Some(callee) = orig.built.callee_at(p_u32) else {
                return Err(ExportLtoError::BadRef {
                    symbol: orig.symbol.0,
                    what: "original func name",
                    ref_id: p_u32,
                });
            };
            funcs.push(callee);
        }
        let mut statics: AVec<'s, ItemKey> = Vec::with_capacity_in(orig.built.statics.len(), scratch);
        statics.extend_from_slice(orig.built.statics.as_slice());
        let mut c_statics: AVec<'s, (ItemKey, &str)> = Vec::with_capacity_in(orig.built.c_statics.len(), scratch);
        for &(item, start, len) in orig.built.c_statics.iter() {
            let Some(cname) = orig.built.c_str(start, len) else {
                return Err(ExportLtoError::BadRef {
                    symbol: orig.symbol.0,
                    what: "original foreign static name",
                    ref_id: 0,
                });
            };
            c_statics.push((item, cname));
        }
        let mut vtables: AVec<'s, Symbol> = Vec::with_capacity_in(orig.built.vtables.len(), scratch);
        vtables.extend_from_slice(orig.built.vtables.as_slice());
        let mut vtable_data: AVec<'a, (Symbol, AVec<'a, u8>)> = Vec::with_capacity_in(orig.built.vtable_data.len(), alloc);
        for (vsym, bytes) in orig.built.vtable_data.iter() {
            push_vtable_datum(&mut vtable_data, *vsym, bytes.as_slice(), alloc);
        }
        let mut tls: AVec<'s, ItemKey> = Vec::with_capacity_in(orig.built.tls.len(), scratch);
        tls.extend_from_slice(orig.built.tls.as_slice());

        for imp in opt.imported_refs.iter() {
            let Some(kind) = RefKind::from_word(imp.rec.kind) else {
                return Err(ExportLtoError::BadRef {
                    symbol: orig.symbol.0,
                    what: "imported ref kind",
                    ref_id: imp.caller_ref.0,
                });
            };
            let place = usize::try_from(imp.rec.place).map_err(|_| ExportLtoError::BadRef {
                symbol: orig.symbol.0,
                what: "imported ref place",
                ref_id: imp.caller_ref.0,
            })?;
            let (origin_idx, origin_rec) = resolve_ref_origin(
                idx,
                imp.caller_ref,
                lto.bodies.as_slice(),
                origins.as_slice(),
                by_sym.as_slice(),
            )?;
            if RefKind::from_word(origin_rec.kind) != Some(kind) {
                return Err(ExportLtoError::RefKindMismatch {
                    symbol: orig.symbol.0,
                    ref_id: imp.caller_ref.0,
                });
            }
            let Some(src_origin) = origins.get(origin_idx) else {
                return Err(ExportLtoError::BadRef {
                    symbol: orig.symbol.0,
                    what: "origin body index",
                    ref_id: imp.caller_ref.0,
                });
            };
            let src_place = usize::try_from(origin_rec.place).map_err(|_| ExportLtoError::BadRef {
                symbol: orig.symbol.0,
                what: "origin ref place",
                ref_id: imp.caller_ref.0,
            })?;
            match kind {
                RefKind::Func => {
                    let Some(callee) = src_origin.built.callee_at(origin_rec.place) else {
                        return Err(ExportLtoError::BadRef {
                            symbol: orig.symbol.0,
                            what: "origin func callee",
                            ref_id: imp.caller_ref.0,
                        });
                    };
                    put_at(&mut funcs, place, callee, orig.symbol.0, "imported func place", imp.caller_ref.0)?;
                }
                RefKind::Static => {
                    let Some(&item) = src_origin.built.statics.get(src_place) else {
                        return Err(ExportLtoError::BadRef {
                            symbol: orig.symbol.0,
                            what: "origin static item",
                            ref_id: imp.caller_ref.0,
                        });
                    };
                    put_at(&mut statics, place, item, orig.symbol.0, "imported static place", imp.caller_ref.0)?;
                    if let Some(cname) = src_origin.built.foreign_static_name(item)
                        && !c_statics.iter().any(|(k, _)| *k == item)
                    {
                        c_statics.push((item, cname));
                    }
                }
                RefKind::Vtable => {
                    let Some(&vsym) = src_origin.built.vtables.get(src_place) else {
                        return Err(ExportLtoError::BadRef {
                            symbol: orig.symbol.0,
                            what: "origin vtable symbol",
                            ref_id: imp.caller_ref.0,
                        });
                    };
                    put_at(&mut vtables, place, vsym, orig.symbol.0, "imported vtable place", imp.caller_ref.0)?;
                    for (sym, bytes) in src_origin.built.vtable_data.iter() {
                        push_vtable_datum(&mut vtable_data, *sym, bytes.as_slice(), alloc);
                    }
                }
                RefKind::Tls => {
                    let Some(&item) = src_origin.built.tls.get(src_place) else {
                        return Err(ExportLtoError::BadRef {
                            symbol: orig.symbol.0,
                            what: "origin tls item",
                            ref_id: imp.caller_ref.0,
                        });
                    };
                    put_at(&mut tls, place, item, orig.symbol.0, "imported tls place", imp.caller_ref.0)?;
                }
                RefKind::Data => {
                    return Err(ExportLtoError::BadRef {
                        symbol: orig.symbol.0,
                        what: "unexpected imported Data ref",
                        ref_id: imp.caller_ref.0,
                    });
                }
            }
        }

        // Bind compiler_builtins soft-float routines AFTER all original + imported function refs.
        let routines = maker
            .bind_routines(&mut funcs)
            .map_err(|error| ExportLtoError::Record { symbol: orig.symbol.0, error })?;
        let names = InstanceNames {
            funcs: funcs.as_slice(),
            statics: statics.as_slice(),
            c_statics: c_statics.as_slice(),
            vtables: vtables.as_slice(),
            tls: tls.as_slice(),
        };
        let verified = verify(opt.func.view())
            .map_err(|error| ExportLtoError::Verify { symbol: orig.symbol.0, error })?;
        let post_unreachables = opt.func.view().ops.iter().filter(|&&op| Op::from_byte(op) == Some(Op::Unreachable)).count();
        let traps: &[TrapSource] = if opt.read.is_empty() && orig.orig_unreachables == post_unreachables {
            orig.built.traps.as_slice()
        } else {
            &[]
        };
        let (record_tmp, called_declared) = emit_code_record_for_machine(verified, &names, routines, traps, opts.machine, scratch)
            .map_err(|error| ExportLtoError::Record { symbol: orig.symbol.0, error })?;
        let mut record: AVec<'a, u8> = Vec::with_capacity_in(record_tmp.len(), alloc);
        record.extend_from_slice(record_tmp.as_slice());
        let mut extra_declared: AVec<'a, (ItemKey, AVec<'a, u8>)> = Vec::with_capacity_in(called_declared.len(), alloc);
        for d in called_declared.iter() {
            let mut name_buf: AVec<'a, u8> = Vec::with_capacity_in(d.name.len(), alloc);
            name_buf.extend_from_slice(d.name);
            extra_declared.push((d.tabled, name_buf));
        }
        out_bodies.push(LtoBodyOut {
            row: orig.row,
            symbol: orig.symbol,
            record,
            vtable_data,
            extra_declared,
            inlined_bodies: opt.read.len(),
        });
    }

    Ok(ExportLtoOut {
        bodies: out_bodies,
        hot_symbols,
        profile_digest,
        module_bodies: n,
        total_inlined: lto.total_inlined_bodies,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use blaze_lir::{
        Builder, Full,
        builder::{Param, Variadic},
    };
    use blaze_tier::Os as TierOs;

    fn arm64_machine() -> Machine {
        Machine {
            arch: Arch::Arm64,
            os: TierOs::MacOs,
            features: Machine::LSE | Machine::DOTPROD | Machine::CRC32 | Machine::SME | Machine::SME2,
        }
    }

    fn x64_machine(features: u64) -> Machine {
        Machine {
            arch: Arch::X86_64,
            os: TierOs::Linux,
            features,
        }
    }

    fn make_built_with_c_callees(
        func: blaze_lir::Func<Alloc<'static>>,
        c_names: &[&str],
        alloc: Alloc<'static>,
    ) -> BuiltLir<'static> {
        let mut funcs: AVec<'static, BuiltCallee> = Vec::with_capacity_in(c_names.len(), alloc);
        let mut c_text: AVec<'static, u8> = Vec::new_in(alloc);
        for &name in c_names {
            let start = u32::try_from(c_text.len()).expect("c_text fits u32");
            let len = u32::try_from(name.len()).expect("c_name fits u32");
            c_text.extend_from_slice(name.as_bytes());
            funcs.push(BuiltCallee::C { start, len });
        }
        BuiltLir {
            func,
            funcs,
            statics: Vec::new_in(alloc),
            c_statics: Vec::new_in(alloc),
            vtables: Vec::new_in(alloc),
            vtable_data: Vec::new_in(alloc),
            tls: Vec::new_in(alloc),
            traps: Vec::new_in(alloc),
            c_text,
        }
    }

    fn build_c_call_fn(
        c_name: &str,
        param_tys: &[Ty],
        ret_ty: Ty,
        variadic: Variadic,
        alloc: Alloc<'static>,
    ) -> BuiltLir<'static> {
        let mut b: Builder<Alloc<'static>, Full> = Builder::new_in(64, alloc);
        let sig_params: std::vec::Vec<Param> = param_tys.iter().copied().map(Param::of).collect();
        let ret_params = [Param::of(ret_ty)];
        let own_sig = b.sig(Conv::C, Variadic::No, &sig_params, &ret_params);
        let callee_sig = b.sig(Conv::C, variadic, &sig_params, &ret_params);
        b.set_signature(own_sig);
        let callee_ref = b.reference(RefKind::Func, 0);

        let entry = b.create_block(BlockKind::Normal);
        b.begin(entry);
        let args: std::vec::Vec<_> = param_tys.iter().copied().map(|ty| b.param(ty)).collect();
        let res = b.call(Flags::NONE, callee_ref, callee_sig, &args);
        b.ret(&[res]);

        let func = b.finish_checked().expect("valid initial LIR");
        make_built_with_c_callees(func, &[c_name], alloc)
    }

    #[test]
    fn lowers_call_fmaf_and_underscored_fma() {
        let alloc = blaze_own::testing::heap().alloc();
        for (name, ty) in [("fmaf", Ty::F32), ("_fmaf", Ty::F32), ("fma", Ty::F64), ("_fma", Ty::F64)] {
            let mut built = build_c_call_fn(name, &[ty, ty, ty], ty, Variadic::No, alloc);
            assert!(lower_ieee_libm_calls(&mut built, arm64_machine(), alloc), "{name} should lower");
            verify(built.func.view()).expect("lowered LIR must verify");

            let view = built.func.view();
            let call_count = (0..view.len()).filter(|&i| matches!(view.op(i), Op::Call | Op::Invoke)).count();
            assert_eq!(call_count, 0, "{name} should have no calls left");

            let fma_inst = (0..view.len()).find(|&i| view.op(i) == Op::Fma).expect("expected Op::Fma");
            assert_eq!(Ty(view.tys[fma_inst]), ty);
            assert_eq!(view.flags(fma_inst), Flags::NONE);
            let mut ops = view.operands(fma_inst);
            assert_eq!(ops.value().0, 0);
            assert_eq!(ops.value().0, 1);
            assert_eq!(ops.value().0, 2);
        }
    }

    #[test]
    fn lowers_call_unary_ieee_libm_ops_on_arm64() {
        let alloc = blaze_own::testing::heap().alloc();
        let cases = [
            ("sqrtf", Ty::F32, Op::Sqrt),
            ("sqrt", Ty::F64, Op::Sqrt),
            ("_sqrtf", Ty::F32, Op::Sqrt),
            ("fabsf", Ty::F32, Op::Fabs),
            ("fabs", Ty::F64, Op::Fabs),
            ("floorf", Ty::F32, Op::Floor),
            ("floor", Ty::F64, Op::Floor),
            ("ceilf", Ty::F32, Op::Ceil),
            ("ceil", Ty::F64, Op::Ceil),
            ("truncf", Ty::F32, Op::Ftrunc),
            ("trunc", Ty::F64, Op::Ftrunc),
            ("rintf", Ty::F32, Op::Nearest),
            ("rint", Ty::F64, Op::Nearest),
            ("roundf", Ty::F32, Op::Fround),
            ("round", Ty::F64, Op::Fround),
        ];
        for (name, ty, expected_op) in cases {
            let mut built = build_c_call_fn(name, &[ty], ty, Variadic::No, alloc);
            assert!(lower_ieee_libm_calls(&mut built, arm64_machine(), alloc), "{name} should lower");
            verify(built.func.view()).expect("lowered unary libm LIR must verify");

            let view = built.func.view();
            assert_eq!(
                (0..view.len()).filter(|&i| matches!(view.op(i), Op::Call | Op::Invoke)).count(),
                0
            );
            let inst = (0..view.len()).find(|&i| view.op(i) == expected_op).expect("expected lowered unary op");
            assert_eq!(Ty(view.tys[inst]), ty);
            assert_eq!(view.flags(inst), Flags::NONE);
            assert_eq!(view.operands(inst).value().0, 0);
        }
    }

    #[test]
    fn lowers_chained_invoke_fmaf_and_sqrtf_and_verifies() {
        let alloc = blaze_own::testing::heap().alloc();
        let mut b: Builder<Alloc<'static>, Full> = Builder::new_in(64, alloc);
        let own_sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::F32), Param::of(Ty::F32), Param::of(Ty::F32), Param::of(Ty::F32)],
            &[Param::of(Ty::F32)],
        );
        let fma_sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::F32), Param::of(Ty::F32), Param::of(Ty::F32)],
            &[Param::of(Ty::F32)],
        );
        let sqrt_sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::F32)], &[Param::of(Ty::F32)]);
        b.set_signature(own_sig);

        let fmaf_ref = b.reference(RefKind::Func, 0);
        let sqrtf_ref = b.reference(RefKind::Func, 1);

        let entry = b.create_block(BlockKind::Normal);
        let after_fma = b.create_block(BlockKind::Normal);
        let after_sqrt = b.create_block(BlockKind::Normal);
        let cleanup = b.create_block(BlockKind::Cleanup);

        b.begin(entry);
        let a = b.param(Ty::F32);
        let x = b.param(Ty::F32);
        let c = b.param(Ty::F32);
        let bias = b.param(Ty::F32);
        let fma_val = b.invoke(Flags::NONE, fmaf_ref, fma_sig, &[a, x, c], after_fma, cleanup);

        b.begin(after_fma);
        let shifted = b.binary(Op::Fadd, Flags::NONE, fma_val, bias);
        let sqrt_val = b.invoke(Flags::NONE, sqrtf_ref, sqrt_sig, &[shifted], after_sqrt, cleanup);

        b.begin(after_sqrt);
        let final_val = b.binary(Op::Fadd, Flags::NONE, sqrt_val, fma_val);
        b.ret(&[final_val]);

        b.begin(cleanup);
        let exc = b.param(Ty::PTR);
        b.resume(exc);

        let func = b.finish_checked().expect("valid initial invoke LIR");
        let mut built = make_built_with_c_callees(func, &["fmaf", "sqrtf"], alloc);

        assert!(lower_ieee_libm_calls(&mut built, arm64_machine(), alloc));
        verify(built.func.view()).expect("lowered invoke LIR must verify");

        let view = built.func.view();
        assert_eq!(
            (0..view.len()).filter(|&i| matches!(view.op(i), Op::Call | Op::Invoke)).count(),
            0,
            "all libm invokes should be lowered"
        );
        let fma_inst = (0..view.len()).find(|&i| view.op(i) == Op::Fma).expect("expected Op::Fma");
        let sqrt_inst = (0..view.len()).find(|&i| view.op(i) == Op::Sqrt).expect("expected Op::Sqrt");
        assert_eq!(view.op(fma_inst + 1), Op::Jump);
        assert_eq!(view.op(sqrt_inst + 1), Op::Jump);

        let ret_inst = (0..view.len()).find(|&i| view.op(i) == Op::Return).expect("expected Op::Return");
        let ret_val = view.operands(ret_inst).values().get(0);
        assert_eq!(view.op(ret_val.index()), Op::Fadd);
        let mut final_ops = view.operands(ret_val.index());
        assert_eq!(final_ops.value().index(), sqrt_inst);
        assert_eq!(final_ops.value().index(), fma_inst);
    }

    #[test]
    fn leaves_unknown_mismatched_variadic_and_unsupported_targets_untouched() {
        let alloc = blaze_own::testing::heap().alloc();

        // Unknown C functions and ops unsupported by `sota` (`copysignf`/`copysign`) stay untouched.
        for (name, params, ret, variadic) in [
            ("sinf", &[Ty::F32][..], Ty::F32, Variadic::No),
            ("printf", &[Ty::PTR][..], Ty::I32, Variadic::Yes),
            ("copysignf", &[Ty::F32, Ty::F32][..], Ty::F32, Variadic::No),
            ("copysign", &[Ty::F64, Ty::F64][..], Ty::F64, Variadic::No),
            // Mismatched signatures for known names: wrong precision, wrong arity, or variadic.
            ("fmaf", &[Ty::F64, Ty::F64, Ty::F64][..], Ty::F64, Variadic::No),
            ("fma", &[Ty::F32, Ty::F32, Ty::F32][..], Ty::F32, Variadic::No),
            ("fmaf", &[Ty::F32, Ty::F32][..], Ty::F32, Variadic::No),
            ("sqrtf", &[Ty::F64][..], Ty::F64, Variadic::No),
            ("sqrt", &[Ty::F32][..], Ty::F32, Variadic::No),
            ("fmaf", &[Ty::F32, Ty::F32, Ty::F32][..], Ty::F32, Variadic::Yes),
        ] {
            let mut built = build_c_call_fn(name, params, ret, variadic, alloc);
            assert!(
                !lower_ieee_libm_calls(&mut built, arm64_machine(), alloc),
                "{name} ({params:?} -> {ret:?}, {variadic:?}) must not lower"
            );
            verify(built.func.view()).expect("untouched LIR must verify");
            assert_eq!(
                (0..built.func.view().len()).filter(|&i| built.func.view().op(i) == Op::Call).count(),
                1
            );
        }

        // x86-64 gating: `Op::Fma` requires `Machine::FMA`; `Op::Sqrt` is unsupported on x86-64 sota.
        let mut fma_no_feat = build_c_call_fn("fmaf", &[Ty::F32, Ty::F32, Ty::F32], Ty::F32, Variadic::No, alloc);
        assert!(!lower_ieee_libm_calls(&mut fma_no_feat, x64_machine(0), alloc));

        let mut fma_with_feat = build_c_call_fn("fmaf", &[Ty::F32, Ty::F32, Ty::F32], Ty::F32, Variadic::No, alloc);
        assert!(lower_ieee_libm_calls(&mut fma_with_feat, x64_machine(Machine::FMA), alloc));
        verify(fma_with_feat.func.view()).expect("x86-64 FMA lowered LIR must verify");

        let mut sqrt_x64 = build_c_call_fn("sqrtf", &[Ty::F32], Ty::F32, Variadic::No, alloc);
        assert!(!lower_ieee_libm_calls(&mut sqrt_x64, x64_machine(Machine::FMA | Machine::AVX2), alloc));
    }

    fn build_straight_line_fn(is_cold: bool, alloc: Alloc<'static>) -> blaze_lir::Func<Alloc<'static>> {
        let mut b: Builder<Alloc<'static>, Full> = Builder::new_in(64, alloc);
        let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        b.set_signature(sig);
        if is_cold {
            b.set_attrs(FnAttrs(FnAttrs::COLD));
        }
        let entry = b.create_block(BlockKind::Normal);
        b.begin(entry);
        let x = b.param(Ty::I64);
        let one = b.iconst(Ty::I64, 1);
        let sum = b.binary(Op::Add, Flags::NONE, x, one);
        b.ret(&[sum]);
        b.finish_checked().expect("valid straight-line LIR")
    }

    fn build_single_loop_leaf(alloc: Alloc<'static>) -> blaze_lir::Func<Alloc<'static>> {
        let mut b: Builder<Alloc<'static>, Full> = Builder::new_in(64, alloc);
        let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        b.set_signature(sig);

        let entry = b.create_block(BlockKind::Normal);
        let header = b.create_block(BlockKind::Normal);
        let body = b.create_block(BlockKind::Normal);
        let exit = b.create_block(BlockKind::Normal);

        b.begin(entry);
        let n = b.param(Ty::I64);
        let zero = b.iconst(Ty::I64, 0);
        b.jump((header, &[zero, zero]));

        b.begin(header);
        let i = b.param(Ty::I64);
        let acc = b.param(Ty::I64);
        let cond = b.icmp(blaze_lir::IntCond::Ult, i, n);
        b.br(Flags::NONE, cond, (body, &[]), (exit, &[acc]));

        b.begin(body);
        let one = b.iconst(Ty::I64, 1);
        let next_i = b.binary(Op::Add, Flags::NONE, i, one);
        let next_acc = b.binary(Op::Add, Flags::NONE, acc, i);
        b.jump((header, &[next_i, next_acc]));

        b.begin(exit);
        let res = b.param(Ty::I64);
        b.ret(&[res]);

        b.finish_checked().expect("valid single-loop leaf LIR")
    }

    fn build_nested_loop_leaf(alloc: Alloc<'static>) -> blaze_lir::Func<Alloc<'static>> {
        let mut b: Builder<Alloc<'static>, Full> = Builder::new_in(64, alloc);
        let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        b.set_signature(sig);

        let entry = b.create_block(BlockKind::Normal);
        let outer_h = b.create_block(BlockKind::Normal);
        let inner_h = b.create_block(BlockKind::Normal);
        let inner_b = b.create_block(BlockKind::Normal);
        let outer_latch = b.create_block(BlockKind::Normal);
        let exit = b.create_block(BlockKind::Normal);

        b.begin(entry);
        let n = b.param(Ty::I64);
        let zero = b.iconst(Ty::I64, 0);
        b.jump((outer_h, &[zero, zero]));

        b.begin(outer_h);
        let i = b.param(Ty::I64);
        let outer_acc = b.param(Ty::I64);
        let outer_cond = b.icmp(blaze_lir::IntCond::Ult, i, n);
        b.br(Flags::NONE, outer_cond, (inner_h, &[zero, outer_acc]), (exit, &[outer_acc]));

        b.begin(inner_h);
        let j = b.param(Ty::I64);
        let inner_acc = b.param(Ty::I64);
        let inner_cond = b.icmp(blaze_lir::IntCond::Ult, j, n);
        b.br(Flags::NONE, inner_cond, (inner_b, &[]), (outer_latch, &[inner_acc]));

        b.begin(inner_b);
        let one = b.iconst(Ty::I64, 1);
        let next_j = b.binary(Op::Add, Flags::NONE, j, one);
        let next_acc = b.binary(Op::Add, Flags::NONE, inner_acc, j);
        b.jump((inner_h, &[next_j, next_acc]));

        b.begin(outer_latch);
        let after_inner = b.param(Ty::I64);
        let one2 = b.iconst(Ty::I64, 1);
        let next_i = b.binary(Op::Add, Flags::NONE, i, one2);
        b.jump((outer_h, &[next_i, after_inner]));

        b.begin(exit);
        let res = b.param(Ty::I64);
        b.ret(&[res]);

        b.finish_checked().expect("valid nested-loop leaf LIR")
    }

    #[test]
    fn static_pgo_heat_ranks_nested_loop_leaf_strictly_ahead_of_shallow_and_cold() {
        let alloc = blaze_own::testing::heap().alloc();

        let sym_init = Symbol([0x01; 32]);
        let sym_cold = Symbol([0x02; 32]);
        let sym_shallow_dep = Symbol([0x03; 32]);
        let sym_shallow_seed = Symbol([0x04; 32]);
        let sym_nested_leaf = Symbol([0xFF; 32]);

        let fn_init = build_straight_line_fn(false, alloc);
        let fn_cold = build_straight_line_fn(true, alloc);
        let fn_shallow_dep = build_single_loop_leaf(alloc);
        let fn_shallow_seed = build_single_loop_leaf(alloc);
        let fn_nested_leaf = build_nested_loop_leaf(alloc);

        assert_eq!(compute_loop_heat(fn_init.view(), alloc), LoopHeat { max_depth: 0, loop_blocks: 0 });
        assert_eq!(compute_loop_heat(fn_shallow_seed.view(), alloc), LoopHeat { max_depth: 1, loop_blocks: 2 });
        assert_eq!(compute_loop_heat(fn_nested_leaf.view(), alloc), LoopHeat { max_depth: 2, loop_blocks: 4 });

        let mut mod_bodies: AVec<'static, ModuleBody<Alloc<'static>>> = Vec::new_in(alloc);
        mod_bodies.push(ModuleBody { id: BodyId(sym_init.0), func: fn_init.view().to_func_in(alloc) });
        mod_bodies.push(ModuleBody { id: BodyId(sym_cold.0), func: fn_cold.view().to_func_in(alloc) });
        mod_bodies.push(ModuleBody { id: BodyId(sym_shallow_dep.0), func: fn_shallow_dep.view().to_func_in(alloc) });
        mod_bodies.push(ModuleBody { id: BodyId(sym_shallow_seed.0), func: fn_shallow_seed.view().to_func_in(alloc) });
        mod_bodies.push(ModuleBody { id: BodyId(sym_nested_leaf.0), func: fn_nested_leaf.view().to_func_in(alloc) });

        let pgo = build_pgo_table(mod_bodies.as_slice(), None, alloc).expect("static PGO table");
        let inputs = [
            (sym_init, true, fn_init.view()),
            (sym_cold, true, fn_cold.view()),
            (sym_shallow_dep, false, fn_shallow_dep.view()),
            (sym_shallow_seed, true, fn_shallow_seed.view()),
            (sym_nested_leaf, true, fn_nested_leaf.view()),
        ];
        let hot = rank_hot_symbols(&inputs, &[], &pgo, alloc, alloc);

        // Nested 0-call leaf kernel ranks strictly first despite highest symbol bytes, followed by
        // the single-loop workspace seed, then the single-loop non-seed callee. Straight-line init
        // and `#[cold]` functions stay out of `hot_symbols`.
        assert_eq!(hot.as_slice(), &[sym_nested_leaf, sym_shallow_seed, sym_shallow_dep]);

        // Deterministic across reversed input order.
        let mut rev_inputs = inputs;
        rev_inputs.reverse();
        let hot_rev = rank_hot_symbols(&rev_inputs, &[], &pgo, alloc, alloc);
        assert_eq!(hot.as_slice(), hot_rev.as_slice());
        assert_eq!(
            compute_profile_digest(&pgo, hot.as_slice(), alloc),
            compute_profile_digest(&pgo, hot_rev.as_slice(), alloc),
        );

        // Persisted BPG1 merge promotes an observed-hot straight-line body and refuses corruption.
        let persisted = PgoSiteTable::new_in(
            &[],
            &[blaze_lir::pgo::BodyWeight { body: BodyId(sym_init.0), weight: 10_000 }],
            1,
            alloc,
        );
        let mut encoded = persisted.encode(alloc);
        let merged = build_pgo_table(mod_bodies.as_slice(), Some(encoded.as_slice()), alloc)
            .expect("valid BPG1 merges");
        let hot_merged = rank_hot_symbols(&inputs, &[], &merged, alloc, alloc);
        assert_eq!(hot_merged.as_slice(), &[sym_init, sym_nested_leaf, sym_shallow_seed, sym_shallow_dep]);

        encoded[10] ^= 0x01;
        assert!(matches!(
            build_pgo_table(mod_bodies.as_slice(), Some(encoded.as_slice()), alloc),
            Err(ExportLtoError::PgoRecord(PgoRecordError::BadDigest))
        ));
    }

    fn build_direct_caller_fn(
        is_cold: bool,
        panic_only: bool,
        alloc: Alloc<'static>,
    ) -> blaze_lir::Func<Alloc<'static>> {
        let mut b: Builder<Alloc<'static>, Full> = Builder::new_in(64, alloc);
        let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        b.set_signature(sig);
        if is_cold {
            b.set_attrs(FnAttrs(FnAttrs::COLD));
        }
        let callee_ref = b.reference(RefKind::Func, 0);
        let entry = b.create_block(BlockKind::Normal);
        let panic_blk = panic_only.then(|| b.create_block(BlockKind::Normal));
        b.begin(entry);
        let x = b.param(Ty::I64);
        let res = b.call(Flags::NONE, callee_ref, sig, &[x]);
        if let Some(trap_blk) = panic_blk {
            b.jump((trap_blk, &[]));
            b.begin(trap_blk);
            b.trap(0);
        } else {
            b.ret(&[res]);
        }
        b.finish_checked().expect("valid direct-caller LIR")
    }

    fn build_loop_caller_fn(alloc: Alloc<'static>) -> blaze_lir::Func<Alloc<'static>> {
        let mut b: Builder<Alloc<'static>, Full> = Builder::new_in(64, alloc);
        let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        b.set_signature(sig);
        let callee_ref = b.reference(RefKind::Func, 0);

        let entry = b.create_block(BlockKind::Normal);
        let header = b.create_block(BlockKind::Normal);
        let body = b.create_block(BlockKind::Normal);
        let exit = b.create_block(BlockKind::Normal);

        b.begin(entry);
        let n = b.param(Ty::I64);
        let zero = b.iconst(Ty::I64, 0);
        b.jump((header, &[zero, zero]));

        b.begin(header);
        let i = b.param(Ty::I64);
        let acc = b.param(Ty::I64);
        let cond = b.icmp(blaze_lir::IntCond::Ult, i, n);
        b.br(Flags::NONE, cond, (body, &[]), (exit, &[acc]));

        b.begin(body);
        let one = b.iconst(Ty::I64, 1);
        let step = b.call(Flags::NONE, callee_ref, sig, &[i]);
        let next_i = b.binary(Op::Add, Flags::NONE, i, one);
        let next_acc = b.binary(Op::Add, Flags::NONE, acc, step);
        b.jump((header, &[next_i, next_acc]));

        b.begin(exit);
        let res = b.param(Ty::I64);
        b.ret(&[res]);

        b.finish_checked().expect("valid loop-caller LIR")
    }

    #[test]
    fn hot_caller_one_hop_bonus_ranks_direct_caller_immediately_after_hot_leaves() {
        let alloc = blaze_own::testing::heap().alloc();

        let sym_nested_leaf = Symbol([0xF1; 32]);
        let sym_shallow_leaf = Symbol([0xF2; 32]);
        let sym_direct_caller_nested = Symbol([0x10; 32]);
        let sym_direct_caller_shallow = Symbol([0x11; 32]);
        let sym_unrelated_loop_caller = Symbol([0x05; 32]);
        let sym_two_hop_caller = Symbol([0x06; 32]);
        let sym_cold_caller = Symbol([0x07; 32]);
        let sym_panic_caller = Symbol([0x08; 32]);
        let sym_unrelated_straight = Symbol([0x09; 32]);

        let fn_nested_leaf = build_nested_loop_leaf(alloc);
        let fn_shallow_leaf = build_single_loop_leaf(alloc);
        let fn_direct_caller_nested = build_direct_caller_fn(false, false, alloc);
        let fn_direct_caller_shallow = build_direct_caller_fn(false, false, alloc);
        let fn_unrelated_loop_caller = build_loop_caller_fn(alloc);
        let fn_two_hop_caller = build_direct_caller_fn(false, false, alloc);
        let fn_cold_caller = build_direct_caller_fn(true, false, alloc);
        let fn_panic_caller = build_direct_caller_fn(false, true, alloc);
        let fn_unrelated_straight = build_straight_line_fn(false, alloc);

        let mut mod_bodies: AVec<'static, ModuleBody<Alloc<'static>>> = Vec::new_in(alloc);
        mod_bodies.push(ModuleBody { id: BodyId(sym_nested_leaf.0), func: fn_nested_leaf.view().to_func_in(alloc) });
        mod_bodies.push(ModuleBody { id: BodyId(sym_shallow_leaf.0), func: fn_shallow_leaf.view().to_func_in(alloc) });
        mod_bodies.push(ModuleBody {
            id: BodyId(sym_direct_caller_nested.0),
            func: fn_direct_caller_nested.view().to_func_in(alloc),
        });
        mod_bodies.push(ModuleBody {
            id: BodyId(sym_direct_caller_shallow.0),
            func: fn_direct_caller_shallow.view().to_func_in(alloc),
        });
        mod_bodies.push(ModuleBody {
            id: BodyId(sym_unrelated_loop_caller.0),
            func: fn_unrelated_loop_caller.view().to_func_in(alloc),
        });
        mod_bodies.push(ModuleBody {
            id: BodyId(sym_two_hop_caller.0),
            func: fn_two_hop_caller.view().to_func_in(alloc),
        });
        mod_bodies.push(ModuleBody { id: BodyId(sym_cold_caller.0), func: fn_cold_caller.view().to_func_in(alloc) });
        mod_bodies.push(ModuleBody { id: BodyId(sym_panic_caller.0), func: fn_panic_caller.view().to_func_in(alloc) });
        mod_bodies.push(ModuleBody {
            id: BodyId(sym_unrelated_straight.0),
            func: fn_unrelated_straight.view().to_func_in(alloc),
        });

        let pgo = build_pgo_table(mod_bodies.as_slice(), None, alloc).expect("static PGO table");
        let inputs = [
            (sym_unrelated_straight, true, fn_unrelated_straight.view()),
            (sym_panic_caller, true, fn_panic_caller.view()),
            (sym_cold_caller, true, fn_cold_caller.view()),
            (sym_two_hop_caller, true, fn_two_hop_caller.view()),
            (sym_unrelated_loop_caller, true, fn_unrelated_loop_caller.view()),
            (sym_direct_caller_shallow, true, fn_direct_caller_shallow.view()),
            (sym_direct_caller_nested, true, fn_direct_caller_nested.view()),
            (sym_shallow_leaf, true, fn_shallow_leaf.view()),
            (sym_nested_leaf, true, fn_nested_leaf.view()),
        ];
        // Include duplicate edges for `sym_direct_caller_shallow` to verify deduplication.
        let edges = [
            (sym_direct_caller_nested, sym_nested_leaf),
            (sym_direct_caller_shallow, sym_shallow_leaf),
            (sym_direct_caller_shallow, sym_shallow_leaf),
            (sym_direct_caller_shallow, sym_shallow_leaf),
            (sym_direct_caller_shallow, sym_shallow_leaf),
            (sym_direct_caller_shallow, sym_shallow_leaf),
            (sym_two_hop_caller, sym_direct_caller_nested),
            (sym_cold_caller, sym_nested_leaf),
            (sym_panic_caller, sym_nested_leaf),
            (sym_unrelated_loop_caller, sym_unrelated_straight),
        ];

        let hot = rank_hot_symbols(&inputs, &edges, &pgo, alloc, alloc);
        // 0-call hot leaves rank first, followed immediately by direct 1-hop callers (ordered by
        // callee heat, deduplicated), then unrelated non-leaf loops. 2-hop callers, `#[cold]`
        // callers, panic-only callers, and unrelated straight-line bodies stay excluded.
        assert_eq!(
            hot.as_slice(),
            &[
                sym_nested_leaf,
                sym_shallow_leaf,
                sym_direct_caller_nested,
                sym_direct_caller_shallow,
                sym_unrelated_loop_caller,
            ]
        );

        // Deterministic across reversed body and call-edge orderings (Rule 25).
        let mut rev_inputs = inputs;
        rev_inputs.reverse();
        let mut rev_edges = edges;
        rev_edges.reverse();
        let hot_rev = rank_hot_symbols(&rev_inputs, &rev_edges, &pgo, alloc, alloc);
        assert_eq!(hot.as_slice(), hot_rev.as_slice());
        assert_eq!(
            compute_profile_digest(&pgo, hot.as_slice(), alloc),
            compute_profile_digest(&pgo, hot_rev.as_slice(), alloc),
        );
    }

    #[test]
    fn depth1_small_loop_leaf_qualifies_as_hot_leaf_ahead_of_hot_callers_while_straight_line_does_not() {
        let alloc = blaze_own::testing::heap().alloc();

        let sym_caller_nested = Symbol([0x01; 32]);
        let sym_caller_depth1 = Symbol([0x02; 32]);
        let sym_caller_warm_straight = Symbol([0x03; 32]);
        let sym_caller_straight = Symbol([0x04; 32]);
        let sym_straight_leaf = Symbol([0x05; 32]);
        let sym_warm_straight_leaf = Symbol([0x06; 32]);
        let sym_nested_leaf = Symbol([0xF1; 32]);
        let sym_depth1_leaf = Symbol([0xF2; 32]);

        let fn_nested_leaf = build_nested_loop_leaf(alloc);
        let fn_depth1_leaf = build_single_loop_leaf(alloc);
        let fn_straight_leaf = build_straight_line_fn(false, alloc);
        let fn_warm_straight_leaf = build_straight_line_fn(false, alloc);
        let fn_caller_nested = build_direct_caller_fn(false, false, alloc);
        let fn_caller_depth1 = build_direct_caller_fn(false, false, alloc);
        let fn_caller_warm_straight = build_direct_caller_fn(false, false, alloc);
        let fn_caller_straight = build_direct_caller_fn(false, false, alloc);

        assert_eq!(
            compute_loop_heat(fn_depth1_leaf.view(), alloc),
            LoopHeat { max_depth: 1, loop_blocks: 2 }
        );
        assert_eq!(
            compute_loop_heat(fn_straight_leaf.view(), alloc),
            LoopHeat { max_depth: 0, loop_blocks: 0 }
        );

        // Simulate post-LTO state where `sym_depth1_leaf` had only baseline straight-line static
        // PGO weight (`10`) pre-LTO before its loop was inlined (`own_score = 20 + 10 = 30`),
        // `sym_warm_straight_leaf` is 0-loop with weight `15` (above baseline `10`, below
        // `HOT_LEAF_THRESHOLD = 20`), and `sym_straight_leaf` sits at baseline `10`.
        let pgo = PgoSiteTable::new_in(
            &[],
            &[
                blaze_lir::pgo::BodyWeight { body: BodyId(sym_nested_leaf.0), weight: 100 },
                blaze_lir::pgo::BodyWeight {
                    body: BodyId(sym_depth1_leaf.0),
                    weight: STATIC_STRAIGHT_LINE_WEIGHT,
                },
                blaze_lir::pgo::BodyWeight {
                    body: BodyId(sym_caller_nested.0),
                    weight: STATIC_STRAIGHT_LINE_WEIGHT,
                },
                blaze_lir::pgo::BodyWeight {
                    body: BodyId(sym_caller_depth1.0),
                    weight: STATIC_STRAIGHT_LINE_WEIGHT,
                },
                blaze_lir::pgo::BodyWeight { body: BodyId(sym_warm_straight_leaf.0), weight: 15 },
                blaze_lir::pgo::BodyWeight {
                    body: BodyId(sym_caller_warm_straight.0),
                    weight: STATIC_STRAIGHT_LINE_WEIGHT,
                },
                blaze_lir::pgo::BodyWeight {
                    body: BodyId(sym_straight_leaf.0),
                    weight: STATIC_STRAIGHT_LINE_WEIGHT,
                },
                blaze_lir::pgo::BodyWeight {
                    body: BodyId(sym_caller_straight.0),
                    weight: STATIC_STRAIGHT_LINE_WEIGHT,
                },
            ],
            1,
            alloc,
        );

        let inputs = [
            (sym_caller_nested, true, fn_caller_nested.view()),
            (sym_caller_depth1, true, fn_caller_depth1.view()),
            (sym_caller_warm_straight, true, fn_caller_warm_straight.view()),
            (sym_caller_straight, true, fn_caller_straight.view()),
            (sym_straight_leaf, true, fn_straight_leaf.view()),
            (sym_warm_straight_leaf, true, fn_warm_straight_leaf.view()),
            (sym_depth1_leaf, false, fn_depth1_leaf.view()),
            (sym_nested_leaf, false, fn_nested_leaf.view()),
        ];
        let edges = [
            (sym_caller_nested, sym_nested_leaf),
            (sym_caller_depth1, sym_depth1_leaf),
            (sym_caller_warm_straight, sym_warm_straight_leaf),
            (sym_caller_straight, sym_straight_leaf),
        ];

        let hot = rank_hot_symbols(&inputs, &edges, &pgo, alloc, alloc);
        // `sym_depth1_leaf` (`own_score = 30 >= HOT_LEAF_THRESHOLD`) qualifies as `HotLeaf` and
        // ranks strictly ahead of `sym_caller_nested` (`HotLeafCaller`, `score = 135`) despite
        // lower numeric score, non-seed status, and higher symbol bytes. `sym_warm_straight_leaf`
        // (`own_score = 15 < HOT_LEAF_THRESHOLD`) stays in `Other` without propagating caller
        // heat, and baseline straight-line `sym_straight_leaf` (`own_score = 10`) stays excluded.
        assert_eq!(
            hot.as_slice(),
            &[
                sym_nested_leaf,
                sym_depth1_leaf,
                sym_caller_nested,
                sym_caller_depth1,
                sym_warm_straight_leaf,
            ]
        );
    }
}
