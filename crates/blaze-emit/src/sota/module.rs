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
    body::{Bodies, BodyId},
    func::{Func, RefId, RefKind},
    inline::{InlinePolicy, SiteWeights},
    lto::{ModuleBody, OptimizedBody, lto_optimize_module, tarjan_sccs},
    superopt::SuperoptOpts,
    verify,
};
use blaze_own::{Alloc, Vec};
use blaze_tier::{Arch, CallClobbers, Emitted, Machine, Named, Names, Outside, RoutinePlaces, Tier, TierError, TierName};

use super::{CallMode, Sota, isel, optimize_sota_midend};

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
    /// Release AOT export defaults (`DirectBl`, full cross-function inlining, superopt, NEON).
    pub const fn for_export(machine: Machine) -> Self {
        Self {
            machine,
            call_mode: CallMode::DirectBl,
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
            if !matches!(view.op(inst), Op::Call | Op::Invoke) {
                continue;
            }
            let Some(&data_at) = view.data.get(inst) else {
                continue;
            };
            let Some(&ref_word) = usize::try_from(data_at).ok().and_then(|at| view.extra.get(at)) else {
                continue;
            };
            let r = view.reference(RefId(ref_word));
            if RefKind::from_word(r.kind) == Some(RefKind::Func)
                && let Some(Named::Symbol(sym)) = names.name(Outside::Func(r.place))
                && let Some(callee_idx) = input_bodies.find_idx(sym.0)
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

    // 4. Emit callees before callers with IPRA (`CallClobbers` propagation).
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

            // Count & feed IPRA call-clobber masks for direct calls to already-compiled callees.
            let view = verified.view();
            let mut narrowed_here = 0usize;
            for inst in 0..view.len() {
                if view.op(inst) == Op::Call
                    && let Some(&data_at) = view.data.get(inst)
                    && let Some(&ref_word) = usize::try_from(data_at).ok().and_then(|at| view.extra.get(at))
                {
                    let r = view.reference(RefId(ref_word));
                    if RefKind::from_word(r.kind) == Some(RefKind::Func)
                        && let Some(Named::Symbol(sym)) = names.name(Outside::Func(r.place))
                        && let Some(cidx) = input_bodies.find_idx(sym.0)
                        && known_clobbers.get(cidx).and_then(|c| *c).is_some()
                    {
                        narrowed_here += 1;
                    }
                }
            }

            let emit_res = isel::emit_sota_a64_with_ipra(
                view,
                &names,
                opts.call_mode,
                opts.machine.features,
                is_macos,
                |inst| {
                    if view.op(inst) == Op::Call
                        && let Some(&data_at) = view.data.get(inst)
                        && let Some(&ref_word) = usize::try_from(data_at).ok().and_then(|at| view.extra.get(at))
                    {
                        let r = view.reference(RefId(ref_word));
                        if RefKind::from_word(r.kind) == Some(RefKind::Func)
                            && let Some(Named::Symbol(sym)) = names.name(Outside::Func(r.place))
                            && let Some(cidx) = input_bodies.find_idx(sym.0)
                            && let Some(clob) = known_clobbers.get(cidx).and_then(|c| *c)
                        {
                            return clob;
                        }
                    }
                    CallClobbers::C_ABI_ARM64
                },
                alloc,
            );

            let (emitted, clobbers, used_fallback) = match emit_res {
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
    Ok(SotaModuleOut {
        functions,
        scc_count: lto.scc_count,
        total_inlined_bodies: lto.total_inlined_bodies,
        ipra_calls_narrowed,
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
