//! The references of an instance's code, from a walk of its MIR (docs/v2/plan/design/codegen.md):
//! what [`blaze_codegen::backend::Described::refs`] lists before the instance is compiled, and
//! all that the IR of any unit of it may ask for by place ([`places::Placer::listed`]).
//!
//! The walk runs the IR builder's own resolution on a [`FunctionCx`] that builds no code: its
//! resolvers of functions, vtables, constants and statics ([`FunctionCx::fn_item`],
//! [`FunctionCx::vtable`], [`FunctionCx::allocation_blob`]) make their requests of the
//! [`Recorder`] as they do for the IR, and the walk reads them off it in the order of the
//! MIR. Where the IR builder decides with code what it calls, the walk makes the same
//! decision of the types. What it lists is the union over the settings of a unit: the blocks
//! that any of them compiles (it walks with unwinding, `ub_checks()` and debug assertions
//! on), each check overflow checks may make, each landing pad's routines, and for a body some
//! unit MIR-inlines, what the inliner takes with debug assertions and without
//! ([`Build::WALKS_INLINED`]), which is no superset of either.

use std::{borrow::Cow, hash::BuildHasher};

use base_db::Db;
use blaze_codegen::backend::FrontRef;
use blaze_hash::{FixedState, HashMap, HashSet};
use blaze_ids::{ContentId, Hasher, bits::BitSet};
use blaze_query::db::AsId;
use cranelift_codegen::ir::{self, ArgumentPurpose};
use hashbrown::{HashTable, hash_table::Entry};
use hir_def::{
    AdtId, CallableDefId, FunctionId, HasModule, StaticId, attrs::AttrFlags,
    signatures::FunctionSignature,
};
use hir_ty::{
    InferBodyId, ParamEnvAndCrate, PointerCast,
    db::HirDatabase,
    drop::DropGlue,
    intrinsic::{Intrinsic, intrinsic_of},
    next_solver::{ConstKind, GenericArgs, ParamEnv, Region, RegionExt, Ty, TyKind},
    traits::StoredParamEnvAndCrate,
};
use hir_ty_mir::mir::{
    AssertKind, BasicBlockId, BinOp, CastKind, InlineAsm, InlinedBody, MirBody, MirPhase, Operand,
    OperandKind, PlaceTy, Rvalue, StatementKind, StoredPlace, TerminatorKind,
    devirt::DevirtWorld,
    dispatch, interpret_mir, is_by_move_coroutine,
    location::{located_concats, track_caller},
    monomorphized_promoted_mir_body_query,
};
use rustc_abi::ExternAbi;
use rustc_ast_ir::Mutability;
use rustc_type_ir::inherent::{GenericArgs as _, IntoKind, SliceLike as _, Ty as _};
use smallvec::{SmallVec, smallvec};
use std::alloc::Global;

use super::{
    HirInstance, Kind,
    build::Build,
    devirt::Facts,
    places::{self, Ref},
};
use crate::{
    Arch, CodegenError, FnAbi, FunctionCx, Instance, InstanceDef, NarrowInt, asm, fn_trait_methods,
    fncache,
    fragment::{Asked, DataRequest, FuncTarget, Recorder},
    is_integer, is_thread_local,
};

mod intrinsics;
mod shims;
#[cfg(test)]
mod tests;

/// The types of a call's operands: inline up to eight, as nearly every call has.
pub(super) type Tys<'db> = SmallVec<[Ty<'db>; 8]>;

/// The domain of [`placement`]: how many requests the walk of an instance made, what they were
/// given ([`PLACED_DOMAIN`]), how many references it lists, and the places of the callees it
/// builds in.
const PLACEMENT_DOMAIN: &str = "blaze v2 reference placement 1";
/// The domain of what each request of a walk was given, in order: a place of the list, or a
/// data object of the function's own by its place among the recorder's requests.
const PLACED_DOMAIN: &str = "blaze v2 reference placed 1";

/// What the references of an instance are walked for: whether a unit of it MIR-inlines bodies
/// of crates other than the library's (`Spec::mir_inline`), whose code then asks for what they
/// call, and the program's dyn facts its body is devirtualized against
/// ([`super::devirt::walking`]), whose direct calls it then asks for.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) struct Walking {
    pub(crate) inlined: bool,
    pub(crate) world: Option<DevirtWorld>,
}

impl Walking {
    /// The walk without the program's world, as every walk was before one is made.
    pub(crate) fn plain(inlined: bool) -> Self {
        Walking { inlined, world: None }
    }

    /// The settings the walk builds the instance's code under, a pass each, the one list of
    /// them ([`code_refs`], [`start_walk`], and the error of a request no pass listed):
    /// [`Build::WALK`], and where a unit of the instance MIR-inlines, each of
    /// [`Build::WALKS_INLINED`] against the program's world. It is the same list for every kind
    /// of instance and every crate: a unit builds each under its own settings, and even where
    /// the instance's own MIR is the same under all of them (a body of the library's, inlined
    /// in every unit; a shim, which has none) the `#[inline(always)]` callees of other crates
    /// it builds in are built under them (`always_inlinable_callee`), their bodies MIR-inlined
    /// where the unit inlines: core's `<&A as PartialEq<&B>>::eq` builds in smol_str's
    /// `<SmolStr as PartialEq<str>>::eq`, whose code inlined asks for `Repr::as_str` and
    /// `memcmp`, where at level 1 it calls `SmolStr::as_str` and `<str as PartialEq>::eq`.
    pub(crate) fn builds(self) -> SmallVec<[Build; 3]> {
        let mut builds: SmallVec<[Build; 3]> = smallvec![Build::WALK];
        if self.inlined {
            builds.extend(Build::WALKS_INLINED.map(|it| Build { world: self.world, ..it }));
        }
        builds
    }
}

/// Everything the IR of `instance` can request, each once, in the order of its MIR: [`walked`],
/// owned.
pub(crate) fn walk(db: &Db, instance: &HirInstance, walking: Walking) -> Result<Vec<Ref>, String> {
    listed(db, instance, walking).map(Cow::into_owned)
}

/// [`walk`], borrowed from the database's memo where it has one: what `prepare` places the
/// IR's requests by, with nothing copied.
pub(crate) fn listed<'db>(
    db: &'db Db,
    instance: &HirInstance,
    walking: Walking,
) -> Result<Cow<'db, [Ref]>, String> {
    match &instance.0 {
        Kind::Allocator(forward) => Ok(Cow::Owned(super::allocator::walk(forward))),
        _ => walked(db, instance, walking).map(Cow::Borrowed),
    }
}

/// Everything the IR of `instance` can request, each once, in the order of its MIR, walked once
/// a revision ([`code_walk`], [`start_walk`]) as rustc's collector has `items_of_instance`: a
/// second asker (`describe`, then `prepare`) gets the list of the first and allocates nothing.
/// The list for `inlined` starts with the one without.
pub(crate) fn walked<'db>(
    db: &'db Db,
    instance: &HirInstance,
    walking: Walking,
) -> Result<&'db [Ref], String> {
    memo(db, instance, walking).map(|it| it.map_or(&[][..], |it| &it.refs))
}

/// The places among [`walked`] of the callees whose bodies the IR of `instance` builds in, in
/// the order found: [`blaze_codegen::backend::Described::built_in`]. Those the MIR optimizer
/// inlined, and those the IR builder builds in itself (`declare_inlined`).
pub(crate) fn built_in<'db>(
    db: &'db Db,
    instance: &HirInstance,
    walking: Walking,
) -> Result<&'db [u32], String> {
    match &instance.0 {
        Kind::Allocator(_) => Ok(&[]),
        _ => memo(db, instance, walking).map(|it| it.map_or(&[][..], |it| &it.built_in)),
    }
}

/// Which place of [`walked`] each request of the walk of `instance` is given, in the walk's
/// order, a request listed already included, and the places of [`built_in`]: the one thing the
/// list's references alone do not say. The list holds each reference once, so two requests of
/// one callee are one place (`size_of::<S>` and `size_of::<D>` in `needs_realloc::<S, D>`, one
/// place where `S` is `D`), and the IR names a reference by its place. Two instances whose
/// compile identities are equal call callees of equal identities, which may be one instance in
/// one of them and two in the other: their code differs only here, so a generic shared under
/// its compile identity is keyed by this too ([`super::frontend`]'s `generic_args`, rule 21).
/// The default for an instance that names nothing.
pub(crate) fn placement(
    db: &Db,
    instance: &HirInstance,
    walking: Walking,
) -> Result<ContentId, String> {
    match &instance.0 {
        // Its references are listed per entry ([`super::allocator::walk`]), never shared.
        Kind::Allocator(_) => Ok(ContentId::default()),
        _ => {
            memo(db, instance, walking).map(|it| it.map_or(ContentId::default(), |it| it.placement))
        }
    }
}

/// What the IR of `instance` does with trait objects ([`super::devirt`]); `None` for one that
/// names nothing.
pub(crate) fn facts<'db>(
    db: &'db Db,
    instance: &HirInstance,
    walking: Walking,
) -> Result<Option<&'db Facts>, String> {
    match &instance.0 {
        Kind::Allocator(_) => Ok(None),
        _ => memo(db, instance, walking).map(|it| it.map(|it| &it.facts)),
    }
}

/// The digests of the dyn facts the body of `instance` read when it was devirtualized
/// (`MirBody::inlined.devirtualized`) and of the facts of other items its passes relied on
/// (`MirBody::inlined.relied`), sorted, each once: what its code is keyed by beside its
/// body ([`super::devirt::code`]). Empty for a walk without the program's world of a body
/// that relied on nothing.
pub(crate) fn read<'db>(
    db: &'db Db,
    instance: &HirInstance,
    walking: Walking,
) -> Result<&'db [[u8; 32]], String> {
    match &instance.0 {
        Kind::Allocator(_) => Ok(&[]),
        _ => memo(db, instance, walking).map(|it| it.map_or(&[][..], |it| &it.read)),
    }
}

/// What the IR of an instance asks for, and which of its callees it builds in.
#[derive(PartialEq, Eq, Debug)]
pub(crate) struct Walked {
    refs: Vec<Ref>,
    built_in: Vec<u32>,
    facts: Facts,
    read: Vec<[u8; 32]>,
    /// [`placement`].
    placement: ContentId,
}

