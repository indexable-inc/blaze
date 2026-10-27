//! An item's normalized form: what is hashed of it, with every item it refers to a slot.

use std::{cell::RefCell, fmt::Debug, rc::Rc};

use hir_def::{
    AdtId, AssocItemId, DefWithBodyId, GenericDefId, ItemContainerId, Lookup, MacroId, VariantId,
    attrs::AttrFlags,
    hir::generics::GenericParams,
    signatures::{
        ConstSignature, FnFlags, FunctionSignature, ImplSignature, StaticSignature, TraitSignature,
        TypeAliasSignature,
    },
};
use hir_ty::{
    GenericPredicates, ImplTraitId, InferBodyId, TyDefId,
    db::{GeneralConstId, HirDatabase, InternedClosure},
    mir::{
        AggregateKind, AsmOperand, AssertKind, BasicBlockId, LocalId, MirBody, Operand,
        OperandKind, ProjectionElem, Rvalue, StatementKind, StoredPlace, TerminatorKind,
    },
    next_solver::{Clause, Const, ConstKind, GenericArgs, Region, TermId, Ty, TyKind},
};
use rustc_hash::{FxHashMap, FxHashSet};
use rustc_type_ir::{
    AliasTermKind, AliasTy, AliasTyKind, BoundVarIndexKind, ClauseKind, ExistentialPredicate,
    GenericArgKind, OutlivesPredicate, RegionKind,
    inherent::{IntoKind, Safety as _, Term as _},
};
use syntax::AstNode;

use crate::ItemId;

pub type Hash = [u8; 32];

/// A form, hashed: its digest, what fills its slots in order, and why part of it could not be
/// written, which leaves its hash coarser than the item.
#[derive(Clone)]
pub(crate) struct Sub {
    pub(crate) digest: Hash,
    pub(crate) refs: Vec<ItemId>,
    pub(crate) missed: Option<&'static str>,
    /// Whether all it missed is a type or constant with an error, which is written again
    /// with the items the error is of.
    pub(crate) erred: bool,
    /// The places in its body its code is of, in the order of the code: two written alike
    /// with these alike have their places at the same nodes of their bodies.
    pub(crate) spans: Hash,
}

pub(crate) struct Form {
    /// `None` for a form written for what it refers to alone.
    hasher: Option<blake3::Hasher>,
    /// Of [`Sub::spans`], beside `hasher`.
    spans: Option<blake3::Hasher>,
    refs: Vec<ItemId>,
    slots: FxHashMap<ItemId, u32>,
    missed: Option<&'static str>,
    erred: bool,
}

impl Form {
    fn with(tag: &str, hashed: bool) -> Form {
        let mut form = Form {
            hasher: hashed.then(blake3::Hasher::new),
            spans: hashed.then(blake3::Hasher::new),
            refs: Vec::new(),
            slots: FxHashMap::default(),
            missed: None,
            erred: true,
        };
        form.str(tag);
        form
    }

    fn update(&mut self, bytes: &[u8]) {
        if let Some(hasher) = &mut self.hasher {
            hasher.update(bytes);
        }
    }

    fn u8(&mut self, value: u8) {
        self.update(&[value]);
    }

    fn u32(&mut self, value: u32) {
        self.update(&value.to_le_bytes());
    }

    fn u64(&mut self, value: u64) {
        self.update(&value.to_le_bytes());
    }

    fn u128(&mut self, value: u128) {
        self.update(&value.to_le_bytes());
    }

    fn len(&mut self, len: usize) {
        self.u64(len as u64);
    }

    fn bytes(&mut self, bytes: &[u8]) {
        self.len(bytes.len());
        self.update(bytes);
    }

    fn str(&mut self, text: &str) {
        self.bytes(text.as_bytes());
    }

    /// For what holds no id: what one build of blaze writes of it is the same in every run, and
    /// the store's keys hash what blaze was built from.
    fn debug(&mut self, value: &dyn Debug) {
        if self.hasher.is_some() {
            self.str(&format!("{value:?}"));
        }
    }

    /// The slot of `id`: the first item referred to is 0, the next other one 1.
    fn item(&mut self, id: ItemId) {
        let next = self.refs.len() as u32;
        let slot = *self.slots.entry(id).or_insert(next);
        if slot == next {
            self.refs.push(id);
        }
        self.u32(slot);
    }

    fn miss(&mut self, why: &'static str) {
        self.error(why);
        self.erred = false;
    }

    fn span(&mut self, span: hir_ty::mir::MirSpan) {
        use hir_ty::mir::MirSpan;
        let Some(spans) = &mut self.spans else { return };
        let (tag, at) = match span {
            MirSpan::ExprId(it) => (0u8, it.into_raw().into_u32()),
            MirSpan::ExprEnd(it) => (1, it.into_raw().into_u32()),
            MirSpan::PatId(it) => (2, it.into_raw().into_u32()),
            MirSpan::BindingId(it) => (3, it.into_raw().into_u32()),
            MirSpan::SelfParam => (4, 0),
            MirSpan::Unknown => (5, 0),
        };
        spans.update(&[tag]).update(&at.to_le_bytes());
    }

    fn error(&mut self, why: &'static str) {
        self.missed.get_or_insert(why);
        self.u8(0xff);
        self.str(why);
    }

    fn sub(&mut self, sub: &Sub) {
        self.update(&sub.digest);
        if let Some(spans) = &mut self.spans {
            spans.update(&sub.spans);
        }
        for &id in &sub.refs {
            self.item(id);
        }
        if let Some(why) = sub.missed {
            self.missed.get_or_insert(why);
            self.erred &= sub.erred;
        }
    }

