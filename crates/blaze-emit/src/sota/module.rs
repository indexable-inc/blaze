//! Whole-Program / Module-Level SOTA Compilation (`ThinLTO` + Cross-Function Priority Inlining
//! + Bottom-Up IPRA + Direct `Branch26` / `GotSlot` Native AArch64 Emission).
//!
//! Connects `blaze_lir::lto::lto_optimize_module` directly to `Sota`'s mid-end optimization stack
//! and IPRA-aware AArch64 backend (`isel::emit_sota_a64_with_ipra`) in one canonical sans-IO home:
//!
//! 1. Builds the intra-module call graph and runs deterministic bottom-up Tarjan SCC ThinLTO
//!    (`lto_optimize_module`): pre-inline SROA/propagate/DCE, priority-guided cross-function
//!    inlining honoring `#[inline(always)]` / `#[inline]` / `#[inline(never)]` / `#[cold]` and
//!    PGO [`SiteWeights`], and post-inline SROA/propagate/DCE across spliced boundaries.
//! 2. Resolves transitive multi-level [`ImportedRef`]s back to their originating body's [`Names`]
//!    without cloning or duplicating external reference tables.
//! 3. Runs the full `Sota` mid-end optimization pipeline on every body (`Aegraph` fixpoint, BCE,
//!    unswitch, if-convert, memory LICM/DSE, loop fusion, interchange/tiling, 4× NEON / memchr /
//!    IV-div / SLP / 4× scalar unroll / rotate, dispatch tail-dup, optional e-graph superopt,
//!    DAG scheduling, cold-block sinking).
//! 4. Emits bodies in reverse-topological (callees-first) order with **Interprocedural Register
//!    Allocation (IPRA)**: every non-inlined direct internal call uses its callee's exact
//!    [`CallClobbers`] summary so callers keep live values in untouched caller-saved registers
//!    across calls with zero spills.

use blaze_code::record::Symbol;
use blaze_lir::{
    Op, Verified, View,
    body::{Bodies, BodyId, DatumId, Target},
    func::{FnAttrs, Func, RefId, RefKind},
    inline::{InlinePolicy, SiteWeights},
    lto::{ModuleBody, OptimizedBody, lto_optimize_module, tarjan_sccs},
    superopt::SuperoptOpts,
    verify,
};
use blaze_own::{Alloc, Vec};
use blaze_tier::{
    Arch, CallClobbers, Emitted, Machine, Named, Names, Outside, Reg, RegSet, RoutinePlaces, Tier,
    TierError, TierName,
};

use super::{
    CallMode, Sota,
    abi::{AbiFnInput, CallConvKind, ZeroAbiOpts, ZeroAbiStats, plan_module_abi},
    bolt::{BoltCallEdge, BoltFn, BoltLayoutStats, BoltOpts, PlacedFn, pack_module_pages},
    isel, optimize_sota_midend,
    regalloc::ScratchOrder,
};

/// One input function in a whole-program / crate SOTA compilation batch.
#[derive(Clone, Copy)]
pub struct ModuleFn<'a> {
    pub symbol: Symbol,
    pub func: Verified<'a>,
    pub names: &'a dyn Names,
}

/// Configuration for [`compile_module_a64`] (Rule 83: one options struct).
#[derive(Clone, Copy, Debug)]
pub struct SotaModuleOpts {
    pub machine: Machine,
    pub call_mode: CallMode,
    pub policy: InlinePolicy,
    pub superopt: Option<SuperoptOpts>,
    pub auto_vectorize: bool,
    pub max_rounds: u32,
}

impl SotaModuleOpts {
    /// Release AOT export defaults (`DirectBl`, aggressive cross-function inlining, superopt, NEON).
    pub const fn for_export(machine: Machine) -> Self {
        Self {
            machine,
            call_mode: CallMode::DirectBl,
            policy: InlinePolicy {
                cost_threshold: 75,
                max_callee_insts: 192,
                hot_multiplier: 4,
                hot_weight_threshold: 10,
                max_depth: 8,
                max_growth: 1024,
                allow_external_refs: true,
                allow_loops: true,
            },
            superopt: Some(SuperoptOpts::apple_silicon()),
            auto_vectorize: true,
            max_rounds: 3,
        }
    }

    /// JIT / hot tier-up defaults (`GotSlot`, cross-function inlining, NEON).
    pub const fn for_jit(machine: Machine) -> Self {
        Self {
            machine,
            call_mode: CallMode::GotSlot,
            policy: InlinePolicy {
                cost_threshold: 45,
                max_callee_insts: 128,
                hot_multiplier: 4,
                hot_weight_threshold: 10,
                max_depth: 6,
                max_growth: 512,
                allow_external_refs: true,
                allow_loops: true,
            },
            superopt: None,
            auto_vectorize: true,
            max_rounds: 3,
        }
    }
}

/// One compiled function emitted by [`compile_module_a64`].
pub struct CompiledModuleFn<'a> {
    pub symbol: Symbol,
    pub emitted: Emitted<'a>,
    pub clobbers: CallClobbers,
    pub inlined_bodies: usize,
    pub used_fallback: bool,
}

/// Whole-module compilation summary & emitted records from [`compile_module_a64`].
pub struct SotaModuleOut<'a> {
    pub functions: Vec<CompiledModuleFn<'a>, Alloc<'a>>,
    pub scc_count: usize,
    pub total_inlined_bodies: usize,
    pub ipra_calls_narrowed: usize,
    pub abi_stats: ZeroAbiStats,
    pub bolt_layout: BoltLayoutStats,
    pub page_order: Vec<PlacedFn, Alloc<'a>>,
}

/// [`Bodies`] view over the original input slice of [`ModuleFn`], resolving `RefKind::Func`
/// references via each function's [`Names`] table.
struct InputBodies<'a, 'b, 'f> {
    fns: &'b [ModuleFn<'f>],
    by_sym: Vec<([u8; 32], usize), Alloc<'a>>,
}

impl<'a, 'b, 'f> InputBodies<'a, 'b, 'f> {
    fn new(fns: &'b [ModuleFn<'f>], alloc: Alloc<'a>) -> Self {
        let mut by_sym: Vec<([u8; 32], usize), Alloc<'a>> = Vec::with_capacity_in(fns.len(), alloc);
        for (i, f) in fns.iter().enumerate() {
            by_sym.push((f.symbol.0, i));
        }
        by_sym.sort_unstable();
        Self { fns, by_sym }
    }

    fn find_idx(&self, sym: [u8; 32]) -> Option<usize> {
        let at = self.by_sym.binary_search_by(|row| row.0.cmp(&sym)).ok()?;
        self.by_sym.get(at).map(|row| row.1)
    }
}

impl Bodies for InputBodies<'_, '_, '_> {
    fn callee(&self, from: BodyId, reference: RefId) -> Option<(BodyId, View<'_>)> {
        let caller_idx = self.find_idx(from.0)?;
        let caller = self.fns.get(caller_idx)?;
        let view = caller.func.view();
        let row = view.refs.get(reference.index())?;
        if RefKind::from_word(row.kind)? != RefKind::Func {
            return None;
        }
        let Named::Symbol(callee_sym) = caller.names.name(Outside::Func(row.place))? else {
            return None;
        };
        let callee_idx = self.find_idx(callee_sym.0)?;
        Some((BodyId(callee_sym.0), self.fns.get(callee_idx)?.func.view()))
    }

    fn target(&self, from: BodyId, reference: RefId) -> Option<Target> {
        let caller_idx = self.find_idx(from.0)?;
        let caller = self.fns.get(caller_idx)?;
        let row = caller.func.view().refs.get(reference.index())?;
        let kind = RefKind::from_word(row.kind)?;
        let outside = match kind {
            RefKind::Func => Outside::Func(row.place),
            RefKind::Static => Outside::Static(row.place),
            RefKind::Vtable => Outside::Vtable(row.place),
            RefKind::Tls => Outside::Tls(row.place),
            RefKind::Data => return None,
        };
        let Named::Symbol(sym) = caller.names.name(outside)? else {
            return None;
        };
        match kind {
            RefKind::Func => Some(Target::Func(BodyId(sym.0))),
            RefKind::Static | RefKind::Vtable | RefKind::Tls => Some(Target::Datum(DatumId(sym.0))),
            RefKind::Data => None,
        }
    }
}