/// The walk of `instance`; `None` for one that names nothing.
fn memo<'db>(
    db: &'db Db,
    instance: &HirInstance,
    Walking { inlined, world }: Walking,
) -> Result<Option<&'db Walked>, String> {
    let walked = match &instance.0 {
        // Its assembly names nothing: `asm::wrapper` refuses a `sym`, and the backend an
        // assembly function with a relocation.
        Kind::Asm(..) => return Ok(None),
        Kind::Start(main) => start_walk(db, *main, inlined),
        Kind::Allocator(forward) => {
            // alloc: the error's message
            return Err(format!("{forward:?}: an allocator entry, whose references are listed"));
        }
        Kind::Code(code, env) => {
            // Walked under each setting a unit of it builds it with ([`Walking::builds`]),
            // whatever its kind or crate. Only a body built with the inliner is built against
            // the world.
            let world = world.filter(|_| inlined);
            // clone: the query's keys by value, each a few `Copy` ids in a `Clone`-only type
            code_walk(db, code.clone(), env.clone(), inlined, world)
        }
    };
    match walked {
        Ok(walked) => Ok(Some(walked)),
        // alloc: the error's message
        Err(what) => Err(blaze_query::db::attach(db, || {
            format!("the references of {}: {what}", super::Named(db, instance))
        })),
    }
}

// Nothing in its arguments is ever collected (the query database frees no interned value),
// so they stay interned and are never verified.
#[blaze_query::tracked(returns(ref))]
fn code_walk<'db>(
    db: &'db Db,
    instance: Instance,
    env: StoredParamEnvAndCrate,
    inlined: bool,
    world: Option<DevirtWorld>,
) -> Result<Walked, String> {
    code_refs(db, &instance, &env, inlined, world)
}

/// `lang_start`'s walk: the library's body, whose `#[inline(always)]` callees of other crates
/// (`main`'s return type's `Termination::report`) a unit builds in under its own settings, so
/// it is walked under each of them as any instance is ([`Walking::builds`]).
#[blaze_query::tracked(returns(ref))]
fn start_walk(db: &Db, main: FunctionId, inlined: bool) -> Result<Walked, String> {
    let env = ParamEnvAndCrate { param_env: ParamEnv::empty(), krate: main.krate(db) };
    scratch(db, env, &Walking::plain(inlined).builds(), |walk| walk.start_shim(main))
}

/// The walk of a function, closure, `async` body or shim, as `instance_code` builds each,
/// under each of [`Walking::builds`]; `world` is what the body with the inliner on is
/// devirtualized against.
fn code_refs(
    db: &Db,
    instance: &Instance,
    env: &StoredParamEnvAndCrate,
    inlined: bool,
    world: Option<DevirtWorld>,
) -> Result<Walked, String> {
    let builds = Walking { inlined, world }.builds();
    let ty = || instance.args.as_ref().type_at(0);
    let shim_env = env.as_ref(db);
    match instance.def {
        InstanceDef::DropGlue => {
            return scratch(db, shim_env, &builds, |walk| walk.drop_glue(ty()));
        }
        InstanceDef::CallOnceShim => {
            return scratch(db, shim_env, &builds, |walk| walk.call_once_shim(ty()));
        }
        InstanceDef::VtableShim(func) => {
            let args = instance.args.as_ref();
            return scratch(db, shim_env, &builds, |walk| walk.vtable_shim(func, args));
        }
        InstanceDef::FnPointerShim => {
            return scratch(db, shim_env, &builds, |walk| walk.fn_pointer_shim(ty()));
        }
        InstanceDef::ReifyShim { receiver } => {
            return scratch(db, shim_env, &builds, |walk| walk.reify_shim(ty(), receiver));
        }
        InstanceDef::Fn(_)
        | InstanceDef::Closure(_)
        | InstanceDef::Coroutine(_)
        | InstanceDef::CoroutineByMove(_) => (),
    }
    // The body each pass walks, as a unit under its settings reads it: under `WALK` first, so
    // the list starts with what a unit that inlines nothing asks for. A body of the library's
    // or a coroutine is the same MIR under each (the library's is inlined in every unit, a
    // coroutine has nothing inlined into it), and is walked under each all the same: the
    // callees it builds in are built under the pass's settings ([`Walking::builds`]).
    // alloc: inline, one body a pass
    let mut bodies: SmallVec<[(Build, _); 3]> = SmallVec::new();
    for &build in &builds {
        // clone: `instance_body` takes the env by value; two `Copy` ids in a `Clone`-only type
        bodies.push((
            build,
            crate::instance_body(db, instance, env.clone(), Some(build)).map_err(told)?,
        ));
    }
    let (tracked, abi, variadic) = match instance.def {
        InstanceDef::Fn(func) => {
            let signature = FunctionSignature::of(db, func);
            (track_caller(db, func), signature.abi, signature.is_varargs())
        }
        _ => (false, ExternAbi::Rust, false),
    };
    // clone: `Found` owns the env the requests hold; two `Copy` ids in a `Clone`-only type
    let mut found = Found::new(env.clone(), Some(&*bodies[0].1));
    let env = env.as_ref(db);
    for (at, (build, body)) in bodies.iter().enumerate() {
        let body = &**body;
        found.read.extend_from_slice(&body.inlined.devirtualized);
        // The facts of other items the body's passes relied on (`SpendFacts`,
        // docs/v2/plan/design/facts.md): its code is the code they changed, so they key it
        // beside the dyn facts, whatever else names the instance.
        found.read.extend_from_slice(&body.inlined.relied);
        found.read.sort_unstable();
        found.read.dedup();
        pass(db, env, Some(body), tracked, (*build, &mut found), |walk| {
            walk.fx.abi = abi;
            walk.fx.variadic = variadic;
            walk.fx.args = Some(instance.args);
            if at == 0 {
                walk.outside()?;
                walk.edge(body, abi)?;
            }
            walk.body(body)
        })?;
    }
    Ok(found.walked())
}

/// Runs `work` on a [`FunctionCx`] in `env` with no body (a shim's, `lang_start`'s), whose
/// function is never built, once under each of `builds` ([`Walking::builds`]), the first with
/// what any function names ([`Walk::outside`]): what its resolvers ask for is all that is kept
/// of it.
fn scratch<'db>(
    db: &'db Db,
    env: ParamEnvAndCrate<'db>,
    builds: &[Build],
    work: impl for<'w, 'cx> Fn(&mut Walk<'w, 'cx, 'db>) -> Result<(), String>,
) -> Result<Walked, String> {
    let mut found = Found::new(env.store(), None);
    for (at, &build) in builds.iter().enumerate() {
        pass(db, env, None, false, (build, &mut found), |walk| {
            if at == 0 {
                walk.outside()?;
            }
            work(walk)
        })?;
    }
    Ok(found.walked())
}

/// One walk of `body` built as `build` would build it, adding to `found`.
fn pass<'db>(
    db: &'db Db,
    env: ParamEnvAndCrate<'db>,
    body: Option<&'db MirBody<'db>>,
    tracked: bool,
    (build, found): (Build, &mut Found<'db>),
    work: impl for<'w, 'cx> FnOnce(&mut Walk<'w, 'cx, 'db>) -> Result<(), String>,
) -> Result<(), String> {
    let target = crate::target_of_crate(db, env.krate).map_err(told)?;
    let isa = &**crate::isa_for(target).map_err(told)?;
    // The settings a walk reads are its own, never the thread's unit's: the walk is a query.
    let mut cg = Recorder::new(db, isa, target, true, Some(build));
    // Every ask, one asked for before included, which `Walk::drain` places ([`placement`]).
    cg.wants.asks = Some(Vec::new()); // alloc: one pass's log of asks, freed with the pass
    let mut done = None;
    crate::build(&mut cg, env, body, tracked, |fx| {
        // The union of what the settings of a unit compile.
        fx.aborts = false;
        fx.debug_assertions = true;
        fx.ub_checks = true;
        // alloc: one pass's few built-in places, freed with the pass
        let mut walk = Walk {
            fx,
            found: &mut *found,
            drained: 0,
            built_here: Vec::new(),
            // alloc: one pass's places of its recorder's requests, freed with the pass
            func_places: Vec::new(),
            data_places: Vec::new(), // alloc: as above
        };
        done = Some(work(&mut walk).and_then(|()| walk.drain()));
        Ok(())
    })
    .map_err(told)?;
    done.expect("the walk ran")
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "the `map_err` callback of the walk's every step, which hands it the error by value"
)]
fn told(error: CodegenError) -> String {
    // alloc: the error's message
    error.to_string()
}

/// A walk of one body.
pub(super) struct Walk<'w, 'cx, 'db> {
    fx: &'w mut FunctionCx<'cx, 'db>,
    found: &'w mut Found<'db>,
    /// How many of the asks of `fx.cg` ([`crate::fragment::Wants::asks`]) are placed.
    drained: usize,
    /// The place in `found.listed` of each function request of `fx.cg`, by its place among
    /// the requests: what an ask of it again is given. The pass's own, as its recorder is.
    func_places: Vec<u32>,
    /// The same of each data request: a static's place, or `None` for a data object of the
    /// function's own, which no reference is.
    data_places: Vec<Option<u32>>,
    /// The places of the callees this pass has built in. Each pass builds its body under its
    /// own settings, so a callee the `WALK` pass built in is built in again by the
    /// `WALK_INLINED` pass, whose copy of the callee's body is MIR-inlined and may call what
    /// the first did not: `found.built_in`, shared by both, cannot say whether this pass has.
    built_here: Vec<u32>,
}

/// What the walks of an instance's bodies found.
struct Found<'db> {
    /// The walk's `fx.env`, as the requests hold it.
    env: StoredParamEnvAndCrate,
    listed: Listed,
    /// The calls listed already, by their interned definition and arguments, and the place of
    /// each: a call made again costs a hash of two ids and no homing (`places::call_ref`).
    called: HashMap<(InstanceDef, GenericArgs<'db>), u32, Global>,
    /// The places in `listed` of the callees whose bodies are built in ([`Walked::built_in`]),
    /// each once.
    built_in: Vec<u32>,
    facts: Facts,
    read: Vec<[u8; 32]>,
}

impl<'db> Found<'db> {
    fn new(env: StoredParamEnvAndCrate, body: Option<&MirBody<'_>>) -> Self {
        // About one reference a block, and the few of `outside`.
        let capacity = body.map_or(0, |it| it.basic_blocks.len()) + 8;
        Found {
            env,
            listed: Listed::with_capacity(capacity),
            called: HashMap::with_capacity_in(capacity, Global),
            // alloc: the memoized output, empty until a callee is built in
            built_in: Vec::new(),
            facts: Facts::default(),
            // alloc: the memoized output, empty but for a devirtualized body
            read: Vec::new(),
        }
    }

