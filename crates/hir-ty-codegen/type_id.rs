//! `TypeId`s as rustc computes them.
//!
//! A `TypeId` is rustc's stable hash of the type, and rustc's code compares it with ours, so
//! this follows `TyCtxt::type_id_hash` of the pinned toolchain input for input. `None` is a
//! type whose hash takes something this does not reproduce, never a guess.

use std::hash::Hasher;

use base_db::Crate;
use hir_def::{
    AdtId, CallableDefId, DefWithBodyId, EnumVariantId, FunctionId, HasModule, ImplId,
    ItemContainerId, Lookup, ModuleIdLt, TraitId, TypeAliasId, VariantId,
    attrs::AttrFlags,
    item_tree::FieldsShape,
    signatures::{
        EnumSignature, FunctionSignature, StructSignature, TraitFlags, TraitSignature,
        TypeAliasSignature, UnionSignature,
    },
    src::HasSource,
    visibility::{Visibility, visibility_from_ast},
};
use rustc_abi::{ExternAbi, Integer, IntegerType, ReprFlags};
use rustc_ast_ir::Mutability;
use rustc_stable_hash::{FromStableHash, SipHasher128Hash, StableSipHasher128};
use rustc_type_ir::{
    BoundVar, BoundVarIndexKind, ExistentialPredicate, FloatTy, GenericArgKind, IntTy, RegionKind,
    UintTy,
    inherent::{GenericArgs as _, IntoKind, Safety as _, Term as _},
};
use syntax::ast::HasVisibility;

use rustc_hash::FxHashSet;

use super::SymbolResolver;
use hir_ty::{
    InferBodyId,
    consteval::try_const_usize,
    db::{HirDatabase, InternedClosure},
    next_solver::{Const, ConstKind, GenericArgs, Region, TermId, Ty, TyKind, default_types},
};

/// rustc's `Fingerprint`, which is also the shape of a `DefPathHash`: the crate, then the item.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Fingerprint(pub(super) u64, pub(super) u64);

impl FromStableHash for Fingerprint {
    type Hash = SipHasher128Hash;

    fn from(SipHasher128Hash([a, b]): Self::Hash) -> Self {
        Fingerprint(a, b)
    }
}