/// [`Names`] adapter that transparently resolves both original `Outside` references on body
/// `body_idx` and transitively imported references from multi-level inlined callees.
struct ImportedNames<'a, 'b, 'f> {
    body_idx: usize,
    fns: &'b [ModuleFn<'f>],
    lto_bodies: &'b [OptimizedBody<Alloc<'a>>],
    by_sym: &'b [([u8; 32], usize)],
}

impl<'a, 'b, 'f> ImportedNames<'a, 'b, 'f> {
    fn find_idx(&self, sym: [u8; 32]) -> Option<usize> {
        let at = self.by_sym.binary_search_by(|row| row.0.cmp(&sym)).ok()?;
        self.by_sym.get(at).map(|row| row.1)
    }

    fn resolve(&self, mut cur_idx: usize, mut of: Outside) -> Option<Named<'_>> {
        for _ in 0..=32 {
            let (want_kind, place) = match of {
                Outside::Func(p) => (RefKind::Func, p),
                Outside::Static(p) => (RefKind::Static, p),
                Outside::Vtable(p) => (RefKind::Vtable, p),
                Outside::Tls(p) => (RefKind::Tls, p),
                Outside::TlsGetter | Outside::TlsDesc => return self.fns.get(cur_idx)?.names.name(of),
            };
            let mut next = None;
            for imp in self.lto_bodies.get(cur_idx)?.imported_refs.iter() {
                if RefKind::from_word(imp.rec.kind) == Some(want_kind) && imp.rec.place == place {
                    let src_idx = self.find_idx(imp.source_body.0)?;
                    let src_row = self.lto_bodies.get(src_idx)?.func.view().refs.get(imp.source_ref.index())?;
                    let src_outside = match of {
                        Outside::Func(_) => Outside::Func(src_row.place),
                        Outside::Static(_) => Outside::Static(src_row.place),
                        Outside::Vtable(_) => Outside::Vtable(src_row.place),
                        Outside::Tls(_) => Outside::Tls(src_row.place),
                        Outside::TlsGetter | Outside::TlsDesc => of,
                    };
                    next = Some((src_idx, src_outside));
                    break;
                }
            }
            if let Some((src_idx, src_outside)) = next {
                cur_idx = src_idx;
                of = src_outside;
            } else {
                return self.fns.get(cur_idx)?.names.name(of);
            }
        }
        None
    }
}

impl Names for ImportedNames<'_, '_, '_> {
    fn name(&self, of: Outside) -> Option<Named<'_>> {
        self.resolve(self.body_idx, of)
    }

    fn routines(&self) -> RoutinePlaces {
        self.fns.get(self.body_idx).map_or(RoutinePlaces::NONE, |f| f.names.routines())
    }
}

/// Estimates caller live-register pressure (`(gpr_pressure, fp_pressure)`) from post-LTO LIR
/// before emission so [`plan_module_abi`] can assign register-avoidance [`ScratchOrder`]s to
/// callees prior to code generation.
fn estimate_caller_live_pressure(view: View<'_>) -> (RegSet, RegSet) {
    const SCRATCH_GPRS: [u8; 7] = [9, 10, 11, 12, 13, 14, 15];
    const SCRATCH_FPS: [u8; 13] = [16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28];

    let mut last_call: Option<usize> = None;
    for inst in 0..view.len() {
        if matches!(view.op(inst), Op::Call | Op::Invoke | Op::CallIndirect | Op::InvokeIndirect | Op::TailCall | Op::TailCallIndirect) {
            last_call = Some(inst);
        }
    }
    let Some(last_call_idx) = last_call else {
        return (RegSet::EMPTY, RegSet::EMPTY);
    };

    let mut gpr_args = 0usize;
    let mut fp_args = 0usize;
    let mut gpr_temps = 0usize;
    let mut fp_temps = 0usize;
    let entry_params = view.params(blaze_lir::Block(0));
    for inst in 0..last_call_idx {
        let Some(&ty_raw) = view.tys.get(inst) else {
            break;
        };
        let ty = blaze_lir::Ty(ty_raw);
        if ty == blaze_lir::Ty::NONE {
            continue;
        }
        let op = view.op(inst);
        if matches!(op, Op::Iconst | Op::IconstWide | Op::Fconst | Op::FconstWide) {
            continue;
        }
        let is_entry_param = inst >= entry_params.start && inst < entry_params.end;
        if ty.is_float() || ty.is_vector() {
            if is_entry_param {
                fp_args += 1;
            } else {
                fp_temps += 1;
            }
        } else if is_entry_param {
            gpr_args += 1;
        } else {
            gpr_temps += 1;
        }
    }

    // Cap pressure strictly below caller-saved pool sizes so callees always retain untouched
    // registers ahead of demoted caller-hot registers in `ScratchOrder`.
    let mut gpr_set = RegSet::EMPTY;
    for &r in SCRATCH_GPRS.get(..gpr_temps.min(4)).unwrap_or(&[]) {
        gpr_set = gpr_set.with(Reg(r));
    }
    for r in 0..u8::try_from(gpr_args.min(4)).unwrap_or(0) {
        gpr_set = gpr_set.with(Reg(r));
    }

    let mut fp_set = RegSet::EMPTY;
    for &v in SCRATCH_FPS.get(..fp_temps.min(6)).unwrap_or(&[]) {
        fp_set = fp_set.with(Reg(32 + v));
    }
    for v in 0..u8::try_from(fp_args.min(4)).unwrap_or(0) {
        fp_set = fp_set.with(Reg(32 + v));
    }
    (gpr_set, fp_set)
}

/// Resolves a direct `Op::Call` or `Op::Invoke` instruction to its intra-module callee index.
fn direct_callee_idx(
    view: View<'_>,
    inst: usize,
    names: &ImportedNames<'_, '_, '_>,
    input_bodies: &InputBodies<'_, '_, '_>,
) -> Option<usize> {
    if !matches!(view.op(inst), Op::Call | Op::Invoke | Op::TailCall) {
        return None;
    }
    let &data_at = view.data.get(inst)?;
    let &ref_word = usize::try_from(data_at).ok().and_then(|at| view.extra.get(at))?;
    let r = view.refs.get(RefId(ref_word).index())?;
    if RefKind::from_word(r.kind) != Some(RefKind::Func) {
        return None;
    }
    let Named::Symbol(sym) = names.name(Outside::Func(r.place))? else {
        return None;
    };
    input_bodies.find_idx(sym.0)
}

