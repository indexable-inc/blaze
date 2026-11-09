//! What the whole program does with trait objects: for each object type (a `dyn` type's
//! principal trait), the concrete types whose vtables for it the program makes, and whether
//! that list is all there can be. The code generator learns it from the walk of everything a
//! final binary reaches and gives it here ([`give`]); `hir-ty-mir`'s `Devirtualize` pass reads
//! it by its content ID, which is in the [`super::MirOptions`] of the body
//! (docs/v2/plan/design/devirt.md).
//!
//! The facts are keyed as the code generator keys a vtable: the principal trait reference with
//! every free region erased, bound regions anonymized and projections normalized
//! (`hir-ty-codegen` `plain_trait`), Self left out. Auto traits and projection bounds of the
//! `dyn` type are not part of a vtable and not part of the key: a pass that reads a key checks
//! a target against the projection bounds of its own `dyn` type.

use std::{
    alloc::Global,
    fmt,
    sync::{Arc, Weak},
};

use blaze_core::lock::Lock;
use blaze_ids::{ContentId, Hasher};
use hir_def::TraitId;
use rustc_type_ir::{
    Binder, RegionKind, TypeFoldable, TypeFolder, TypeSuperFoldable, inherent::IntoKind as _,
};

use crate::next_solver::{
    DbInterner, GenericArgs, ParamEnv, Region, RegionExt, StoredGenericArgs, StoredTy, TraitRef,
    Ty,
    infer::{InferCtxt, traits::ObligationCause},
    obligation_ctxt::ObligationCtxt,
};

/// The content ID of a program's [`World`]: what an optimized body is devirtualized against,
/// in its [`super::MirOptions`]. Equal IDs are equal facts.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, blaze_query::Value)]
pub struct DevirtWorld(pub [u8; 32]);

impl fmt::Debug for DevirtWorld {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DevirtWorld(")?;
        for byte in &self.0[..6] {
            write!(f, "{byte:02x}")?;
        }
        write!(f, ")")
    }
}

/// An object type, as the code generator keys its vtables: the principal trait and its
/// arguments after Self, regions erased ([`object_key`]).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ObjectKey {
    pub trait_: TraitId,
    pub args: StoredGenericArgs,
}

/// A concrete type whose vtable for an object type the program makes.
#[derive(Clone, Debug)]
pub struct Target {
    pub ty: StoredTy,
    /// The type by content (the code generator's `Hashes::ty`): what targets are ordered by
    /// and what the object's digest holds, never the interned row.
    pub id: ContentId,
    /// How often a call through the object reached this type in a profile, where one is
    /// given: what orders the compares, hottest first.
    pub count: Option<u64>,
}

/// What the program does with one object type.
#[derive(Clone, Debug)]
pub struct Object {
    pub key: ObjectKey,
    /// The object type as written in messages: `dyn path::Trait<..>`.
    pub name: Box<str>,
    /// Every type the program makes a vtable of for it, in [`Target::id`] order.
    pub targets: Box<[Target]>,
    /// Why `targets` may not be all: something outside the program can make an object of it,
    /// or the program reaches its vtables in a way that does not keep the pointer. `None`:
    /// the list is closed.
    pub open: Option<Box<str>>,
    /// The content of the facts above ([`object_digest`]): what a body that read them is keyed
    /// by (rule 21).
    pub digest: ContentId,
}

/// What a whole program does with trait objects.
pub struct World {
    pub id: DevirtWorld,
    /// The program, for messages: `the executable of crate foo`.
    pub program: Box<str>,
    objects: Box<[Object]>,
    index: blaze_hash::HashMap<ObjectKey, u32, Global>,
}

pub const OBJECT_DOMAIN: &str = "blaze v2 devirt object 1";
pub const SITE_DOMAIN: &str = "blaze v2 devirt site 1";
pub const WORLD_DOMAIN: &str = "blaze v2 devirt world 1";
/// What a body records for a site whose object the program never makes: no vtable of it
/// exists, so no call through it can run, and the call is left as it is.
pub const ABSENT: ContentId = ContentId([0; 32]);

