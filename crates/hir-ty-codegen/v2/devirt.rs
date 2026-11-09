//! The whole program's trait objects, for `hir-ty-mir`'s `Devirtualize`
//! (docs/v2/plan/design/devirt.md). Each walked body lists what it does with trait
//! objects ([`Facts`], found by [`super::refs`] beside its references); once the walk of a
//! final binary has reached everything, [`State::closed`] makes the program's [`World`] of
//! them, and the walk runs again (`blaze_codegen::driver`) with the bodies that call through a
//! `dyn` built against it, whose new references (the direct callees) it then lists.
//!
//! What makes an object's list of types closed: the program is an executable or a test (a
//! `cdylib`'s every object is open: a program that loads it may make more), no code of a
//! crate other than the library's names a dynamic loader (`dlopen`, `LoadLibrary`), no
//! signature at the program's edge (a foreign function, a definition with another ABI than
//! Rust's, or a static of an `extern` block) carries the object type, and every place that
//! makes a vtable is visited: the walk's IR resolution makes each vtable the code of any
//! reached instance or static can point to (`FunctionCx::vtable`), so its requests are all of
//! them. An upcast hands the supertrait's object the subtrait's types, which may point to the
//! subtrait's vtable (`Upcast::Same`), so the supertrait's list takes the subtrait's, and its
//! openness.

use std::alloc::Global;

use base_db::{Crate, Db};
use blaze_codegen::{CodegenError, InstanceKey, Owner, backend::FrontRef, plan::Origin};
use blaze_ids::{ContentId, Hasher};
use blaze_items::{ItemId, ItemKey};
use hir_def::{CallableDefId, ItemContainerId, TraitId, signatures::TraitSignature};
use hir_ty::{
    next_solver::{
        GenericArgs, ParamEnv, StoredGenericArgs, StoredTy, Ty, TyKind, infer::InferCtxt,
    },
    traits::StoredParamEnvAndCrate,
};
use hir_ty_mir::mir::{
    MirBody, TerminatorKind,
    devirt::{self, DevirtWorld, Held, Object, ObjectKey, Target, World},
};
use blaze_core::lock::Lock;
use rustc_type_ir::inherent::IntoKind as _;

use super::{Hashes as _, HirFrontend, HirInstance, HirStatic, Kind, RootKind, build::Build, refs};
use crate::{Instance, InstanceDef};

const ARGS_DOMAIN: &str = "blaze v2 devirt object args 1";
const CODE_DOMAIN: &str = "blaze v2 devirt code 1";

/// The symbols that load code at run time: a program that calls one may get objects whose
/// vtables it never made.
const LOADERS: &[&str] =
    &["dlopen", "dlmopen", "LoadLibraryA", "LoadLibraryW", "LoadLibraryExA", "LoadLibraryExW"];

/// What one walked body does with trait objects, each once, in the order found.
#[derive(PartialEq, Eq, Debug, Default)]
pub(crate) struct Facts {
    /// The vtables its IR makes: the type and the object type.
    pub(crate) vtables: Vec<(StoredTy, ObjectKey)>,
    /// The upcasts its IR makes, of the first object type to the second.
    pub(crate) upcasts: Vec<(ObjectKey, ObjectKey)>,
    /// The object types a signature at the program's edge carries, or the type of a static
    /// of an `extern` block it reads: code outside the program may make or take them.
    pub(crate) foreign: Vec<ObjectKey>,
}

impl Facts {
    /// A vtable request of the IR ([`crate::fragment::DataRequest::Vtable`]), whose trait
    /// reference holds Self first.
    pub(crate) fn vtable(&mut self, ty: &StoredTy, trait_: TraitId, args: &StoredGenericArgs) {
        let after_self = GenericArgs::new_from_slice(&args.as_ref().as_slice()[1..]);
        let key = ObjectKey { trait_, args: after_self.store() };
        if !self.vtables.iter().any(|(theirs, their_key)| theirs == ty && *their_key == key) {
            // clone: a reference count, once per vtable a body makes
            self.vtables.push((ty.clone(), key));
        }
    }

