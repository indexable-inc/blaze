//! What the compiles of a whole program build into each other, across crates.
//!
//! Each function a compile calls is built once in the process, alone: its IR, lowered to
//! LLVM and simplified by LLVM's ThinLTO pre-link pipeline, is its [`Export`]. A compile
//! imports the exports of what it calls as `available_externally` bodies where they are small
//! enough for LLVM to build in, and the exports of what those call in turn with less room
//! each time ([`chosen`]), as ThinLTO chooses its imports from summaries before any backend
//! runs. LLVM then builds in what it finds worth it, and keeps the calls of the rest.
//!
//! A compile used to build the body of every callee again from MIR, as far as 16 calls deep
//! and up to 120 000 instructions, and LLVM optimized all of it from scratch in each caller:
//! bun's whole program, 68 315 functions, took 564 s on 16 cores, 3572 s of them, summed over
//! the threads, building callees' IR and most of 3757 s optimizing it. With imports it takes
//! 118 s against 135 s for cargo's release build, and runs 1.244x where it ran 1.267x: 372 283
//! exports built in 284 s, and 4.6 million imports of 31.6 million instructions linked into
//! 110 103 compiles in 211 s and optimized in 586 s (c8a.24xlarge, 2026-10-23).
//!
//! Each compile is still a module of its own, so two that call one function each link it in
//! and optimize it: helix and nushell build in about twice the time of cargo's release build,
//! and would with nothing imported. Codegen units that share their imports are #176.

use std::{
    collections::VecDeque,
    hash::BuildHasher,
    sync::{Arc, LazyLock, Mutex, OnceLock, atomic::Ordering},
};

use cranelift_codegen::ir;
use hir_def::{attrs::AttrFlags, signatures::FunctionSignature};
use hir_ty::{db::HirDatabase, next_solver::StoredGenericArgs, traits::StoredParamEnvAndCrate};
use rustc_abi::ExternAbi;
use rustc_hash::{FxBuildHasher, FxHashMap, FxHashSet};

use super::{
    Instance, InstanceDef,
    backend::{self, Phase, Tally},
    fragment::{FuncTarget, Hint, Recorder, Requests},
};

/// The most instructions, as LLVM counts them after its pre-link pipeline, a body may have
/// to be imported: one LLVM would not build in at `-O3` is not worth importing, as every
/// importer links and simplifies it again. A body that asks to be built in has more room, and
/// one that asks always to be has all a compile has ([`BUDGET`]). Bun on 16 cores, built and
/// run as against cargo's release build: 200 and 600 took 161.5 s and ran 1.242x, these
/// 118.0 s and 1.244x, 60 and 150 102.9 s and 1.272x, 40 and 100 93.6 s and 1.311x; cargo's
/// takes 135 s. What a compile imports is decided before LLVM runs, so a body it does not
/// build in costs the compile all the same (c8a.24xlarge, 2026-10-23).
const PLAIN: usize = 100;
const INLINE: usize = 300;
/// How much of its room each import leaves to the imports of what it calls, as ThinLTO's
/// `-import-instr-evolution-factor`, and how deep imports go. Bun's hot paths go through
/// chains of small wrappers: 3 deep it ran 1.785x, 12 deep as 8, and leaving 0.9 of the room
/// imported more for no faster a run.
const EVOLUTION: f32 = 0.7;
const DEPTH: u32 = 8;
/// The most instructions a compile imports in all.
pub(super) const BUDGET: usize = 40_000;
/// The most instructions of Cranelift IR a body has that an export is built for, at each
/// instruction of the most LLVM imports: what is larger is not built, as none would be.
/// Cranelift's IR of small calls of `std` is many times LLVM's: SipHash's `d_rounds` is 1792
/// of it and some 50 of LLVM's, so at 4 it was never imported and `words` ran 1.27x (x86) and
/// 1.38x (aarch64) where at 16 it runs 1.14x and 1.23x; 64 imports no more.
const CLIF_PER_IMPORTED: usize = 16;

/// A function as the compiles that call it import it.
pub(super) struct Export {
    /// The module of the function alone, under `llvm::OWN`, with what it refers to declared
    /// under the names of `requests`, as LLVM's pre-link pipeline left it.
    pub(super) bitcode: Vec<u8>,
    pub(super) requests: Requests,
    pub(super) signature: ir::Signature,
    /// Its instructions, as LLVM counts them.
    pub(super) size: usize,
    pub(super) hint: Hint,
}