    pub(crate) fn finish(self) -> Sub {
        let digest = self.hasher.map_or([0; 32], |it| *it.finalize().as_bytes());
        let spans = self.spans.map_or([0; 32], |it| *it.finalize().as_bytes());
        Sub {
            digest,
            spans,
            refs: self.refs,
            missed: self.missed,
            erred: self.missed.is_some() && self.erred,
        }
    }
}

/// What an item is written as: what its dependents see, and its body.
pub(crate) struct Node {
    pub(crate) interface: Sub,
    pub(crate) body: Option<Sub>,
}

pub(crate) struct Cx<'db> {
    db: &'db dyn HirDatabase,
    tys: RefCell<FxHashMap<Ty<'db>, Rc<Sub>>>,
    /// Whether forms are hashed, or written for what they refer to alone.
    hashed: bool,
}

/// The attributes that change what code is, or how it is linked or called: not the ones of
/// documentation, lints or completion.
fn attrs_that_matter() -> AttrFlags {
    AttrFlags::TRACK_CALLER
        | AttrFlags::NO_MANGLE
        | AttrFlags::HAS_TARGET_FEATURE
        | AttrFlags::THREAD_LOCAL
        | AttrFlags::IS_INLINE
        | AttrFlags::INLINE_NEVER
        | AttrFlags::INLINE_ALWAYS
        | AttrFlags::RUSTC_FORCE_INLINE
        | AttrFlags::USED
        | AttrFlags::RUSTC_INHERIT_OVERFLOW_CHECKS
        | AttrFlags::IS_TEST
        | AttrFlags::IS_BENCH
        | AttrFlags::IS_IGNORE
        | AttrFlags::IS_EXPORT_NAME_MAIN
        | AttrFlags::HAS_REPR
        | AttrFlags::RUSTC_LAYOUT_SCALAR_VALID_RANGE
        | AttrFlags::LINKED_BY_NAME
        | AttrFlags::RUSTC_INTRINSIC
        | AttrFlags::RUSTC_ALLOCATOR
        | AttrFlags::RUSTC_DEALLOCATOR
        | AttrFlags::RUSTC_REALLOCATOR
        | AttrFlags::RUSTC_ALLOCATOR_ZEROED
        | AttrFlags::FUNDAMENTAL
        | AttrFlags::RUSTC_PASS_INDIRECTLY_IN_NON_RUSTIC_ABIS
}

