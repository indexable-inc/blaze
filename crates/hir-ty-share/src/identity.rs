//! The full identity of a type, constant or argument list, spelled by content: what tells two
//! types apart wherever a program or its code can (a `TypeId`, a `type_name`, a vtable, a
//! `dyn` call, debug info, a closure's or coroutine's body). This is the one spelling of a type
//! by content. The code generator's `Hashes::ty` and `Hashes::konst` (instance arguments,
//! `TypeId` values, the types of its debug info) are this, and so are the share key's identity
//! records, so a share key covers every type id a template holds.
//!
//! - An ADT by its key, its interface, its layout and the `Drop` impls its drop glue reads
//!   ([`adt`], a tracked query); its arguments the same way.
//! - A closure by its key and arguments; a coroutine by its key, its compiled code
//!   ([`crate::code`]: its code hash, its bindings' names, its positions) and its arguments.
//! - Regions are left out.
//!
//! Each spelling streams into one [`Hasher`]: nothing is allocated but a trait object's
//! bounds past four, which are sorted.

use base_db::Db;
use blaze_hash::HashSet;
use blaze_ids::{ContentId, Hasher};
use blaze_items::{ItemId, ItemKey, Unhashable, interface_hash, item_key, key, layout_hash};
use hir_def::{AdtId, CallableDefId, HasModule};
use hir_ty::{
    ParamEnvAndCrate,
    db::HirDatabase,
    mir::is_by_move_coroutine,
    next_solver::{
        AliasConst, AliasConstKind, Binder, Const, ConstExt as _, ConstKind, DbInterner,
        FreeConstAliasId, GenericArgs, InherentAssocConstId, ParamEnv, Pattern, TermId, Ty, TyKind,
        ValTree,
    },
};
use rustc_type_ir::{
    AliasTyKind, BoundTyKind, BoundVarIndexKind, ExistentialPredicate, GenericArgKind, InferConst,
    PatternKind, TermKind, TypeVisitableExt as _, ValTreeKind,
    inherent::{GenericArgs as _, IntoKind},
};
use smallvec::SmallVec;

use crate::{Unshared, code};

/// The domain of a type's identity.
pub const TY_DOMAIN: &str = "blaze v2 identity type 1";
/// The domain of a constant's identity.
pub const CONST_DOMAIN: &str = "blaze v2 identity const 1";
/// The domain of an argument list's identity.
pub const ARGS_DOMAIN: &str = "blaze v2 identity args 1";
/// The domain of one bound of a trait object, hashed alone so the bounds can be sorted.
pub const BOUND_DOMAIN: &str = "blaze v2 identity bound 1";
/// The domain of an ADT's identity: its key, interface, layout and the drop impls it reaches.
pub const ADT_DOMAIN: &str = "blaze v2 identity adt 1";

/// The identity of `ty`. Call it with the database attached (`hir_ty::attach_db`).
pub fn ty<'db>(db: &'db Db, ty: Ty<'db>) -> Result<ContentId, Unshared> {
    let mut spell = Spell::new(db, TY_DOMAIN);
    spell.ty(ty)?;
    Ok(spell.out.finish())
}

/// The identity of `args`: each type and constant, and a lifetime as a placeholder.
pub fn args<'db>(db: &'db Db, args: GenericArgs<'db>) -> Result<ContentId, Unshared> {
    let mut spell = Spell::new(db, ARGS_DOMAIN);
    spell.args(args)?;
    Ok(spell.out.finish())
}

/// The identity of `konst`: its value and its type's identity.
pub fn konst<'db>(db: &'db Db, konst: Const<'db>) -> Result<ContentId, Unshared> {
    let mut spell = Spell::new(db, CONST_DOMAIN);
    spell.konst(konst)?;
    Ok(spell.out.finish())
}

pub(crate) fn unhashable(db: &Db) -> impl Fn(Unhashable) -> Unshared + '_ {
    move |it| Unshared::Unhashable(it.describe(db))
}