/// The digest of an object's facts: its key by content (`trait_id`, `args_id`, from the
/// code generator's content hashes), its targets in order with their counts, and why it is
/// open.
pub fn object_digest(
    trait_id: &ContentId,
    args_id: &ContentId,
    targets: &[Target],
    open: Option<&str>,
) -> ContentId {
    let mut hasher = Hasher::new(OBJECT_DOMAIN);
    hasher.id(trait_id).id(args_id).update(&(targets.len() as u64).to_le_bytes());
    for target in targets {
        hasher.id(&target.id);
        match target.count {
            Some(count) => hasher.update(&[1]).update(&count.to_le_bytes()),
            None => hasher.update(&[0]),
        };
    }
    hasher.opt_part(open);
    hasher.finish()
}

/// The digest of a site that read `object`'s facts and found its targets as `found` says,
/// one byte a target in the object's order (`hir-ty-mir`'s `Found`): what the body's crate
/// sees of each target's impl, which a body's code hash does not cover (rule 21).
pub fn site_digest(object: &ContentId, found: &[u8]) -> ContentId {
    let mut hasher = Hasher::new(SITE_DOMAIN);
    hasher.id(object).part(found);
    hasher.finish()
}

impl World {
    /// The world of `objects`, each once, which the caller has sorted by digest so that the
    /// ID is the same for the same facts however they were found. Two objects of one key are
    /// a refusal naming the key.
    pub fn new(program: Box<str>, objects: Vec<Object>) -> Result<World, String> {
        let mut index = blaze_hash::HashMap::with_capacity_in(objects.len(), Global);
        let mut hasher = Hasher::new(WORLD_DOMAIN);
        hasher.part(program.as_bytes()).update(&(objects.len() as u64).to_le_bytes());
        for (at, object) in objects.iter().enumerate() {
            if index.insert(object.key, at as u32).is_some() {
                // alloc: the error's message
                return Err(format!("the program's dyn facts list {} twice", object.name));
            }
            hasher.id(&object.digest);
        }
        let id = DevirtWorld(hasher.finish().0);
        Ok(World { id, program, objects: objects.into_boxed_slice(), index })
    }

    /// The facts of the objects of `key`; `None` where the program makes no vtable of it.
    pub fn object(&self, key: &ObjectKey) -> Option<&Object> {
        self.index.get(key).map(|&at| &self.objects[at as usize])
    }

    pub fn objects(&self) -> &[Object] {
        &self.objects
    }
}

impl fmt::Debug for World {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("World")
            .field("id", &self.id)
            .field("program", &self.program)
            .field("objects", &self.objects.len())
            .finish()
    }
}

/// The worlds given and still held, by ID: a handful a process (one a final binary being
/// built), so a list.
static GIVEN: Lock<Vec<(DevirtWorld, Weak<World>)>> = Lock::new("ty.devirt.given", Vec::new());

/// A world [`given`] hands out while this is held: the code generator holds it for as long as
/// it builds bodies against it.
#[derive(Clone, Debug)]
pub struct Held(Arc<World>);

impl Held {
    pub fn world(&self) -> &World {
        &self.0
    }
}