/// Compiles a whole module of verified LIR functions through `ThinLTO` cross-function inlining,
/// `Sota`'s full mid-end optimization pipeline, and bottom-up IPRA AArch64 code generation.
///
/// If a function contains ops outside `Sota`'s subset (e.g. inline `asm!` or `f128`) and
/// `fallback` is `Some`, it compiles the already inlined & SOTA-optimized LIR via `fallback`.
pub fn compile_module_a64<'a, 'f, F: Tier>(
    fns: &[ModuleFn<'f>],
    weights: &dyn SiteWeights,
    opts: SotaModuleOpts,
    fallback: Option<&F>,
    alloc: Alloc<'a>,
) -> Result<SotaModuleOut<'a>, TierError> {
    if opts.machine.arch != Arch::Arm64 {
        return Err(TierError::Refused {
            tier: TierName::Sota,
            inst: None,
            what: "compile_module_a64 requires Arch::Arm64",
        });
    }
    let n = fns.len();
    let input_bodies = InputBodies::new(fns, alloc);

    // 1. Bottom-up Tarjan SCC ThinLTO: pre-inline simplify, priority-guided cross-function
    //    inlining honoring `FnAttrs` & PGO weights, and post-inline SROA/propagate/DCE.
    let mut mod_bodies: Vec<ModuleBody<Alloc<'a>>, Alloc<'a>> = Vec::with_capacity_in(n, alloc);
    for f in fns {
        mod_bodies.push(ModuleBody {
            id: BodyId(f.symbol.0),
            func: f.func.view().to_func_in(alloc),
        });
    }
    let lto = lto_optimize_module(mod_bodies, &input_bodies, weights, opts.policy, alloc).map_err(|_| {
        TierError::Refused {
            tier: TierName::Sota,
            inst: None,
            what: "ThinLTO module inlining failed",
        }
    })?;

    // 2. Run full Sota per-function mid-end optimization pipeline on every post-inline body.
    let mut opt_funcs: Vec<Func<Alloc<'a>>, Alloc<'a>> = Vec::with_capacity_in(n, alloc);
    for body in lto.bodies.iter() {
        let opt_f = optimize_sota_midend(
            body.func.view(),
            opts.machine,
            opts.max_rounds,
            opts.auto_vectorize,
            opts.superopt,
            alloc,
        )?;
        opt_funcs.push(opt_f);
    }

    // 3. Build post-inline direct call graph for bottom-up IPRA emission order.
    let mut adj_offsets: Vec<u32, Alloc<'a>> = Vec::with_capacity_in(n + 1, alloc);
    let mut adj: Vec<u32, Alloc<'a>> = Vec::with_capacity_in(n * 2, alloc);
    adj_offsets.push(0);
    for (idx, opt_func) in opt_funcs.iter().enumerate() {
        let names = ImportedNames {
            body_idx: idx,
            fns,
            lto_bodies: &lto.bodies,
            by_sym: &input_bodies.by_sym,
        };
        let view = opt_func.view();
        let edge_start = adj.len();
        for inst in 0..view.len() {
            if let Some(callee_idx) = direct_callee_idx(view, inst, &names, &input_bodies)
                && let Ok(callee_u32) = u32::try_from(callee_idx)
            {
                adj.push(callee_u32);
            }
        }
        if let Some(slice) = adj.get_mut(edge_start..) {
            slice.sort_unstable();
        }
        let mut w = edge_start;
        let mut prev: Option<u32> = None;
        for r in edge_start..adj.len() {
            let Some(&val) = adj.get(r) else {
                break;
            };
            if prev != Some(val) {
                if let Some(dst) = adj.get_mut(w) {
                    *dst = val;
                }
                w += 1;
                prev = Some(val);
            }
        }
        adj.truncate(w);
        let adj_len = u32::try_from(adj.len()).map_err(|_| TierError::Refused {
            tier: TierName::Sota,
            inst: None,
            what: "module call graph exceeded u32",
        })?;
        adj_offsets.push(adj_len);
    }
    let emit_sccs = tarjan_sccs(n, &adj_offsets, &adj, alloc);

    // 4. Whole-module Zero-ABI planning BEFORE emission: internal fast ABI, cold `preserve_all`,
    //    global SCC pointer pinning, and caller-pressure register-avoidance `ScratchOrder`s from
    //    the post-LTO call graph & `FnAttrs::COLD`.
    let mut in_degree: Vec<usize, Alloc<'a>> = Vec::with_capacity_in(n, alloc);
    let mut address_taken: Vec<bool, Alloc<'a>> = Vec::with_capacity_in(n, alloc);
    for _ in 0..n {
        in_degree.push(0);
        address_taken.push(false);
    }
    let mut callees_per_fn: Vec<Vec<usize, Alloc<'a>>, Alloc<'a>> = Vec::with_capacity_in(n, alloc);
    for caller in 0..n {
        let start = adj_offsets.get(caller).copied().and_then(|v| usize::try_from(v).ok()).unwrap_or(0);
        let end = adj_offsets.get(caller + 1).copied().and_then(|v| usize::try_from(v).ok()).unwrap_or(start);
        let mut row: Vec<usize, Alloc<'a>> = Vec::with_capacity_in(end.saturating_sub(start), alloc);
        for &callee_u32 in adj.get(start..end).unwrap_or(&[]) {
            if let Ok(cidx) = usize::try_from(callee_u32)
                && cidx < n
            {
                row.push(cidx);
                if let Some(deg) = in_degree.get_mut(cidx) {
                    *deg += 1;
                }
            }
        }
        callees_per_fn.push(row);
    }
    for (idx, opt_func) in opt_funcs.iter().enumerate() {
        let names = ImportedNames {
            body_idx: idx,
            fns,
            lto_bodies: &lto.bodies,
            by_sym: &input_bodies.by_sym,
        };
        let view = opt_func.view();
        for inst in 0..view.len() {
            if view.op(inst) == Op::FuncAddr
                && let Some(&ref_word) = view.data.get(inst)
                && let Some(r) = view.refs.get(RefId(ref_word).index())
                && RefKind::from_word(r.kind) == Some(RefKind::Func)
                && let Some(Named::Symbol(sym)) = names.name(Outside::Func(r.place))
                && let Some(target_idx) = input_bodies.find_idx(sym.0)
                && let Some(slot) = address_taken.get_mut(target_idx)
            {
                *slot = true;
            }
        }
        for reloc in view.data_relocs {
            if let Some(r) = view.refs.get(RefId(reloc.target).index())
                && RefKind::from_word(r.kind) == Some(RefKind::Func)
                && let Some(Named::Symbol(sym)) = names.name(Outside::Func(r.place))
                && let Some(target_idx) = input_bodies.find_idx(sym.0)
                && let Some(slot) = address_taken.get_mut(target_idx)
            {
                *slot = true;
            }
        }
    }
    const EMPTY_CALLEES: &[usize] = &[];
    let mut abi_inputs: Vec<AbiFnInput<'_>, Alloc<'a>> = Vec::with_capacity_in(n, alloc);
    for idx in 0..n {
        let Some(opt_func) = opt_funcs.get(idx) else {
            break;
        };
        let view = opt_func.view();
        let is_exported = in_degree.get(idx).copied().unwrap_or(0) == 0;
        let is_addr_taken = address_taken.get(idx).copied().unwrap_or(false);
        let is_cold = (view.head.attrs & FnAttrs::COLD) != 0;
        let direct_callees = callees_per_fn.get(idx).map_or(EMPTY_CALLEES, Vec::as_slice);
        let (caller_live_pressure_gprs, caller_live_pressure_fps) = estimate_caller_live_pressure(view);
        abi_inputs.push(AbiFnInput {
            view,
            is_exported,
            address_taken: is_addr_taken,
            is_cold,
            direct_callees,
            caller_live_pressure_gprs,
            caller_live_pressure_fps,
        });
    }
    let abi_plan = plan_module_abi(&abi_inputs, ZeroAbiOpts::arm64_export(), alloc);
    let abi_stats = abi_plan.stats;

    // 5. Emit callees before callers with Zero-ABI `ScratchOrder` + `ColdPreserveAll` + IPRA
    //    (`CallClobbers` propagation).
    let mut compiled: Vec<Option<CompiledModuleFn<'a>>, Alloc<'a>> = Vec::with_capacity_in(n, alloc);
    let mut known_clobbers: Vec<Option<CallClobbers>, Alloc<'a>> = Vec::with_capacity_in(n, alloc);
    for _ in 0..n {
        compiled.push(None);
        known_clobbers.push(None);
    }
    let is_macos = opts.machine.os == blaze_tier::Os::MacOs;
    let mut ipra_calls_narrowed = 0usize;

    for scc_idx in 0..emit_sccs.len() {
        let comp = emit_sccs.component(scc_idx);
        for &node in comp {
            let idx = usize::try_from(node).map_err(|_| TierError::Refused {
                tier: TierName::Sota,
                inst: None,
                what: "module SCC node index overflow",
            })?;
            let names = ImportedNames {
                body_idx: idx,
                fns,
                lto_bodies: &lto.bodies,
                by_sym: &input_bodies.by_sym,
            };
            let opt_func = opt_funcs.get(idx).ok_or(TierError::Refused {
                tier: TierName::Sota,
                inst: None,
                what: "missing module function",
            })?;
            let verified = verify(opt_func.view()).map_err(|_| TierError::Refused {
                tier: TierName::Sota,
                inst: None,
                what: "post-LTO optimized LIR failed verify",
            })?;

            // Count & feed IPRA call-clobber masks for direct `Op::Call` / `Op::Invoke` sites
            // targeting already-compiled callees with strictly narrower clobber sets than C ABI.
            let view = verified.view();
            let lookup_clobbers = |inst: usize| -> Option<CallClobbers> {
                let cidx = direct_callee_idx(view, inst, &names, &input_bodies)?;
                known_clobbers.get(cidx).and_then(|c| *c)
            };
            let mut narrowed_here = 0usize;
            for inst in 0..view.len() {
                if let Some(clob) = lookup_clobbers(inst)
                    && (CallClobbers::C_ABI_ARM64.clobbered.0 & !clob.clobbered.0) != 0
                {
                    narrowed_here += 1;
                }
            }

            let fn_plan = abi_plan.plans.get(idx);
            let scratch_order = fn_plan.map(|p| ScratchOrder {
                gpr: p.scratch_gpr_order,
                fp: p.scratch_fp_order,
            });
            let preserve_all_cold = fn_plan.is_some_and(|p| p.conv == CallConvKind::ColdPreserveAll);

            let emit_res = isel::emit_sota_a64_with_abi(
                view,
                &names,
                opts.call_mode,
                opts.machine.features,
                is_macos,
                |inst| lookup_clobbers(inst).unwrap_or(CallClobbers::C_ABI_ARM64),
                scratch_order.as_ref(),
                preserve_all_cold,
                alloc,
            );

            let (emitted, mut clobbers, used_fallback) = match emit_res {
                Ok((em, clob)) => {
                    ipra_calls_narrowed += narrowed_here;
                    (em, clob, false)
                }
                Err(err) => {
                    if let Some(fb) = fallback {
                        let em = fb.emit(verified, &names, opts.machine, alloc)?;
                        (em, CallClobbers::C_ABI_ARM64, true)
                    } else {
                        return Err(err);
                    }
                }
            };

            // Soundly union callee clobbers for tail-called `Op::Call`/`Op::CallIndirect` sites
            // (which `regalloc` excludes from `call_positions` since they do not clobber locals).
            // `preserve_all_cold` disables TCO and saves/restores all callee-clobbered scratch regs.
            if !used_fallback && !preserve_all_cold {
                for inst in 0..view.len() {
                    match view.op(inst) {
                        Op::Call | Op::Invoke | Op::TailCall => {
                            let callee_clob = lookup_clobbers(inst).unwrap_or(CallClobbers::C_ABI_ARM64);
                            clobbers.clobbered = clobbers.clobbered.union(callee_clob.clobbered);
                        }
                        Op::CallIndirect | Op::InvokeIndirect | Op::TailCallIndirect => {
                            clobbers.clobbered = clobbers.clobbered.union(CallClobbers::C_ABI_ARM64.clobbered);
                        }
                        _ => {}
                    }
                }
            }

            if let Some(slot) = known_clobbers.get_mut(idx) {
                *slot = Some(clobbers);
            }
            let sym = fns
                .get(idx)
                .ok_or(TierError::Refused {
                    tier: TierName::Sota,
                    inst: None,
                    what: "missing module function symbol",
                })?
                .symbol;
            let inlined_bodies = lto.bodies.get(idx).map_or(0, |b| b.read.len());
            if let Some(slot) = compiled.get_mut(idx) {
                *slot = Some(CompiledModuleFn {
                    symbol: sym,
                    emitted,
                    clobbers,
                    inlined_bodies,
                    used_fallback,
                });
            }
        }
    }

    let mut functions: Vec<CompiledModuleFn<'a>, Alloc<'a>> = Vec::with_capacity_in(n, alloc);
    for slot in compiled {
        let compiled_fn = slot.ok_or(TierError::Refused {
            tier: TierName::Sota,
            inst: None,
            what: "module function omitted during compilation",
        })?;
        functions.push(compiled_fn);
    }

    // 6. 16 KB Apple Silicon BOLT page-exact layout & hot/cold section packing over the compiled
    //    module's code sizes and post-inline weighted call graph.
    let mut fn_weights: Vec<u64, Alloc<'a>> = Vec::with_capacity_in(n, alloc);
    for _ in 0..n {
        fn_weights.push(1);
    }
    let mut bolt_edges: Vec<BoltCallEdge, Alloc<'a>> = Vec::with_capacity_in(adj.len(), alloc);
    for (idx, opt_func) in opt_funcs.iter().enumerate() {
        let Some(caller_fn) = fns.get(idx) else {
            break;
        };
        let caller_id = BodyId(caller_fn.symbol.0);
        let names = ImportedNames {
            body_idx: idx,
            fns,
            lto_bodies: &lto.bodies,
            by_sym: &input_bodies.by_sym,
        };
        let view = opt_func.view();
        let caller_u32 = u32::try_from(idx).unwrap_or(u32::MAX);
        for inst in 0..view.len() {
            if let Some(callee_idx) = direct_callee_idx(view, inst, &names, &input_bodies)
                && let Ok(callee_u32) = u32::try_from(callee_idx)
            {
                let site_u32 = u32::try_from(inst).unwrap_or(u32::MAX);
                let w = weights.weight(caller_id, site_u32).max(1);
                bolt_edges.push(BoltCallEdge {
                    caller_idx: caller_u32,
                    callee_idx: callee_u32,
                    call_weight: w,
                });
                if let Some(cw) = fn_weights.get_mut(idx) {
                    *cw = cw.saturating_add(w);
                }
                if let Some(tw) = fn_weights.get_mut(callee_idx) {
                    *tw = tw.saturating_add(w);
                }
            }
        }
    }
    let mut bolt_fns: Vec<BoltFn, Alloc<'a>> = Vec::with_capacity_in(n, alloc);
    for (idx, compiled_fn) in functions.iter().enumerate() {
        let is_cold = opt_funcs
            .get(idx)
            .is_some_and(|f| (f.view().head.attrs & FnAttrs::COLD) != 0);
        let body_weight = if is_cold {
            0
        } else {
            fn_weights.get(idx).copied().unwrap_or(1).max(1)
        };
        let id = u32::try_from(idx).unwrap_or(u32::MAX);
        let code_bytes = u32::try_from(compiled_fn.emitted.code.len()).unwrap_or(u32::MAX);
        bolt_fns.push(BoltFn {
            id,
            code_bytes,
            body_weight,
            is_cold,
        });
    }
    let layout_plan = pack_module_pages(&bolt_fns, &bolt_edges, BoltOpts::apple_silicon(), alloc);
    let bolt_layout = layout_plan.stats;
    let mut page_order: Vec<PlacedFn, Alloc<'a>> =
        Vec::with_capacity_in(layout_plan.hot_order.len() + layout_plan.cold_order.len(), alloc);
    page_order.extend_from_slice(&layout_plan.hot_order);
    page_order.extend_from_slice(&layout_plan.cold_order);

    Ok(SotaModuleOut {
        functions,
        scc_count: lto.scc_count,
        total_inlined_bodies: lto.total_inlined_bodies,
        ipra_calls_narrowed,
        abi_stats,
        bolt_layout,
        page_order,
    })
}