    /// An upcast of a `from` object to a `to` one, which both have a principal: the walk's
    /// `unsize_metadata`.
    pub(crate) fn upcast<'db>(
        &mut self,
        infcx: &InferCtxt<'db>,
        env: ParamEnv<'db>,
        (from, to): (Ty<'db>, Ty<'db>),
    ) {
        let interner = infcx.interner;
        let key = |ty| {
            devirt::principal_of(interner, ty, ty).map(|it| devirt::object_key(infcx, env, it))
        };
        let (Some(from), Some(to)) = (key(from), key(to)) else { return };
        if from != to && !self.upcasts.contains(&(from, to)) {
            self.upcasts.push((from, to));
        }
    }

    /// The object types `ty` names, as a signature at the program's edge carries them.
    pub(crate) fn foreign<'db>(&mut self, infcx: &InferCtxt<'db>, env: ParamEnv<'db>, ty: Ty<'db>) {
        let interner = infcx.interner;
        for inner in ty.walk().filter_map(|it| it.ty()) {
            if !matches!(inner.kind(), TyKind::Dynamic(..)) {
                continue;
            }
            let Some(principal) = devirt::principal_of(interner, inner, inner) else { continue };
            let key = devirt::object_key(infcx, env, principal);
            if !self.foreign.contains(&key) {
                self.foreign.push(key);
            }
        }
    }
}

/// The walk of an instance's body with the program's world (`refs::Walking`): `world` where
/// the body takes it ([`takes_world`]); else the walk without one, so a body with nothing to
/// devirtualize is walked and keyed as it is in every program.
pub(crate) fn walking(
    db: &Db,
    world: Option<DevirtWorld>,
    instance: &HirInstance,
    inlined: bool,
    owner: impl FnOnce() -> Owner,
) -> Result<refs::Walking, String> {
    let plain = refs::Walking::plain(inlined);
    let Some(world) = world else { return Ok(plain) };
    match takes_world(db, instance, inlined, owner)? {
        true => Ok(refs::Walking { inlined, world: Some(world) }),
        false => Ok(plain),
    }
}

/// Whether a body of `instance` is built against the program's world, the one rule for it:
/// both what a walk takes ([`walking`]) and whether a program has a world to build at all
/// ([`world_of`]). It is so when `inlined` (only a body at MIR level 2 with the inliner on is
/// devirtualized), the instance is a function or closure of a crate other than the library's
/// (whose bodies are one for every program), it is the workspace's (`owner`, asked only
/// then: the dependency image's code is one for every program its dependencies build, and
/// built against the world a call through a `dyn` made direct to the program's one type,
/// hashbrown's `resize_inner` calling the `reserve_rehash` closure of a map keyed by a
/// member's type, would refer to the workspace's code from the image; a build that links no
/// image owns every crate but the library's), and its body calls through a `dyn`
/// ([`has_sites`]).
pub(crate) fn takes_world(
    db: &Db,
    instance: &HirInstance,
    inlined: bool,
    owner: impl FnOnce() -> Owner,
) -> Result<bool, String> {
    let Kind::Code(code, env) = &instance.0 else { return Ok(false) };
    let ours = inlined
        && matches!(code.def, InstanceDef::Fn(_) | InstanceDef::Closure(_))
        && super::frontend::crate_of(db, &instance.0)
            .is_some_and(|it| !it.data(db).origin.is_lang());
    if !ours || owner() != Owner::Workspace {
        return Ok(false);
    }
    // clone: the query's keys by value, each a few `Copy` ids in a `Clone`-only type
    has_sites(db, code.clone(), env.clone()).clone()
}

