//! Which types are linear, and which hold a linear value.
//!
//! A type is linear itself when it is a struct, enum or union marked `#[blaze::linear]`, a type
//! parameter or a trait's `Self` marked so, a projection of an associated type marked so, a
//! `dyn` of a trait marked so, or an opaque type whose hidden type is one. It holds a linear
//! value when it is one or a part of it does: a field of a struct, a variant's field, a tuple's
//! or an array's element, a closure's, coroutine's or `async` block's capture, a `Box`'s
//! contents. A reference, a raw pointer, `ManuallyDrop` and `PhantomData` hold none: dropping
//! them drops nothing they point at.

use base_db::Db;
use blaze_mem::Frame;
use hir_def::{AdtId, AttrDefId, HasModule, VariantId, attrs::AttrFlags, signatures::StructFlags};
use hir_ty::{
    FrameMap, FrameSet,
    consteval::try_const_usize,
    db::HirDatabase,
    frame_map, frame_set,
    layout::{enum_variant_ids, struct_flags},
    next_solver::{AliasTy, AssocTyId, DbInterner, ParamEnv, TermKind, Ty, TyKind},
};
use hir_ty_mir::mir::MirBody;
use rustc_type_ir::{
    AliasTyKind, Interner as _,
    inherent::{BoundExistentialPredicates as _, GenericArgs as _, IntoKind},
};
use std::cell::RefCell;

use crate::BodyKind;

/// Whether `adt` is marked `#[blaze::linear]`.
pub fn is_linear_adt(db: &Db, adt: AdtId) -> bool {
    AttrFlags::query(db, AttrDefId::AdtId(adt)).contains(AttrFlags::LINEAR)
}

/// Whether a value of `ty` holds a linear value ([`self`]). Read off `ty` alone: a type
/// parameter is linear by its own mark, and an associated type by its trait's, never by what a
/// caller substitutes.
pub fn contains_linear<'db>(db: &'db Db, ty: Ty<'db>) -> bool {
    blaze_mem::frame::task(blaze_mem::Tag::Mir, |frame| {
        let interner = DbInterner::new_no_crate(db);
        Walk { db, interner, exempt: &[], visiting: frame_set(frame) }.holds(ty)
    })
}

/// A structural walk of a type, with the types a `#[blaze::consumes]` body takes by value
/// linear nowhere in it.
struct Walk<'a, 'f, 'db> {
    db: &'db Db,
    interner: DbInterner<'db>,
    exempt: &'a [Ty<'db>],
    /// The types being walked: a recursive type holds a linear value only through a part it
    /// reaches on its way back to itself, which the walk sees there.
    visiting: FrameSet<'f, Ty<'db>>,
}

impl<'db> Walk<'_, '_, 'db> {
    fn holds(&mut self, ty: Ty<'db>) -> bool {
        if is_marked(self.db, ty) && !self.exempt.contains(&ty) {
            return true;
        }
        if !self.visiting.insert(ty) {
            return false;
        }
        let held = parts(self.db, self.interner, ty, &mut |part| self.holds(part));
        self.visiting.remove(&ty);
        held
    }
}

/// Whether `ty` is linear itself (not by a part of it).
pub(crate) fn is_marked(db: &Db, ty: Ty<'_>) -> bool {
    match ty.kind() {
        TyKind::Adt(def, _) => is_linear_adt(db, def.def_id()),
        TyKind::Param(param) => {
            AttrFlags::query_type_or_const_param(db, param.id.into()).contains(AttrFlags::LINEAR)
        }
        TyKind::Alias(_, AliasTy { kind: AliasTyKind::Projection { def_id }, .. }) => {
            match def_id.0 {
                AssocTyId::TypeAliasId(alias) => {
                    AttrFlags::query(db, AttrDefId::TypeAliasId(alias)).contains(AttrFlags::LINEAR)
                }
                // A trait method's `impl Trait` has no attributes of its own.
                AssocTyId::RpititId(_) => false,
            }
        }
        TyKind::Dynamic(predicates, ..) => predicates.principal_def_id().is_some_and(|it| {
            AttrFlags::query(db, AttrDefId::TraitId(it.0)).contains(AttrFlags::LINEAR)
        }),
        _ => false,
    }
}

/// Calls `part` on each type a value of `ty` holds by value, the hidden type of an opaque one
/// included, until one returns true; whether one did.
fn parts<'db>(
    db: &'db Db,
    interner: DbInterner<'db>,
    ty: Ty<'db>,
    part: &mut dyn FnMut(Ty<'db>) -> bool, // dyn: one walk, recursive through `Walk::holds`
) -> bool {
    match ty.kind() {
        TyKind::Adt(def, args) => {
            if def.is_box() {
                return part(args.type_at(0));
            }
            let fields = |variant: VariantId, part: &mut dyn FnMut(Ty<'db>) -> bool| {
                db.field_types(variant)
                    .iter()
                    .any(|(_, field)| part(field.ty().instantiate(interner, args).skip_norm_wip()))
            };
            match def.def_id() {
                AdtId::StructId(id) => {
                    if struct_flags(db, id)
                        .intersects(StructFlags::IS_MANUALLY_DROP | StructFlags::IS_PHANTOM_DATA)
                    {
                        return false;
                    }
                    fields(id.into(), part)
                }
                // A union drops none of its fields.
                AdtId::UnionId(_) => false,
                AdtId::EnumId(id) => {
                    enum_variant_ids(db, id).iter().any(|&variant| fields(variant.into(), part))
                }
            }
        }
        TyKind::Tuple(tys) => tys.iter().any(part),
        TyKind::Array(element, len) => try_const_usize(db, len) != Some(0) && part(element),
        TyKind::Slice(element) | TyKind::Pat(element, _) => part(element),
        TyKind::Closure(_, args) => part(args.as_closure().tupled_upvars_ty()),
        TyKind::CoroutineClosure(_, args) => part(args.as_coroutine_closure().tupled_upvars_ty()),
        TyKind::Coroutine(_, args) => part(args.as_coroutine().tupled_upvars_ty()),
        TyKind::Alias(_, AliasTy { kind: AliasTyKind::Opaque { def_id }, args, .. }) => {
            part(interner.type_of(def_id.into()).instantiate(interner, args).skip_norm_wip())
        }
        TyKind::UnsafeBinder(inner) => part(inner.skip_binder()),
        _ => false,
    }
}