/// An export a compile imports, with what it refers to as the compile names it.
pub(super) struct Import {
    pub(super) export: Arc<Export>,
    /// The number of the function among the compile's requests.
    pub(super) id: u32,
    /// What each of the export's requests is among the compile's.
    pub(super) funcs: Vec<u32>,
    pub(super) datas: Vec<u32>,
}

/// Whether the compiles of the build under way import what they call: LLVM at `-O2` or above
/// in a build of a whole program ([`super::tier`]'s `across`), unless `RA_JIT_IMPORT=0`,
/// which has them build their callees in as compiles at a level above 1 do.
pub(super) fn enabled() -> bool {
    static ASKED: LazyLock<bool> =
        LazyLock::new(|| std::env::var_os("RA_JIT_IMPORT").is_none_or(|it| it != "0"));
    *ASKED && super::tier::across() && backend::whole_program()
}

/// The exports of what `recorder` calls that its compile imports, and of what those call in
/// turn, the first first: what a call asks for is in the compile's names from then on.
pub(super) fn chosen(recorder: &mut Recorder<'_>) -> Vec<Import> {
    tallied(chosen_for(recorder, &|_| false, BUDGET))
}

/// `chosen`, counted as imported.
pub(super) fn tallied(chosen: Vec<Import>) -> Vec<Import> {
    let tally = Tally::global();
    tally.imported.fetch_add(chosen.len(), Ordering::Relaxed);
    let spent = chosen.iter().map(|it| it.export.size).sum();
    tally.imported_size.fetch_add(spent, Ordering::Relaxed);
    chosen
}

/// [`chosen`], for a compile of the functions of a unit ([`super::units`]), which has the
/// room of all of them and imports none of them: `defined` says which instances it has.
pub(super) fn chosen_for(
    recorder: &mut Recorder<'_>,
    defined: &dyn Fn(&Instance) -> bool,
    budget: usize,
) -> Vec<Import> {
    let db = recorder.db;
    let mut chosen = Vec::new();
    let mut taken: FxHashSet<u32> = FxHashSet::default();
    let mut spent = 0;
    // The calls to look at, by the number of the callee among the compile's requests, with
    // the share of a body's room left to it.
    let mut next: VecDeque<(u32, f32, u32)> =
        (0..recorder.requests.funcs.len() as u32).map(|id| (id, 1.0, 0)).collect();
    while let Some((id, share, depth)) = next.pop_front() {
        if taken.contains(&id) {
            continue;
        }
        let request = &recorder.requests.funcs[id as usize];
        let FuncTarget::Instance(instance, env) = &request.target else { continue };
        if !super::tier::shared(db, instance) || defined(instance) {
            continue;
        }
        let Some(export) = export(db, instance, env) else { continue };
        let room = match export.hint {
            Hint::Never => continue,
            Hint::None => PLAIN,
            Hint::Inline => INLINE,
            Hint::Always => BUDGET,
        };
        if export.signature != request.sig
            || export.size as f32 > room as f32 * share
            || spent + export.size > budget
        {
            continue;
        }
        taken.insert(id);
        spent += export.size;
        let (funcs, datas) = recorder.moved_requests(&export.requests);
        if depth + 1 < DEPTH {
            next.extend(funcs.iter().map(|it| (it.as_u32(), share * EVOLUTION, depth + 1)));
        }
        let funcs = funcs.iter().map(|it| it.as_u32()).collect();
        let datas = datas.iter().map(|it| it.as_u32()).collect();
        chosen.push(Import { export, id, funcs, datas });
    }
    chosen
}

/// Of an instance, as [`super::tier`] keys the bodies it builds: whether it checks overflow is
/// part of the instance, and its environment changes nothing of a body without generic
/// parameters.
type Key = (InstanceDef, StoredGenericArgs, bool);

/// The exports of the process, each built once: a compile that asks for one another is
/// building waits for it, and no export asks for another while it is built.
struct Exports {
    shards: Vec<Mutex<FxHashMap<Key, Arc<OnceLock<Option<Arc<Export>>>>>>>,
}

static EXPORTS: LazyLock<Exports> =
    LazyLock::new(|| Exports { shards: (0..64).map(|_| Mutex::default()).collect() });