impl<F: Tier> Sota<F> {
    /// Whole-program / crate optimizing compilation: runs bottom-up ThinLTO cross-function
    /// inlining + full `Sota` mid-end optimization + IPRA native AArch64 emission across `fns`.
    pub fn compile_module_a64<'a, 'f>(
        &self,
        fns: &[ModuleFn<'f>],
        weights: &dyn SiteWeights,
        machine: Machine,
        policy: InlinePolicy,
        alloc: Alloc<'a>,
    ) -> Result<SotaModuleOut<'a>, TierError> {
        let opts = SotaModuleOpts {
            machine,
            call_mode: self.call_mode,
            policy,
            superopt: self.superopt,
            auto_vectorize: self.auto_vectorize,
            max_rounds: self.max_rounds,
        };
        compile_module_a64(fns, weights, opts, self.fallback.as_ref(), alloc)
    }
}

#[cfg(test)]
mod tests {
    use blaze_code::record::Symbol;
    use blaze_lir::{
        Flags, IntCond, Op, Ty,
        builder::{Builder, Full, MemAttrs, Param, Variadic},
        func::{BlockKind, Conv, FnAttrs, Inline, RefKind},
        inline::NoWeights,
        verify,
    };
    use blaze_own::Alloc;
    use blaze_tier::{Arch, CallClobbers, Machine, Named, Names, Os, Outside, RelocKind, RoutinePlaces};

    use super::{ModuleFn, SotaModuleOpts, compile_module_a64};

    const MAC_ARM64: Machine = Machine { arch: Arch::Arm64, os: Os::MacOs, features: Machine::LSE };