/// rustc's `DefPathData`, as far as types are named through it.
#[derive(Clone, Copy)]
enum DefPathData<'a> {
    Impl,
    TypeNs(&'a str),
    ValueNs(&'a str),
    Closure,
    Ctor,
    AnonConst,
}

impl Fingerprint {
    fn crate_root(stable_crate_id: u64) -> Self {
        Fingerprint(stable_crate_id, stable_crate_id)
    }

    /// `DefKey::compute_stable_hash`, for a child no sibling shares its name with.
    fn child(self, data: DefPathData<'_>) -> Self {
        self.child_at(data, 0)
    }

    /// `DefKey::compute_stable_hash`, for the child that `earlier` siblings of the same name
    /// come before.
    fn child_at(self, data: DefPathData<'_>, earlier: u32) -> Self {
        let mut hasher = StableSipHasher128::new();
        hasher.write_u64(self.1);
        // The variant's index in rustc's `DefPathData`, where `TestBinderConstraints` comes
        // between `Closure` (9) and `Ctor`.
        let (discriminant, name) = match data {
            DefPathData::Impl => (1, None),
            DefPathData::TypeNs(name) => (5, Some(name)),
            DefPathData::ValueNs(name) => (6, Some(name)),
            DefPathData::Closure => (9, None),
            DefPathData::Ctor => (11, None),
            DefPathData::AnonConst => (12, None),
        };
        hasher.write_isize(discriminant);
        if let Some(name) = name {
            hasher.write(name.as_bytes());
            hasher.write_u8(0xff);
        }
        hasher.write_u32(earlier);
        let Fingerprint(local, _) = hasher.finish();
        Fingerprint(self.0, local)
    }

    fn hash(self, hasher: &mut StableSipHasher128) {
        hasher.write_u64(self.0);
        hasher.write_u64(self.1);
    }
}

fn hash_str(hasher: &mut StableSipHasher128, text: &str) {
    hasher.write_usize(text.len());
    hasher.write(text.as_bytes());
}

pub(super) struct TypeIdHasher<'a, 'db> {
    db: &'db dyn HirDatabase,
    symbols: &'a dyn SymbolResolver,
    /// The binders around what is being hashed, outermost first: for each, its bound
    /// lifetimes in the order they were met. rustc renumbers them that way before it hashes,
    /// so that `for<'a, 'b> fn(&'b u8, &'a u8)` and `for<'a, 'b> fn(&'a u8, &'b u8)` are one
    /// type.
    binders: Vec<Vec<BoundVar>>,
    /// How many function signatures what is being hashed is inside of. A lifetime left out of
    /// one (`fn(&u8)`, `dyn Fn(&u8)`) is bound by it, where rust-analyzer has no lifetime at
    /// all, so there only a lifetime it did bind is one this knows.
    signatures: usize,
    /// The crates whose code is ours.
    owned: &'a FxHashSet<Crate>,
    /// Whether a hash of our own will do: see [`Self::type_id`].
    ours: bool,
    /// Whether the type names something of a crate rustc compiled nothing of.
    unbuilt: bool,
    /// Whether the type names a closure of a crate whose code is ours, whose `TypeId` only
    /// code instantiated with it asks for: see [`Self::closure`].
    own_closure: bool,
    /// The innermost type that could not be hashed.
    pub(super) stuck: Option<String>,
}

impl<'a, 'db> TypeIdHasher<'a, 'db> {
    pub(super) fn new(
        db: &'db dyn HirDatabase,
        symbols: &'a dyn SymbolResolver,
        owned: &'a FxHashSet<Crate>,
    ) -> Self {
        Self {
            db,
            symbols,
            binders: Vec::new(),
            signatures: 0,
            owned,
            ours: false,
            unbuilt: false,
            own_closure: false,
            stuck: None,
        }
    }

    /// rustc's hash of the type, or one of our own for a type that names something of a crate
    /// rustc compiled nothing of, or a closure of a crate whose code is ours: only code
    /// instantiated with such a type can ask for its `TypeId`, and that code is all ours, so
    /// there is no hash of rustc's to agree with. Ours stands on what the type is written in
    /// rather than on where, so that it is the same in every build.
    pub(super) fn type_id(&mut self, ty: Ty<'db>) -> Option<u128> {
        let mut hasher = StableSipHasher128::new();
        if self.ty(&mut hasher, ty).is_none() {
            (self.ours, self.unbuilt, self.own_closure, self.stuck) = (true, false, false, None);
            self.binders.clear();
            self.signatures = 0;
            hasher = StableSipHasher128::new();
            hasher.write(b"blaze type");
            self.ty(&mut hasher, ty)?;
            if !self.unbuilt && !self.own_closure {
                return None;
            }
        }
        let Fingerprint(low, high) = hasher.finish();
        Some(u128::from(low) | u128::from(high) << 64)
    }

    fn crate_root(&mut self, krate: Crate) -> Option<Fingerprint> {
        self.unbuilt |= !self.symbols.rustc_built(krate);
        self.symbols.stable_crate_id(krate).map(Fingerprint::crate_root)
    }

    /// What a block is written in, told apart from the other blocks of it by where it is.
    fn block(&mut self, block: hir_def::BlockIdLt<'db>) -> Option<Fingerprint> {
        let db = self.db;
        let (owner, _) = hir_ty_mir::mir::type_name::block_owner(db, block)?;
        let (container, name, place) = match owner {
            DefWithBodyId::FunctionId(it) => {
                (it.lookup(db).container, FunctionSignature::of(db, it).name.clone(), 0)
            }
            DefWithBodyId::ConstId(it) => {
                let loc = it.lookup(db);
                match hir_def::signatures::ConstSignature::of(db, it).name.clone() {
                    Some(name) => (loc.container, name, 0),
                    // One `const _` of many: by where it is.
                    None => (
                        loc.container,
                        hir_expand::name::Name::new_symbol_root(intern::sym::underscore),
                        loc.id.value.erase().into_raw(),
                    ),
                }
            }
            DefWithBodyId::StaticId(it) => (
                it.lookup(db).container,
                hir_def::signatures::StaticSignature::of(db, it).name.clone(),
                0,
            ),
            _ => return None,
        };
        let parent = match container {
            ItemContainerId::ModuleId(module) => self.module(module)?,
            ItemContainerId::TraitId(trait_) => self.trait_path(trait_)?,
            ItemContainerId::ImplId(id) => self.own_impl(id)?,
            ItemContainerId::ExternBlockId(_) => return None,
        };
        let owner = parent.child_at(DefPathData::ValueNs(name.as_str()), place);
        Some(owner.child_at(DefPathData::AnonConst, block.ast_id(db).value.erase().into_raw()))
    }

    /// An `impl` by what it is of, where rustc counts the `impl`s before it.
    fn own_impl(&mut self, id: hir_def::ImplId) -> Option<Fingerprint> {
        let db = self.db;
        let module = self.module(id.lookup(db).container)?;
        let mut hasher = StableSipHasher128::new();
        module.hash(&mut hasher);
        self.ty(&mut hasher, db.impl_self_ty(id).instantiate_identity().skip_norm_wip())?;
        if let Some(trait_ref) = db.impl_trait(id) {
            let trait_ref = trait_ref.instantiate_identity().skip_norm_wip();
            self.trait_path(trait_ref.def_id.0)?.hash(&mut hasher);
            self.args(trait_ref.args)?.hash(&mut hasher);
        }
        let Fingerprint(_, local) = hasher.finish();
        Some(Fingerprint(module.0, local))
    }

    fn module(&mut self, module: ModuleIdLt<'db>) -> Option<Fingerprint> {
        let db = self.db;
        // An item in a body is a child of what the body belongs to, which a module does not say.
        if module.is_block_module(db) {
            return if self.ours { self.block(module.block(db)?) } else { None };
        }
        match module.containing_module(db) {
            Some(parent) => {
                let name = module.name(db)?;
                Some(self.module(parent)?.child(DefPathData::TypeNs(name.as_str())))
            }
            None => self.crate_root(module.krate(db)),
        }
    }

    fn adt_path(&mut self, adt: AdtId) -> Option<Fingerprint> {
        let db = self.db;
        let name = match adt {
            AdtId::StructId(it) => &StructSignature::of(db, it).name,
            AdtId::UnionId(it) => &UnionSignature::of(db, it).name,
            AdtId::EnumId(it) => &EnumSignature::of(db, it).name,
        };
        let module = adt.module(db);
        if module.is_block_module(db) && !self.ours {
            return self.function_adt_path(self.symbols.declared_in(adt)?, name);
        }
        Some(self.module(module)?.child(DefPathData::TypeNs(name.as_str())))
    }

    /// An ADT declared in the body of `func`. rustc gives the function's type parameters
    /// their paths before the items of its body, so one of the ADT's name comes first.
    fn function_adt_path(
        &mut self,
        func: FunctionId,
        name: &hir_expand::name::Name,
    ) -> Option<Fingerprint> {
        let earlier = FunctionSignature::of(self.db, func)
            .generic_params
            .iter_type_or_consts()
            .filter_map(|(_, param)| param.type_param()?.name.as_ref())
            .any(|param| param == name);
        Some(self.body_path(func)?.child_at(DefPathData::TypeNs(name.as_str()), u32::from(earlier)))
    }

    /// An `impl`, which rustc tells from the others of its module by how many come before it.
    fn impl_path(&mut self, id: ImplId) -> Option<Fingerprint> {
        let earlier = self.symbols.impl_place(id)?;
        Some(self.module(id.lookup(self.db).container)?.child_at(DefPathData::Impl, earlier))
    }

    /// A function that the items of its body are children of.
    fn body_path(&mut self, func: FunctionId) -> Option<Fingerprint> {
        let db = self.db;
        let parent = match func.lookup(db).container {
            ItemContainerId::ImplId(id) if self.ours => self.own_impl(id)?,
            ItemContainerId::ImplId(id) => self.impl_path(id)?,
            ItemContainerId::ModuleId(module) => self.module(module)?,
            ItemContainerId::TraitId(trait_) => self.trait_path(trait_)?,
            ItemContainerId::ExternBlockId(_) => return None,
        };
        let name = &FunctionSignature::of(db, func).name;
        // `#[test]` and `#[bench]` put a constant of the function's name ahead of it.
        let flags = AttrFlags::query(db, func.into());
        let earlier = u32::from(flags.intersects(AttrFlags::IS_TEST | AttrFlags::IS_BENCH));
        Some(parent.child_at(DefPathData::ValueNs(name.as_str()), earlier))
    }

    fn assoc_type_path(&mut self, alias: TypeAliasId) -> Option<Fingerprint> {
        let ItemContainerId::TraitId(trait_) = alias.lookup(self.db).container else {
            return None;
        };
        let name = &TypeAliasSignature::of(self.db, alias).name;
        Some(self.trait_path(trait_)?.child(DefPathData::TypeNs(name.as_str())))
    }

    fn trait_path(&mut self, trait_: TraitId) -> Option<Fingerprint> {
        let name = &TraitSignature::of(self.db, trait_).name;
        Some(self.module(trait_.module(self.db))?.child(DefPathData::TypeNs(name.as_str())))
    }

    /// A function of a module or a trait. One of an `impl` is under the `impl`'s disambiguator,
    /// which counts the `impl`s before it as rustc saw them.
    fn function_path(&mut self, func: FunctionId) -> Option<Fingerprint> {
        let db = self.db;
        let name = &FunctionSignature::of(db, func).name;
        let mut earlier = 0;
        let parent = match func.lookup(db).container {
            ItemContainerId::ModuleId(module) if module.is_block_module(db) && !self.ours => {
                let around = self.around_local(module.block(db)?, name)?;
                // A const parameter of the function around it has its path before the items
                // of its body, so it comes first.
                let params = &FunctionSignature::of(db, around).generic_params;
                earlier = u32::from(params.iter_type_or_consts().any(|(_, param)| {
                    param.const_param().is_some_and(|param| param.name == *name)
                }));
                self.body_path(around)?
            }
            ItemContainerId::ModuleId(module) => self.module(module)?,
            ItemContainerId::TraitId(trait_) => self.trait_path(trait_)?,
            ItemContainerId::ImplId(id) => self.impl_path(id)?,
            ItemContainerId::ExternBlockId(_) => return None,
        };
        Some(parent.child_at(DefPathData::ValueNs(name.as_str()), earlier))
    }

    /// The function whose body declares, in `block`, the one function of it named `name`:
    /// rustc makes that function a child of this one, whichever block it is written in.
    fn around_local(
        &self,
        block: hir_def::BlockIdLt<'db>,
        name: &hir_expand::name::Name,
    ) -> Option<FunctionId> {
        let db = self.db;
        let (owner, _) = hir_ty_mir::mir::type_name::block_owner(db, block)?;
        let DefWithBodyId::FunctionId(around) = owner else { return None };
        // One written in a closure is a child of the closure.
        if !hir_ty_mir::mir::type_name::is_outside_closures(db, owner, block) {
            return None;
        }
        let named_so = hir_def::expr_store::Body::of(db, owner)
            .blocks(db)
            .flat_map(|(_, def_map)| def_map.modules(db))
            .flat_map(|(_, module)| module.scope(db).declarations())
            .filter(|def| match def {
                hir_def::ModuleDefId::FunctionId(it) => {
                    FunctionSignature::of(db, *it).name == *name
                }
                _ => false,
            });
        if named_so.count() != 1 {
            return None;
        }
        Some(around)
    }

    /// The `DefPathHash` rustc gives a closure of a crate it compiled, which only code rustc
    /// compiled can have instantiated, so its `TypeId` has to be rustc's: the function's path,
    /// then a `Closure` child for each closure it is written in and for itself, by its place
    /// among the closures of the one before (`{closure#n}`). `None` leaves it to
    /// [`Self::closure`], as for a closure of a crate whose code is ours.
    fn rustc_closure_path(&mut self, closure: InternedClosure<'_>) -> Option<Fingerprint> {
        let db = self.db;
        let InternedClosure { owner, expr, .. } = closure;
        let InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(func)) = owner else {
            return None;
        };
        if self.ours || self.owned.contains(&func.krate(db)) {
            return None;
        }
        // An `async` closure is two definitions to rustc, the closure and its body's
        // coroutine, where the body has one expression.
        let body = hir_def::expr_store::Body::of(db, func.into());
        if let hir_def::hir::Expr::Closure { closure_kind, .. } = &body[expr]
            && let hir_def::hir::ClosureKind::Coroutine { source, .. } = closure_kind
            && *source == hir_def::hir::CoroutineSource::Closure
        {
            return None;
        }
        let places = hir_ty_mir::mir::type_name::rustc_closure_places(db, func.into(), expr)?;
        let mut path = self.function_path(func)?;
        for place in places {
            path = path.child_at(DefPathData::Closure, u32::try_from(place).ok()?);
        }
        Some(path)
    }

    /// A coroutine's arguments as rustc has them. The body of an `async` block or `async fn`
    /// is lowered to take the task's context as a `ResumeTy`, which is its resume type, where
    /// rust-analyzer has `()`. `None` for an `async gen` one, whose yield type rustc wraps.
    fn rustc_coroutine_args(
        &self,
        coroutine: InternedClosure<'_>,
        args: GenericArgs<'db>,
    ) -> Option<GenericArgs<'db>> {
        use hir_def::hir::{ClosureKind, CoroutineKind, Expr};
        let db = self.db;
        let InternedClosure { owner, expr, .. } = coroutine;
        let InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(func)) = owner else {
            return None;
        };
        let body = hir_def::expr_store::Body::of(db, func.into());
        let Expr::Closure { closure_kind: ClosureKind::Coroutine { kind, .. }, .. } = &body[expr]
        else {
            return None;
        };
        match kind {
            CoroutineKind::Gen => Some(args),
            CoroutineKind::AsyncGen => None,
            CoroutineKind::Async => {
                let interner = hir_ty::next_solver::DbInterner::new_with(db, func.krate(db));
                let resume_ty = interner.lang_items().ResumeTy?;
                let resume_ty =
                    Ty::new_adt(interner, resume_ty.into(), GenericArgs::new_from_slice(&[]));
                // After the parent's: the kind, then the resume type.
                let at = args.as_coroutine().parent_args().len() + 1;
                let mut args: Vec<_> = args.iter().collect();
                *args.get_mut(at)? = resume_ty.into();
                Some(GenericArgs::new_from_slice(&args))
            }
        }
    }

    /// A closure of a crate we compile, which is named by what only we know.
    ///
    /// rustc hashes the signature and the captures it inferred for the closure, with the
    /// lifetimes the signature binds, and rust-analyzer infers no lifetimes. But only code that
    /// was instantiated with the closure can ask for its `TypeId`, and that is all ours when the
    /// function it is written in is, so there is no hash of rustc's to agree with.
    // FIXME: a function of the crate that is left to rustc, and got the closure from one that
    // is ours, has rustc's hash for it.
    fn closure(
        &mut self,
        hasher: &mut StableSipHasher128,
        closure: InternedClosure<'_>,
    ) -> Option<()> {
        let db = self.db;
        let InternedClosure { owner, expr, .. } = closure;
        let InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(func)) = owner else {
            return None;
        };
        // One of another crate is hashed as ours are when what it is instantiated with makes
        // the type one only our code has, as a closure of ours does.
        match self.owned.contains(&func.krate(db)) {
            true => self.own_closure = true,
            false if !self.ours => return None,
            false => {}
        }
        let loc = func.lookup(db);
        let parent = match loc.container {
            // A function written in a body: told apart by where it is in the file.
            ItemContainerId::ModuleId(mut module) if module.is_block_module(db) => {
                while module.is_block_module(db) {
                    module = module.containing_module(db)?;
                }
                self.module(module)?.hash(hasher);
                Fingerprint(1, u64::from(loc.id.value.erase().into_raw()))
            }
            ItemContainerId::ModuleId(module) => self.module(module)?,
            ItemContainerId::TraitId(trait_) => self.trait_path(trait_)?,
            ItemContainerId::ImplId(impl_) => {
                let loc = impl_.lookup(db);
                let module = self.module(loc.container)?;
                module.hash(hasher);
                Fingerprint(0, u64::from(loc.id.value.erase().into_raw()))
            }
            ItemContainerId::ExternBlockId(_) => return None,
        };
        hasher.write(b"blaze-engine closure");
        parent.hash(hasher);
        hash_str(hasher, FunctionSignature::of(db, func).name.as_str());
        hasher.write_u32(expr.into_raw().into_u32());
        Some(())
    }

    fn ty(&mut self, hasher: &mut StableSipHasher128, ty: Ty<'db>) -> Option<()> {
        let hashed = self.kind(hasher, ty);
        if hashed.is_none() && self.stuck.is_none() {
            self.stuck = Some(format!("{ty:?}"));
        }
        hashed
    }

    fn kind(&mut self, hasher: &mut StableSipHasher128, ty: Ty<'db>) -> Option<()> {
        let mutability = |hasher: &mut StableSipHasher128, it| {
            hasher.write_isize(match it {
                Mutability::Not => 0,
                Mutability::Mut => 1,
            })
        };
        match ty.kind() {
            TyKind::Bool => hasher.write_isize(0),
            TyKind::Char => hasher.write_isize(1),
            TyKind::Int(it) => {
                hasher.write_isize(2);
                hasher.write_isize(match it {
                    IntTy::Isize => 0,
                    IntTy::I8 => 1,
                    IntTy::I16 => 2,
                    IntTy::I32 => 3,
                    IntTy::I64 => 4,
                    IntTy::I128 => 5,
                });
            }
            TyKind::Uint(it) => {
                hasher.write_isize(3);
                hasher.write_isize(match it {
                    UintTy::Usize => 0,
                    UintTy::U8 => 1,
                    UintTy::U16 => 2,
                    UintTy::U32 => 3,
                    UintTy::U64 => 4,
                    UintTy::U128 => 5,
                });
            }
            TyKind::Float(it) => {
                hasher.write_isize(4);
                hasher.write_isize(match it {
                    FloatTy::F16 => 0,
                    FloatTy::F32 => 1,
                    FloatTy::F64 => 2,
                    FloatTy::F128 => 3,
                });
            }
            TyKind::Adt(adt, args) => {
                // `Ref<'a, dyn Trait>` is `Ref<'a, dyn Trait + 'a>` to rustc and `+ 'static`
                // here, which only matters where a signature binds the `'a`.
                let lifetimes = args.iter().any(|arg| arg.region().is_some());
                let object = args
                    .iter()
                    .any(|arg| matches!(arg.ty().map(|ty| ty.kind()), Some(TyKind::Dynamic(..))));
                if self.signatures > 0 && lifetimes && object {
                    return None;
                }
                hasher.write_isize(5);
                self.adt_def(adt.def_id())?.hash(hasher);
                self.args(args)?.hash(hasher);
            }
            TyKind::Str => hasher.write_isize(7),
            TyKind::Array(element, len) => {
                hasher.write_isize(8);
                self.ty(hasher, element)?;
                self.usize_const(hasher, len)?;
            }
            TyKind::Slice(element) => {
                hasher.write_isize(10);
                self.ty(hasher, element)?;
            }
            TyKind::RawPtr(pointee, mutbl) => {
                hasher.write_isize(11);
                self.ty(hasher, pointee)?;
                mutability(hasher, mutbl);
            }
            TyKind::Ref(region, pointee, mutbl) => {
                hasher.write_isize(12);
                self.region(hasher, region)?;
                self.ty(hasher, pointee)?;
                mutability(hasher, mutbl);
            }
            TyKind::FnDef(def, args) => {
                hasher.write_isize(13);
                let path = match def.0 {
                    CallableDefId::FunctionId(func) => {
                        // A lifetime of a function is mostly late bound, and then no argument
                        // to rustc.
                        if args.iter().any(|arg| matches!(arg.kind(), GenericArgKind::Lifetime(_)))
                        {
                            return None;
                        }
                        self.function_path(func)?
                    }
                    // A constructor (`Some`, `Ok`, `Tuple`) is the `Ctor` child of its variant
                    // or struct, with the ADT's own parameters, which are all early bound.
                    CallableDefId::EnumVariantId(variant) => {
                        let loc = variant.lookup(self.db);
                        let adt = self.adt_path(loc.parent.into())?;
                        adt.child(DefPathData::TypeNs(loc.name.as_str())).child(DefPathData::Ctor)
                    }
                    CallableDefId::StructId(it) => {
                        self.adt_path(it.into())?.child(DefPathData::Ctor)
                    }
                };
                path.hash(hasher);
                // The arguments are under a binder, which binds nothing.
                self.binder(hasher, |this, hasher| {
                    this.args(args)?.hash(hasher);
                    Some(())
                })?;
            }
            TyKind::Closure(id, args) => {
                hasher.write_isize(17);
                let closure = id.0.loc(self.db);
                if let Some(path) = self.rustc_closure_path(closure) {
                    // rustc's: the closure's `DefPathHash`, then all of its arguments: those of
                    // what it is written in, its kind, its signature and what it captured.
                    path.hash(hasher);
                    self.args(args)?.hash(hasher);
                } else {
                    self.closure(hasher, closure)?;
                    let parent = GenericArgs::new_from_slice(args.as_closure().parent_args());
                    self.args(parent)?.hash(hasher);
                }
            }
            TyKind::CoroutineClosure(id, args) => {
                hasher.write_isize(18);
                self.closure(hasher, id.0.loc(self.db))?;
                let parent = args.as_coroutine_closure().parent_args();
                self.args(GenericArgs::new_from_slice(parent))?.hash(hasher);
            }
            TyKind::Coroutine(id, args) => {
                hasher.write_isize(19);
                let coroutine = id.0.loc(self.db);
                // The future of an `async` closure is another type when it owns what the
                // closure captured.
                let by_move =
                    hir_ty_mir::mir::is_by_move_coroutine(self.db, id.0, args.as_coroutine());
                // rustc names that one apart (`SyntheticCoroutineBody`), so only the others
                // have its `DefPath` here; its arguments are the resume, yield and return types
                // and the captures after the parent's.
                let rustcs = self.rustc_closure_path(coroutine).filter(|_| !by_move);
                if let Some(path) = rustcs {
                    path.hash(hasher);
                    self.args(self.rustc_coroutine_args(coroutine, args)?)?.hash(hasher);
                } else {
                    self.closure(hasher, coroutine)?;
                    hasher.write_u8(by_move.into());
                    let parent = GenericArgs::new_from_slice(args.as_coroutine().parent_args());
                    self.args(parent)?.hash(hasher);
                }
            }
            TyKind::FnPtr(signature, header) => {
                hasher.write_isize(14);
                self.signatures += 1;
                let hashed = self.binder(hasher, |this, hasher| {
                    let tys = signature.skip_binder().inputs_and_output;
                    this.list(hasher, tys.iter(), |this, list, ty| this.ty(list, ty))
                });
                self.signatures -= 1;
                hashed?;
                // `FnSigKind`'s flags: the ABI's index in `ExternAbi::ALL_VARIANTS`, then safe,
                // then variadic. The index is the vendored rustc_abi's, which is the pinned
                // nightly's: `rust-call` moved from 21 to 22 when `Unadjusted` became
                // `LlvmIntrinsic`, after 1.99.0.
                let abi = match header.abi() {
                    // A closure's signature, as its arguments hold it, is `RustCall`.
                    abi @ (ExternAbi::C { unwind: false }
                    | ExternAbi::C { unwind: true }
                    | ExternAbi::Rust
                    | ExternAbi::RustCall) => abi.as_packed(),
                    _ => return None,
                };
                let safe = u8::from(header.safety().is_safe()) << 6;
                let variadic = u8::from(header.c_variadic()) << 7;
                hasher.write_u8(abi | safe | variadic);
                // No argument is splatted: the syntax is unstable.
                hasher.write_u8(u8::MAX);
            }
            TyKind::Dynamic(predicates, region) => {
                hasher.write_isize(16);
                // rustc keeps them as the trait, then its associated types, then the auto
                // traits, each group sorted by `DefPathHash`.
                let mut sorted = Vec::new();
                let mut signature = false;
                for predicate in predicates.iter() {
                    let key = match predicate.skip_binder() {
                        ExistentialPredicate::Trait(it) => {
                            // `dyn Fn(&u8)`: the trait's arguments are written as a signature.
                            signature = TraitSignature::of(self.db, it.def_id.0)
                                .flags
                                .contains(TraitFlags::RUSTC_PAREN_SUGAR);
                            (0, None)
                        }
                        ExistentialPredicate::Projection(it) => {
                            let TermId::TypeAliasId(alias) = it.def_id.0 else { return None };
                            (1, Some(self.assoc_type_path(alias)?))
                        }
                        ExistentialPredicate::AutoTrait(it) => (2, Some(self.trait_path(it.0)?)),
                    };
                    sorted.push((key, predicate));
                }
                sorted.sort_by_key(|&(key, _)| key);
                self.signatures += usize::from(signature);
                let hashed =
                    self.list(hasher, sorted.into_iter(), |this, list, (key, predicate)| {
                        let (_, path) = key;
                        this.binder(list, |this, list| {
                            match predicate.skip_binder() {
                                ExistentialPredicate::Trait(it) => {
                                    list.write_isize(0);
                                    this.trait_path(it.def_id.0)?.hash(list);
                                    this.args(it.args)?.hash(list);
                                }
                                ExistentialPredicate::Projection(it) => {
                                    list.write_isize(1);
                                    path?.hash(list);
                                    this.args(it.args)?.hash(list);
                                    // A constant is `TermKind::Const`, which no stable code writes.
                                    list.write_isize(0);
                                    this.ty(list, it.term.as_type()?)?;
                                }
                                ExistentialPredicate::AutoTrait(_) => {
                                    list.write_isize(2);
                                    path?.hash(list);
                                }
                            }
                            Some(())
                        })
                    });
                self.signatures -= usize::from(signature);
                hashed?;
                self.region(hasher, region)?;
            }
            TyKind::Never => hasher.write_isize(21),
            // What an `impl` is generic over, in the type it is of.
            TyKind::Param(param) if self.ours => {
                hasher.write_isize(23);
                hasher.write_u32(param.index);
            }
            TyKind::Tuple(tys) => {
                hasher.write_isize(22);
                self.list(hasher, tys.iter(), |this, list, ty| this.ty(list, ty))?;
            }
            _ => return None,
        }
        Some(())
    }

    /// An interned list, which rustc hashes into a fingerprint first and that into the type.
    fn list<T>(
        &mut self,
        hasher: &mut StableSipHasher128,
        items: impl ExactSizeIterator<Item = T>,
        mut item: impl FnMut(&mut Self, &mut StableSipHasher128, T) -> Option<()>,
    ) -> Option<()> {
        let mut list = StableSipHasher128::new();
        list.write_usize(items.len());
        for it in items {
            item(self, &mut list, it)?;
        }
        list.finish::<Fingerprint>().hash(hasher);
        Some(())
    }

    /// A `Binder`: its value, then the kinds of the variables it binds.
    fn binder(
        &mut self,
        hasher: &mut StableSipHasher128,
        value: impl FnOnce(&mut Self, &mut StableSipHasher128) -> Option<()>,
    ) -> Option<()> {
        self.binders.push(Vec::new());
        let hashed = value(self, hasher);
        let bound = self.binders.pop()?;
        hashed?;
        self.list(hasher, bound.into_iter(), |_, list, _| {
            // `BoundVariableKind::Region(BoundRegionKind::Anon)`.
            list.write_isize(1);
            list.write_isize(0);
            Some(())
        })
    }

    /// A lifetime, which is erased unless a binder in the type binds it.
    fn region(&mut self, hasher: &mut StableSipHasher128, region: Region<'db>) -> Option<()> {
        let RegionKind::ReBound(index, bound) = region.kind() else {
            // An error in a signature is a lifetime nothing settled, which rustc may have bound.
            if self.signatures > 0 && region.is_error() && !self.ours {
                return None;
            }
            // `ReErased`.
            hasher.write_isize(6);
            return Some(());
        };
        let BoundVarIndexKind::Bound(debruijn) = index else { return None };
        let depth = self.binders.len().checked_sub(1 + debruijn.as_usize())?;
        let met = &mut self.binders[depth];
        let var = met.iter().position(|&it| it == bound.var).unwrap_or_else(|| {
            met.push(bound.var);
            met.len() - 1
        });
        hasher.write_isize(1);
        hasher.write_isize(0);
        hasher.write_u32(debruijn.as_u32());
        hasher.write_u32(var as u32);
        // `BoundRegionKind::Anon`.
        hasher.write_isize(0);
        Some(())
    }

    /// A `usize` constant: `ConstKind::Value` of a `ValTreeKind::Leaf`.
    fn usize_const(&mut self, hasher: &mut StableSipHasher128, konst: Const<'db>) -> Option<()> {
        let value = try_const_usize(self.db, konst)?;
        let word = super::Target::current().word() as u8;
        self.leaf(hasher, default_types().types.usize, value, word)
    }

    /// `ConstKind::Value` of a `ValTreeKind::Leaf`: the type, then the `ScalarInt`'s bits
    /// and its size in bytes.
    fn leaf(
        &mut self,
        hasher: &mut StableSipHasher128,
        ty: Ty<'db>,
        bits: u128,
        size: u8,
    ) -> Option<()> {
        hasher.write_isize(5);
        self.ty(hasher, ty)?;
        hasher.write_isize(0);
        hasher.write_u128(bits);
        hasher.write_u8(size);
        Some(())
    }

    /// A constant that is an argument of a type: a `bool`, a `char` or an integer.
    fn scalar_const(&mut self, hasher: &mut StableSipHasher128, konst: Const<'db>) -> Option<()> {
        let ConstKind::Value(value) = konst.kind() else {
            return self.usize_const(hasher, konst);
        };
        let size = match value.ty.kind() {
            TyKind::Bool => 1,
            TyKind::Char => 4,
            TyKind::Int(it) => {
                it.bit_width().map_or(super::Target::current().word() as u64, |it| it / 8)
            }
            TyKind::Uint(it) => {
                it.bit_width().map_or(super::Target::current().word() as u64, |it| it / 8)
            }
            _ => return None,
        };
        let bits = value.value.inner().to_leaf().to_bits_unchecked();
        self.leaf(hasher, value.ty, bits, size as u8)
    }

    fn args(&mut self, args: GenericArgs<'db>) -> Option<Fingerprint> {
        let mut list = StableSipHasher128::new();
        list.write_usize(args.len());
        for arg in args.iter() {
            match arg.kind() {
                GenericArgKind::Lifetime(region) => {
                    list.write_isize(0);
                    self.region(&mut list, region)?;
                }
                GenericArgKind::Type(ty) => {
                    list.write_isize(1);
                    self.ty(&mut list, ty)?;
                }
                GenericArgKind::Const(konst) => {
                    list.write_isize(2);
                    self.scalar_const(&mut list, konst)?;
                }
            }
        }
        Some(list.finish())
    }

    /// `AdtDefData`'s hash: the definition with its variants, fields and representation.
    fn adt_def(&mut self, adt: AdtId) -> Option<Fingerprint> {
        let db = self.db;
        let did = self.adt_path(adt)?;
        let attrs = AttrFlags::query(db, adt.into());
        let lang = AttrFlags::lang_item(db, adt.into());
        let lang = lang.as_ref().map(|it| it.as_str());

        let mut hasher = StableSipHasher128::new();
        did.hash(&mut hasher);

        let mut flags = 0u16;
        match adt {
            AdtId::StructId(it) => {
                hasher.write_usize(1);
                let shape = StructSignature::of(db, it).shape;
                self.variant(&mut hasher, did, it.into(), shape, did, Discriminant::Relative(0))?;
                flags |= 1 << 2;
                if shape != FieldsShape::Record {
                    flags |= 1 << 3;
                }
            }
            AdtId::UnionId(it) => {
                hasher.write_usize(1);
                let shape = FieldsShape::Record;
                self.variant(&mut hasher, did, it.into(), shape, did, Discriminant::Relative(0))?;
                flags |= 1 << 1;
            }
            AdtId::EnumId(it) => {
                let variants = &it.enum_variants(db).variants;
                hasher.write_usize(variants.len());
                let mut relative = 0;
                for (name, &(variant, shape)) in variants {
                    let path = did.child(DefPathData::TypeNs(name.as_str()));
                    let discriminant = if self.has_explicit_discriminant(variant) {
                        relative = 0;
                        Discriminant::Explicit(path.child(DefPathData::AnonConst))
                    } else {
                        Discriminant::Relative(relative)
                    };
                    relative += 1;
                    self.variant(&mut hasher, path, variant.into(), shape, did, discriminant)?;
                }
                flags |= 1 << 0;
                if attrs.contains(AttrFlags::NON_EXHAUSTIVE) {
                    flags |= 1 << 8;
                }
            }
        }
        if attrs.contains(AttrFlags::FUNDAMENTAL) {
            flags |= 1 << 5;
        }
        flags |= match lang {
            Some("phantom_data") => 1 << 4,
            Some("owned_box") => 1 << 6,
            Some("manually_drop") => 1 << 7,
            Some("unsafe_cell") => 1 << 9,
            Some("unsafe_pinned") => 1 << 10,
            Some("pin") => 1 << 11,
            Some("field_representing_type") => 1 << 13,
            Some("maybe_dangling") => 1 << 14,
            _ => 0,
        };
        hasher.write_u16(flags);

        let repr = AttrFlags::repr(db, adt).unwrap_or_default();
        match repr.int {
            None => hasher.write_u8(0),
            Some(int) => {
                hasher.write_u8(1);
                let signed = match int {
                    IntegerType::Pointer(signed) => {
                        hasher.write_isize(0);
                        signed
                    }
                    IntegerType::Fixed(int, signed) => {
                        hasher.write_isize(1);
                        hasher.write_isize(match int {
                            Integer::I8 => 0,
                            Integer::I16 => 1,
                            Integer::I32 => 2,
                            Integer::I64 => 3,
                            Integer::I128 => 4,
                        });
                        signed
                    }
                };
                hasher.write_u8(signed.into());
            }
        }
        for align in [repr.align, repr.pack] {
            match align {
                None => hasher.write_u8(0),
                Some(align) => {
                    hasher.write_u8(1);
                    hasher.write_u8(align.bytes().trailing_zeros() as u8);
                }
            }
        }
        let mut repr_flags =
            repr.flags & (ReprFlags::IS_C | ReprFlags::IS_SIMD | ReprFlags::IS_TRANSPARENT);
        if lang == Some("owned_box") {
            repr_flags |= ReprFlags::IS_LINEAR;
        }
        hasher.write_u8(repr_flags.bits());
        // Not a scalable vector.
        hasher.write_u8(0);
        // The layout seed, which is the path's `Fingerprint::to_smaller_hash`.
        hasher.write_u64(did.0.wrapping_mul(3).wrapping_add(did.1));
        Some(hasher.finish())
    }

    fn has_explicit_discriminant(&self, variant: EnumVariantId) -> bool {
        let loc = variant.lookup(self.db);
        loc.source(self.db).value.const_arg().is_some()
    }

    /// A `VariantDef`. `path` is the variant's own, which for a struct or union is the type's.
    fn variant(
        &mut self,
        hasher: &mut StableSipHasher128,
        path: Fingerprint,
        variant: VariantId,
        shape: FieldsShape,
        adt: Fingerprint,
        discriminant: Discriminant,
    ) -> Option<()> {
        let db = self.db;
        let _ = adt;
        path.hash(hasher);
        match shape {
            FieldsShape::Record => hasher.write_u8(0),
            FieldsShape::Tuple | FieldsShape::Unit => {
                hasher.write_u8(1);
                hasher.write_isize(if shape == FieldsShape::Tuple { 0 } else { 1 });
                path.child(DefPathData::Ctor).hash(hasher);
            }
        }
        let name = match variant {
            VariantId::EnumVariantId(it) => it.lookup(db).name.clone(),
            VariantId::StructId(it) => StructSignature::of(db, it).name.clone(),
            VariantId::UnionId(it) => UnionSignature::of(db, it).name.clone(),
        };
        hash_str(hasher, name.as_str());
        match discriminant {
            Discriminant::Explicit(konst) => {
                hasher.write_isize(0);
                konst.hash(hasher);
            }
            Discriminant::Relative(distance) => {
                hasher.write_isize(1);
                hasher.write_u32(distance);
            }
        }

        let fields = variant.fields(db);
        let visibilities = hir_def::signatures::VariantFields::field_visibilities(db, variant);
        // A variant's fields are as visible as the enum.
        let enum_visibility = match variant {
            VariantId::EnumVariantId(it) => {
                let parent = it.lookup(db).parent;
                let source = parent.lookup(db).source(db);
                Some(visibility_from_ast(db, parent, source.map(|it| it.visibility())))
            }
            _ => None,
        };
        hasher.write_usize(fields.fields().len());
        for (id, field) in fields.fields().iter() {
            // A default value is a constant of its own.
            if field.default_value.is_some() {
                return None;
            }
            path.child(DefPathData::ValueNs(field.name.as_str())).hash(hasher);
            hash_str(hasher, field.name.as_str());
            match enum_visibility.unwrap_or(visibilities[id]) {
                Visibility::Public => hasher.write_isize(0),
                Visibility::PubCrate(krate) => {
                    hasher.write_isize(1);
                    self.crate_root(krate)?.hash(hasher);
                }
                Visibility::Module(mut module, _) => {
                    hasher.write_isize(1);
                    // What is private to a body is to rustc private to the module around it.
                    while module.is_block_module(db) {
                        module = module.containing_module(db)?;
                    }
                    self.module(module)?.hash(hasher);
                }
            }
            // Its `mut` is not restricted: the syntax is unstable.
            hasher.write_isize(0);
            hasher.write_isize(if field.is_unsafe { 0 } else { 1 });
            hasher.write_u8(0);
        }
        // Not tainted by an error.
        hasher.write_u8(0);
        let attrs = match variant {
            VariantId::EnumVariantId(it) => AttrFlags::query(db, it.into()),
            VariantId::StructId(it) => AttrFlags::query(db, AdtId::from(it).into()),
            VariantId::UnionId(it) => AttrFlags::query(db, AdtId::from(it).into()),
        };
        hasher.write_u8(attrs.contains(AttrFlags::NON_EXHAUSTIVE).into());
        Some(())
    }
}

/// rustc's `VariantDiscr`.
enum Discriminant {
    /// The constant the variant is assigned.
    Explicit(Fingerprint),
    /// How far past the last assigned variant, or the first, this one is.
    Relative(u32),
}
