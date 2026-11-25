//! Whole-program demand-driven cross-function LIR inlining and LTO for `blaze export`.
//!
//! 1. Starts from workspace seed instances already reached by `Walk` and performs a bounded BFS
//!    (default `depth <= 16`, up to `4096` bodies) following normal-block non-cold `Op::Call` edges
//!    first (and `Op::Invoke` edges only with remaining budget) into reachable non-cold,
//!    non-noreturn callees and library callers (up to `max_caller_insts`, default `2048` raw LIR
//!    instructions, including `#[inline(never)]` hot algorithms whose own small callees inline into
//!    them while `lto_optimize_module` continues respecting `Inline::Never` and
//!    `policy.max_callee_insts` when deciding what to inline into callers). Cold (unless
//!    `#[inline(always)]`) and `-> !` noreturn bodies stay excluded.
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
    inline::{InlinePolicy, NoWeights},
    lto::{ModuleBody, OptimizedBody, lto_optimize_module},
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

/// Configuration for [`optimize_export_workspace`] (Rule 83: one options struct).
#[derive(Clone, Copy, Debug)]
pub struct ExportLtoOpts {
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
    /// Maximum LIR instruction count for keeping a reachable non-cold library caller (e.g.
    /// `core::slice::sort::drift::sort`, `hashbrown::RawTable::find`) in the LTO module so its
    /// own small callees inline bottom-up into it and it emits with host-ISA SOTA codegen.
    pub max_caller_insts: usize,
    /// Cross-function inlining policy handed to `blaze_lir::lto::lto_optimize_module`.
    pub policy: InlinePolicy,
}

impl ExportLtoOpts {
    /// Release export defaults: bounded demand-driven BFS (`depth <= 16`, `<= 4096` bodies,
    /// non-cold library callers `<= 2048` LIR insts) and aggressive cross-function inlining
    /// (`lto_optimize_module` enforces `Inline::Never` and `policy.max_callee_insts` when
    /// deciding whether a body may be inlined into its own callers).
    pub fn for_export(isa: Isa) -> Self {
        Self {
            machine: export_machine(isa),
            max_depth: 16,
            max_bodies: 4096,
            max_callee_insts: 384,
            max_always_inline_insts: 640,
            max_caller_insts: 2048,
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

/// Runs demand-driven whole-program cross-function LIR inlining and host-ISA SOTA emission over
/// Decides whether a non-workspace callee reached during demand-driven export LTO BFS enters
/// the LTO module. Non-cold, non-noreturn library callers up to `opts.max_caller_insts` (including
/// `#[inline(never)]` hot algorithms such as `core::slice::sort::drift::sort`) are kept so their
/// own small callees inline bottom-up into them and they emit with host-ISA SOTA codegen, while
/// `lto_optimize_module` continues enforcing `Inline::Never` and `opts.policy.max_callee_insts`
/// when deciding whether to inline them into their own callers. Cold and noreturn bodies stay out
/// of the module unless explicitly marked `#[inline(always)]`.
fn should_keep_non_seed(
    attrs: u32,
    insts: usize,
    in_tiny_reserve: bool,
    tiny_leaf_insts: usize,
    opts: &ExportLtoOpts,
) -> bool {
    let is_cold = (attrs & FnAttrs::COLD) != 0;
    let is_noreturn = (attrs & FnAttrs::NORETURN) != 0;
    let inline_attr = FnAttrs(attrs).inline();
    if is_noreturn || (is_cold && inline_attr != Inline::Always) {
        return false;
    }
    if in_tiny_reserve {
        return matches!(inline_attr, Inline::Always | Inline::Hint) && insts <= tiny_leaf_insts;
    }
    let keep_limit = opts
        .max_caller_insts
        .max(if inline_attr == Inline::Always || inline_attr == Inline::Hint {
            opts.max_always_inline_insts
        } else {
            opts.max_callee_insts
        });
    insts <= keep_limit
}

/// Whole-program demand-driven cross-function LIR inlining and host-ISA SOTA emission over
/// workspace seed rows and their small inlineable callees.
#[expect(clippy::too_many_arguments, reason = "whole-program export LTO over walk, seeds, gate, maker, options, output allocator, and scratch allocator")]
pub fn optimize_export_workspace<'a, 's, 'n, G, T>(
    walk: &Walk<'_>,
    seeds: &[usize],
    is_eligible_callee: impl Fn(usize) -> bool,
    mut grant_for: impl FnMut(ItemKey) -> Option<G>,
    maker: &mut Maker<'n, G, T>,
    opts: ExportLtoOpts,
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
        if !is_seed && !should_keep_non_seed(view.head.attrs, view.len(), in_tiny_reserve, TINY_INLINE_LEAF_INSTS, &opts) {
            continue;
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
            module_bodies: 0,
            total_inlined: 0,
        });
    }

    // 2. Bottom-up Tarjan SCC cross-function LIR inlining + SROA/propagate/DCE/CFG-coalesce.
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

    let bodies_view = LtoBodies { origins: origins.as_slice(), by_sym: by_sym.as_slice() };
    let lto = lto_optimize_module(mod_bodies, &bodies_view, &NoWeights, opts.policy, scratch)
        .map_err(ExportLtoError::Optimize)?;

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

    #[test]
    fn non_seed_filter_keeps_non_cold_library_callers_and_excludes_cold_and_noreturn() {
        let opts = ExportLtoOpts::for_export(Isa { lse: true, os: blaze_emit::Os::Linux });
        let no_inline = FnAttrs(0).with_inline(Inline::Never).0;
        let always_inline = FnAttrs(0).with_inline(Inline::Always).0;
        let hint_inline = FnAttrs(0).with_inline(Inline::Hint).0;

        // Non-cold library callers (even `#[inline(never)]` sorting/hashing kernels) up to
        // `max_caller_insts` enter the module so their own small callees inline into them.
        assert!(should_keep_non_seed(0, 900, false, 24, &opts));
        assert!(should_keep_non_seed(no_inline, 1500, false, 24, &opts));
        assert!(!should_keep_non_seed(0, opts.max_caller_insts + 1, false, 24, &opts));

        // Cold and noreturn bodies stay excluded (except explicitly `#[inline(always)]` cold).
        assert!(!should_keep_non_seed(FnAttrs::COLD, 16, false, 24, &opts));
        assert!(!should_keep_non_seed(FnAttrs::COLD | no_inline, 16, false, 24, &opts));
        assert!(!should_keep_non_seed(FnAttrs::NORETURN, 16, false, 24, &opts));
        assert!(!should_keep_non_seed(FnAttrs::NORETURN | always_inline, 16, false, 24, &opts));
        assert!(should_keep_non_seed(FnAttrs::COLD | always_inline, 16, false, 24, &opts));

        // Tiny-reserve tail admits only `<= 24`-inst `#[inline]` / `#[inline(always)]` leaves.
        assert!(should_keep_non_seed(hint_inline, 24, true, 24, &opts));
        assert!(should_keep_non_seed(always_inline, 12, true, 24, &opts));
        assert!(!should_keep_non_seed(0, 12, true, 24, &opts));
        assert!(!should_keep_non_seed(hint_inline, 25, true, 24, &opts));
    }
}