    fn test_alloc() -> Alloc<'static> {
        blaze_own::testing::heap().alloc()
    }

    struct TableNames<'a>(&'a [(u32, Symbol)]);

    impl Names for TableNames<'_> {
        fn name(&self, of: Outside) -> Option<Named<'_>> {
            let Outside::Func(place) = of else {
                return None;
            };
            self.0.iter().find(|&&(p, _)| p == place).map(|&(_, sym)| Named::Symbol(sym))
        }

        fn routines(&self) -> RoutinePlaces {
            RoutinePlaces::NONE
        }
    }

    fn decode_words(code: &[u8]) -> impl Iterator<Item = u32> + '_ {
        code.chunks_exact(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
    }

    #[test]
    fn for_export_inlines_three_layer_pipeline_and_vectorizes_loop() {
        let alloc = test_alloc();
        let opts = SotaModuleOpts::for_export(MAC_ARM64);
        assert_eq!(opts.policy.cost_threshold, 75);
        assert_eq!(opts.policy.max_callee_insts, 192);
        assert_eq!(opts.policy.hot_multiplier, 4);
        assert_eq!(opts.policy.max_depth, 8);
        assert_eq!(opts.policy.max_growth, 1024);

        let sym_transform = Symbol::of_name("sota_mod_test::helper_transform");
        let sym_combine = Symbol::of_name("sota_mod_test::helper_combine");
        let sym_main = Symbol::of_name("sota_mod_test::main_loop");

        // 1. helper_transform(x: i32) -> i32 { (x * 5) ^ 7 }
        let mut bt = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
        let tx_sig = bt.sig(Conv::C, Variadic::No, &[Param::of(Ty::I32)], &[Param::of(Ty::I32)]);
        bt.set_signature(tx_sig);
        let t_entry = bt.create_block(BlockKind::Normal);
        bt.begin(t_entry);
        let tx_x = bt.param(Ty::I32);
        let five = bt.iconst(Ty::I32, 5);
        let prod = bt.binary(Op::Mul, Flags::NONE, tx_x, five);
        let seven = bt.iconst(Ty::I32, 7);
        let tx_res = bt.binary(Op::Xor, Flags::NONE, prod, seven);
        bt.ret(&[tx_res]);
        let func_transform = bt.finish_checked().expect("helper_transform LIR builds");

        // 2. helper_combine(acc: i32, x: i32) -> i32 { acc + helper_transform(x) }
        let mut bc = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
        let comb_sig = bc.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I32), Param::of(Ty::I32)],
            &[Param::of(Ty::I32)],
        );
        let comb_tx_sig = bc.sig(Conv::C, Variadic::No, &[Param::of(Ty::I32)], &[Param::of(Ty::I32)]);
        bc.set_signature(comb_sig);
        let tx_ref = bc.reference(RefKind::Func, 0);
        let c_entry = bc.create_block(BlockKind::Normal);
        bc.begin(c_entry);
        let c_acc = bc.param(Ty::I32);
        let c_x = bc.param(Ty::I32);
        let c_t = bc.call(Flags::NONE, tx_ref, comb_tx_sig, &[c_x]);
        let c_sum = bc.binary(Op::Add, Flags::NONE, c_acc, c_t);
        bc.ret(&[c_sum]);
        let func_combine = bc.finish_checked().expect("helper_combine LIR builds");

        // 3. main_loop(ptr: *const i32, n: i64) -> i32 { sum helper_combine(acc, ptr[i]) }
        let mut bm = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
        let main_sig = bm.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::PTR), Param::of(Ty::I64)],
            &[Param::of(Ty::I32)],
        );
        let main_comb_sig = bm.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I32), Param::of(Ty::I32)],
            &[Param::of(Ty::I32)],
        );
        bm.set_signature(main_sig);
        let comb_ref = bm.reference(RefKind::Func, 0);
        let m_entry = bm.create_block(BlockKind::Normal);
        let m_header = bm.create_block(BlockKind::Normal);
        let m_body = bm.create_block(BlockKind::Normal);
        let m_exit = bm.create_block(BlockKind::Normal);

        bm.begin(m_entry);
        let ptr = bm.param(Ty::PTR);
        let n = bm.param(Ty::I64);
        let zero64 = bm.iconst(Ty::I64, 0);
        let zero32 = bm.iconst(Ty::I32, 0);
        bm.jump((m_header, &[zero64, zero32]));

        bm.begin(m_header);
        let i = bm.param(Ty::I64);
        let acc = bm.param(Ty::I32);
        let cond = bm.icmp(IntCond::Ult, i, n);
        bm.br(Flags::NONE, cond, (m_body, &[]), (m_exit, &[acc]));

        bm.begin(m_body);
        let shift2 = bm.iconst(Ty::I64, 2);
        let byte_off = bm.binary(Op::Shl, Flags::NONE, i, shift2);
        let addr = bm.ptr_add(Flags::NONE, ptr, byte_off);
        let elem = bm.load(Ty::I32, Flags::NONE, addr, 0, MemAttrs::aligned(2));
        let next_acc = bm.call(Flags::NONE, comb_ref, main_comb_sig, &[acc, elem]);
        let one = bm.iconst(Ty::I64, 1);
        let next_i = bm.binary(Op::Add, Flags::NONE, i, one);
        bm.jump((m_header, &[next_i, next_acc]));

        bm.begin(m_exit);
        let exit_acc = bm.param(Ty::I32);
        bm.ret(&[exit_acc]);
        let func_main = bm.finish_checked().expect("main_loop LIR builds");

        let no_names = TableNames(&[]);
        let combine_names = TableNames(&[(0, sym_transform)]);
        let main_names = TableNames(&[(0, sym_combine)]);

        // Supply functions in top-down order to verify bottom-up SCC ordering too.
        let fns = [
            ModuleFn {
                symbol: sym_main,
                func: verify(func_main.view()).expect("main_loop verifies"),
                names: &main_names,
            },
            ModuleFn {
                symbol: sym_combine,
                func: verify(func_combine.view()).expect("helper_combine verifies"),
                names: &combine_names,
            },
            ModuleFn {
                symbol: sym_transform,
                func: verify(func_transform.view()).expect("helper_transform verifies"),
                names: &no_names,
            },
        ];

        let out = compile_module_a64(&fns, &NoWeights, opts, Option::<&crate::Reference>::None, alloc)
            .expect("whole-module SOTA export compilation succeeds");
        assert_eq!(out.functions.len(), 3);
        assert!(out.total_inlined_bodies >= 2, "expected both helper layers to inline");

        let main_out = &out.functions[0];
        assert!(!main_out.used_fallback);
        assert!(main_out.inlined_bodies >= 1, "main_loop should inline helper_combine");
        assert!(
            main_out.emitted.relocs.is_empty(),
            "3-function pipeline should inline away all calls, left {:?}",
            main_out.emitted.relocs.len()
        );
        assert!(
            !decode_words(&main_out.emitted.code).any(|w| (w & 0xfc00_0000) == 0x9400_0000),
            "inlined main_loop must contain zero direct `bl` calls"
        );
        // Post-inlining SOTA mid-end must auto-vectorize the `i32` reduction into NEON `<4 x i32>`.
        let has_neon_4s = decode_words(&main_out.emitted.code).any(|w| {
            let masked = w & 0xffe0_fc00;
            masked == 0x4ea0_8400 || masked == 0x4ea0_9c00 || masked == 0x4ea0_9400
        });
        assert!(has_neon_4s, "expected post-inline main_loop to vectorize into NEON `.4s` instructions");
    }

    #[test]
    fn ipra_noinline_leaf_in_loop_avoids_spills_and_callee_saves() {
        let alloc = test_alloc();
        let sym_leaf = Symbol::of_name("sota_mod_test::noinline_leaf");
        let sym_caller = Symbol::of_name("sota_mod_test::ipra_caller_loop");

        // 1. #[inline(never)] fn noinline_leaf(x: i64) -> i64 { (x ^ 0x5555) * 3 + 1 }
        let mut bl = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
        let leaf_sig = bl.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        bl.set_signature(leaf_sig);
        bl.set_attrs(FnAttrs::default().with_inline(Inline::Never));
        let l_entry = bl.create_block(BlockKind::Normal);
        bl.begin(l_entry);
        let lx = bl.param(Ty::I64);
        let mask = bl.iconst(Ty::I64, 0x5555);
        let xored = bl.binary(Op::Xor, Flags::NONE, lx, mask);
        let three = bl.iconst(Ty::I64, 3);
        let prod = bl.binary(Op::Mul, Flags::NONE, xored, three);
        let one = bl.iconst(Ty::I64, 1);
        let l_res = bl.binary(Op::Add, Flags::NONE, prod, one);
        bl.ret(&[l_res]);
        let func_leaf = bl.finish_checked().expect("noinline_leaf LIR builds");

        // 2. Caller loop keeping 4 live loop values (`i`, `n`, `step`, `salt`) across `bl noinline_leaf`.
        let mut bc = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
        let caller_sig = bc.sig(
            Conv::C,
            Variadic::No,
            &[
                Param::of(Ty::I64),
                Param::of(Ty::I64),
                Param::of(Ty::I64),
                Param::of(Ty::I64),
            ],
            &[Param::of(Ty::I64)],
        );
        let call_leaf_sig = bc.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        bc.set_signature(caller_sig);
        let leaf_ref = bc.reference(RefKind::Func, 0);
        let c_entry = bc.create_block(BlockKind::Normal);
        let c_header = bc.create_block(BlockKind::Normal);
        let c_body = bc.create_block(BlockKind::Normal);
        let c_exit = bc.create_block(BlockKind::Normal);

        bc.begin(c_entry);
        let n = bc.param(Ty::I64);
        let seed = bc.param(Ty::I64);
        let step = bc.param(Ty::I64);
        let salt = bc.param(Ty::I64);
        let zero = bc.iconst(Ty::I64, 0);
        bc.jump((c_header, &[zero, seed]));

        bc.begin(c_header);
        let i = bc.param(Ty::I64);
        let acc = bc.param(Ty::I64);
        let cond = bc.icmp(IntCond::Ult, i, n);
        bc.br(Flags::NONE, cond, (c_body, &[]), (c_exit, &[]));

        bc.begin(c_body);
        let mixed = bc.call(Flags::NONE, leaf_ref, call_leaf_sig, &[acc]);
        let added = bc.binary(Op::Add, Flags::NONE, mixed, step);
        let next_acc = bc.binary(Op::Xor, Flags::NONE, added, salt);
        let step_one = bc.iconst(Ty::I64, 1);
        let next_i = bc.binary(Op::Add, Flags::NONE, i, step_one);
        bc.jump((c_header, &[next_i, next_acc]));

        bc.begin(c_exit);
        bc.ret(&[acc]);
        let func_caller = bc.finish_checked().expect("ipra_caller_loop LIR builds");

        let no_names = TableNames(&[]);
        let caller_names = TableNames(&[(0, sym_leaf)]);

        // Supply caller before leaf to verify reverse-topological IPRA emission order.
        let fns = [
            ModuleFn {
                symbol: sym_caller,
                func: verify(func_caller.view()).expect("ipra_caller_loop verifies"),
                names: &caller_names,
            },
            ModuleFn {
                symbol: sym_leaf,
                func: verify(func_leaf.view()).expect("noinline_leaf verifies"),
                names: &no_names,
            },
        ];

        let opts = SotaModuleOpts::for_export(MAC_ARM64);
        let out = compile_module_a64(&fns, &NoWeights, opts, Option::<&crate::Reference>::None, alloc)
            .expect("whole-module IPRA compilation succeeds");
        assert_eq!(out.functions.len(), 2);
        assert!(out.ipra_calls_narrowed >= 1, "expected IPRA to narrow direct call to noinline_leaf");

        let caller_out = &out.functions[0];
        let leaf_out = &out.functions[1];
        assert!(!caller_out.used_fallback && !leaf_out.used_fallback);
        assert_eq!(caller_out.inlined_bodies, 0, "Inline::Never callee must not be inlined");
        assert!(leaf_out.emitted.unwind_frame.is_none(), "noinline_leaf should be frameless");
        assert!(
            (CallClobbers::C_ABI_ARM64.clobbered.0 & !leaf_out.clobbers.clobbered.0) != 0,
            "noinline_leaf must expose a narrowed IPRA clobber mask"
        );
        assert!(
            caller_out.emitted.relocs.iter().any(|r| r.kind == RelocKind::Branch26),
            "DirectBl mode must emit a direct `bl` (`RelocKind::Branch26`) relocation"
        );

        let frame = caller_out.emitted.unwind_frame.expect("caller with `bl` saves fp/lr");
        assert_eq!(frame.x_pairs, 0, "IPRA caller must save zero callee-saved GPR pairs");
        assert_eq!(frame.d_pairs, 0, "IPRA caller must save zero callee-saved FP pairs");
        assert!(
            !decode_words(&caller_out.emitted.code).any(|w| (w & 0xff80_03ff) == 0xd100_03ff),
            "IPRA caller must allocate zero stack spill frame (`sub sp, sp, #imm`)"
        );
        assert!(
            !decode_words(&caller_out.emitted.code).any(|w| (w & 0x3b80_03e0) == 0x3900_03e0),
            "IPRA caller must emit zero stack spill loads/stores (`ldr`/`str [sp, #imm]`)"
        );
    }

    #[test]
    fn compile_module_a64_populates_zero_abi_and_bolt_16kb_layout() {
        let alloc = test_alloc();
        let sym_entry = Symbol::of_name("sota_mod_test::bolt_entry");
        let sym_fast = Symbol::of_name("sota_mod_test::bolt_internal_fast");
        let sym_cold = Symbol::of_name("sota_mod_test::bolt_cold_sink");

        // 1. Internal non-inlined fast helper: fn(x: i64, y: i64) -> i64 { x * y + y }
        let mut bf = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
        let fast_sig = bf.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        bf.set_signature(fast_sig);
        bf.set_attrs(FnAttrs::default().with_inline(Inline::Never));
        let f_entry = bf.create_block(BlockKind::Normal);
        bf.begin(f_entry);
        let fx = bf.param(Ty::I64);
        let fy = bf.param(Ty::I64);
        let fprod = bf.binary(Op::Mul, Flags::NONE, fx, fy);
        let fres = bf.binary(Op::Add, Flags::NONE, fprod, fy);
        bf.ret(&[fres]);
        let func_fast = bf.finish_checked().expect("fast helper LIR builds");

        // 2. Internal #[cold] #[inline(never)] sink: fn(x: i64) -> i64 { x ^ 42 }
        let mut bcold = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
        let cold_sig = bcold.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        bcold.set_signature(cold_sig);
        bcold.set_attrs(FnAttrs(FnAttrs::COLD).with_inline(Inline::Never));
        let c_entry = bcold.create_block(BlockKind::Normal);
        bcold.begin(c_entry);
        let cx = bcold.param(Ty::I64);
        let c42 = bcold.iconst(Ty::I64, 42);
        let cres = bcold.binary(Op::Xor, Flags::NONE, cx, c42);
        bcold.ret(&[cres]);
        let func_cold = bcold.finish_checked().expect("cold sink LIR builds");

        // 3. Exported entry dispatching to `fast` on hot path and `cold` on `a == 0`.
        let mut be = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
        let entry_sig = be.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        let call_fast_sig = be.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        let call_cold_sig = be.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        be.set_signature(entry_sig);
        let fast_ref = be.reference(RefKind::Func, 0);
        let cold_ref = be.reference(RefKind::Func, 1);
        let e_entry = be.create_block(BlockKind::Normal);
        let e_cold = be.create_block(BlockKind::Cold);
        let e_hot = be.create_block(BlockKind::Normal);

        be.begin(e_entry);
        let ea = be.param(Ty::I64);
        let eb = be.param(Ty::I64);
        let zero = be.iconst(Ty::I64, 0);
        let is_zero = be.icmp(IntCond::Eq, ea, zero);
        be.br(Flags::NONE, is_zero, (e_cold, &[]), (e_hot, &[]));

        be.begin(e_cold);
        let cold_r = be.call(Flags::NONE, cold_ref, call_cold_sig, &[eb]);
        be.ret(&[cold_r]);

        be.begin(e_hot);
        let hot_r = be.call(Flags::NONE, fast_ref, call_fast_sig, &[ea, eb]);
        be.ret(&[hot_r]);
        let func_entry = be.finish_checked().expect("entry LIR builds");

        let no_names = TableNames(&[]);
        let entry_names = TableNames(&[(0, sym_fast), (1, sym_cold)]);
        let fns = [
            ModuleFn {
                symbol: sym_entry,
                func: verify(func_entry.view()).expect("entry verifies"),
                names: &entry_names,
            },
            ModuleFn {
                symbol: sym_fast,
                func: verify(func_fast.view()).expect("fast helper verifies"),
                names: &no_names,
            },
            ModuleFn {
                symbol: sym_cold,
                func: verify(func_cold.view()).expect("cold sink verifies"),
                names: &no_names,
            },
        ];

        let opts = SotaModuleOpts::for_export(MAC_ARM64);
        let out = compile_module_a64(&fns, &NoWeights, opts, Option::<&crate::Reference>::None, alloc)
            .expect("module compilation with Zero-ABI + BOLT succeeds");

        assert_eq!(out.functions.len(), 3);
        assert!(
            out.abi_stats.internal_fast_fns > 0,
            "expected internal non-exported helper to upgrade to InternalFast, got {:?}",
            out.abi_stats
        );
        assert!(
            out.abi_stats.cold_preserve_all_fns > 0,
            "expected internal #[cold] sink to upgrade to ColdPreserveAll, got {:?}",
            out.abi_stats
        );
        assert!(
            out.bolt_layout.hot_pages >= 1,
            "expected at least 1 hot 16 KB page, got {:?}",
            out.bolt_layout
        );
        assert!(
            out.bolt_layout.cold_pages >= 1,
            "expected cold sink packed onto a dedicated 16 KB .text.cold page, got {:?}",
            out.bolt_layout
        );
        assert_eq!(
            out.page_order.len(),
            out.functions.len(),
            "page_order must place every compiled module function"
        );
        let cold_placed = out
            .page_order
            .iter()
            .find(|p| p.fn_idx == 2)
            .expect("cold sink present in page_order");
        assert!(
            cold_placed.page_index >= out.bolt_layout.hot_pages,
            "cold sink must reside strictly after hot pages"
        );
    }

    #[test]
    fn cold_preserve_all_helper_preserves_hot_caller_regs_with_zero_spills() {
        let alloc = test_alloc();
        let sym_ext = Symbol::of_name("sota_mod_test::external_panic_sink");
        let sym_cold = Symbol::of_name("sota_mod_test::cold_preserve_helper");
        let sym_hot = Symbol::of_name("sota_mod_test::hot_loop_caller");

        // 1. Internal #[cold] #[inline(never)] helper calling an external C-ABI symbol (which
        //    clobbers all C_ABI_ARM64 regs). Under `ColdPreserveAll`, its prologue/epilogue must
        //    save/restore all modified caller-saved GPRs/FPs outside `x0` and report narrow IPRA clobbers.
        let mut bc = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
        let cold_sig = bc.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        let ext_sig = bc.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        bc.set_signature(cold_sig);
        bc.set_attrs(FnAttrs(FnAttrs::COLD).with_inline(Inline::Never));
        let ext_ref = bc.reference(RefKind::Func, 0);
        let c_entry = bc.create_block(BlockKind::Normal);
        bc.begin(c_entry);
        let cx = bc.param(Ty::I64);
        let c_ext = bc.call(Flags::NONE, ext_ref, ext_sig, &[cx]);
        let one = bc.iconst(Ty::I64, 1);
        let c_out = bc.binary(Op::Add, Flags::NONE, c_ext, one);
        bc.ret(&[c_out]);
        let func_cold = bc.finish_checked().expect("cold helper LIR builds");

        // 2. Hot caller loop keeping 4 live values (`n`, `step`, `salt`, `i`) across a cold branch
        //    calling `cold_preserve_helper`.
        let mut bh = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
        let hot_sig = bh.sig(
            Conv::C,
            Variadic::No,
            &[
                Param::of(Ty::I64),
                Param::of(Ty::I64),
                Param::of(Ty::I64),
                Param::of(Ty::I64),
            ],
            &[Param::of(Ty::I64)],
        );
        let call_cold_sig = bh.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        bh.set_signature(hot_sig);
        let cold_ref = bh.reference(RefKind::Func, 0);
        let h_entry = bh.create_block(BlockKind::Normal);
        let h_header = bh.create_block(BlockKind::Normal);
        let h_body = bh.create_block(BlockKind::Normal);
        let h_cold = bh.create_block(BlockKind::Cold);
        let h_merge = bh.create_block(BlockKind::Normal);
        let h_exit = bh.create_block(BlockKind::Normal);

        bh.begin(h_entry);
        let n = bh.param(Ty::I64);
        let seed = bh.param(Ty::I64);
        let step = bh.param(Ty::I64);
        let salt = bh.param(Ty::I64);
        let zero = bh.iconst(Ty::I64, 0);
        bh.jump((h_header, &[zero, seed]));

        bh.begin(h_header);
        let i = bh.param(Ty::I64);
        let acc = bh.param(Ty::I64);
        let keep = bh.icmp(IntCond::Ult, i, n);
        bh.br(Flags::NONE, keep, (h_body, &[]), (h_exit, &[]));

        bh.begin(h_body);
        let is_zero = bh.icmp(IntCond::Eq, acc, zero);
        bh.br(Flags::NONE, is_zero, (h_cold, &[]), (h_merge, &[acc]));

        bh.begin(h_cold);
        let fixed = bh.call(Flags::NONE, cold_ref, call_cold_sig, &[acc]);
        bh.jump((h_merge, &[fixed]));

        bh.begin(h_merge);
        let cur = bh.param(Ty::I64);
        let added = bh.binary(Op::Add, Flags::NONE, cur, step);
        let next_acc = bh.binary(Op::Xor, Flags::NONE, added, salt);
        let h_one = bh.iconst(Ty::I64, 1);
        let next_i = bh.binary(Op::Add, Flags::NONE, i, h_one);
        bh.jump((h_header, &[next_i, next_acc]));

        bh.begin(h_exit);
        bh.ret(&[acc]);
        let func_hot = bh.finish_checked().expect("hot caller LIR builds");

        let cold_names = TableNames(&[(0, sym_ext)]);
        let hot_names = TableNames(&[(0, sym_cold)]);
        let fns = [
            ModuleFn {
                symbol: sym_hot,
                func: verify(func_hot.view()).expect("hot caller verifies"),
                names: &hot_names,
            },
            ModuleFn {
                symbol: sym_cold,
                func: verify(func_cold.view()).expect("cold helper verifies"),
                names: &cold_names,
            },
        ];

        let opts = SotaModuleOpts::for_export(MAC_ARM64);
        let out = compile_module_a64(&fns, &NoWeights, opts, Option::<&crate::Reference>::None, alloc)
            .expect("ColdPreserveAll module compilation succeeds");

        assert_eq!(out.abi_stats.cold_preserve_all_fns, 1);
        assert!(out.ipra_calls_narrowed >= 1, "expected caller to narrow cold helper call via ColdPreserveAll");

        let hot_out = &out.functions[0];
        let cold_out = &out.functions[1];
        assert!(!hot_out.used_fallback && !cold_out.used_fallback);

        // Cold helper preserves every caller-saved GPR outside its `x0` return slot and every scratch FP.
        for r in 1..=15u8 {
            assert!(
                !cold_out.clobbers.clobbers_gpr(r),
                "ColdPreserveAll helper must preserve caller-saved x{r}"
            );
        }
        for v in (0..=7u8).chain(16..=28u8) {
            assert!(
                !cold_out.clobbers.clobbers_fp(v),
                "ColdPreserveAll helper must preserve caller-saved v{v}"
            );
        }

        // Hot caller keeps all 4 live loop values in caller-saved registers with zero spills/callee saves!
        let hot_frame = hot_out.emitted.unwind_frame.expect("hot caller saves fp/lr");
        assert_eq!(hot_frame.x_pairs, 0, "hot caller must save zero callee-saved GPR pairs");
        assert_eq!(hot_frame.d_pairs, 0, "hot caller must save zero callee-saved FP pairs");
        assert!(
            !decode_words(&hot_out.emitted.code).any(|w| (w & 0xff80_03ff) == 0xd100_03ff),
            "hot caller must allocate zero stack spill frame (`sub sp, sp, #imm`)"
        );
    }

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    #[test]
    fn sota_end_to_end_i8_gemm_executes_on_sme2_hardware() {
        use blaze_tier::Tier;

        unsafe extern "C" {
            fn sysctlbyname(
                name: *const core::ffi::c_char,
                oldp: *mut core::ffi::c_void,
                oldlenp: *mut usize,
                newp: *const core::ffi::c_void,
                newlen: usize,
            ) -> core::ffi::c_int;
        }

        let mut sme2_val: i32 = 0;
        let mut sme2_len = core::mem::size_of::<i32>();
        // SAFETY: read-only sysctl query into valid stack `i32` storage.
        let rc = unsafe {
            sysctlbyname(
                c"hw.optional.arm.FEAT_SME2".as_ptr(),
                (&raw mut sme2_val).cast::<core::ffi::c_void>(),
                &raw mut sme2_len,
                core::ptr::null(),
                0,
            )
        };
        if rc != 0 || sme2_val == 0 {
            return;
        }

        let alloc = test_alloc();
        let no_names = TableNames(&[]);
        let mac_sme2 = Machine {
            arch: Arch::Arm64,
            os: Os::MacOs,
            features: Machine::LSE | Machine::SME2,
        };

        // Build idiomatic safe-Rust-style `i8 -> i32` GEMM (both accumulate and sibling zero-init overwrite).
        let build_i8_gemm = |with_zero_init: bool| {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::PTR), Param::of(Ty::PTR), Param::of(Ty::PTR), Param::of(Ty::I64)],
                &[],
            );
            b.set_signature(sig);
            let [entry, i_hdr, i_body, k_pre, k_hdr, k_body, j_hdr, j_body, k_latch, i_latch, exit] =
                [(); 11].map(|_| b.create_block(BlockKind::Normal));
            let zero_blocks = with_zero_init
                .then(|| (b.create_block(BlockKind::Normal), b.create_block(BlockKind::Normal)));

            b.begin(entry);
            let pc = b.param(Ty::PTR);
            let pa = b.param(Ty::PTR);
            let pb = b.param(Ty::PTR);
            let n = b.param(Ty::I64);
            let zero = b.iconst(Ty::I64, 0);
            let one = b.iconst(Ty::I64, 1);
            let two = b.iconst(Ty::I64, 2);
            let izero = b.iconst(Ty::I32, 0);
            b.jump((i_hdr, &[zero]));

            b.begin(i_hdr);
            let i = b.param(Ty::I64);
            let keep_i = b.icmp(IntCond::Ult, i, n);
            b.br(Flags::NONE, keep_i, (i_body, &[]), (exit, &[]));

            b.begin(i_body);
            let i_n = b.binary(Op::Mul, Flags::NONE, i, n);
            if let Some((z_hdr, z_body)) = zero_blocks {
                b.jump((z_hdr, &[zero]));

                b.begin(z_hdr);
                let jz = b.param(Ty::I64);
                let keep_jz = b.icmp(IntCond::Ult, jz, n);
                b.br(Flags::NONE, keep_jz, (z_body, &[]), (k_pre, &[]));

                b.begin(z_body);
                let z_idx = b.binary(Op::Add, Flags::NONE, i_n, jz);
                let z_off = b.binary(Op::Shl, Flags::NONE, z_idx, two);
                let z_ptr = b.ptr_add(Flags::NONE, pc, z_off);
                b.store(Flags::NONE, izero, z_ptr, 0, MemAttrs::aligned(2));
                let jz2 = b.binary(Op::Add, Flags::NONE, jz, one);
                b.jump((z_hdr, &[jz2]));
            } else {
                b.jump((k_pre, &[]));
            }

            b.begin(k_pre);
            b.jump((k_hdr, &[zero]));

            b.begin(k_hdr);
            let k = b.param(Ty::I64);
            let keep_k = b.icmp(IntCond::Ult, k, n);
            b.br(Flags::NONE, keep_k, (k_body, &[]), (i_latch, &[]));

            b.begin(k_body);
            let a_idx = b.binary(Op::Add, Flags::NONE, i_n, k);
            let a_ptr = b.ptr_add(Flags::NONE, pa, a_idx);
            let a_raw = b.load(Ty::I8, Flags::NONE, a_ptr, 0, MemAttrs::aligned(0));
            let a_val = b.convert(Op::Sext, Flags::NONE, Ty::I32, a_raw);
            let k_n = b.binary(Op::Mul, Flags::NONE, k, n);
            b.jump((j_hdr, &[zero]));

            b.begin(j_hdr);
            let j = b.param(Ty::I64);
            let keep_j = b.icmp(IntCond::Ult, j, n);
            b.br(Flags::NONE, keep_j, (j_body, &[]), (k_latch, &[]));

            b.begin(j_body);
            let b_idx = b.binary(Op::Add, Flags::NONE, k_n, j);
            let b_ptr = b.ptr_add(Flags::NONE, pb, b_idx);
            let b_raw = b.load(Ty::I8, Flags::NONE, b_ptr, 0, MemAttrs::aligned(0));
            let b_val = b.convert(Op::Sext, Flags::NONE, Ty::I32, b_raw);
            let c_idx = b.binary(Op::Add, Flags::NONE, i_n, j);
            let c_off = b.binary(Op::Shl, Flags::NONE, c_idx, two);
            let c_ptr = b.ptr_add(Flags::NONE, pc, c_off);
            let c_val = b.load(Ty::I32, Flags::NONE, c_ptr, 0, MemAttrs::aligned(2));
            let prod = b.binary(Op::Mul, Flags::NONE, a_val, b_val);
            let c_new = b.binary(Op::Add, Flags::NONE, c_val, prod);
            b.store(Flags::NONE, c_new, c_ptr, 0, MemAttrs::aligned(2));
            let j2 = b.binary(Op::Add, Flags::NONE, j, one);
            b.jump((j_hdr, &[j2]));

            b.begin(k_latch);
            let k2 = b.binary(Op::Add, Flags::NONE, k, one);
            b.jump((k_hdr, &[k2]));

            b.begin(i_latch);
            let i2 = b.binary(Op::Add, Flags::NONE, i, one);
            b.jump((i_hdr, &[i2]));

            b.begin(exit);
            b.ret(&[]);
            b.finish_checked().expect("i8 GEMM LIR builds")
        };

        for &with_zero_init in &[false, true] {
            let func = build_i8_gemm(with_zero_init);
            let verified = verify(func.view()).expect("i8 GEMM LIR verifies");
            let emitted = super::Sota::standalone()
                .emit(verified, &no_names, mac_sme2, alloc)
                .expect("Sota SME2 i8->i32 GEMM emission succeeds");
            assert!(emitted.relocs.is_empty(), "self-contained SME2 GEMM must have zero relocs");
            assert!(
                decode_words(&emitted.code).any(|w| w == super::super::sme2::smstart()),
                "emitted code must embed SME2 smstart (with_zero_init={with_zero_init})"
            );
            assert!(
                decode_words(&emitted.code).any(|w| (w & 0xffe0_0000) == 0xa080_0000),
                "emitted code must embed hardware SMOPA instructions (with_zero_init={with_zero_init})"
            );

            let exec = blaze_inproc::exec::Executable::new(&emitted.code, 0)
                .expect("W^X mapping for end-to-end Sota SME2 i8->i32 GEMM succeeds");
            type MatmulI8Fn = extern "C" fn(*mut i32, *const i8, *const i8, i64);
            // SAFETY: `emitted.code` is valid AArch64+SME2 machine code matching `MatmulI8Fn`'s C ABI.
            let kernel: MatmulI8Fn = unsafe { exec.as_fn() };

            for &n in &[3usize, 16, 33, 64] {
                let a: std::vec::Vec<i8> = (0..n * n)
                    .map(|idx| (((idx.wrapping_mul(37).wrapping_add(11)) % 251) as i32 - 125) as i8)
                    .collect();
                let b: std::vec::Vec<i8> = (0..n * n)
                    .map(|idx| (((idx.wrapping_mul(53).wrapping_add(7)) % 241) as i32 - 120) as i8)
                    .collect();
                let init_c: std::vec::Vec<i32> = (0..n * n)
                    .map(|idx| ((idx.wrapping_mul(19).wrapping_add(3)) % 1009) as i32 - 500)
                    .collect();
                let mut got = init_c.clone();
                let mut expected = init_c;

                kernel(got.as_mut_ptr(), a.as_ptr(), b.as_ptr(), n as i64);

                for i in 0..n {
                    for j in 0..n {
                        let mut sum = if with_zero_init { 0i32 } else { expected[i * n + j] };
                        for k in 0..n {
                            sum = sum.wrapping_add(i32::from(a[i * n + k]).wrapping_mul(i32::from(b[k * n + j])));
                        }
                        expected[i * n + j] = sum;
                    }
                }
                assert_eq!(
                    got, expected,
                    "end-to-end Sota SME2 i8->i32 GEMM mismatch at n={n}, with_zero_init={with_zero_init}"
                );
            }
        }
    }
}