    fn walked(self) -> Walked {
        let Listed { mut refs, given, requests, .. } = self.listed;
        refs.shrink_to_fit();
        let mut placement = Hasher::new(PLACEMENT_DOMAIN);
        placement.update(&requests.to_le_bytes()).id(&given.finish());
        placement.update(&(refs.len() as u64).to_le_bytes());
        placement.update(&(self.built_in.len() as u64).to_le_bytes());
        for at in &self.built_in {
            placement.update(&at.to_le_bytes());
        }
        let placement = placement.finish();
        Walked { refs, built_in: self.built_in, facts: self.facts, read: self.read, placement }
    }
}

/// The references found so far, each once, in the order found, as rustc's collector keeps the
/// items of an instance in an index set. Each is hashed once and kept once: the
/// tables hold its hash and place (16 bytes an entry, one probe a lookup), and growing them
/// hashes nothing again. A symbol is looked up by its name, so one listed already makes no
/// `String`, nor does a routine of Cranelift's listed already.
struct Listed {
    refs: Vec<Ref>,
    /// The places in `refs` of all but the symbols.
    at: HashTable<Slot>,
    /// The places in `refs` of the symbols ([`FrontRef::Extern`]), by the hash of the name.
    symbols: HashTable<Slot>,
    /// Each routine listed, with its place.
    libcalls: SmallVec<[(ir::LibCall, u32); 8]>,
    /// The place each request is given, in the order asked, one already listed included:
    /// What each request was given, in the order asked, one already listed included
    /// ([`PLACED_DOMAIN`]), and how many there were: [`placement`].
    given: Hasher,
    requests: u64,
}

#[derive(Clone, Copy)]
struct Slot {
    hash: u64,
    at: u32,
}

const _: () = assert!(size_of::<Slot>() == 16);

impl Listed {
    fn with_capacity(capacity: usize) -> Self {
        Listed {
            refs: Vec::with_capacity(capacity),
            at: HashTable::with_capacity(capacity),
            symbols: HashTable::with_capacity(8),
            libcalls: SmallVec::new(),
            given: Hasher::new(PLACED_DOMAIN),
            requests: 0,
        }
    }

    /// Records that a request is given `at`, giving it back.
    fn placed(&mut self, at: u32) -> u32 {
        self.given.update(&[0]).update(&at.to_le_bytes());
        self.requests += 1;
        at
    }

    /// Records that a request is given a data object of the function's own, the `at`th of
    /// its recorder's requests: which earlier ones it is one with is in the code too.
    fn own_data(&mut self, at: u32) {
        self.given.update(&[1]).update(&at.to_le_bytes());
        self.requests += 1;
    }

    fn next(&self) -> u32 {
        u32::try_from(self.refs.len()).expect("fewer than 2^32 references")
    }

    /// Lists `it` where it is not yet, giving its place.
    fn add(&mut self, it: Ref) -> u32 {
        let it = match it {
            FrontRef::Extern(name) => return self.symbol(Cow::Owned(name)),
            it => it,
        };
        let hash = FixedState.hash_one(&it);
        let at = self.next();
        let refs = &self.refs;
        let at = match self.at.entry(hash, |slot| refs[slot.at as usize] == it, |slot| slot.hash) {
            Entry::Occupied(found) => found.get().at,
            Entry::Vacant(vacant) => {
                vacant.insert(Slot { hash, at });
                self.refs.push(it);
                at
            }
        };
        self.placed(at)
    }

    fn symbol(&mut self, name: Cow<'_, str>) -> u32 {
        let hash = FixedState.hash_one(&*name);
        let at = self.next();
        let refs = &self.refs;
        let same =
            |slot: &Slot| matches!(&refs[slot.at as usize], FrontRef::Extern(it) if **it == *name);
        let at = match self.symbols.entry(hash, same, |slot| slot.hash) {
            Entry::Occupied(found) => found.get().at,
            Entry::Vacant(vacant) => {
                vacant.insert(Slot { hash, at });
                self.refs.push(FrontRef::Extern(name.into_owned()));
                at
            }
        };
        self.placed(at)
    }

    fn libcall(&mut self, libcall: ir::LibCall) {
        let listed = self.libcalls.iter().find(|(it, _)| *it == libcall).map(|&(_, at)| at);
        match listed {
            Some(at) => {
                self.placed(at);
            }
            None => {
                let at = self.symbol(Cow::Owned(places::libcall_symbol(libcall)));
                self.libcalls.push((libcall, at));
            }
        }
    }
}

/// A set of the blocks of a body, a bit a block, by the block's index in the body's arena.
struct Blocks(BitSet);

impl Blocks {
    fn new(count: usize) -> Self {
        Blocks(BitSet::new(count))
    }

    /// Adds `id`, saying whether it was not in the set.
    fn insert(&mut self, id: BasicBlockId) -> bool {
        self.0.insert(id.into_raw().into_u32() as usize)
    }

    fn contains(&self, id: BasicBlockId) -> bool {
        self.0.contains(id.into_raw().into_u32() as usize)
    }
}