/// The export of `instance`, built if no compile asked for it before. `None` for one that is
/// not imported: too large, a coroutine, or a body LLVM has no lowering for.
fn export(
    db: &dyn HirDatabase,
    instance: &Instance,
    env: &StoredParamEnvAndCrate,
) -> Option<Arc<Export>> {
    let key = (instance.def, instance.args.clone(), instance.checked);
    let shards = &EXPORTS.shards;
    let shard = &shards[FxBuildHasher.hash_one(&key) as usize % shards.len()];
    let slot = shard.lock().unwrap_or_else(|e| e.into_inner()).entry(key).or_default().clone();
    slot.get_or_init(|| {
        let start = stdx::time::Instant::now();
        let built = build(db, instance, env).map(Arc::new);
        let tally = Tally::global();
        let bytes = built.as_ref().map_or(0, |it| it.bitcode.len());
        tally.exports.add(start.elapsed(), bytes);
        built
    })
    .clone()
}

/// The export of `instance`: its body built alone, with nothing of what it calls built in but
/// the small functions of the standard library every compile builds in.
fn build(
    db: &dyn HirDatabase,
    instance: &Instance,
    env: &StoredParamEnvAndCrate,
) -> Option<Export> {
    let (tracked, flags) = match instance.def {
        InstanceDef::Fn(func) => {
            let signature = FunctionSignature::of(db, func);
            if signature.abi != ExternAbi::Rust
                || signature.is_varargs()
                || FunctionSignature::is_intrinsic(db, func)
            {
                return None;
            }
            (super::track_caller(db, func), Some(AttrFlags::query(db, func.into())))
        }
        InstanceDef::Closure(_) | InstanceDef::DropGlue => (false, None),
        _ => return None,
    };
    let has = |flag| flags.is_some_and(|it| it.contains(flag));
    let hint = if has(AttrFlags::INLINE_NEVER) {
        return None;
    } else if has(AttrFlags::INLINE_ALWAYS) {
        Hint::Always
    } else if has(AttrFlags::IS_INLINE) {
        Hint::Inline
    } else {
        Hint::None
    };
    let room = match hint {
        Hint::None => PLAIN,
        Hint::Inline => INLINE,
        _ => BUDGET,
    };
    let backend::Backend::Llvm(optimization) = backend::Backend::from_env().ok()? else {
        return None;
    };
    super::tier::plain(|| {
        if let InstanceDef::Fn(func) = instance.def
            && db.mir_body(func.into()).ok()?.coroutine.is_some()
        {
            return None;
        }
        // Drop glue is built from the type it drops, and has no MIR.
        let body = match instance.def {
            InstanceDef::DropGlue => None,
            _ => Some(super::instance_body(db, instance, env.clone()).ok()?),
        };
        let mut cg = Recorder::new(
            db,
            &**super::target().ok()?,
            super::selected_target().ok()?,
            instance.checked,
        );
        cg.unlocated = true;
        let args = instance.args.clone();
        let mut ctx =
            super::build(&mut cg, env.as_ref(db), body.as_deref(), tracked, |fx| match fx.body {
                Some(_) => {
                    fx.args = Some(args);
                    fx.codegen()
                }
                None => {
                    use rustc_type_ir::inherent::GenericArgs as _;
                    fx.drop_glue(args.as_ref().type_at(0))
                }
            })
            .ok()?;
        if cg.naked.is_some() || cg.opaque {
            return None;
        }
        super::inline(&mut ctx, &cg.inlined).ok()?;
        super::promote::promote(&mut ctx.func, cg.named_slots());
        super::tier::fold_known_branches(&mut ctx, cg.isa);
        if ctx.func.dfg.num_insts() > room * CLIF_PER_IMPORTED {
            return None;
        }
        let pointees = std::mem::take(&mut cg.pointees);
        let lowered = Tally::global().time(Phase::Export, || {
            super::llvm::export(&cg.wants, &ctx.func, &pointees, hint, optimization)
        });
        let (bitcode, size) = lowered.ok()?;
        let requests = cg.finish().0;
        if size > room || !requests.ordered() {
            return None;
        }
        let signature = ctx.func.signature.clone();
        Some(Export { bitcode, requests, signature, size, hint })
    })
}
