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
//!    probed host [`Machine`] (`export_machine`: `LSE | DOTPROD | SME | SME2 | CSSC | SHA3`).
//! 4. Keeps host-specific LTO records strictly in the caller's export allocator — never writing
//!    them back to portable `CodeStore` (Rule 25).

use core::fmt;

use blaze_code::record::{Symbol, TrapSource};
use blaze_emit::Isa;
use blaze_lir::{
    Block, BlockKind, Flags, Op, VerifyError,
    body::{Bodies, BodyId, DatumId, Target},
    func::{FnAttrs, Inline, RefId, RefKind, RefRec},
    inline::{InlinePolicy, NoWeights},
    lto::{ModuleBody, OptimizedBody, lto_optimize_module},
    simplify::SimplifyError,
    verify,
};
use blaze_own::{AVec, Alloc, Vec};
use blaze_tier::Machine;
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
    /// Cross-function inlining policy handed to `blaze_lir::lto::lto_optimize_module`.
    pub policy: InlinePolicy,
}

impl ExportLtoOpts {
    /// Release export defaults: bounded demand-driven BFS (`depth <= 12`, `<= 1536` bodies,
    /// raw non-workspace callees `<= 384` LIR insts / `#[inline]` & `#[inline(always)]` `<= 640`)
    /// and aggressive cross-function inlining.
    pub fn for_export(isa: Isa) -> Self {
        Self {
            machine: export_machine(isa),
            max_depth: 12,
            max_bodies: 1536,
            max_callee_insts: 384,
            max_always_inline_insts: 640,
            policy: InlinePolicy {
                cost_threshold: 120,
                max_callee_insts: 256,
                hot_multiplier: 4,
                hot_weight_threshold: 10,
                max_depth: 12,
                max_growth: 2048,
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

/// Runs demand-driven whole-program cross-function LIR inlining and host-ISA SOTA emission over
/// workspace seed rows and their small inlineable callees.
#[expect(clippy::too_many_arguments, reason = "whole-program export LTO over walk, seeds, gate, maker, options, and allocator")]
pub fn optimize_export_workspace<'a, 'n, G, T>(
    walk: &Walk<'_>,
    seeds: &[usize],
    is_eligible_callee: impl Fn(usize) -> bool,
    mut grant_for: impl FnMut(ItemKey) -> Option<G>,
    maker: &mut Maker<'n, G, T>,
    opts: ExportLtoOpts,
    alloc: Alloc<'a>,
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

    let mut visited: AVec<'a, u8> = Vec::with_capacity_in(walk.len(), alloc);
    visited.resize(walk.len(), UNSEEN);
    let mut call_queue: AVec<'a, (usize, u8, bool)> = Vec::with_capacity_in(seeds.len().min(opts.max_bodies), alloc);
    let mut invoke_queue: AVec<'a, (usize, u8, bool)> = Vec::new_in(alloc);
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
    let mut origins: AVec<'a, LtoOrigin<'a>> = Vec::with_capacity_in(call_queue.len(), alloc);
    let mut call_head = 0usize;
    let mut invoke_head = 0usize;
    while origins.len() < opts.max_bodies {
        let next = if let Some(&item) = call_queue.get(call_head) {
            call_head += 1;
            item
        } else if let Some(&item) = invoke_queue.get(invoke_head) {
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
        let tabled = walk.tabled(row);
        let symbol = walk.symbol(row);
        let name = walk.name(row);
        let Some(grant) = grant_for(tabled) else {
            return Err(ExportLtoError::NotRunnable { symbol: symbol.0, tabled });
        };
        let built = maker
            .build_lir(&grant, tabled, name, alloc)
            .map_err(|error| ExportLtoError::BuildLir { symbol: symbol.0, error })?;

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
    let mut by_sym: AVec<'a, ([u8; 32], usize)> = Vec::with_capacity_in(n, alloc);
    let mut mod_bodies: AVec<'a, ModuleBody<Alloc<'a>>> = Vec::with_capacity_in(n, alloc);
    for (idx, orig) in origins.iter().enumerate() {
        by_sym.push((orig.symbol.0, idx));
        mod_bodies.push(ModuleBody {
            id: BodyId(orig.symbol.0),
            func: orig.built.func.view().to_func_in(alloc),
        });
    }
    by_sym.sort_unstable();

    let bodies_view = LtoBodies { origins: origins.as_slice(), by_sym: by_sym.as_slice() };
    let lto = lto_optimize_module(mod_bodies, &bodies_view, &NoWeights, opts.policy, alloc)
        .map_err(ExportLtoError::Optimize)?;

    // 3. Expand reference tables transitively for every `ImportedRef` and emit host-ISA records.
    let mut out_bodies: AVec<'a, LtoBodyOut<'a>> = Vec::with_capacity_in(n, alloc);
    for (idx, (orig, opt)) in origins.iter().zip(lto.bodies.iter()).enumerate() {
        if !orig.is_seed && opt.read.is_empty() {
            continue;
        }
        let mut funcs: AVec<'a, Callee<'_>> = Vec::with_capacity_in(orig.built.funcs.len() + opt.imported_refs.len(), alloc);
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
        let mut statics: AVec<'a, ItemKey> = Vec::with_capacity_in(orig.built.statics.len(), alloc);
        statics.extend_from_slice(orig.built.statics.as_slice());
        let mut c_statics: AVec<'a, (ItemKey, &str)> = Vec::with_capacity_in(orig.built.c_statics.len(), alloc);
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
        let mut vtables: AVec<'a, Symbol> = Vec::with_capacity_in(orig.built.vtables.len(), alloc);
        vtables.extend_from_slice(orig.built.vtables.as_slice());
        let mut vtable_data: AVec<'a, (Symbol, AVec<'a, u8>)> = Vec::with_capacity_in(orig.built.vtable_data.len(), alloc);
        for (vsym, bytes) in orig.built.vtable_data.iter() {
            push_vtable_datum(&mut vtable_data, *vsym, bytes.as_slice(), alloc);
        }
        let mut tls: AVec<'a, ItemKey> = Vec::with_capacity_in(orig.built.tls.len(), alloc);
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
        let (record, called_declared) = emit_code_record_for_machine(verified, &names, routines, traps, opts.machine, alloc)
            .map_err(|error| ExportLtoError::Record { symbol: orig.symbol.0, error })?;
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