impl<'cx, 'db> Walk<'_, 'cx, 'db> {
    pub(super) fn db(&self) -> &'db Db {
        self.fx.cg.db
    }

    /// Lists what the resolvers have asked of the recorder since last time, as the IR
    /// builder's requests are placed: a function by its target, a static as itself or its
    /// symbol, with what Cranelift calls to reach a thread-local one.
    fn drain(&mut self) -> Result<(), String> {
        let Walk { fx, found, drained, func_places, data_places, .. } = self;
        let Found { listed, facts, .. } = &mut **found;
        let db = fx.cg.db;
        let requests = &fx.cg.wants.requests;
        let Some(asks) = fx.cg.wants.asks.as_deref() else {
            // alloc: the error's message
            return Err(
                "the walk's recorder logs no asks (`Wants::asks`), which its places are read from"
                    .to_owned(),
            );
        };
        // Each ask in order, one asked for before included: a request's first ask is listed,
        // and every later one given the place the first was ([`placement`]).
        for &ask in &asks[*drained..] {
            match ask {
                Asked::Func(at) => {
                    let known = func_places.len();
                    match (at as usize).cmp(&known) {
                        std::cmp::Ordering::Less => {
                            listed.placed(func_places[at as usize]);
                        }
                        std::cmp::Ordering::Equal => {
                            let place = match &requests.funcs[at as usize].target {
                                FuncTarget::Symbol(symbol) => {
                                    listed.symbol(Cow::Borrowed(symbol.as_str()))
                                }
                                target => listed.add(places::func_ref(db, target)?),
                            };
                            func_places.push(place);
                        }
                        // alloc: the error's message
                        std::cmp::Ordering::Greater => {
                            return Err(format!(
                                "the walk's recorder gave function request {at} before request {known} \
                             was asked for: an ask that `Wants::asks` did not log"
                            ));
                        }
                    }
                }
                Asked::Data(at) => {
                    let known = data_places.len();
                    match (at as usize).cmp(&known) {
                        std::cmp::Ordering::Less => match data_places[at as usize] {
                            Some(place) => {
                                listed.placed(place);
                            }
                            None => listed.own_data(at),
                        },
                        std::cmp::Ordering::Equal => {
                            let request = &requests.datas[at as usize];
                            if let DataRequest::Vtable {
                                ty, trait_ref: Some((trait_, args)), ..
                            } = request
                            {
                                facts.vtable(ty, *trait_, args);
                            }
                            let place = match request {
                                DataRequest::Static(id) => {
                                    let place = listed.add(places::static_ref(db, *id)?);
                                    // A static of an `extern` block is at the program's edge, as a foreign
                                    // function's signature is: code outside the program made what it holds.
                                    let flags =
                                        hir_def::signatures::StaticSignature::of(db, *id).flags;
                                    if flags.contains(hir_def::signatures::StaticFlags::EXTERN) {
                                        facts.foreign(
                                            &fx.infcx,
                                            fx.env.param_env,
                                            fx.static_ty(*id),
                                        );
                                    }
                                    if is_thread_local(db, *id) {
                                        listed.libcall(ir::LibCall::ElfTlsGetAddr);
                                    }
                                    Some(place)
                                }
                                _ => {
                                    listed.own_data(at);
                                    None
                                }
                            };
                            data_places.push(place);
                        }
                        // alloc: the error's message
                        std::cmp::Ordering::Greater => {
                            return Err(format!(
                                "the walk's recorder gave data request {at} before request {known} \
                             was asked for: an ask that `Wants::asks` did not log"
                            ));
                        }
                    }
                }
            }
        }
        *drained = asks.len();
        Ok(())
    }

    /// Runs one of the IR builder's resolvers, which builds no code, and lists what it asked
    /// for.
    pub(super) fn recorded<T>(
        &mut self,
        work: impl FnOnce(&mut FunctionCx<'cx, 'db>) -> crate::Result<T>,
    ) -> Result<T, String> {
        let out = work(&mut *self.fx).map_err(told)?;
        self.drain()?;
        Ok(out)
    }

    pub(super) fn add(&mut self, it: Ref) -> Result<(), String> {
        self.drain()?;
        self.found.listed.add(it);
        Ok(())
    }

    /// The object types a signature at the program's edge carries ([`Facts::foreign`]): the
    /// function's own where its ABI is not Rust's, and each foreign function's it calls. Code
    /// outside the program may make or take objects of them.
    fn edge(&mut self, body: &'db MirBody<'db>, abi: ExternAbi) -> Result<(), String> {
        let db = self.fx.cg.db;
        let (infcx, env) = (&self.fx.infcx, self.fx.env.param_env);
        let facts = &mut self.found.facts;
        if !matches!(abi, ExternAbi::Rust | ExternAbi::RustCall) {
            for &local in body.param_locals.iter().chain([&crate::returned_local(body)]) {
                facts.foreign(infcx, env, body.locals[local].ty.as_ref());
            }
        }
        for (_, block) in body.basic_blocks.iter() {
            let Some(TerminatorKind::Call { func, .. }) =
                block.terminator.as_ref().map(|it| &it.kind)
            else {
                continue;
            };
            let Some((CallableDefId::FunctionId(callee), _)) = func.fn_def() else { continue };
            if !matches!(callee.loc(db).container, hir_def::ItemContainerId::ExternBlockId(_)) {
                continue;
            }
            let sig = db.callable_item_signature(callee.into()).skip_binder().skip_binder();
            for &ty in sig.inputs_and_output.as_slice() {
                facts.foreign(infcx, env, ty);
            }
        }
        Ok(())
    }

    /// A call of `def` with `args`, as `Recorder::callee` asks for it.
    pub(super) fn call(&mut self, def: InstanceDef, args: GenericArgs<'db>) -> Result<(), String> {
        self.listed_call(def, args).map(drop)
    }

    /// [`Self::call`], giving the callee's place the first time it is listed.
    fn listed_call(
        &mut self,
        def: InstanceDef,
        args: GenericArgs<'db>,
    ) -> Result<Option<u32>, String> {
        self.drain()?;
        if let Some(&at) = self.found.called.get(&(def, args)) {
            self.found.listed.placed(at);
            return Ok(None);
        }
        self.call_place(def, args).map(Some)
    }

    /// The place of the callee `def` with `args`, listed where it is not yet.
    fn call_place(&mut self, def: InstanceDef, args: GenericArgs<'db>) -> Result<u32, String> {
        let db = self.fx.cg.db;
        let wasm = self.fx.cg.target.is_wasm();
        let Found { env, listed, called, .. } = &mut *self.found;
        // A callee listed already is given its place again: which earlier request a call
        // shares its place with is what [`placement`] holds.
        if let Some(&at) = called.get(&(def, args)) {
            return Ok(listed.placed(at));
        }
        // A unit of a proc macro's wasm module calls the bridge's import in place of what
        // `wasm::bridged` names (`Recorder::callee`), any other unit the function: the walk
        // lists both.
        if let InstanceDef::Fn(func) = def
            && wasm
            && let Some(import) = super::wasm::bridged(db, func)
        {
            listed.symbol(Cow::Borrowed(import));
        }
        let at = listed.add(places::call_ref(db, def, args, env)?);
        called.insert((def, args), at);
        Ok(at)
    }

    /// Lists the callee at `at` as one whose body is built in, once.
    fn build_in(&mut self, at: u32) {
        if !self.built_here.contains(&at) {
            self.built_here.push(at);
        }
        if !self.found.built_in.contains(&at) {
            self.found.built_in.push(at);
        }
    }

    /// The callees the MIR optimizer inlined into `body` (`MirBody::inlined`), whose code is
    /// built in: a unit's key holds theirs ([`super::frontend::inlined`]). Their bodies'
    /// requests are of `body`, which the walk of its blocks lists.
    fn inlined_callees(&mut self, body: &'db MirBody<'db>) -> Result<(), String> {
        for callee in &body.inlined.callees {
            let def = match callee.body {
                InlinedBody::Fn(func) => InstanceDef::Fn(func),
                InlinedBody::Closure(closure) => InstanceDef::Closure(closure.as_id()),
            };
            let at = self.call_place(def, callee.args.as_ref())?;
            if !matches!(self.found.listed.refs[at as usize], FrontRef::Function(_)) {
                // alloc: the error's message
                return Err(format!(
                    "{def:?} is inlined, but its callers name it as {:?}",
                    self.found.listed.refs[at as usize]
                ));
            }
            self.build_in(at);
        }
        Ok(())
    }

    /// A call `call_as` makes of an instance, whose body the IR of a v2 unit builds in where
    /// `always_inlinable_callee` gives one, as LLVM's always-inliner does at -O0. Its body's
    /// requests are then the caller's, listed as `declare_inlined` makes them; the callee
    /// stays listed, as a unit whose IR calls it after all (their signatures differ) needs.
    /// Whether it is built in is asked at every call, not only where the callee is first
    /// listed: a `become` of it lists it first and builds none in ([`Self::tail_call`]).
    fn instance_call(&mut self, def: InstanceDef, args: GenericArgs<'db>) -> Result<(), String> {
        self.drain()?;
        let at = self.call_place(def, args)?;
        if self.built_here.contains(&at) {
            return Ok(());
        }
        let Some((instance, body)) = self.fx.always_inlinable_callee(def, args) else {
            return Ok(());
        };
        if !matches!(self.found.listed.refs[at as usize], FrontRef::Function(_)) {
            // alloc: the error's message
            return Err(format!(
                "{instance:?} is built in, but its callers name it as {:?}",
                self.found.listed.refs[at as usize]
            ));
        }
        self.fx.cg.declare_inlined(&instance, body, self.fx.builder.func);
        self.build_in(at);
        // A unit builds the body under its own settings, where a callee this build has in
        // it may not be built in and is called instead. What the body has in it, built in or
        // put there by the MIR inliner under these settings, is the caller's code: listed
        // here, as the callee's own units may have none of it (`built_in_body`).
        for nested in &body.built_in {
            let at = self.call_place(nested.def, nested.args.as_ref())?;
            if !matches!(self.found.listed.refs[at as usize], FrontRef::Function(_)) {
                // alloc: the error's message
                return Err(format!(
                    "{nested:?} is built into {instance:?}, but its callers name it as {:?}",
                    self.found.listed.refs[at as usize]
                ));
            }
            self.build_in(at);
        }
        self.drain()
    }

    /// A function of no crate of the build, by its symbol: `Recorder::import`.
    pub(super) fn symbol(&mut self, name: &str) -> Result<(), String> {
        self.drain()?;
        self.found.listed.symbol(Cow::Borrowed(name));
        Ok(())
    }

    /// A routine Cranelift calls itself, which no request names.
    pub(super) fn libcall(&mut self, libcall: ir::LibCall) -> Result<(), String> {
        self.drain()?;
        self.found.listed.libcall(libcall);
        Ok(())
    }

    /// The function of assembly `text`: `Recorder::assembly`.
    pub(super) fn assembly(&mut self, text: String) -> Result<(), String> {
        self.add(FrontRef::Function(HirInstance(Kind::Asm(text, false))))
    }

    /// What any function may name without asking: the copies Cranelift makes calls of, and
    /// what a landing pad needs.
    fn outside(&mut self) -> Result<(), String> {
        for libcall in [ir::LibCall::Memcpy, ir::LibCall::Memmove, ir::LibCall::Memset] {
            self.libcall(libcall)?;
        }
        self.symbol(places::PERSONALITY)?;
        let resume = match self.fx.cg.target.exception_in_register() {
            true => "_Unwind_Resume",
            false => crate::unwind::seh::RESUME,
        };
        self.symbol(resume)
    }

    /// The type of a place, as `place_ref` finds it.
    pub(super) fn place_ty(&self, place: &StoredPlace) -> PlaceTy<'db> {
        let fx = &*self.fx;
        let body = fx.body.expect("MIR body");
        let mut ty = PlaceTy::from_ty(body.locals[place.local].ty.as_ref());
        ty.ty = fx.normalize(ty.ty);
        for proj in place.projection.as_slice() {
            ty = ty.projection_ty(&fx.infcx, proj, fx.env.param_env);
            ty.ty = fx.normalize(ty.ty);
        }
        ty
    }

    pub(super) fn operand_ty(&self, operand: &Operand) -> Ty<'db> {
        match &operand.kind {
            OperandKind::Copy(place) | OperandKind::Move(place) => self.place_ty(place).ty,
            OperandKind::Constant { ty, .. } => ty.as_ref(),
            OperandKind::Allocation { allocation } => allocation.as_ref().ty,
            OperandKind::Static(id) => self.fx.static_ty(*id),
        }
    }

    /// What reading `operand` asks for, as `FunctionCx::operand` reads it: a constant's data
    /// and what it points to, a static. Its type.
    pub(super) fn operand(&mut self, operand: &Operand) -> Result<Ty<'db>, String> {
        match &operand.kind {
            OperandKind::Copy(place) | OperandKind::Move(place) => Ok(self.place_ty(place).ty),
            OperandKind::Constant { konst, ty } => {
                let ty = ty.as_ref();
                if self.located(operand) {
                    return Ok(ty);
                }
                match konst.as_ref().kind() {
                    ConstKind::Value(_) if is_integer(ty) => (),
                    ConstKind::Value(value) => {
                        self.recorded(|fx| fx.valtree_blob(ty, value.value))?;
                    }
                    ConstKind::Alias(_, unevaluated) => {
                        self.recorded(|fx| {
                            let (allocation, whose) = fx.eval_named_const(unevaluated)?;
                            fx.allocation_blob(allocation, whose, false)
                        })?;
                    }
                    // alloc: the error's message
                    kind => return Err(format!("constant {kind:?}")),
                }
                Ok(ty)
            }
            OperandKind::Allocation { allocation } => {
                let allocation = allocation.as_ref();
                let ty = allocation.ty;
                if !self.located(operand) {
                    self.recorded(|fx| fx.allocation_blob(allocation, fx.in_place(), false))?;
                }
                Ok(ty)
            }
            OperandKind::Static(id) => {
                self.static_(*id)?;
                Ok(self.fx.static_ty(*id))
            }
        }
    }

    pub(super) fn operands(&mut self, operands: &[Operand]) -> Result<Tys<'db>, String> {
        // alloc: into `Tys`, inline up to eight operands
        operands.iter().map(|it| self.operand(it)).collect()
    }

    /// Whether `operand` is what `file!()`, or a `concat!` with it, expanded to, which
    /// `builtin_location` makes of where it is: data that points nowhere else, a literal's
    /// when it does not.
    fn located(&self, operand: &Operand) -> bool {
        let Some((owner, expr)) = self.fx.written_expr(operand.span()) else { return false };
        let InferBodyId::DefWithBodyId(def) = owner else { return false };
        self.fx.location_macro(owner, expr).is_some()
            || located_concats(self.db(), def).contains_key(&expr)
    }

    pub(super) fn static_(&mut self, id: StaticId) -> Result<(), String> {
        self.recorded(|fx| {
            fx.cg.static_data(id);
            Ok(())
        })
    }

    /// The panic function of core at `path`, as `call_panic` calls it.
    pub(super) fn panic(&mut self, path: &[&str]) -> Result<(), String> {
        match self.fx.core_function(path) {
            Some(func) => self.call(InstanceDef::Fn(func), GenericArgs::new_from_slice(&[])),
            // alloc: the error's message
            None => Err(format!(
                "a call of rustc's `core::{}`, which a build without rustc has none of",
                path.join("::")
            )),
        }
    }

    /// `drop_value`: the drop glue of `ty`, when dropping it does anything.
    pub(super) fn drop_value(&mut self, ty: Ty<'db>) -> Result<(), String> {
        let ty = self.fx.normalize(ty);
        if self.fx.glue_of(ty) == DropGlue::None {
            return Ok(());
        }
        self.call(InstanceDef::DropGlue, GenericArgs::new_from_slice(&[ty.into()]))
    }

    /// `call_direct` of `func` with arguments of `arg_tys`.
    pub(super) fn call_direct(
        &mut self,
        func: FunctionId,
        generic_args: GenericArgs<'db>,
        arg_tys: &[Ty<'db>],
        dst_ty: Ty<'db>,
    ) -> Result<(), String> {
        let db = self.db();
        let callee = dispatch::callee(&self.fx.infcx, self.fx.env, func, generic_args)
            .map_err(|why| crate::no_impl_method(db, func, generic_args, why))?;
        let (callee, callee_args) = match callee {
            dispatch::Callee::Poll(body, parent_args) => {
                return self.emit_call(crate::instance_def(body), parent_args, arg_tys, dst_ty);
            }
            // Through the object's vtable, which the coercion to it listed.
            dispatch::Callee::Virtual => return Ok(()),
            dispatch::Callee::Derived(impl_, method) => {
                let derived =
                    crate::unexpanded_derive(db, (impl_, method), generic_args.type_at(0));
                // alloc: the error's message
                return Err(format!("call to {derived}"));
            }
            dispatch::Callee::CloneBuiltin(_) if arg_tys.len() == 1 => {
                return self.clone_builtin(func, dst_ty);
            }
            dispatch::Callee::FnPtr(_) if arg_tys.len() == 1 => return Ok(()),
            dispatch::Callee::CloneBuiltin(callee_args) | dispatch::Callee::FnPtr(callee_args) => {
                (func, callee_args)
            }
            dispatch::Callee::Fn(callee, callee_args) => (callee, callee_args),
        };
        self.emit_call(InstanceDef::Fn(callee), callee_args, arg_tys, dst_ty)
    }

    /// `clone_builtin`: each part cloned, and the ones cloned before dropped when one unwinds.
    fn clone_builtin(&mut self, clone: FunctionId, ty: Ty<'db>) -> Result<(), String> {
        let interner = self.fx.interner;
        let part = |this: &mut Self, part: Ty<'db>| {
            let reference =
                Ty::new_ref(interner, Region::new_erased(interner), part, Mutability::Not);
            let args = GenericArgs::new_from_slice(&[part.into()]);
            this.call_direct(clone, args, &[reference], part)
        };
        let fields = match ty.kind() {
            TyKind::Tuple(fields) => Some(fields),
            TyKind::Closure(..) | TyKind::CoroutineClosure(..) => {
                match ty.closure_upvars_ty().map(|it| it.kind()) {
                    Some(TyKind::Tuple(fields)) => Some(fields),
                    // alloc: the error's message
                    _ => {
                        return Err("builtin `Clone` of a closure without tupled upvars".to_owned());
                    }
                }
            }
            _ => None,
        };
        if let Some(fields) = fields {
            for field in fields.iter() {
                part(self, field)?;
                self.drop_value(field)?;
            }
            return Ok(());
        }
        match ty.kind() {
            TyKind::Array(element, _) => {
                part(self, element)?;
                self.drop_value(element)
            }
            TyKind::FnDef(..)
            | TyKind::FnPtr(..)
            | TyKind::Never
            | TyKind::RawPtr(..)
            | TyKind::Ref(_, _, Mutability::Not) => Ok(()),
            // alloc: the error's message
            _ => Err(format!("builtin `Clone` of {ty:?}")),
        }
    }

    /// `emit_call` of an instance: the instance, and for one that takes `...`, the function
    /// the call goes through.
    pub(super) fn emit_call(
        &mut self,
        def: InstanceDef,
        args: GenericArgs<'db>,
        arg_tys: &[Ty<'db>],
        dst_ty: Ty<'db>,
    ) -> Result<(), String> {
        self.instance_call(def, args)?;
        let InstanceDef::Fn(func) = def else { return Ok(()) };
        let db = self.db();
        let signature = FunctionSignature::of(db, func);
        if !signature.is_varargs() {
            return Ok(());
        }
        self.variadic_call(signature.abi, arg_tys, dst_ty, track_caller(db, func))
    }

    /// `emit_call` through a pointer: `named` is how many of the arguments are for named
    /// parameters, when the rest are for `...`.
    pub(super) fn emit_pointer_call(
        &mut self,
        abi: ExternAbi,
        named: Option<usize>,
        arg_tys: &[Ty<'db>],
        dst_ty: Ty<'db>,
    ) -> Result<(), String> {
        match named {
            Some(_) => self.variadic_call(abi, arg_tys, dst_ty, false),
            None => Ok(()),
        }
    }

    /// The function `counted_variadic` sends a call of a variadic function through, where the
    /// System V convention of x86-64 says how many vector registers hold arguments.
    fn variadic_call(
        &mut self,
        abi: ExternAbi,
        arg_tys: &[Ty<'db>],
        dst_ty: Ty<'db>,
        tracked: bool,
    ) -> Result<(), String> {
        let target = self.fx.cg.target;
        if target.is_wasm()
            || target.call_conv != cranelift_codegen::isa::CallConv::SystemV
            || target.arch != Arch::X86_64
        {
            return Ok(());
        }
        let FnAbi { ret, args: modes } =
            self.fx.fn_abi(abi, dst_ty, arg_tys.iter().copied()).map_err(told)?;
        let mut sig = self.fx.signature(&modes, ret);
        if tracked {
            sig.params.push(ir::AbiParam::new(target.pointer()));
        }
        if !sig.params.iter().any(|param| param.value_type.is_float()) {
            return Ok(());
        }
        let (mut ints, mut floats, mut stack) = (0, 0, 0);
        for param in &sig.params {
            match param.purpose {
                ArgumentPurpose::Normal | ArgumentPurpose::StructReturn => (),
                _ => return Err("an argument in memory with a float to a variadic function".into()),
            }
            match param.value_type {
                ty if ty.is_float() && floats < 8 => floats += 1,
                ty if ty.is_float() => stack += 1,
                ty if ty.is_int() && ty.bytes() <= 8 && ints < 6 => ints += 1,
                ty if ty.is_int() && ty.bytes() <= 8 => stack += 1,
                _ => return Err("a float and a wide argument to a variadic function".into()),
            }
        }
        self.assembly(asm::counted_variadic(ints, stack))
    }

    /// The body of a function, closure or `async` block, as `FunctionCx::codegen` builds it.
    fn body(&mut self, body: &'db MirBody<'db>) -> Result<(), String> {
        if body.phase != MirPhase::Runtime {
            // alloc: the error's message
            return Err("a body whose drops are not elaborated".to_owned());
        }
        self.bfloat(body)?;
        self.promoted(body)?;
        if body.coroutine.is_none() && self.neon_fused()? {
            return Ok(());
        }
        let (reachable, decided) = self.reachable(body);
        if body.coroutine.is_some() {
            self.enter_coroutine(body)?;
        }
        for (id, block) in body.basic_blocks.iter() {
            if !reachable.contains(id) {
                continue;
            }
            for statement in &block.statements {
                self.fx.span = statement.span;
                match &statement.kind {
                    StatementKind::Assign(place, rvalue) => self.rvalue(place, rvalue)?,
                    StatementKind::InlineAsm(asm) => self.inline_asm(asm)?,
                    // alloc: the error's message
                    StatementKind::Deinit(_) => return Err("deinit".to_owned()),
                    StatementKind::StorageLive(_)
                    | StatementKind::StorageDead(_)
                    | StatementKind::FakeRead(_)
                    | StatementKind::Nop => (),
                }
            }
            let Some(terminator) = &block.terminator else {
                // alloc: the error's message
                return Err("block without terminator".to_owned());
            };
            self.fx.span = terminator.span;
            if !decided.contains(id) {
                self.terminator(&terminator.kind)?;
            }
        }
        self.inlined_callees(body)
    }

    /// The blocks `codegen` compiles with every check on and unwinding, which are all that it
    /// compiles with any setting: a check that is off only decides a switch, and `panic =
    /// "abort"` only leaves out cleanup blocks. With the blocks whose switch is decided, which
    /// only jump.
    fn reachable(&mut self, body: &'db MirBody<'db>) -> (Blocks, Blocks) {
        let off = self.fx.checks_off(body);
        let resumes_only = match body.coroutine {
            Some(_) => HashSet::new_in(Global),
            None => hir_ty_mir::mir::noop_landing_pads(body),
        };
        let count = body.basic_blocks.len();
        let mut reachable = Blocks::new(count);
        let mut decided = Blocks::new(count);
        let mut worklist = Vec::with_capacity(count);
        worklist.push(body.start_block);
        while let Some(id) = worklist.pop() {
            if !reachable.insert(id) {
                continue;
            }
            match body.basic_blocks[id].terminator.as_ref().map(|it| &it.kind) {
                Some(TerminatorKind::SwitchInt { discr, targets }) => {
                    match self.fx.switch_bits(&body.basic_blocks[id], discr, &off) {
                        Some(bits) => {
                            decided.insert(id);
                            worklist.push(targets.target_for_value(bits))
                        }
                        None => worklist.extend_from_slice(targets.all_targets()),
                    }
                }
                Some(
                    TerminatorKind::Goto { target }
                    | TerminatorKind::Drop { target, .. }
                    | TerminatorKind::DropAndReplace { target, .. }
                    | TerminatorKind::Assert { target, .. }
                    | TerminatorKind::FalseEdge { real_target: target, .. }
                    | TerminatorKind::FalseUnwind { real_target: target, .. },
                ) => worklist.push(*target),
                Some(TerminatorKind::Call { target, .. }) => worklist.extend(*target),
                Some(TerminatorKind::Yield { resume, drop, .. }) => {
                    worklist.push(*resume);
                    worklist.extend(*drop);
                }
                _ => (),
            }
            let block = &body.basic_blocks[id];
            let unwind = block.terminator.as_ref().and_then(|it| it.kind.unwind());
            worklist.extend(unwind.filter(|it| !resumes_only.contains(it)));
        }
        (reachable, decided)
    }

    /// `enter_coroutine`: an `async` body polled after it ended panics
    /// (`resumed_after_end`), and dropped before it ran drops what it captured.
    fn enter_coroutine(&mut self, body: &'db MirBody<'db>) -> Result<(), String> {
        for path in crate::RESUMED_AFTER_END {
            self.panic(path)?;
        }
        let receiver = body.param_locals[0];
        let Some(ty) = body.locals[receiver].ty.as_ref().builtin_deref(true) else {
            // alloc: the error's message
            return Err("async body without a reference to its future".to_owned());
        };
        let TyKind::Coroutine(_, args) = ty.kind() else {
            // alloc: the error's message
            return Err(format!("async body of {ty:?}"));
        };
        for field in args.as_coroutine().tupled_upvars_ty().tuple_fields().iter() {
            self.drop_value(field)?;
        }
        Ok(())
    }

    /// The read-only data of each promoted local, as `promoted_constant` makes it. Its
    /// requests are made whether or not the data is then kept: a local the data is not kept
    /// for, or that is kept in a register, is assigned by its statements, which the walk of
    /// the blocks lists too.
    fn promoted(&mut self, body: &'db MirBody<'db>) -> Result<(), String> {
        let db = self.db();
        for promoted in crate::promoted_constants(db, body) {
            let local = promoted.local;
            let Ok(layout) = self.fx.layout(body.locals[local].ty.as_ref()) else { continue };
            if layout.size.bytes() == 0 || self.fx.mentions_location(promoted.owner, promoted.expr)
            {
                continue;
            }
            let env = self.fx.env;
            let Some(args) = promoted.args(body, self.fx.args.as_ref()) else { continue };
            // A promoted constant whose MIR does not build is the IR builder's error too
            // (`promoted_constant`), never left out of the walk.
            let constant = monomorphized_promoted_mir_body_query(
                db,
                promoted.owner,
                promoted.expr,
                args,
                env.store(),
            )
            .map_err(|error| crate::no_promoted_mir(db, promoted, error).to_string())?;
            let allocation = interpret_mir(db, constant, Some(env)).ok().and_then(|it| it.0.ok());
            if let Some(allocation) = allocation {
                // A constant whose data can't be made is not promoted, and leaves what it
                // asked for before then.
                _ = self.fx.allocation_blob(allocation, crate::ConstOf::Promoted(promoted), false);
                self.drain()?;
            }
        }
        Ok(())
    }

    /// `assign`: what the value assigned asks for.
    fn rvalue(&mut self, place: &StoredPlace, rvalue: &Rvalue) -> Result<(), String> {
        match rvalue {
            Rvalue::Use(operand) | Rvalue::Repeat(operand, _) => {
                self.operand(operand)?;
            }
            Rvalue::CopyForDeref(_)
            | Rvalue::Ref(..)
            | Rvalue::Len(_)
            | Rvalue::Discriminant(_) => (),
            Rvalue::BinaryOp(op, lhs, rhs) => {
                self.operand(lhs)?;
                self.operand(rhs)?;
                self.binary_op(op, lhs, rhs)?;
            }
            Rvalue::UnaryOp(op, operand) => {
                self.operand(operand)?;
                self.unary_op(op, operand)?;
            }
            Rvalue::Cast(kind, operand, target) => {
                let from = self.operand(operand)?;
                self.cast(kind, from, target.as_ref())?;
            }
            Rvalue::Aggregate(_, operands) => {
                self.operands(operands)?;
            }
            Rvalue::CheckedBinaryOp(..) => {
                let dst = self.place_ty(place).ty;
                // alloc: the error's message
                return Err(format!("rvalue {rvalue:?} of {dst:?}"));
            }
            Rvalue::ThreadLocalRef(never) | Rvalue::AddressOf(never) | Rvalue::NullaryOp(never) => {
                match *never {}
            }
        }
        Ok(())
    }

    /// `cast`: an unsizing coercion's vtables, a function pointer's function, a conversion's
    /// routines.
    fn cast(&mut self, kind: &CastKind, from: Ty<'db>, to: Ty<'db>) -> Result<(), String> {
        match kind {
            CastKind::PointerCoercion(PointerCast::Unsize) => self.unsize(from, to),
            CastKind::PointerCoercion(
                PointerCast::ReifyFnPointer | PointerCast::ClosureFnPointer(_),
            ) => self.recorded(|fx| fx.fn_item(from)).map(drop),
            CastKind::PointerCoercion(
                PointerCast::MutToConstPointer
                | PointerCast::ArrayToPointer
                | PointerCast::UnsafeFnPointer,
            )
            | CastKind::IntToInt
            | CastKind::PtrToPtr
            | CastKind::PointerExposeAddress
            | CastKind::PointerFromExposedAddress
            | CastKind::FnPtrToPtr
            | CastKind::Transmute => Ok(()),
            CastKind::IntToFloat | CastKind::FloatToInt | CastKind::FloatToFloat => {
                self.float_cast(kind, from, to)
            }
            // alloc: the error's message
            _ => Err(format!("cast {kind:?}")),
        }
    }

    /// `unsize`: the vtable of each pointer that becomes one to a `dyn`.
    pub(super) fn unsize(&mut self, from_ty: Ty<'db>, to_ty: Ty<'db>) -> Result<(), String> {
        if from_ty == to_ty {
            return Ok(());
        }
        match (from_ty.kind(), to_ty.kind()) {
            (
                TyKind::RawPtr(from, _) | TyKind::Ref(_, from, _),
                TyKind::RawPtr(to, _) | TyKind::Ref(_, to, _),
            ) => self.unsize_metadata(from, to),
            (TyKind::Pat(from, _), TyKind::Pat(to, _)) => self.unsize(from, to),
            (TyKind::Adt(def, args), TyKind::Adt(to_def, to_args))
                if def.def_id() == to_def.def_id() =>
            {
                let AdtId::StructId(id) = def.def_id() else {
                    // alloc: the error's message
                    return Err(format!("unsizing {from_ty:?} to {to_ty:?}"));
                };
                let interner = self.fx.interner;
                let field_types = self.db().field_types(id.into());
                for (_, field) in field_types.iter() {
                    let from = field.ty().instantiate(interner, args).skip_norm_wip();
                    let to = field.ty().instantiate(interner, to_args).skip_norm_wip();
                    let (from, to) = (self.fx.normalize(from), self.fx.normalize(to));
                    if self.fx.layout(to).map_err(told)?.size.bytes() == 0 {
                        continue;
                    }
                    self.unsize(from, to)?;
                }
                Ok(())
            }
            // alloc: the error's message
            _ => Err(format!("unsizing {from_ty:?} to {to_ty:?}")),
        }
    }

    /// `unsize_metadata`: a pointee that becomes a `dyn` has the vtable of its type, made of
    /// its methods and drop glue.
    fn unsize_metadata(&mut self, from: Ty<'db>, to: Ty<'db>) -> Result<(), String> {
        match (from.kind(), to.kind()) {
            (TyKind::Array(..), TyKind::Slice(_)) => Ok(()),
            // The vtable it is upcast to is in the one it has; the object it becomes holds
            // the types of the one it was ([`super::devirt`]).
            (TyKind::Dynamic(..), TyKind::Dynamic(..)) => {
                self.found.facts.upcast(&self.fx.infcx, self.fx.env.param_env, (from, to));
                Ok(())
            }
            (_, TyKind::Dynamic(..)) => {
                if self.fx.is_unsized(from) {
                    // alloc: the error's message
                    return Err(format!("unsizing {from:?} to {to:?}"));
                }
                self.recorded(|fx| {
                    let trait_ref = fx.principal(to, from);
                    fx.vtable(from, trait_ref)
                })
                .map(drop)
            }
            (TyKind::Adt(def, args), TyKind::Adt(to_def, to_args))
                if def.def_id() == to_def.def_id() =>
            {
                let AdtId::StructId(id) = def.def_id() else {
                    // alloc: the error's message
                    return Err(format!("unsizing {from:?}"));
                };
                let field_types = self.db().field_types(id.into());
                let Some((_, last)) = field_types.iter().last() else {
                    // alloc: the error's message
                    return Err("unsizing a struct without fields".to_owned());
                };
                let interner = self.fx.interner;
                let from = last.ty().instantiate(interner, args).skip_norm_wip();
                let to = last.ty().instantiate(interner, to_args).skip_norm_wip();
                let (from, to) = (self.fx.normalize(from), self.fx.normalize(to));
                self.unsize_metadata(from, to)
            }
            // alloc: the error's message
            _ => Err(format!("unsizing {from:?} to {to:?}")),
        }
    }

    /// `terminator`, of a block whose switch is not decided.
    fn terminator(&mut self, kind: &TerminatorKind) -> Result<(), String> {
        match kind {
            TerminatorKind::Goto { .. }
            | TerminatorKind::FalseEdge { .. }
            | TerminatorKind::FalseUnwind { .. }
            | TerminatorKind::Return
            | TerminatorKind::Yield { .. }
            | TerminatorKind::Unreachable
            | TerminatorKind::CoroutineDrop
            // `outside` has what it resumes with.
            | TerminatorKind::UnwindResume => Ok(()),
            TerminatorKind::SwitchInt { discr, .. } => {
                if self.fx.const_bits(discr).is_none() {
                    self.operand(discr)?;
                }
                Ok(())
            }
            TerminatorKind::Drop { place, .. } => {
                let ty = self.place_ty(place).ty;
                self.drop_value(ty)?;
                // The walk reads bodies that keep every allocation (`Build::WALK`); a body of
                // a unit that moves one to the stack drops the value the box or `Rc` held
                // itself (`hir-ty-mir`'s `HeapToStack`), so the walk lists that drop too.
                match hir_ty_mir::mir::heap_held(self.fx.cg.db, ty) {
                    Some(held) if self.fx.infcx.type_is_sized_modulo_regions(self.fx.env.param_env, held) => {
                        self.drop_value(held)
                    }
                    _ => Ok(()),
                }
            }
            TerminatorKind::Assert { msg, .. } => self.assert(msg),
            TerminatorKind::Call { func, args, destination, target, intrinsic, .. } => {
                // A promoted call already holds its value.
                if target.is_some() && self.fx.constant_locals.contains(&destination.local) {
                    return Ok(());
                }
                self.call_terminator(func, args, destination, *intrinsic)
            }
            TerminatorKind::DropAndReplace { .. } | TerminatorKind::Abort => {
                // alloc: the error's message
                Err(format!("terminator {kind:?}"))
            }
            TerminatorKind::TailCall { func, args, .. } => self.tail_call(func, args),
            TerminatorKind::ComptimeFor { .. } | TerminatorKind::ComptimeNext { .. } => {
                // alloc: the error's message
                Err(format!(
                    "{} (in {:?})",
                    hir_ty::mir::ComptimeUnexpanded::new(
                        hir_ty::mir::MirComptimeForm::of_terminator(kind)
                            .expect("a comptime terminator"),
                        "the code generator's walk",
                    ),
                    self.fx.body.map(|it| it.owner),
                ))
            }
        }
    }

    /// The panic an `Assert` makes when it fails, which a unit makes where it checks: an
    /// overflow check is made by a unit with overflow checks only, and listed all the same.
    fn assert(&mut self, msg: &AssertKind) -> Result<(), String> {
        // A check of an arbitrary width panics through core's `panic`, naming it
        // (`FunctionCx::panic_if`).
        if let AssertKind::Overflow(
            BinOp::Add
            | BinOp::Sub
            | BinOp::Mul
            | BinOp::Div
            | BinOp::Rem
            | BinOp::Shl
            | BinOp::Shr,
            of,
            _,
        )
        | AssertKind::OverflowNeg(of)
        | AssertKind::DivisionByZero(of)
        | AssertKind::RemainderByZero(of) = msg
            && NarrowInt::of(self.operand_ty(of)).is_some()
        {
            return self.panic(&["panicking", "panic"]);
        }
        match crate::OverflowOp::of(msg) {
            Some(Ok(op)) => return self.panic(&["panicking", "panic_const", op.panic()]),
            // A unit that makes this check refuses it, naming it.
            Some(Err(_)) if msg.is_optional_overflow_check() => return Ok(()),
            // alloc: the error's message
            Some(Err(op)) => return Err(format!("overflow check of {op:?}")),
            None => (),
        }
        match msg {
            AssertKind::BoundsCheck { .. } => self.panic(&["panicking", "panic_bounds_check"]),
            // The UB checks' (`check_pointers`, `check_enums`): `core::panicking`'s own.
            AssertKind::MisalignedPointerDereference { .. } => {
                self.panic(&["panicking", "panic_misaligned_pointer_dereference"])
            }
            AssertKind::NullPointerDereference => {
                self.panic(&["panicking", "panic_null_pointer_dereference"])
            }
            AssertKind::NullReferenceConstructed => {
                self.panic(&["panicking", "panic_null_reference_constructed"])
            }
            AssertKind::InvalidEnumConstruction(_) => {
                self.panic(&["panicking", "panic_invalid_enum_construction"])
            }
            // alloc: the error's message
            _ => Err(format!("{msg:?}, which is no check the walk knows")),
        }
    }

    /// `tail_call`: the one function a `become` jumps to, never built in. What the IR
    /// builder refuses of the callee (`tail_call_refused`), this refuses too: a unit with it
    /// is never compiled.
    fn tail_call(&mut self, func: &Operand, args: &[Operand]) -> Result<(), String> {
        let fn_ty = self.operand_ty(func);
        self.operands(args)?;
        if let TyKind::FnPtr(..) = fn_ty.kind() {
            if fn_ty.fn_sig(self.fx.interner).c_variadic() {
                // alloc: the error's message
                return Err(format!("a tail call (`become`) through a C-variadic `{fn_ty:?}`"));
            }
            return self.operand(func).map(drop);
        }
        let TyKind::FnDef(def, generic_args) = fn_ty.kind() else {
            // alloc: the error's message
            return Err(format!("a tail call (`become`) of {fn_ty:?}"));
        };
        let generic_args = generic_args.skip_binder();
        let CallableDefId::FunctionId(func) = def.0 else {
            // alloc: the error's message
            return Err(format!("a tail call (`become`) of a constructor, {fn_ty:?}"));
        };
        let db = self.db();
        if let Some(why) = crate::tail_call_refused(db, self.fx.cg.target, self.fx.interner, func) {
            return Err(why);
        }
        let callee = dispatch::callee(&self.fx.infcx, self.fx.env, func, generic_args)
            .map_err(|why| crate::no_impl_method(db, func, generic_args, why))?;
        match callee {
            dispatch::Callee::Fn(callee, callee_args) if track_caller(db, callee) => {
                let def = CallableDefId::FunctionId(callee).into();
                let item = Ty::new_fn_def(self.fx.interner, def, callee_args);
                let args = GenericArgs::new_from_slice(&[item.into()]);
                self.call(InstanceDef::ReifyShim { receiver: false }, args)
            }
            dispatch::Callee::Fn(callee, callee_args) => {
                self.call(InstanceDef::Fn(callee), callee_args)
            }
            dispatch::Callee::Poll(body, parent_args) => {
                self.call(crate::instance_def(body), parent_args)
            }
            dispatch::Callee::Virtual if track_caller(db, func) => {
                let name = FunctionSignature::of(db, func).name.as_str().to_owned();
                // alloc: the error's message
                Err(format!(
                    "a tail call (`become`) through a vtable of `{name}`, which is `#[track_caller]`"
                ))
            }
            // Through the object's vtable, which the coercion to it listed.
            dispatch::Callee::Virtual => Ok(()),
            dispatch::Callee::Derived(..)
            | dispatch::Callee::CloneBuiltin(_)
            | dispatch::Callee::FnPtr(_) => {
                let name = FunctionSignature::of(db, func).name.as_str().to_owned();
                // alloc: the error's message
                Err(format!("a tail call (`become`) of `{name}`, which is no function to jump to"))
            }
        }
    }

    /// `call_with`: what a call terminator calls, as the IR builder sorts it out.
    fn call_terminator(
        &mut self,
        func: &Operand,
        args: &[Operand],
        destination: &StoredPlace,
        intrinsic: Option<Intrinsic>,
    ) -> Result<(), String> {
        let dst_ty = self.place_ty(destination).ty;
        let fn_ty = self.operand_ty(func);
        if let TyKind::FnPtr(..) = fn_ty.kind() {
            self.operand(func)?;
            let arg_tys = self.operands(args)?;
            let sig = fn_ty.fn_sig(self.fx.interner);
            let named = sig.c_variadic().then(|| sig.inputs().skip_binder().len());
            return self.emit_pointer_call(sig.abi(), named, &arg_tys, dst_ty);
        }
        let TyKind::FnDef(def, generic_args) = fn_ty.kind() else {
            // alloc: the error's message
            return Err(format!("call of {fn_ty:?}"));
        };
        let generic_args = generic_args.skip_binder();
        let func = match def.0 {
            CallableDefId::FunctionId(func) => func,
            CallableDefId::StructId(_) | CallableDefId::EnumVariantId(_) => {
                self.operands(args)?;
                return Ok(());
            }
        };
        let db = self.db();
        if fncache::is_code(db, func) {
            return Ok(());
        }
        let lang_items = self.fx.interner.lang_items();
        if fn_trait_methods(lang_items).iter().any(|family| family.contains(&Some(func))) {
            return self.call_fn_trait(func, generic_args, args, dst_ty);
        }
        if [lang_items.DropInPlace, lang_items.DropGlue].contains(&Some(func)) {
            let [pointer] = args else {
                // alloc: the error's message
                return Err(format!("drop glue with {} arguments", args.len()));
            };
            self.operand(pointer)?;
            return self.drop_value(generic_args.type_at(0));
        }
        let signature = FunctionSignature::of(db, func);
        if intrinsic.is_none() && FunctionSignature::is_intrinsic(db, func) {
            // alloc: the error's message
            return Err(format!(
                "a call of the intrinsic `{}` that MIR lowering did not recognize",
                signature.name.as_str()
            ));
        }
        if intrinsic == Some(Intrinsic::ConstEvalSelect) {
            let [tuple, _, runtime] = args else {
                // alloc: the error's message
                return Err(format!("const_eval_select with {} arguments", args.len()));
            };
            let TyKind::FnDef(runtime, runtime_args) = self.operand_ty(runtime).kind() else {
                // alloc: the error's message
                return Err("const_eval_select with a non-fn-item runtime function".to_owned());
            };
            let runtime_args = runtime_args.skip_binder();
            let CallableDefId::FunctionId(runtime) = runtime.0 else {
                // alloc: the error's message
                return Err("const_eval_select with a constructor".to_owned());
            };
            let tuple_ty = self.operand(tuple)?;
            let TyKind::Tuple(fields) = tuple_ty.kind() else {
                // alloc: the error's message
                return Err("const_eval_select with non-tuple arguments".to_owned());
            };
            return self.call_direct(runtime, runtime_args, fields.as_slice(), dst_ty);
        }
        if intrinsic.is_none()
            && func.krate(db).data(db).origin.is_lang()
            && crate::is_ub_check(signature.name.symbol())
        {
            return Ok(());
        }
        if let Some(intrinsic) = intrinsic {
            if self.intrinsic(intrinsic, generic_args, args, dst_ty)? {
                return Ok(());
            }
            if AttrFlags::query(db, func.into())
                .contains(AttrFlags::RUSTC_INTRINSIC_MUST_BE_OVERRIDDEN)
            {
                // alloc: the error's message
                return Err(format!("intrinsic `{intrinsic}`, which must be overridden"));
            }
            if crate::compile_time_only(db, func) || !signature.has_body() {
                // alloc: the error's message
                return Err(format!("intrinsic `{intrinsic}`, which has no body to run"));
            }
        }
        if let Some(name) = crate::link_name_for(db, self.fx.cg.target, func)
            && let Some(name) = name.strip_prefix("llvm.")
        {
            return self.llvm_intrinsic(name, args, dst_ty);
        }
        self.inlined_fma(func, dst_ty)?;
        let arg_tys = self.operands(args)?;
        self.call_direct(func, generic_args, &arg_tys, dst_ty)
    }

    /// `call_fn_trait`: `Fn::call`, `FnMut::call_mut` or `FnOnce::call_once` of what MIR
    /// passes first, with the arguments one by one or as their tuple.
    fn call_fn_trait(
        &mut self,
        method: FunctionId,
        method_args: GenericArgs<'db>,
        args: &[Operand],
        dst_ty: Ty<'db>,
    ) -> Result<(), String> {
        let Some((callee, rest)) = args.split_first() else {
            // alloc: the error's message
            return Err("Fn-trait call without a callee".to_owned());
        };
        // A `dyn FnOnce` called by value: through its vtable's `call_once` slot.
        if let OperandKind::Copy(place) | OperandKind::Move(place) = &callee.kind
            && let TyKind::Dynamic(..) = self.place_ty(place).ty.kind()
        {
            self.operands(rest)?;
            return Ok(());
        }
        let db = self.db();
        let interner = self.fx.interner;
        let mut ty = self.operand(callee)?;
        let mut arg_tys = self.operands(rest)?;
        let tuple_ty = method_args.type_at(1);
        let spread = match (&arg_tys[..], tuple_ty.kind()) {
            (&[only], TyKind::Tuple(fields)) if only == tuple_ty => Some(fields),
            _ => None,
        };
        if let Some(fields) = spread {
            // alloc: into `Tys`, inline up to eight arguments
            arg_tys = fields.iter().collect();
        }
        let mut by_ref = None;
        while let TyKind::Ref(_, inner, mutability) = ty.kind() {
            if self.fx.implements_fn_itself(method, ty, tuple_ty)? {
                break;
            }
            by_ref = Some((ty, mutability));
            if let TyKind::Dynamic(..) = inner.kind() {
                if self.fx.fn_method_of_object(inner).is_none() {
                    // alloc: the error's message
                    return Err(format!("Fn-trait call on {inner:?}"));
                }
                // Through the object's vtable.
                return Ok(());
            }
            ty = inner;
        }
        match ty.kind() {
            TyKind::FnDef(def, generic_args) => match def.0 {
                CallableDefId::FunctionId(func) => {
                    // alloc: the error's message
                    if let Some(intrinsic) = intrinsic_of(db, func).map_err(|it| it.to_string())? {
                        // alloc: the error's message
                        return Err(format!("intrinsic `{intrinsic}` called through an Fn trait"));
                    }
                    self.call_direct(func, generic_args.skip_binder(), &arg_tys, dst_ty)
                }
                CallableDefId::StructId(_) | CallableDefId::EnumVariantId(_) => Ok(()),
            },
            // Called with no `...`, through the pointer.
            TyKind::FnPtr(..) => Ok(()),
            TyKind::Closure(..) | TyKind::CoroutineClosure(..) => {
                let (id, parent_args) = match ty.kind() {
                    TyKind::Closure(id, generic_args) => {
                        (id.0, generic_args.as_closure().parent_args())
                    }
                    TyKind::CoroutineClosure(id, generic_args) => {
                        let closure = generic_args.as_coroutine_closure();
                        if let TyKind::Coroutine(future, future_args) = dst_ty.kind()
                            && is_by_move_coroutine(db, future.0, future_args.as_coroutine())
                        {
                            // `future_by_move`: the future is made in place.
                            return Ok(());
                        }
                        (id.0.as_closure(db), closure.parent_args())
                    }
                    _ => unreachable!(),
                };
                let parent_args = GenericArgs::new_from_slice(parent_args);
                let def = InstanceDef::Closure(id.as_id());
                let body = self.fx.body_types(def, parent_args).map_err(told)?;
                if body.params.len() != arg_tys.len() + 1 {
                    // alloc: the error's message
                    return Err("closure called with a tuple of arguments".to_owned());
                }
                let self_ty = body.params[0].as_ref();
                let borrows = self_ty.as_reference().is_some();
                // alloc: `Tys`, inline up to eight values
                let mut values: Tys<'db> = smallvec![if borrows { self_ty } else { ty }];
                values.extend(arg_tys);
                self.emit_call(def, parent_args, &values, dst_ty)?;
                // A closure passed by value to a body that borrows it is the caller's to drop.
                if by_ref.is_none() && borrows {
                    self.drop_value(ty)?;
                }
                Ok(())
            }
            TyKind::Adt(..) | TyKind::Ref(..) => {
                let [by_shared, by_mut, _] = fn_trait_methods(interner.lang_items())
                    .into_iter()
                    .find(|family| family.contains(&Some(method)))
                    .expect("an Fn-trait method");
                let (method, self_ty) = match by_ref {
                    None => (Some(method), ty),
                    Some((ref_ty, Mutability::Not)) => (by_shared, ref_ty),
                    Some((ref_ty, Mutability::Mut)) => (by_mut, ref_ty),
                };
                let Some(method) = method else {
                    // alloc: the error's message
                    return Err(format!("Fn-trait call on {ty:?}"));
                };
                let args = GenericArgs::new_from_slice(&[ty.into(), tuple_ty.into()]);
                self.call_direct(method, args, &[self_ty, tuple_ty], dst_ty)
            }
            // alloc: the error's message
            _ => Err(format!("Fn-trait call on {ty:?}")),
        }
    }

    /// `inline_asm`: the function an `asm!` block is assembled as, by its text, and what a
    /// `#[naked]` function's or `throw_exception`'s `sym` operands name.
    fn inline_asm(&mut self, block: &InlineAsm) -> Result<(), String> {
        use hir_ty_mir::mir::AsmOperand;
        let db = self.db();
        let seh = self.fx.is_seh_throw_info(block);
        let mut operands = Vec::with_capacity(block.operands.len());
        for operand in &block.operands {
            match operand {
                AsmOperand::Sym(_) if !block.naked && !seh => {
                    // alloc: the error's message
                    return Err("`asm!` with a `sym`".to_owned());
                }
                AsmOperand::Sym(Operand { kind: OperandKind::Static(id), .. }) => {
                    if is_thread_local(db, *id) {
                        // alloc: the error's message
                        return Err("inline assembly with a `sym` that is thread local".to_owned());
                    }
                    self.static_(*id)?;
                }
                AsmOperand::Sym(function) => {
                    let ty = self.operand_ty(function);
                    self.recorded(|fx| fx.fn_item(ty))?;
                }
                AsmOperand::Reg { .. } | AsmOperand::Const(_) | AsmOperand::Label => (),
            }
            operands.push(match operand {
                AsmOperand::Sym(_) => asm::Operand::Sym,
                AsmOperand::Reg { reg, input, output, written } => {
                    let mut size = None;
                    let tys = [
                        input.as_ref().map(|it| self.operand_ty(it)),
                        output.as_ref().map(|it| self.place_ty(it).ty),
                    ];
                    for ty in tys.into_iter().flatten() {
                        let bytes = self.fx.layout(ty).map_err(told)?.size.bytes() as u32;
                        if size.is_some_and(|it| it != bytes) {
                            return Err(
                                "an `asm!` register that goes in and comes out in two sizes"
                                    .to_owned(), // alloc: the error's message
                            );
                        }
                        size = Some(bytes);
                    }
                    asm::Operand::Reg {
                        reg,
                        size,
                        input: input.is_some(),
                        output: output.is_some(),
                        written: *written,
                    }
                }
                AsmOperand::Const(value) => {
                    let ty = self.operand_ty(value);
                    let bits = match &value.kind {
                        OperandKind::Allocation { allocation }
                            if matches!(ty.kind(), TyKind::Int(_) | TyKind::Uint(_)) =>
                        {
                            let mut bytes = [0; 16];
                            let memory = &allocation.as_ref().memory;
                            bytes[..memory.len()].copy_from_slice(memory);
                            Some(u128::from_le_bytes(bytes))
                        }
                        OperandKind::Constant { konst, .. }
                            if matches!(ty.kind(), TyKind::Int(_) | TyKind::Uint(_))
                                && let ConstKind::Alias(_, named) = konst.as_ref().kind() =>
                        {
                            let mut bytes = [0; 16];
                            let memory = &self.fx.eval_const(named).map_err(told)?.memory;
                            bytes[..memory.len()].copy_from_slice(memory);
                            Some(u128::from_le_bytes(bytes))
                        }
                        _ => self.fx.const_bits(value),
                    };
                    let Some(bits) = bits else {
                        // alloc: the error's message
                        return Err("an `asm!` constant that is not a literal".to_owned());
                    };
                    let unused = 128 - self.fx.layout(ty).map_err(told)?.size.bits() as u32;
                    asm::Operand::Const(match ty.kind() {
                        TyKind::Int(_) => (bits << unused) as i128 >> unused,
                        _ => bits as i128,
                    })
                }
                AsmOperand::Label => asm::Operand::Label,
            });
        }
        if seh {
            // `seh_throw_info`: the image's base, which the offsets it writes are from.
            return self.symbol("GetModuleHandleW");
        }
        // A `#[naked]` function is its assembly, which names its `sym`s.
        if block.naked {
            return Ok(());
        }
        let target = self.fx.cg.target;
        let features = self.fx.body.and_then(|it| it.owner.as_function());
        let features = features.map(|func| AttrFlags::target_features(db, func));
        let extensions =
            asm::extensions(target.arch, features.into_iter().flatten().map(|it| it.as_str()));
        let mut wrapper = asm::wrapper(target, &block.template, block.options, &operands)?;
        wrapper.text.insert_str(0, &extensions);
        for (operand, offset) in block.operands.iter().zip(&wrapper.offsets) {
            if let (AsmOperand::Reg { input: Some(input), .. }, Some(_)) = (operand, offset) {
                self.operand(input)?;
            }
        }
        self.assembly(wrapper.text)
    }

    /// `start_shim`: the C `main` hands `main` to `lang_start`.
    fn start_shim(&mut self, main: FunctionId) -> Result<(), String> {
        let interner = self.fx.interner;
        let Some(start) = interner.lang_items().Start else {
            let krate = main.krate(self.db()).named(self.db());
            // alloc: the error's message
            return Err(format!(
                "the C `main` of crate `{krate}`: no `start` lang item (std's `lang_start`) in \
                 the crates `{krate}` loads, which its `main` is handed to"
            ));
        };
        let fn_ty = |func: FunctionId, args| {
            Ty::new_fn_def(interner, CallableDefId::FunctionId(func).into(), args)
        };
        let sig_of =
            |ty: Ty<'db>| interner.instantiate_bound_regions_with_erased(ty.fn_sig(interner));
        let main_ty = fn_ty(main, GenericArgs::new_from_slice(&[]));
        let returned = self.fx.normalize(sig_of(main_ty).output());
        let start_args = GenericArgs::new_from_slice(&[returned.into()]);
        let start_sig = sig_of(fn_ty(start, start_args));
        self.recorded(|fx| fx.fn_item(main_ty))?;
        let inputs: Tys<'db> =
            // alloc: into `Tys`, inline up to eight; once per binary's `main`
            start_sig.inputs().as_slice().iter().map(|&ty| self.fx.normalize(ty)).collect();
        self.call_direct(start, start_args, &inputs, start_sig.output())
    }
}