/// Makes `world` what [`given`] answers for its ID. A world given again (another build of the
/// same program) is the one held already: equal IDs are equal facts. The ID is of the facts'
/// content, while a world holds them as one database's interned rows (its keys' traits, its
/// targets' types): a world of the same ID whose rows differ (a second database that interned
/// the same program otherwise) is refused, naming the program and the first object that
/// differs, never handed the other's rows (rule 21).
pub fn give(world: World) -> Result<Held, String> {
    let held = {
        let mut given = GIVEN.lock();
        given.retain(|(_, it)| it.strong_count() > 0);
        match given.iter().find(|(id, _)| *id == world.id).and_then(|(_, it)| it.upgrade()) {
            Some(held) => held,
            None => {
                let held = Arc::new(world);
                given.push((held.id, Arc::downgrade(&held)));
                return Ok(Held(held));
            }
        }
    };
    let same = |a: &Object, b: &Object| {
        a.key == b.key
            && a.targets.len() == b.targets.len()
            && a.targets.iter().zip(&b.targets).all(|(a, b)| a.ty == b.ty)
    };
    let differs = if held.objects.len() != world.objects.len() {
        Some("another number of objects")
    } else {
        held.objects.iter().zip(&world.objects).find(|(a, b)| !same(a, b)).map(|(_, it)| &*it.name)
    };
    match differs {
        None => Ok(Held(held)),
        // alloc: the error's message
        Some(what) => Err(format!(
            "the dyn facts {:?} of {} are given again with other interned rows behind the same \
             content (first at {what}): a world's ID must cover what it holds",
            world.id, world.program,
        )),
    }
}

/// The world of `id`, while something holds it. `None` is an error of the asker: an options
/// that names a world nobody holds.
pub fn given(id: DevirtWorld) -> Option<Arc<World>> {
    let given = GIVEN.lock();
    given.iter().find(|(it, _)| *it == id).and_then(|(_, it)| it.upgrade())
}

/// The key of the vtables of `principal` (any Self): its trait and the arguments after Self,
/// with free regions erased, bound ones anonymized and projections normalized, as the code
/// generator's `plain_trait` keys a vtable. Both sides key through this one function, so a
/// vtable the walk lists and a call the pass reads are keyed alike.
pub fn object_key<'db>(
    infcx: &InferCtxt<'db>,
    env: ParamEnv<'db>,
    principal: TraitRef<'db>,
) -> ObjectKey {
    let interner = infcx.interner;
    let erased = principal.fold_with(&mut LifetimeEraser { interner });
    let plain = ObligationCtxt::new(infcx)
        .deeply_normalize(&ObligationCause::dummy(), env, erased)
        .unwrap_or(erased);
    let after_self = GenericArgs::new_from_slice(&plain.args.as_slice()[1..]);
    ObjectKey { trait_: plain.def_id.0, args: after_self.store() }
}

/// The principal of `dyn_ty` with Self `self_ty`, its outer bound regions erased: what the
/// code generator makes a vtable of when `self_ty` becomes a `dyn_ty` (`principal`).
pub fn principal_of<'db>(
    interner: DbInterner<'db>,
    dyn_ty: Ty<'db>,
    self_ty: Ty<'db>,
) -> Option<TraitRef<'db>> {
    use rustc_type_ir::inherent::BoundExistentialPredicates as _;
    let crate::next_solver::TyKind::Dynamic(bounds, _) = dyn_ty.kind() else { return None };
    let principal = bounds.principal()?.with_self_ty(interner, self_ty);
    Some(interner.instantiate_bound_regions_with_erased(principal))
}

/// `hir-ty-codegen` `vtable::without_lifetimes`, which keys vtables: free regions erased,
/// bound ones kept with their binders anonymized.
struct LifetimeEraser<'db> {
    interner: DbInterner<'db>,
}

impl<'db> TypeFolder<DbInterner<'db>> for LifetimeEraser<'db> {
    fn cx(&self) -> DbInterner<'db> {
        self.interner
    }

    fn fold_binder<T: TypeFoldable<DbInterner<'db>>>(
        &mut self,
        binder: Binder<DbInterner<'db>, T>,
    ) -> Binder<DbInterner<'db>, T> {
        use rustc_type_ir::Interner as _;
        self.interner.anonymize_bound_vars(binder).super_fold_with(self)
    }

    fn fold_region(&mut self, region: Region<'db>) -> Region<'db> {
        match region.kind() {
            RegionKind::ReBound(..) => region,
            _ => Region::new_erased(self.interner),
        }
    }
}