impl<'db> Cx<'db> {
    pub(crate) fn new(db: &'db dyn HirDatabase) -> Cx<'db> {
        Cx { db, tys: RefCell::default(), hashed: true }
    }

    /// A `Cx` whose forms say what they refer to and are not hashed.
    pub(crate) fn refs_only(db: &'db dyn HirDatabase) -> Cx<'db> {
        Cx { db, tys: RefCell::default(), hashed: false }
    }

    fn form(&self, tag: &str) -> Form {
        Form::with(tag, self.hashed)
    }

    pub(crate) fn node(&self, id: ItemId) -> Node {
        self.node_with(id, true)
    }

    /// [`Cx::node`], and without the body of a function or a static when `bodies` is false: what
    /// its dependents see is all there is of it then.
    pub(crate) fn node_with(&self, id: ItemId, bodies: bool) -> Node {
        let db = self.db;
        let mut form = self.form(id.kind());
        let f = &mut form;
        let mut body = None;
        match id {
            ItemId::Function(func) => {
                self.container(f, func.lookup(db).container);
                self.generics(f, func.into());
                let sig = db.callable_item_signature(func.into()).skip_binder().skip_binder();
                f.len(sig.inputs_and_output.len());
                for ty in sig.inputs_and_output.iter() {
                    self.ty(f, ty);
                }
                f.debug(&sig.abi());
                f.u8(sig.c_variadic() as u8);
                f.u8(sig.safety().is_safe() as u8);
                let data = FunctionSignature::of(db, func);
                f.u64(u64::from(data.flags.bits()));
                self.attrs(f, func.into());
                if bodies && data.flags.contains(FnFlags::HAS_BODY) {
                    body = Some(self.body(func.into()));
                }
            }
            ItemId::Adt(adt) => {
                self.generics(f, adt.into());
                f.debug(&AttrFlags::repr(db, adt));
                self.attrs(f, adt.into());
                match adt {
                    AdtId::StructId(it) => self.fields(f, it.into()),
                    AdtId::UnionId(it) => self.fields(f, it.into()),
                    AdtId::EnumId(it) => {
                        let variants = &it.enum_variants(db).variants;
                        f.len(variants.len());
                        for &(variant, _) in variants.values() {
                            self.fields(f, variant.into());
                            self.attrs(f, variant.into());
                            // An explicit discriminant is a body; a variant without one has none.
                            match db.mir_body(InferBodyId::DefWithBodyId(variant.into())) {
                                Ok(body) => {
                                    f.u8(1);
                                    self.mir(f, body);
                                }
                                Err(_) => f.u8(0),
                            }
                        }
                    }
                }
            }
            ItemId::Trait(it) => {
                self.generics(f, it.into());
                f.debug(&TraitSignature::of(db, it).flags);
                self.attrs(f, it.into());
                // A trait's items are named by what implements and calls them.
                let items = &it.trait_items(db).items;
                f.len(items.len());
                for &(ref name, item) in items.iter() {
                    f.str(name.as_str());
                    f.item(item.into());
                }
            }
            ItemId::Impl(it) => {
                self.generics(f, it.into());
                f.debug(&ImplSignature::of(db, it).flags);
                match db.impl_trait(it) {
                    Some(trait_ref) => {
                        let trait_ref = trait_ref.skip_binder();
                        f.u8(1);
                        f.item(ItemId::Trait(trait_ref.def_id.0));
                        self.args(f, trait_ref.args);
                        // What a projection through the impl is: its types and constants.
                        for &(ref name, item) in it.impl_items(db).items.iter() {
                            if !matches!(item, AssocItemId::FunctionId(_)) {
                                f.str(name.as_str());
                                f.item(item.into());
                            }
                        }
                    }
                    None => f.u8(0),
                }
                self.ty(f, db.impl_self_ty(it).skip_binder());
            }
            ItemId::Const(it) => {
                self.container(f, it.lookup(db).container);
                self.generics(f, it.into());
                self.ty(f, db.type_for_const(it).skip_binder());
                let data = ConstSignature::of(db, it);
                f.debug(&data.flags);
                self.attrs(f, it.into());
                // A constant's value is what its users see: it is in their code.
                if data.has_body() {
                    let value = self.body(it.into());
                    f.sub(&value);
                }
            }
            ItemId::Static(it) => {
                self.ty(f, db.type_for_static(it).skip_binder());
                let data = StaticSignature::of(db, it);
                f.debug(&data.flags);
                self.attrs(f, it.into());
                f.debug(&AttrFlags::export_name(db, it.into()));
                if bodies && data.flags.contains(hir_def::signatures::StaticFlags::HAS_BODY) {
                    body = Some(self.body(it.into()));
                }
            }
            ItemId::Alias(it) => {
                self.container(f, it.lookup(db).container);
                self.generics(f, it.into());
                f.debug(&TypeAliasSignature::of(db, it).flags);
                self.ty(f, db.ty(TyDefId::TypeAliasId(it)).skip_binder());
            }
            ItemId::Macro(it) => self.macro_(f, it),
        }
        if let ItemId::Function(func) = id {
            f.debug(&AttrFlags::export_name(db, func.into()));
            if AttrFlags::query(db, func.into()).contains(AttrFlags::HAS_TARGET_FEATURE) {
                let mut features: Vec<_> =
                    AttrFlags::target_features(db, func).iter().map(|it| it.as_str()).collect();
                features.sort_unstable();
                f.debug(&features);
            }
        }
        Node { interface: form.finish(), body }
    }

    fn body(&self, def: DefWithBodyId) -> Sub {
        let mut form = self.form("body");
        match self.db.mir_body(InferBodyId::DefWithBodyId(def)) {
            Ok(body) => self.mir(&mut form, body),
            Err(_) => form.miss("no MIR"),
        }
        form.finish()
    }

    /// A body with the coroutines written in it, `async` blocks and the body of an `async fn`:
    /// all the code it is, which [`Cx::body`] leaves them out of.
    pub(crate) fn code(&self, def: DefWithBodyId) -> Sub {
        let db = self.db;
        let mut form = self.form("code");
        form.sub(&self.body(def));
        let store = hir_def::expr_store::Body::of(db, def);
        let infer = hir_ty::InferenceResult::of(db, def);
        for (expr, it) in store.exprs() {
            if !matches!(it, hir_def::hir::Expr::Closure { .. }) {
                continue;
            }
            let id = match infer.expr_ty(expr).kind() {
                TyKind::Coroutine(id, _) => id.0,
                TyKind::Closure(..) | TyKind::CoroutineClosure(..) => continue,
                _ => {
                    form.miss("a closure of no closure type");
                    continue;
                }
            };
            match db.mir_body_for_coroutine(id) {
                Ok(body) => self.mir(&mut form, body),
                Err(_) => form.miss("no MIR for a coroutine"),
            }
        }
        form.finish()
    }

    /// What generic arguments refer to.
    pub(crate) fn of_args(&self, args: GenericArgs<'db>) -> Sub {
        let mut form = self.form("args");
        self.args(&mut form, args);
        form.finish()
    }

    fn container(&self, f: &mut Form, container: ItemContainerId) {
        match container {
            ItemContainerId::ModuleId(_) => f.u8(0),
            ItemContainerId::ImplId(it) => {
                f.u8(1);
                f.item(ItemId::Impl(it));
            }
            ItemContainerId::TraitId(it) => {
                f.u8(2);
                f.item(ItemId::Trait(it));
            }
            ItemContainerId::ExternBlockId(_) => f.u8(3),
        }
    }

    fn attrs(&self, f: &mut Form, owner: hir_def::AttrDefId) {
        let flags = AttrFlags::query(self.db, owner) & attrs_that_matter();
        f.u64(flags.bits() as u64);
    }

    fn generics(&self, f: &mut Form, def: GenericDefId) {
        let params = GenericParams::of(self.db, def);
        f.len(params.len_lifetimes());
        f.len(params.len_type_or_consts());
        for (_, param) in params.iter_type_or_consts() {
            f.u8(param.const_param().is_some() as u8);
        }
        self.clauses(f, GenericPredicates::query_all(self.db, def).skip_binder());
    }

    fn fields(&self, f: &mut Form, variant: VariantId) {
        let fields = self.db.field_types(variant);
        f.len(fields.iter().count());
        for (at, field) in fields.iter() {
            f.u32(at.into_raw().into_u32());
            self.ty(f, field.ty().skip_binder());
        }
    }

    /// A `macro_rules!` by its rules, without trivia and without its name.
    fn macro_(&self, f: &mut Form, id: MacroId) {
        let db = self.db;
        let tree = match id {
            MacroId::MacroRulesId(it) => it.lookup(db).id.to_node(db).token_tree(),
            MacroId::Macro2Id(it) => it.lookup(db).id.to_node(db).body(),
            MacroId::ProcMacroId(_) => return f.miss("a procedural macro is its function"),
        };
        let Some(tree) = tree else { return f.miss("a macro without rules") };
        for token in tree.syntax().descendants_with_tokens().filter_map(|it| it.into_token()) {
            if !token.kind().is_trivia() {
                f.str(token.text());
            }
        }
    }

    /// A type: its digest and slots are the same wherever it is written.
    fn ty(&self, f: &mut Form, ty: Ty<'db>) {
        let known = self.tys.borrow().get(&ty).cloned();
        let sub = match known {
            Some(it) => it,
            None => {
                let mut form = self.form("ty");
                self.spell_ty(&mut form, ty);
                let sub = Rc::new(form.finish());
                self.tys.borrow_mut().insert(ty, sub.clone());
                sub
            }
        };
        f.sub(&sub);
    }

    fn spell_ty(&self, f: &mut Form, ty: Ty<'db>) {
        let db = self.db;
        let kind = ty.kind();
        f.debug(&std::mem::discriminant(&kind));
        match kind {
            TyKind::Bool | TyKind::Char | TyKind::Str | TyKind::Never => {}
            TyKind::Int(it) => f.str(it.name_str()),
            TyKind::Uint(it) => f.str(it.name_str()),
            TyKind::Float(it) => f.str(it.name_str()),
            TyKind::Ref(region, pointee, mutability) => {
                self.region(f, region);
                f.u8(mutability.is_mut() as u8);
                self.ty(f, pointee);
            }
            TyKind::RawPtr(pointee, mutability) => {
                f.u8(mutability.is_mut() as u8);
                self.ty(f, pointee);
            }
            TyKind::Slice(element) => self.ty(f, element),
            TyKind::Array(element, len) => {
                self.ty(f, element);
                self.konst(f, len);
            }
            TyKind::Tuple(tys) => {
                f.len(tys.len());
                for ty in tys.iter() {
                    self.ty(f, ty);
                }
            }
            TyKind::Adt(adt, args) => {
                f.item(ItemId::Adt(adt.def_id()));
                self.args(f, args);
            }
            TyKind::FnDef(def, args) => {
                match def.0 {
                    hir_def::CallableDefId::FunctionId(it) => f.item(ItemId::Function(it)),
                    hir_def::CallableDefId::StructId(it) => f.item(ItemId::Adt(it.into())),
                    hir_def::CallableDefId::EnumVariantId(it) => self.variant(f, it.into()),
                }
                self.args(f, args);
            }
            TyKind::FnPtr(signature, header) => {
                f.debug(&header.abi());
                f.u8(header.safety().is_safe() as u8);
                f.u8(header.c_variadic() as u8);
                let tys = signature.skip_binder().inputs_and_output;
                f.len(tys.len());
                for ty in tys.iter() {
                    self.ty(f, ty);
                }
            }
            TyKind::Closure(id, args) => {
                self.closure(f, id.0.loc(db));
                self.args(f, args);
            }
            TyKind::CoroutineClosure(id, args) => {
                self.closure(f, id.0.loc(db));
                self.args(f, args);
            }
            TyKind::Coroutine(id, args) => {
                self.closure(f, id.0.loc(db));
                self.args(f, args);
            }
            TyKind::Dynamic(predicates, region) => {
                // In an order of their own: the list is sorted by ids of this run.
                let mut subs = Vec::new();
                for predicate in predicates.iter() {
                    let mut g = self.form("existential");
                    match predicate.skip_binder() {
                        ExistentialPredicate::Trait(it) => {
                            g.u8(0);
                            g.item(ItemId::Trait(it.def_id.0));
                            self.args(&mut g, it.args);
                        }
                        ExistentialPredicate::Projection(it) => {
                            g.u8(1);
                            match it.def_id.0 {
                                TermId::TypeAliasId(alias) => g.item(ItemId::Alias(alias)),
                                TermId::ConstId(konst) => g.item(ItemId::Const(konst)),
                            }
                            self.args(&mut g, it.args);
                            match it.term.as_type() {
                                Some(ty) => self.ty(&mut g, ty),
                                None => g.miss("a projection to a constant"),
                            }
                        }
                        ExistentialPredicate::AutoTrait(it) => {
                            g.u8(2);
                            g.item(ItemId::Trait(it.0));
                        }
                    }
                    subs.push(g.finish());
                }
                self.sorted(f, subs);
                self.region(f, region);
            }
            TyKind::Alias(AliasTy { kind: AliasTyKind::Projection { def_id }, args, .. }) => {
                f.item(ItemId::Alias(def_id.0));
                self.args(f, args);
            }
            TyKind::Alias(AliasTy { kind: AliasTyKind::Opaque { def_id }, args, .. }) => {
                match def_id.0.loc(db) {
                    ImplTraitId::ReturnTypeImplTrait(of, index) => {
                        f.item(ItemId::Function(of));
                        f.u32(index.into_raw().into_u32());
                    }
                    ImplTraitId::TypeAliasImplTrait(of, index) => {
                        f.item(ItemId::Alias(of));
                        f.u32(index.into_raw().into_u32());
                    }
                }
                self.args(f, args);
            }
            TyKind::Alias(_) => f.miss("an alias type"),
            TyKind::Param(param) => f.u32(param.index),
            TyKind::Bound(BoundVarIndexKind::Bound(depth), bound) => {
                f.u32(depth.as_u32());
                f.u32(bound.var.as_u32());
            }
            TyKind::Foreign(def_id) => f.item(ItemId::Alias(def_id.0)),
            TyKind::Error(_) => f.error("a type with an error"),
            _ => f.miss("another type"),
        }
    }

    /// A closure or coroutine by the body it is written in and where in it.
    fn closure(&self, f: &mut Form, closure: InternedClosure<'db>) {
        match self.owner(closure.owner) {
            Some(owner) => f.item(owner),
            None => f.miss("a closure in an anonymous constant"),
        }
        f.u32(closure.expr.into_raw().into_u32());
    }

    fn owner(&self, owner: InferBodyId<'db>) -> Option<ItemId> {
        let InferBodyId::DefWithBodyId(owner) = owner else { return None };
        Some(match owner {
            DefWithBodyId::FunctionId(it) => ItemId::Function(it),
            DefWithBodyId::ConstId(it) => ItemId::Const(it),
            DefWithBodyId::StaticId(it) => ItemId::Static(it),
            DefWithBodyId::VariantId(it) => ItemId::Adt(it.lookup(self.db).parent.into()),
        })
    }

    /// A variant by its type and its place in it: a variant's name is in what derives write.
    fn variant(&self, f: &mut Form, variant: VariantId) {
        match variant {
            VariantId::EnumVariantId(it) => {
                let parent = it.lookup(self.db).parent;
                f.item(ItemId::Adt(parent.into()));
                let place = parent
                    .enum_variants(self.db)
                    .variants
                    .values()
                    .position(|&(other, _)| other == it);
                f.u32(place.map_or(u32::MAX, |it| it as u32));
            }
            VariantId::StructId(it) => f.item(ItemId::Adt(it.into())),
            VariantId::UnionId(it) => f.item(ItemId::Adt(it.into())),
        }
    }

    fn region(&self, f: &mut Form, region: Region<'db>) {
        match region.kind() {
            RegionKind::ReBound(BoundVarIndexKind::Bound(depth), bound) => {
                f.u8(1);
                f.u32(depth.as_u32());
                f.u32(bound.var.as_u32());
            }
            RegionKind::ReEarlyParam(param) => {
                f.u8(2);
                f.u32(param.index);
            }
            RegionKind::ReStatic => f.u8(3),
            _ => f.u8(0),
        }
    }

    fn konst(&self, f: &mut Form, konst: Const<'db>) {
        match konst.kind() {
            ConstKind::Param(param) => {
                f.u8(1);
                f.u32(param.index);
            }
            ConstKind::Value(value) => {
                f.u8(2);
                self.ty(f, value.ty);
                f.debug(&value.value);
            }
            ConstKind::Unevaluated(unevaluated) => {
                f.u8(3);
                match unevaluated.def.0 {
                    GeneralConstId::ConstId(it) => f.item(ItemId::Const(it)),
                    GeneralConstId::StaticId(it) => f.item(ItemId::Static(it)),
                    // Written where it is used, it is as much a part of it as a closure is.
                    GeneralConstId::AnonConstId(it) => {
                        match self.db.mir_body(InferBodyId::AnonConstId(it)) {
                            Ok(body) => self.mir(f, body),
                            Err(_) => f.miss("no MIR for an anonymous constant"),
                        }
                    }
                }
                self.args(f, unevaluated.args);
            }
            ConstKind::Error(_) => f.error("a constant with an error"),
            _ => f.miss("another constant"),
        }
    }

    fn args(&self, f: &mut Form, args: GenericArgs<'db>) {
        f.len(args.len());
        for arg in args.iter() {
            match arg.kind() {
                GenericArgKind::Lifetime(region) => {
                    f.u8(0);
                    self.region(f, region);
                }
                GenericArgKind::Type(ty) => {
                    f.u8(1);
                    self.ty(f, ty);
                }
                GenericArgKind::Const(konst) => {
                    f.u8(2);
                    self.konst(f, konst);
                }
            }
        }
    }

    /// Forms in the order of their digests, which no order they are written in changes.
    fn sorted(&self, f: &mut Form, mut subs: Vec<Sub>) {
        subs.sort_by(|a, b| a.digest.cmp(&b.digest));
        f.len(subs.len());
        for sub in &subs {
            f.sub(sub);
        }
    }

    /// Where clauses as a set: reordering them changes nothing.
    fn clauses(&self, f: &mut Form, clauses: impl Iterator<Item = Clause<'db>>) {
        let subs = clauses
            .map(|clause| {
                let mut g = self.form("clause");
                self.clause(&mut g, clause);
                g.finish()
            })
            .collect();
        self.sorted(f, subs);
    }

    fn clause(&self, f: &mut Form, clause: Clause<'db>) {
        let kind = clause.kind().skip_binder();
        f.debug(&std::mem::discriminant(&kind));
        match kind {
            ClauseKind::Trait(it) => {
                f.item(ItemId::Trait(it.trait_ref.def_id.0));
                self.args(f, it.trait_ref.args);
                f.debug(&it.polarity);
            }
            ClauseKind::Projection(it) => {
                match it.projection_term.kind {
                    AliasTermKind::ProjectionTy { def_id } => f.item(ItemId::Alias(def_id.0)),
                    _ => f.miss("a projection of another kind"),
                }
                self.args(f, it.projection_term.args);
                match (it.term.as_type(), it.term.as_const()) {
                    (Some(ty), _) => self.ty(f, ty),
                    (_, Some(konst)) => self.konst(f, konst),
                    _ => f.miss("a projection to another term"),
                }
            }
            ClauseKind::TypeOutlives(OutlivesPredicate(ty, region)) => {
                self.ty(f, ty);
                self.region(f, region);
            }
            ClauseKind::RegionOutlives(OutlivesPredicate(a, b)) => {
                self.region(f, a);
                self.region(f, b);
            }
            ClauseKind::ConstArgHasType(konst, ty) => {
                self.konst(f, konst);
                self.ty(f, ty);
            }
            _ => f.miss("another clause"),
        }
    }

    /// A body as the code generator is given it: no names, no spans, blocks and locals by
    /// number in the order a walk from the start block reaches them.
    pub(crate) fn mir(&self, f: &mut Form, body: &MirBody<'db>) {
        let mut canon = Canon::new(body);
        self.walk(&mut self.form(""), &canon);
        canon.collecting = false;
        self.walk(f, &canon);
        // A closure is part of the body it is written in.
        f.len(body.closures.len());
        for &closure in &body.closures {
            match self.db.mir_body_for_closure(closure) {
                Ok(closure) => self.mir(f, closure),
                Err(_) => f.miss("no MIR for a closure"),
            }
        }
    }

    fn walk(&self, f: &mut Form, c: &Canon<'_, 'db>) {
        let body = c.body;
        self.local(f, c, LocalId::from_raw(0.into()));
        f.len(body.param_locals.len());
        for &local in &body.param_locals {
            self.local(f, c, local);
        }
        f.u8(body.inherits_overflow_checks as u8);
        f.debug(&body.phase);
        match body.coroutine {
            Some(coroutine) => {
                f.u8(1);
                self.local(f, c, coroutine.poll);
                self.local(f, c, coroutine.context);
            }
            None => f.u8(0),
        }
        f.len(c.order.len());
        for &id in &c.order {
            let block = &body.basic_blocks[id];
            f.u8(block.is_cleanup as u8);
            for (at, statement) in block.statements.iter().enumerate() {
                f.span(statement.span);
                if let StatementKind::StorageLive(local) | StatementKind::StorageDead(local) =
                    statement.kind
                    && !c.used(local)
                {
                    continue;
                }
                if let StatementKind::Nop = statement.kind {
                    continue;
                }
                self.statement(f, c, &statement.kind);
                if let Some(&unwind) = c.unwinds.get(&(id, at)) {
                    f.u8(0xfd);
                    self.block(f, c, Some(unwind));
                }
            }
            match &block.terminator {
                Some(terminator) => {
                    f.span(terminator.span);
                    self.terminator(f, c, &terminator.kind)
                }
                None => f.u8(0xfe),
            }
        }
        // What lowering keeps beside the blocks, which the code generator reads: the order
        // scopes drop their locals in, the temporaries promoted to constants, the captures of an
        // `async` body used in place, and the blocks no test leads to.
        let locals = |f: &mut Form, locals: &mut dyn Iterator<Item = LocalId>| {
            let used: Vec<_> = locals.filter(|&it| c.used(it)).collect();
            f.len(used.len());
            for local in used {
                self.local(f, c, local);
            }
        };
        locals(f, &mut body.scope_ends.iter().copied());
        locals(f, &mut body.promoted_constants.iter().copied());
        f.len(body.promoted_locals.len());
        for &(local, expr) in &body.promoted_locals {
            self.local(f, c, local);
            f.u32(expr.into_raw().into_u32());
        }
        f.len(body.upvars_in_place.len());
        for &(local, field) in &body.upvars_in_place {
            self.local(f, c, local);
            f.u32(field);
        }
        let unreached: Vec<_> = body.unreached.iter().filter_map(|it| c.blocks.get(it)).collect();
        f.len(unreached.len());
        for &block in unreached {
            f.u32(block);
        }
    }

    /// A local by the order the walk first meets it, with its type the first time: what
    /// lowering numbers it does not matter.
    fn local(&self, f: &mut Form, c: &Canon<'_, 'db>, local: LocalId) {
        if c.collecting {
            c.uses.borrow_mut().insert(local);
            return;
        }
        let mut numbers = c.numbers.borrow_mut();
        let next = numbers.len() as u32;
        let number = *numbers.entry(local).or_insert(next);
        drop(numbers);
        if number == next {
            f.u8(0);
            self.ty(f, c.body.locals[local].ty.as_ref());
        } else {
            f.u8(1);
            f.u32(number);
        }
    }

    fn place(&self, f: &mut Form, c: &Canon<'_, 'db>, place: &StoredPlace) {
        self.local(f, c, place.local);
        let projection = place.projection.as_slice();
        f.len(projection.len());
        for elem in projection {
            match *elem {
                ProjectionElem::Deref => f.u8(0),
                ProjectionElem::Field(field) => {
                    f.u8(1);
                    f.u32(field.0);
                }
                ProjectionElem::Index(local) => {
                    f.u8(2);
                    self.local(f, c, local);
                }
                ProjectionElem::ConstantIndex { offset, min_length, from_end } => {
                    f.u8(3);
                    f.u64(offset);
                    f.u64(min_length);
                    f.u8(from_end as u8);
                }
                ProjectionElem::Subslice { from, to } => {
                    f.u8(4);
                    f.u64(from);
                    f.u64(to);
                }
                ProjectionElem::Downcast(variant) => {
                    f.u8(5);
                    self.variant(f, variant);
                }
            }
        }
    }

    fn operand(&self, f: &mut Form, c: &Canon<'_, 'db>, operand: &Operand) {
        match &operand.kind {
            OperandKind::Copy(place) => {
                f.u8(0);
                self.place(f, c, place);
            }
            OperandKind::Move(place) => {
                f.u8(1);
                self.place(f, c, place);
            }
            OperandKind::Constant { konst, ty } => {
                f.u8(2);
                self.konst(f, konst.as_ref());
                self.ty(f, ty.as_ref());
            }
            OperandKind::Allocation { allocation } => {
                f.u8(3);
                let allocation = allocation.as_ref();
                self.ty(f, allocation.ty);
                f.bytes(&allocation.memory);
                let (pieces, vtables, statics) = allocation.memory_map.parts();
                let mut pieces = pieces;
                pieces.sort_unstable_by_key(|it| it.0);
                f.len(pieces.len());
                for (at, memory) in pieces {
                    f.u64(at as u64);
                    f.bytes(memory);
                }
                f.len(vtables.len());
                for &ty in vtables {
                    self.ty(f, ty);
                }
                f.len(statics.len());
                for (id, start, size) in statics {
                    f.item(ItemId::Static(id));
                    f.u64(start as u64);
                    f.u64(size as u64);
                }
            }
            OperandKind::Static(id) => {
                f.u8(4);
                f.item(ItemId::Static(*id));
            }
        }
    }

    fn operands(&self, f: &mut Form, c: &Canon<'_, 'db>, operands: &[Operand]) {
        f.len(operands.len());
        for operand in operands {
            self.operand(f, c, operand);
        }
    }

    fn rvalue(&self, f: &mut Form, c: &Canon<'_, 'db>, rvalue: &Rvalue) {
        f.debug(&std::mem::discriminant(rvalue));
        match rvalue {
            Rvalue::Use(operand) | Rvalue::UnaryOp(_, operand) => {
                if let Rvalue::UnaryOp(op, _) = rvalue {
                    f.debug(op);
                }
                self.operand(f, c, operand);
            }
            Rvalue::Repeat(operand, len) => {
                self.operand(f, c, operand);
                self.konst(f, len.as_ref());
            }
            Rvalue::Ref(kind, place) => {
                f.debug(kind);
                self.place(f, c, place);
            }
            Rvalue::Len(place) | Rvalue::Discriminant(place) | Rvalue::CopyForDeref(place) => {
                self.place(f, c, place)
            }
            Rvalue::Cast(kind, operand, ty) => {
                f.debug(kind);
                self.operand(f, c, operand);
                self.ty(f, ty.as_ref());
            }
            Rvalue::BinaryOp(op, a, b) | Rvalue::CheckedBinaryOp(op, a, b) => {
                f.debug(op);
                self.operand(f, c, a);
                self.operand(f, c, b);
            }
            Rvalue::Aggregate(kind, operands) => {
                match kind {
                    AggregateKind::Array(ty) | AggregateKind::Tuple(ty) => {
                        f.u8(0);
                        self.ty(f, ty.as_ref());
                    }
                    AggregateKind::Closure(ty) => {
                        f.u8(1);
                        self.ty(f, ty.as_ref());
                    }
                    AggregateKind::Adt(variant, args) => {
                        f.u8(2);
                        self.variant(f, *variant);
                        self.args(f, args.as_ref());
                    }
                    AggregateKind::Union(union, field) => {
                        f.u8(3);
                        f.item(ItemId::Adt((*union).into()));
                        f.u32(field.local_id.into_raw().into_u32());
                    }
                }
                self.operands(f, c, operands);
            }
            Rvalue::ThreadLocalRef(never) | Rvalue::AddressOf(never) | Rvalue::NullaryOp(never) => {
                match *never {}
            }
        }
    }

    fn statement(&self, f: &mut Form, c: &Canon<'_, 'db>, statement: &StatementKind) {
        f.debug(&std::mem::discriminant(statement));
        match statement {
            StatementKind::Assign(place, rvalue) => {
                self.place(f, c, place);
                self.rvalue(f, c, rvalue);
            }
            StatementKind::FakeRead(place) | StatementKind::Deinit(place) => {
                self.place(f, c, place)
            }
            StatementKind::StorageLive(local) | StatementKind::StorageDead(local) => {
                self.local(f, c, *local)
            }
            StatementKind::InlineAsm(asm) => {
                f.debug(&asm.template);
                f.debug(&asm.options);
                f.u8(asm.naked as u8);
                f.u8(asm.clobber_abi as u8);
                f.len(asm.operands.len());
                for operand in &asm.operands {
                    match operand {
                        AsmOperand::Reg { reg, input, output, written } => {
                            f.u8(0);
                            f.debug(reg);
                            f.u8(*written as u8);
                            if let Some(input) = input {
                                self.operand(f, c, input);
                            }
                            if let Some(output) = output {
                                self.place(f, c, output);
                            }
                        }
                        AsmOperand::Const(operand) => {
                            f.u8(1);
                            self.operand(f, c, operand);
                        }
                        AsmOperand::Label => f.u8(2),
                        AsmOperand::Sym(operand) => {
                            f.u8(3);
                            self.operand(f, c, operand);
                        }
                    }
                }
                if let Some(taken) = &asm.taken {
                    self.place(f, c, taken);
                }
            }
            StatementKind::Nop => {}
        }
    }

    fn block(&self, f: &mut Form, c: &Canon<'_, 'db>, block: Option<BasicBlockId>) {
        f.u32(block.map_or(u32::MAX, |it| c.blocks[&it]));
    }

    fn terminator(&self, f: &mut Form, c: &Canon<'_, 'db>, terminator: &TerminatorKind) {
        f.debug(&std::mem::discriminant(terminator));
        match terminator {
            TerminatorKind::Goto { target } => self.block(f, c, Some(*target)),
            TerminatorKind::SwitchInt { discr, targets } => {
                self.operand(f, c, discr);
                for (value, target) in targets.iter() {
                    f.u128(value);
                    self.block(f, c, Some(target));
                }
                self.block(f, c, Some(targets.otherwise()));
            }
            TerminatorKind::UnwindResume
            | TerminatorKind::Abort
            | TerminatorKind::Return
            | TerminatorKind::Unreachable
            | TerminatorKind::CoroutineDrop => {}
            TerminatorKind::Drop { place, target, unwind } => {
                self.place(f, c, place);
                self.block(f, c, Some(*target));
                self.block(f, c, *unwind);
            }
            TerminatorKind::DropAndReplace { place, value, target, unwind } => {
                self.place(f, c, place);
                self.operand(f, c, value);
                self.block(f, c, Some(*target));
                self.block(f, c, *unwind);
            }
            TerminatorKind::Call { func, args, destination, target, cleanup, from_hir_call } => {
                self.operand(f, c, func);
                self.operands(f, c, args);
                self.place(f, c, destination);
                self.block(f, c, *target);
                self.block(f, c, *cleanup);
                f.u8(*from_hir_call as u8);
            }
            TerminatorKind::Assert { cond, expected, msg, target, cleanup } => {
                self.operand(f, c, cond);
                f.u8(*expected as u8);
                f.debug(&std::mem::discriminant(msg));
                match msg {
                    AssertKind::BoundsCheck { len, index } => {
                        self.operand(f, c, len);
                        self.operand(f, c, index);
                    }
                    AssertKind::Overflow(op, a, b) => {
                        f.debug(op);
                        self.operand(f, c, a);
                        self.operand(f, c, b);
                    }
                    AssertKind::OverflowNeg(it)
                    | AssertKind::DivisionByZero(it)
                    | AssertKind::RemainderByZero(it) => self.operand(f, c, it),
                }
                self.block(f, c, Some(*target));
                self.block(f, c, *cleanup);
            }
            TerminatorKind::Yield { value, resume, resume_arg, drop } => {
                self.operand(f, c, value);
                self.block(f, c, Some(*resume));
                self.place(f, c, resume_arg);
                self.block(f, c, *drop);
            }
            TerminatorKind::FalseEdge { real_target, imaginary_target } => {
                self.block(f, c, Some(*real_target));
                self.block(f, c, Some(*imaginary_target));
            }
            TerminatorKind::FalseUnwind { real_target, unwind } => {
                self.block(f, c, Some(*real_target));
                self.block(f, c, *unwind);
            }
        }
    }
}

/// The order a body is written in. Blocks no walk from the start reaches, and the storage
/// marks of a local nothing else uses (`return 5;` has one where `return 5` has none), change
/// nothing the code does and are left out.
struct Canon<'b, 'db> {
    body: &'b MirBody<'db>,
    order: Vec<BasicBlockId>,
    blocks: FxHashMap<BasicBlockId, u32>,
    unwinds: FxHashMap<(BasicBlockId, usize), BasicBlockId>,
    /// The first walk only finds which locals are used.
    collecting: bool,
    uses: RefCell<FxHashSet<LocalId>>,
    numbers: RefCell<FxHashMap<LocalId, u32>>,
}

impl<'b, 'db> Canon<'b, 'db> {
    fn new(body: &'b MirBody<'db>) -> Self {
        let mut order = Vec::new();
        let mut blocks = FxHashMap::default();
        let mut stack = vec![body.start_block];
        while let Some(id) = stack.pop() {
            if blocks.contains_key(&id) {
                continue;
            }
            blocks.insert(id, order.len() as u32);
            order.push(id);
            if let Some(terminator) = &body.basic_blocks[id].terminator {
                let next = successors(&terminator.kind);
                stack.extend(next.into_iter().rev());
            }
        }
        let unwinds = body
            .panic_unwinds
            .iter()
            .filter(|it| blocks.contains_key(&it.0) && blocks.contains_key(&it.2))
            .map(|&(block, at, to)| ((block, at), to))
            .collect();
        Canon {
            body,
            order,
            blocks,
            unwinds,
            collecting: true,
            uses: RefCell::default(),
            numbers: RefCell::default(),
        }
    }

    fn used(&self, local: LocalId) -> bool {
        self.uses.borrow().contains(&local)
    }
}

fn successors(kind: &TerminatorKind) -> Vec<BasicBlockId> {
    match kind {
        TerminatorKind::Goto { target } => vec![*target],
        TerminatorKind::SwitchInt { targets, .. } => targets.all_targets().to_vec(),
        TerminatorKind::Drop { target, unwind, .. }
        | TerminatorKind::DropAndReplace { target, unwind, .. }
        | TerminatorKind::FalseUnwind { real_target: target, unwind } => {
            std::iter::once(*target).chain(*unwind).collect()
        }
        TerminatorKind::Call { target, cleanup, .. } => {
            target.iter().chain(cleanup).copied().collect()
        }
        TerminatorKind::Assert { target, cleanup, .. } => {
            std::iter::once(*target).chain(*cleanup).collect()
        }
        TerminatorKind::Yield { resume, drop, .. } => {
            std::iter::once(*resume).chain(*drop).collect()
        }
        TerminatorKind::FalseEdge { real_target, imaginary_target } => {
            vec![*real_target, *imaginary_target]
        }
        TerminatorKind::UnwindResume
        | TerminatorKind::Abort
        | TerminatorKind::Return
        | TerminatorKind::Unreachable
        | TerminatorKind::CoroutineDrop => Vec::new(),
    }
}

impl From<AssocItemId> for ItemId {
    fn from(item: AssocItemId) -> ItemId {
        match item {
            AssocItemId::FunctionId(it) => ItemId::Function(it),
            AssocItemId::ConstId(it) => ItemId::Const(it),
            AssocItemId::TypeAliasId(it) => ItemId::Alias(it),
        }
    }
}