fn no_content(what: String) -> Unshared {
    Unshared::NoContent(what)
}

/// An ADT as its identity says it: its key, its interface (fields, variants, `repr`), its
/// layout, and the `Drop` impls of it and of every ADT its fields' types name, transitively,
/// which drop glue reads.
#[blaze_query::tracked(returns(ref))]
pub fn adt(db: &Db, adt: AdtId) -> Result<ContentId, Unshared> {
    let item = ItemId::Adt(adt);
    let mut h = Hasher::new(ADT_DOMAIN);
    h.id(&item_key(db, item).0)
        .id(&interface_hash(db, item).map_err(unhashable(db))?.0)
        .id(&layout_hash(db, adt).map_err(unhashable(db))?.0);
    let mut seen: HashSet<AdtId> = HashSet::default();
    seen.insert(adt);
    // alloc: the ADTs the fields reach, once per ADT: this query's miss, memoized after
    let mut next = vec![adt];
    // alloc: as `next`; sorted, because the walk's order is not the ADT's
    let mut dropping: Vec<(ItemKey, ContentId)> = Vec::new();
    while let Some(at) = next.pop() {
        if let Some(imp) = hir_ty::drop::destructor(db, at) {
            let interface = interface_hash(db, ItemId::Impl(imp)).map_err(unhashable(db))?;
            dropping.push((item_key(db, ItemId::Adt(at)), interface.0));
        }
        for variant in at.variants(db) {
            for field in db.field_types(variant).values() {
                for arg in field.ty().skip_binder().walk() {
                    if let Some(ty) = arg.ty()
                        && let TyKind::Adt(def, _) = ty.kind()
                        && seen.insert(def.def_id())
                    {
                        next.push(def.def_id());
                    }
                }
            }
        }
    }
    dropping.sort_unstable();
    h.update(&(dropping.len() as u64).to_le_bytes());
    for (key, interface) in &dropping {
        h.id(&key.0).id(interface);
    }
    Ok(h.finish())
}

/// A type or constant spelled by content straight into its hash: nothing is buffered.
struct Spell<'db> {
    db: &'db Db,
    out: Hasher,
}

/// `fmt::Write` into a spelling: a `Debug` name hashed without a `String`.
struct Text<'a>(&'a mut Hasher);

impl std::fmt::Write for Text<'_> {
    fn write_str(&mut self, text: &str) -> std::fmt::Result {
        self.0.update(text.as_bytes());
        Ok(())
    }
}