/// What the checks of one body ask of its types.
pub(crate) struct Types<'a, 'db> {
    pub(crate) db: &'db Db,
    pub(crate) interner: DbInterner<'db>,
    pub(crate) env: ParamEnv<'db>,
    /// The crate of the body.
    pub(crate) krate: base_db::Crate,
    pub(crate) frame: &'a Frame,
    /// The types a `#[blaze::consumes]` function takes by value: linear nowhere in its body.
    exempt: &'a [Ty<'db>],
    /// [`Types::holds`] of each type asked, which a body asks of the same types many times.
    held: RefCell<FrameMap<'a, Ty<'db>, bool>>,
    deep: RefCell<FrameMap<'a, Ty<'db>, bool>>,
}

impl<'a, 'db> Types<'a, 'db> {
    pub(crate) fn new(db: &'db Db, body: &MirBody<'db>, kind: BodyKind, frame: &'a Frame) -> Self {
        let krate = body.owner.krate(db);
        let interner = DbInterner::new_with(db, krate);
        let env = db.trait_environment(body.owner.generic_def(db));
        // With what the body's `for const` loops assume of their binders, as inference had it.
        let env = hir_ty::comptime_env::owner_param_env(db, body.owner, env);
        let mut exempt = Vec::new_in(frame);
        if let BodyKind::Item(hir_def::DefWithBodyId::FunctionId(func)) = kind
            && AttrFlags::query(db, AttrDefId::FunctionId(func)).contains(AttrFlags::CONSUMES)
        {
            exempt.extend(body.param_locals.iter().map(|&it| body.locals[it].ty.as_ref()));
        }
        Types {
            db,
            interner,
            env,
            krate,
            frame,
            exempt: exempt.leak(),
            held: RefCell::new(frame_map(frame)),
            deep: RefCell::new(frame_map(frame)),
        }
    }

    /// Whether `ty` is linear itself here.
    pub(crate) fn is_linear(&self, ty: Ty<'db>) -> bool {
        is_marked(self.db, ty) && !self.exempt.contains(&ty)
    }

    /// Whether a value of `ty` holds a linear value here.
    pub(crate) fn holds(&self, ty: Ty<'db>) -> bool {
        if let Some(&known) = self.held.borrow().get(&ty) {
            return known;
        }
        let visiting = frame_set(self.frame);
        let held =
            Walk { db: self.db, interner: self.interner, exempt: self.exempt, visiting }.holds(ty);
        self.held.borrow_mut().insert(ty, held);
        held
    }

    /// Calls `part` on each type a value of `ty` holds by value.
    pub(crate) fn parts(&self, ty: Ty<'db>, part: &mut dyn FnMut(Ty<'db>) -> bool) -> bool {
        parts(self.db, self.interner, ty, part)
    }

    /// Whether code generic over a parameter given `ty` may come to drop a linear value: `ty`
    /// holds one, or it is a function, a closure or a `dyn` whose signature or projections
    /// take or give one, which calling it hands that code (`<F as FnOnce<()>>::Output`), seen
    /// through references and raw pointers.
    pub(crate) fn reaches(&self, ty: Ty<'db>) -> bool {
        if let Some(&known) = self.deep.borrow().get(&ty) {
            return known;
        }
        let reaches = self.holds(ty) || self.signature_reaches(ty);
        self.deep.borrow_mut().insert(ty, reaches);
        reaches
    }

    fn signature_reaches(&self, ty: Ty<'db>) -> bool {
        let interner = self.interner;
        match ty.kind() {
            TyKind::Ref(_, inner, _) | TyKind::RawPtr(inner, _) => self.reaches(inner),
            TyKind::FnDef(callable, args) => {
                let sig = self.db.callable_item_signature(callable.0);
                let sig = sig.instantiate(interner, args.skip_binder()).skip_norm_wip();
                sig.skip_binder().inputs_and_output.iter().any(|it| self.holds(it))
            }
            TyKind::FnPtr(sig, _) => {
                sig.skip_binder().inputs_and_output.iter().any(|it| self.holds(it))
            }
            TyKind::Closure(_, args) => args
                .as_closure()
                .sig()
                .skip_binder()
                .inputs_and_output
                .iter()
                .any(|it| self.holds(it)),
            TyKind::Dynamic(predicates, ..) => predicates.projection_bounds().into_iter().any(
                |it| matches!(it.skip_binder().term.kind(), TermKind::Ty(it) if self.holds(it)),
            ),
            _ => false,
        }
    }
}