/// Whether the body of `instance` as any of [`Build::WALKS_INLINED`] builds it calls a trait
/// method through a `dyn`: what `Devirtualize` looks at.
#[blaze_query::tracked]
pub(crate) fn has_sites(
    db: &Db,
    instance: Instance,
    env: StoredParamEnvAndCrate,
) -> Result<bool, String> {
    for build in Build::WALKS_INLINED {
        // clone: the query's keys by value, two `Copy` ids in a `Clone`-only type
        let body = crate::instance_body(db, &instance, env.clone(), Some(build))
            .map_err(|it| it.to_string())?;
        if sites(db, &body) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn sites(db: &Db, body: &MirBody<'_>) -> bool {
    body.basic_blocks.iter().any(|(_, block)| {
        let Some(TerminatorKind::Call { func, .. }) = block.terminator.as_ref().map(|it| &it.kind)
        else {
            return false;
        };
        let Some((CallableDefId::FunctionId(method), args)) = func.fn_def() else { return false };
        matches!(method.loc(db).container, ItemContainerId::TraitId(_))
            && args
                .as_slice()
                .first()
                .and_then(|it| it.ty())
                .is_some_and(|it| matches!(it.kind(), TyKind::Dynamic(..)))
    })
}

/// `code`, the code hash of an instance's body, with the facts its body read of the world
/// (`MirBody::inlined.devirtualized`, as [`refs::read`] lists them): the key of what was
/// built against them (rule 21). A body that read none keeps its code.
pub(crate) fn code(code: blaze_items::CodeHash, read: &[[u8; 32]]) -> blaze_items::CodeHash {
    if read.is_empty() {
        return code;
    }
    let mut hasher = Hasher::new(CODE_DOMAIN);
    hasher.id(&code.0).update(&(read.len() as u64).to_le_bytes());
    for digest in read {
        hasher.update(digest);
    }
    blaze_items::CodeHash(hasher.finish())
}

/// What a build's front end knows of the program it walks, for its world.
pub(crate) struct State(Lock<Program>);

impl Default for State {
    fn default() -> State {
        State(Lock::new("codegen.devirt.program", Program::default()))
    }
}

#[derive(Default)]
struct Program {
    /// What the unit makes, from its roots ([`HirFrontend::roots`]): `None` before.
    makes: Option<(RootKind, Crate)>,
    /// What the walk reached, each level's appended ([`blaze_codegen::backend::Frontend::reached`]).
    functions: Vec<(HirInstance, Origin)>,
    statics: Vec<HirStatic>,
    held: Option<Held>,
}

impl State {
    pub(crate) fn makes(&self, kind: RootKind, krate: Crate) {
        self.0.lock().makes = Some((kind, krate));
    }

    pub(crate) fn reached(&self, functions: &[(HirInstance, Origin)], statics: &[HirStatic]) {
        let mut program = self.0.lock();
        if matches!(
            program.makes,
            Some((RootKind::Executable | RootKind::Test | RootKind::Shared, _))
        ) {
            // clone: once per instance a final binary's walk reaches, its handle
            program.functions.extend_from_slice(functions);
            // clone: once per static a final binary's walk reaches, its handle
            program.statics.extend_from_slice(statics);
        }
    }

    /// The world the walk's bodies are built against: `None` before the first walk ends, and
    /// for a program that has none.
    pub(crate) fn world(&self) -> Option<DevirtWorld> {
        self.0.lock().held.as_ref().map(|it| it.world().id)
    }

    /// Called when a walk has reached everything: the program's world from what it reached,
    /// `true` when it is a new one the bodies are to be built against (the walk then runs
    /// again), `false` when there is none or it is the one the bodies were built against. A
    /// second walk that finds other facts than the first is an error naming the program: the
    /// bodies built against the first would be keyed by facts that are not the program's.
    pub(crate) fn closed(&self, frontend: &HirFrontend<'_>) -> Result<bool, CodegenError> {
        let (makes, functions, statics, held) = {
            let mut program = self.0.lock();
            let lists =
                (std::mem::take(&mut program.functions), std::mem::take(&mut program.statics));
            (program.makes, lists.0, lists.1, program.held.as_ref().map(|it| it.world().id))
        };
        let Some((kind, krate)) = makes.filter(|it| it.0 != RootKind::Library) else {
            return Ok(false);
        };
        let made = frontend.with(|db| {
            let program = program_name(db, kind, krate);
            let _span =
                tracing::info_span!(target: "blaze", "devirt world", program = &*program).entered();
            world_of(frontend, db, (kind, program), &functions, &statics)
                .map_err(|what| refused(db, krate, kind, what))
        })?;
        match (made, held) {
            (None, None) => Ok(false),
            (Some(world), None) => {
                let held = devirt::give(world).map_err(|what| frontend.with(|db| refused(db, krate, kind, what)))?;
                self.0.lock().held = Some(held);
                Ok(true)
            }
            (Some(world), Some(id)) if world.id == id => Ok(false),
            (made, Some(id)) => Err(frontend.with(|db| {
                // alloc: the error's message
                let found = made.map_or_else(|| "none".to_owned(), |it| format!("{:?}", it.id));
                refused(db, krate, kind, format!(
                    "the walk of the program against its dyn facts {id:?} found others ({found}); \
                     the facts may not change with what is built against them"
                ))
            })),
        }
    }
}

fn program_name(db: &Db, kind: RootKind, krate: Crate) -> Box<str> {
    let what = match kind {
        RootKind::Executable => "the executable",
        RootKind::Test => "the test",
        RootKind::Shared => "the cdylib",
        RootKind::Library => "the library",
    };
    // alloc: once a program, its name in messages
    format!("{what} of crate `{}`", krate.named(db)).into_boxed_str()
}

fn refused(db: &Db, krate: Crate, kind: RootKind, what: impl std::fmt::Display) -> CodegenError {
    CodegenError {
        unit: InstanceKey(ContentId::default()),
        item: ItemKey(ContentId::default()),
        // alloc: the error's message
        what: format!("the dyn facts of {}: {what}", program_name(db, kind, krate)),
        front: None,
    }
}

/// One object type's types and why its list is open, while the world is made.
#[derive(Default)]
struct Found {
    tys: Vec<StoredTy>,
    open: Option<String>,
}

/// The program's world from what its walk reached; `None` where no reached body calls through
/// a `dyn`, so nothing is built against one.
fn world_of(
    frontend: &HirFrontend<'_>,
    db: &Db,
    (kind, program): (RootKind, Box<str>),
    functions: &[(HirInstance, Origin)],
    statics: &[HirStatic],
) -> Result<Option<World>, String> {
    let mut objects: blaze_hash::HashMap<ObjectKey, Found, Global> =
        blaze_hash::HashMap::new_in(Global);
    // alloc: once a program, the upcasts its bodies make
    let mut upcasts = Vec::new();
    let mut loads = None;
    let mut sites = false;
    let mut seen = blaze_hash::HashSet::new_in(Global);
    for (instance, requester) in functions {
        if !seen.insert(instance) {
            continue;
        }
        let inlined = frontend.inlines_for(db, instance, requester)?;
        sites |= takes_world(db, instance, inlined, || frontend.owner(db, &instance.0))?;
        let Some(facts) = refs::facts(db, instance, refs::Walking::plain(inlined))? else {
            continue;
        };
        for (ty, key) in &facts.vtables {
            // clone: a reference count, once per vtable the program makes
            objects.entry(*key).or_default().tys.push(ty.clone());
        }
        upcasts.extend_from_slice(&facts.upcasts);
        for key in &facts.foreign {
            let found = objects.entry(*key).or_default();
            if found.open.is_none() {
                // alloc: the reason, once an object
                found.open = Some(format!(
                    "{} carries it across the program's edge",
                    super::Named(db, instance)
                ));
            }
        }
        let ours = super::frontend::crate_of(db, &instance.0)
            .is_some_and(|it| !it.data(db).origin.is_lang());
        if ours && loads.is_none() {
            for it in refs::listed(db, instance, refs::Walking::plain(inlined))?.iter() {
                if let FrontRef::Extern(name) = it
                    && LOADERS.contains(&name.as_str())
                {
                    // alloc: the reason, once a program
                    loads = Some(format!(
                        "{} loads code at run time (`{name}`)",
                        super::Named(db, instance)
                    ));
                }
            }
        }
    }
    if !sites {
        return Ok(None);
    }
    for value in statics {
        // An `extern_weak` static holds a foreign symbol's address: no vtable of its value.
        let super::statics::Source::Compiled(compiled) = super::statics::source(db, value.0)?
        else {
            continue;
        };
        let mut facts = Facts::default();
        for request in &compiled.requests.datas {
            if let crate::fragment::DataRequest::Vtable {
                ty,
                trait_ref: Some((trait_, args)),
                ..
            } = request
            {
                facts.vtable(ty, *trait_, args);
            }
        }
        for (ty, key) in facts.vtables {
            objects.entry(key).or_default().tys.push(ty);
        }
    }
    // An upcast object holds the types of the one it is upcast from: propagated to a fixpoint,
    // each round adding at least one type or reason, so at most one round more than there
    // are objects times their types.
    let bound = objects.values().map(|it| it.tys.len() + 1).sum::<usize>() + upcasts.len() + 1;
    let mut rounds = 0;
    loop {
        let mut changed = false;
        for (from, to) in &upcasts {
            let (tys, open) = match objects.get(from) {
                // clone: a few reference counts, once an upcast a round
                Some(found) => (found.tys.clone(), found.open.clone()),
                None => continue,
            };
            let into = objects.entry(*to).or_default();
            for ty in tys {
                if !into.tys.contains(&ty) {
                    into.tys.push(ty);
                    changed = true;
                }
            }
            if into.open.is_none()
                && let Some(open) = open
            {
                into.open = Some(open);
                changed = true;
            }
        }
        rounds += 1;
        if !changed {
            break;
        }
        if rounds > bound {
            // alloc: the error's message
            return Err(format!("the upcasts of the program did not settle in {bound} rounds"));
        }
    }
    let everything = match kind {
        RootKind::Shared => Some(
            "the program is a cdylib: a program that loads it may make objects of its traits"
                .to_owned(),
        ),
        _ => loads,
    };
    // alloc: once a program, its objects
    let mut made = Vec::with_capacity(objects.len());
    for (key, found) in objects {
        made.push(object(frontend, db, key, found, everything.as_deref())?);
    }
    made.sort_unstable_by(|a, b| a.digest.cmp(&b.digest));
    World::new(program, made).map(Some)
}

/// The facts of one object type: its types by content, in order, each once.
fn object(
    frontend: &HirFrontend<'_>,
    db: &Db,
    key: ObjectKey,
    found: Found,
    everything: Option<&str>,
) -> Result<Object, String> {
    let hashes = &frontend.hashes;
    let trait_id = hashes.item(db, ItemId::Trait(key.trait_))?.0;
    let mut args = Hasher::new(ARGS_DOMAIN);
    let after_self = key.args.as_ref();
    args.update(&(after_self.len() as u64).to_le_bytes());
    for arg in after_self.iter() {
        match (arg.ty(), arg.konst()) {
            (Some(ty), _) => args.update(&[0]).id(&hashes.ty(db, ty)?),
            (None, Some(konst)) => args.update(&[1]).id(&hashes.konst(db, konst)?),
            // A region, which the key has erased.
            (None, None) => args.update(&[2]),
        };
    }
    let args_id = args.finish();
    // alloc: once an object, its types
    let mut targets = Vec::with_capacity(found.tys.len());
    for ty in found.tys {
        let id = hashes.ty(db, ty.as_ref())?;
        targets.push(Target { ty, id, count: None });
    }
    targets.sort_unstable_by(|a, b| a.id.cmp(&b.id));
    let mut kept: Vec<Target> = Vec::with_capacity(targets.len()); // alloc: as above
    for target in targets {
        match kept.last() {
            Some(last) if last.id == target.id && last.ty == target.ty => {}
            Some(last) if last.id == target.id => {
                // alloc: the error's message
                return Err(blaze_query::db::attach(db, || {
                    format!("{:?} and {:?} have one content ID {:?}", last.ty, target.ty, target.id)
                }));
            }
            _ => kept.push(target),
        }
    }
    let open = found.open.as_deref().or(everything);
    let digest = devirt::object_digest(&trait_id, &args_id, &kept, open);
    let name = blaze_query::db::attach(db, || {
        let trait_name = TraitSignature::of(db, key.trait_).name.as_str().to_owned();
        match after_self.is_empty() {
            true => format!("dyn {trait_name}"),
            false => format!("dyn {trait_name}{:?}", after_self),
        }
    });
    Ok(Object {
        key,
        name: name.into_boxed_str(),
        targets: kept.into_boxed_slice(),
        open: open.map(Into::into),
        digest,
    })
}