impl<'db> Spell<'db> {
    fn new(db: &'db Db, domain: &'static str) -> Spell<'db> {
        Spell { db, out: Hasher::new(domain) }
    }

    fn u8(&mut self, value: u8) {
        self.out.update(&[value]);
    }

    fn u32(&mut self, value: u32) {
        self.out.update(&value.to_le_bytes());
    }

    fn len(&mut self, len: usize) {
        self.out.update(&(len as u64).to_le_bytes());
    }

    fn id(&mut self, id: &ContentId) {
        self.out.id(id);
    }

    fn str(&mut self, text: &str) {
        self.out.part(text.as_bytes());
    }

    /// `value`'s `Debug` text, ended by `0xff`, which no UTF-8 text holds.
    fn debug(&mut self, value: &impl std::fmt::Debug) {
        use std::fmt::Write as _;
        // `Text` never fails, so neither does the write.
        let _ = write!(Text(&mut self.out), "{value:?}");
        self.u8(0xff);
    }

    fn key(&mut self, key: ItemKey) {
        self.id(&key.0);
    }

    fn item(&mut self, item: ItemId) {
        self.key(item_key(self.db, item));
    }

    fn ty(&mut self, ty: Ty<'db>) -> Result<(), Unshared> {
        let db = self.db;
        match ty.kind() {
            TyKind::Bool => self.u8(0),
            TyKind::Char => self.u8(1),
            TyKind::Int(it) => {
                self.u8(2);
                self.str(it.name_str());
            }
            TyKind::Uint(it) => {
                self.u8(3);
                self.str(it.name_str());
            }
            TyKind::Float(it) => {
                self.u8(4);
                self.str(it.name_str());
            }
            TyKind::Adt(def, args) => {
                self.u8(5);
                let content = adt(db, def.def_id()).as_ref().map_err(Clone::clone)?;
                self.id(content);
                self.args(args)?;
            }
            TyKind::Foreign(it) => {
                self.u8(6);
                self.item(ItemId::Alias(it.0));
            }
            TyKind::Str => self.u8(7),
            TyKind::Array(element, len) => {
                self.u8(8);
                self.ty(element)?;
                self.konst(len)?;
            }
            TyKind::Pat(base, pattern) => {
                self.u8(9);
                self.ty(base)?;
                self.pattern(pattern)?;
            }
            TyKind::Slice(element) => {
                self.u8(10);
                self.ty(element)?;
            }
            TyKind::RawPtr(pointee, mutability) => {
                self.u8(11);
                self.u8(mutability.is_mut() as u8);
                self.ty(pointee)?;
            }
            TyKind::Ref(_, pointee, mutability) => {
                self.u8(12);
                self.u8(mutability.is_mut() as u8);
                self.ty(pointee)?;
            }
            TyKind::FnDef(def, args) => {
                let args = args.no_bound_vars().unwrap();
                self.u8(13);
                match def.0 {
                    CallableDefId::FunctionId(it) => {
                        self.u8(0);
                        self.item(ItemId::Function(it));
                    }
                    CallableDefId::StructId(it) => {
                        self.u8(1);
                        self.item(ItemId::Adt(it.into()));
                    }
                    CallableDefId::EnumVariantId(it) => {
                        self.u8(2);
                        self.key(key::variant_key(db, it));
                    }
                }
                self.args(args)?;
            }
            TyKind::FnPtr(tys, header) => {
                self.u8(14);
                self.len(tys.bound_vars().len());
                let tys = tys.skip_binder().inputs_and_output;
                self.len(tys.len());
                for ty in tys.iter() {
                    self.ty(ty)?;
                }
                self.debug(&header.abi());
                self.debug(&header.safety());
                self.u8(header.c_variadic() as u8);
            }
            TyKind::UnsafeBinder(inner) => {
                self.u8(15);
                let binder: Binder<'db, Ty<'db>> = inner.into();
                self.len(binder.bound_vars().len());
                self.ty(binder.skip_binder())?;
            }
            TyKind::Dynamic(predicates, _) => {
                self.u8(16);
                // The order the bounds are written in is not the type's.
                let mut each: SmallVec<[ContentId; 4]> = SmallVec::new();
                for predicate in predicates.iter() {
                    let mut inner = Spell::new(db, BOUND_DOMAIN);
                    inner.len(predicate.bound_vars().len());
                    match predicate.skip_binder() {
                        ExistentialPredicate::Trait(it) => {
                            inner.u8(0);
                            inner.item(ItemId::Trait(it.def_id.0));
                            inner.args(it.args)?;
                        }
                        ExistentialPredicate::Projection(it) => {
                            inner.u8(1);
                            match it.def_id.0 {
                                TermId::TypeAliasId(alias) => inner.item(ItemId::Alias(alias)),
                                TermId::ConstId(konst) => inner.item(ItemId::Const(konst)),
                            }
                            inner.args(it.args)?;
                            match it.term.kind() {
                                TermKind::Ty(ty) => {
                                    inner.u8(0);
                                    inner.ty(ty)?;
                                }
                                TermKind::Const(konst) => {
                                    inner.u8(1);
                                    inner.konst(konst)?;
                                }
                            }
                        }
                        ExistentialPredicate::AutoTrait(it) => {
                            inner.u8(2);
                            inner.item(ItemId::Trait(it.0));
                        }
                    }
                    each.push(inner.out.finish());
                }
                each.sort_unstable();
                self.len(each.len());
                for it in &each {
                    self.id(it);
                }
            }
            TyKind::Closure(id, args) => {
                self.u8(17);
                self.key(key::closure_key(db, id.0));
                self.args(args)?;
            }
            TyKind::CoroutineClosure(id, args) => {
                self.u8(18);
                self.key(key::coroutine_closure_key(db, id.0));
                self.args(args)?;
            }
            // A coroutine's layout is of the locals its body keeps across its yields, which
            // its arguments do not say, and its debug info names them: its compiled code says
            // both.
            TyKind::Coroutine(id, args) => {
                self.u8(19);
                self.key(key::coroutine_key(db, id.0));
                let code = match is_by_move_coroutine(db, id.0, args.as_coroutine()) {
                    true => code::of_coroutine_by_move(db, id.0),
                    false => code::of_coroutine(db, id.0),
                };
                self.id(code.as_ref().map_err(Clone::clone)?);
                self.args(args)?;
            }
            TyKind::CoroutineWitness(id, args) => {
                self.u8(20);
                self.key(key::coroutine_key(db, id.0));
                self.id(code::of_coroutine(db, id.0).as_ref().map_err(Clone::clone)?);
                self.args(args)?;
            }
            TyKind::Never => self.u8(21),
            TyKind::Tuple(tys) => {
                self.u8(22);
                self.len(tys.len());
                for ty in tys.iter() {
                    self.ty(ty)?;
                }
            }
            TyKind::Param(param) => {
                self.u8(23);
                self.u32(param.index);
            }
            TyKind::Bound(index, bound) => {
                self.u8(24);
                self.bound_index(index);
                self.u32(bound.var.as_u32());
                self.u8(match bound.kind {
                    BoundTyKind::Anon => 0,
                    BoundTyKind::Param(_) => 1,
                });
            }
            TyKind::Alias(_, alias) => {
                let what = match alias.kind {
                    AliasTyKind::Projection { .. } => "a projection",
                    AliasTyKind::Inherent { .. } => "an inherent associated type",
                    AliasTyKind::Opaque { .. } => "an opaque type",
                    AliasTyKind::Free { .. } => "a type alias",
                };
                return Err(no_content(format!("{what} left unnormalized in `{ty:?}`"))); // alloc: error path
            }
            TyKind::Placeholder(_) => {
                return Err(no_content(format!("a placeholder type `{ty:?}`"))); // alloc: error path
            }
            TyKind::Infer(_) => {
                return Err(no_content(format!("an inference variable `{ty:?}`"))); // alloc: error path
            }
            TyKind::Error(_) => return Err(no_content("a type with an error".to_owned())), // alloc: error path
        }
        Ok(())
    }

    fn bound_index(&mut self, index: BoundVarIndexKind) {
        match index {
            BoundVarIndexKind::Bound(depth) => {
                self.u8(0);
                self.u32(depth.as_u32());
            }
            BoundVarIndexKind::Canonical => self.u8(1),
        }
    }

    fn args(&mut self, args: GenericArgs<'db>) -> Result<(), Unshared> {
        self.len(args.len());
        for arg in args.iter() {
            match arg.kind() {
                GenericArgKind::Lifetime(_) => self.u8(0),
                GenericArgKind::Type(ty) => {
                    self.u8(1);
                    self.ty(ty)?;
                }
                GenericArgKind::Const(konst) => {
                    self.u8(2);
                    self.konst(konst)?;
                }
            }
        }
        Ok(())
    }

    fn pattern(&mut self, pattern: Pattern<'db>) -> Result<(), Unshared> {
        match pattern.kind() {
            PatternKind::Range { start, end } => {
                self.u8(0);
                self.konst(start)?;
                self.konst(end)?;
            }
            PatternKind::Or(patterns) => {
                self.u8(1);
                self.len(patterns.len());
                for it in patterns.iter() {
                    self.pattern(it)?;
                }
            }
            PatternKind::NotNull => self.u8(2),
        }
        Ok(())
    }

    /// A constant by its value and its type's identity.
    fn konst(&mut self, konst: Const<'db>) -> Result<(), Unshared> {
        match konst.kind() {
            ConstKind::Value(value) => {
                self.u8(0);
                self.ty(value.ty)?;
                self.valtree(value.value)?;
            }
            ConstKind::Param(param) => {
                self.u8(1);
                self.u32(param.index);
            }
            ConstKind::Bound(index, bound) => {
                self.u8(2);
                self.bound_index(index);
                self.u32(bound.var.as_u32());
            }
            // As rustc normalizes it before codegen: `[u8; HALF_USIZE]` is `[u8; 4]`, one type,
            // one `TypeId`.
            ConstKind::Alias(_, alias) => {
                let value = self.evaluated(konst, alias)?;
                self.konst(value)?;
            }
            ConstKind::Expr(_) => {
                return Err(no_content(format!("the constant expression `{konst:?}`"))); // alloc: error path
            }
            ConstKind::Infer(InferConst::Var(_) | InferConst::Fresh(_)) => {
                return Err(no_content(format!("an inference variable `{konst:?}`"))); // alloc: error path
            }
            ConstKind::Placeholder(_) => {
                return Err(no_content(format!("a placeholder constant `{konst:?}`"))); // alloc: error path
            }
            ConstKind::Error(_) => {
                return Err(no_content("a constant with an error".to_owned())); // alloc: error path
            }
        }
        Ok(())
    }

    /// The value tree of a constant named by a path (a free, inherent or anonymous constant)
    /// with no generic parameter left, evaluated in its own crate.
    fn evaluated(&self, konst: Const<'db>, alias: AliasConst<'db>) -> Result<Const<'db>, Unshared> {
        let db = self.db;
        // alloc: error path
        let unevaluated = || no_content(format!("the constant `{konst:?}` is not evaluated"));
        if alias.args.has_non_region_param() || alias.args.has_infer() {
            return Err(unevaluated());
        }
        let (krate, allocation) = match alias.kind {
            AliasConstKind::Free { def_id: FreeConstAliasId(id) }
            | AliasConstKind::InherentImpl { def_id: InherentAssocConstId(id) } => {
                (id.module(db).krate(db), db.const_eval(id, alias.args, None))
            }
            AliasConstKind::Anon { def_id } => {
                (def_id.0.module(db).krate(db), db.anon_const_eval(def_id.0, alias.args, None))
            }
            // A trait's constant is its impl's, which needs the trait solver to find.
            AliasConstKind::Projection { .. } | AliasConstKind::InherentSelf { .. } => {
                return Err(unevaluated());
            }
        };
        // alloc: error path
        let allocation = allocation.map_err(|error| Unshared::ConstEval(format!("`{konst:?}`: {error:?}")))?;
        let env = ParamEnvAndCrate { param_env: ParamEnv::empty(), krate };
        Const::new_from_allocation(DbInterner::new_with(db, krate), &allocation, env)
            // alloc: error path
            .map_err(|error| no_content(format!("the constant `{konst:?}` has no value tree: {error:?}")))
    }

    fn valtree(&mut self, value: ValTree<'db>) -> Result<(), Unshared> {
        match value.kind() {
            ValTreeKind::Leaf(scalar) => {
                self.u8(0);
                self.out.update(&scalar.size().bytes().to_le_bytes());
                self.out.update(&scalar.to_bits_unchecked().to_le_bytes());
            }
            ValTreeKind::Branch(consts) => {
                self.u8(1);
                self.len(consts.len());
                for it in consts.iter() {
                    self.konst(it)?;
                }
            }
        }
        Ok(())
    }
}
