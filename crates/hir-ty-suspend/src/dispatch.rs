//! What a monomorphized body can run, site by site, as the code generator dispatches it: each
//! call and drop to its instances (the impl's method of a trait method with a concrete `Self`,
//! `Fn*` of a closure or function, drop glue, the `Future::poll` of an `async` block, a builtin
//! derive's calls on the fields), each vtable made to the impls in its slots, and each function
//! pointer made to what it points at.
//!
//! A call through `dyn` or a function pointer goes nowhere here: what it can reach is judged by
//! the marker `#[blaze::may_suspend]` on the trait method, or on the struct field the pointer
//! was loaded from, and the vtables and pointers made are where the promise of an unmarked one
//! is checked (`crate::check`).

use base_db::Db;
use blaze_hash::{HashMap, HashSet};
use blaze_query::db::{AsId, FromId};
use either::Either;
use hir_def::{
    AdtId, AssocItemId, CallableDefId, FieldId, FunctionId, GenericParamId, ItemContainerId,
    VariantId,
    attrs::AttrFlags,
    lang_item::{LangItems, lang_items},
    signatures::FunctionSignature,
};
use hir_expand::name::Name;
use hir_ty::{
    InferBodyId, ParamEnvAndCrate, PointerCast,
    db::{HirDatabase, InternedClosureId, InternedCoroutineId},
    drop::{DropGlue, destructor, has_drop_glue},
    dyn_compatibility::generics_require_sized_self,
    intrinsic::{Intrinsic, intrinsic_of},
    method_resolution::is_dyn_method,
    next_solver::{
        DbInterner, GenericArg, GenericArgs, Region, RegionExt as _, StoredGenericArgs, TraitRef,
        Ty, TyKind, TypingMode,
        infer::{DbInternerInferExt as _, InferCtxt},
    },
    traits::StoredParamEnvAndCrate,
};
use hir_ty_mir::mir::{
    BasicBlockId, CastKind, LocalId, MirBody, MirLowerError, MirSpan, Operand, OperandKind,
    PlaceTy, ProjectionElem, Rvalue, StatementKind, StoredPlace, TerminatorKind,
    is_by_move_coroutine, normalize_or_keep,
};
use intern::sym;
use rustc_type_ir::{
    Interner as _,
    inherent::{BoundExistentialPredicates as _, Clause as _, GenericArgs as _, IntoKind as _},
};
use smallvec::SmallVec;

/// The most type nodes an instance's generic arguments may have: one past it is a type that
/// grows without end (polymorphic recursion), which the walk reports instead of following.
pub(crate) const LONGEST: usize = 256;

/// How deep a builtin derive's calls on its fields are followed inline: past it, a chain of
/// derived types nested that deep is reported.
const DERIVE_DEPTH: u32 = 64;

/// What an instance runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Def {
    Fn(FunctionId),
    /// An `InternedClosureId`, without its database lifetime.
    Closure(blaze_query::db::Id),
    /// An `InternedCoroutineId`: the `Future::poll` of an `async` block.
    Coroutine(blaze_query::db::Id),
    /// The same of the future an `async` closure gives when called by value.
    CoroutineByMove(blaze_query::db::Id),
}

/// An instance: what it runs and its generic arguments (a closure's or coroutine's are its
/// parent's).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Instance {
    pub(crate) def: Def,
    pub(crate) args: StoredGenericArgs,
}

/// Where the walk can't see what a call runs, though it is not a `dyn` or pointer call: such an
/// instance is reported, never taken to suspend or not.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Unseen {
    /// The body does not lower to MIR.
    NoMir,
    /// A call of an `async` closure.
    AsyncClosure,
    /// A `dyn*`.
    DynStar,
    /// Generic arguments past [`LONGEST`] type nodes.
    TooLong,
    /// Builtin derives nested past [`DERIVE_DEPTH`].
    TooDeep,
}

impl Unseen {
    pub fn said(self) -> &'static str {
        match self {
            Unseen::NoMir => "its body does not lower to MIR",
            Unseen::AsyncClosure => "it calls an `async` closure, which the walk does not follow",
            Unseen::DynStar => "it makes a `dyn*`, which the walk does not follow",
            Unseen::TooLong => "it reaches an instance whose type arguments grow without end",
            Unseen::TooDeep => "it reaches builtin derives nested past 64 types",
        }
    }
}

/// A call the call graph can't see past, marked `#[blaze::may_suspend]`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Marked {
    /// A trait method called through `dyn`.
    Method(FunctionId),
    /// A function pointer loaded from this field.
    Field(FieldId),
}

/// What a site reaches.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Target {
    /// An instance with a body, walked.
    Walk(Instance),
    /// A root: the call that parks the fiber.
    Root(FunctionId),
    /// A marked `dyn` or pointer call: it may suspend.
    Marked(Marked),
}

/// A vtable made: an unsizing cast of `concrete` to `dyn Trait`.
pub(crate) struct Slot<'db> {
    pub(crate) span: MirSpan,
    pub(crate) concrete: Ty<'db>,
    /// The trait method of the slot; `None` for the vtable's drop.
    pub(crate) method: Option<FunctionId>,
    pub(crate) targets: SmallVec<[Target; 1]>,
}

/// A function pointer made of a function or closure.
pub(crate) struct Reified {
    pub(crate) span: MirSpan,
    pub(crate) targets: SmallVec<[Target; 1]>,
    /// Whether the pointer goes only into fields marked `#[blaze::may_suspend]`.
    pub(crate) into_marked: bool,
}

/// One body's sites.
pub(crate) struct Sites<'db> {
    /// The calls and drops, by the block they end, with what each reaches.
    pub(crate) calls: Vec<(BasicBlockId, SmallVec<[Target; 2]>)>,
    pub(crate) slots: Vec<Slot<'db>>,
    pub(crate) reified: Vec<Reified>,
    pub(crate) unseen: Option<Unseen>,
}

/// What the queries of one environment share.
pub(crate) struct Cx<'db> {
    pub(crate) db: &'db Db,
    pub(crate) interner: DbInterner<'db>,
    pub(crate) infcx: InferCtxt<'db>,
    pub(crate) env: ParamEnvAndCrate<'db>,
    pub(crate) stored: StoredParamEnvAndCrate,
    pub(crate) lang: &'db LangItems,
    /// The roots of this crate graph ([`crate::root::roots`]).
    pub(crate) roots: &'db [FunctionId],
}

impl<'db> Cx<'db> {
    pub(crate) fn new(db: &'db Db, stored: StoredParamEnvAndCrate) -> Cx<'db> {
        let env = stored.as_ref(db);
        let interner = DbInterner::new_with(db, env.krate);
        let infcx = interner.infer_ctxt().build(TypingMode::Codegen);
        let roots = crate::root::roots(db, env.krate);
        Cx { db, interner, infcx, env, stored, lang: lang_items(db, env.krate), roots }
    }

    pub(crate) fn normalize(&self, ty: Ty<'db>) -> Ty<'db> {
        normalize_or_keep(&self.infcx, self.env.param_env, ty)
    }
}

/// The monomorphized runtime body of `instance` in `env`.
pub(crate) fn body_of<'db>(
    db: &'db Db,
    env: &StoredParamEnvAndCrate,
    instance: &Instance,
) -> Result<&'db MirBody<'db>, MirLowerError<'db>> {
    let (args, stored) = (instance.args, *env);
    match instance.def {
        Def::Fn(func) => db.monomorphized_runtime_mir_body(InferBodyId::from(func), args, stored),
        Def::Closure(id) => {
            db.monomorphized_runtime_mir_body_for_closure(InternedClosureId::from_id(id), args, stored)
        }
        Def::Coroutine(id) => {
            db.monomorphized_runtime_mir_body_for_coroutine(InternedCoroutineId::from_id(id), args, stored)
        }
        Def::CoroutineByMove(id) => db.monomorphized_runtime_mir_body_for_coroutine_by_move(
            InternedCoroutineId::from_id(id),
            args,
            stored,
        ),
    }
}

/// The arguments of the function `func` with every lifetime erased; `None` when it has a type
/// or const parameter, its impl's or trait's included.
pub(crate) fn concrete_args<'db>(interner: DbInterner<'db>, func: FunctionId) -> Option<GenericArgs<'db>> {
    let mut generic = false;
    let args = GenericArgs::fill_rest(interner, func.into(), std::iter::empty(), |_, id, _| {
        generic |= !matches!(id, GenericParamId::LifetimeParamId(_));
        Region::new_erased(interner).into()
    });
    (!generic).then_some(args)
}

fn short(args: GenericArgs<'_>) -> bool {
    args.types().map(|it| it.walk().count()).sum::<usize>() <= LONGEST
}

/// The sites of `body`, every block's (cleanup blocks too), in block order.
pub(crate) fn sites<'db>(cx: &Cx<'db>, body: &MirBody<'db>) -> Sites<'db> {
    let mut scan = Scan {
        cx,
        body,
        out: Sites { calls: Vec::new(), slots: Vec::new(), reified: Vec::new(), unseen: None },
        loads: HashMap::default(),
        derive_depth: 0,
    };
    scan.loads();
    for (id, block) in body.basic_blocks.iter() {
        for statement in &block.statements {
            if let StatementKind::Assign(place, rvalue) = &statement.kind {
                scan.rvalue(place, rvalue, statement.span);
            }
        }
        let Some(terminator) = &block.terminator else { continue };
        let mut targets = SmallVec::new();
        match &terminator.kind {
            TerminatorKind::Call { func, .. } | TerminatorKind::TailCall { func, .. } => {
                scan.call_site(func, &mut targets);
            }
            TerminatorKind::Drop { place, .. } | TerminatorKind::DropAndReplace { place, .. } => {
                let ty = scan.place_ty(place);
                scan.drop_glue(ty, &mut HashSet::default(), &mut targets);
            }
            _ => continue,
        }
        if !targets.is_empty() {
            scan.out.calls.push((id, targets));
        }
    }
    scan.out
}

struct Scan<'a, 'db> {
    cx: &'a Cx<'db>,
    body: &'a MirBody<'db>,
    out: Sites<'db>,
    /// Each local assigned a copy or move of a place, as a whole: where a function pointer
    /// called through it was loaded from.
    loads: HashMap<LocalId, &'a StoredPlace>,
    derive_depth: u32,
}

impl<'a, 'db> Scan<'a, 'db> {
    fn unseen(&mut self, what: Unseen) {
        self.out.unseen.get_or_insert(what);
    }

    fn loads(&mut self) {
        for (_, block) in self.body.basic_blocks.iter() {
            for statement in &block.statements {
                let StatementKind::Assign(place, rvalue) = &statement.kind else { continue };
                if !place.projection.as_slice().is_empty() {
                    continue;
                }
                let from = match rvalue {
                    Rvalue::Use(operand) | Rvalue::Cast(CastKind::PointerCoercion(PointerCast::UnsafeFnPointer), operand, _) => {
                        operand
                    }
                    _ => continue,
                };
                if let OperandKind::Copy(from) | OperandKind::Move(from) = &from.kind {
                    self.loads.insert(place.local, from);
                }
            }
        }
    }

    fn place_ty(&self, place: &StoredPlace) -> Ty<'db> {
        let ty = place.as_ref().ty(self.body, &self.cx.infcx, self.cx.env.param_env).ty;
        self.cx.normalize(ty)
    }

    fn operand_ty(&self, operand: &Operand) -> Option<Ty<'db>> {
        match &operand.kind {
            OperandKind::Copy(place) | OperandKind::Move(place) => Some(self.place_ty(place)),
            OperandKind::Constant { ty, .. } => Some(ty.as_ref()),
            OperandKind::Allocation { allocation } => Some(allocation.as_ref().ty),
            OperandKind::Static(_) => None,
        }
    }

    /// The field marked `#[blaze::may_suspend]` that `place` is, if it is one.
    fn marked_field(&self, place: &StoredPlace) -> Option<FieldId> {
        marked_field(self.cx, self.body, place)
    }

    /// A call terminator's callee.
    fn call_site(&mut self, func: &Operand, out: &mut SmallVec<[Target; 2]>) {
        let ty = self.operand_ty(func);
        let callee = func.fn_def().or_else(|| match ty?.kind() {
            TyKind::FnDef(def, args) => Some((def.0, args.skip_binder())),
            _ => None,
        });
        match callee {
            Some((CallableDefId::FunctionId(f), args)) => self.call(f, args, out),
            Some(_) => {}
            None => {
                // A function pointer: may-suspend only when loaded from a marked field.
                let place = match &func.kind {
                    OperandKind::Copy(place) | OperandKind::Move(place) => place,
                    _ => return,
                };
                let from = match place.projection.as_slice() {
                    [] => self.loads.get(&place.local).copied(),
                    _ => Some(place),
                };
                if let Some(field) = from.and_then(|it| self.marked_field(it)) {
                    out.push(Target::Marked(Marked::Field(field)));
                }
            }
        }
    }

    fn walk(&mut self, def: Def, args: GenericArgs<'db>, out: &mut SmallVec<[Target; 2]>) {
        if !short(args) {
            return self.unseen(Unseen::TooLong);
        }
        out.push(Target::Walk(Instance { def, args: args.store() }));
    }

    /// What calling `f` with `args` runs.
    fn call(&mut self, f: FunctionId, args: GenericArgs<'db>, out: &mut SmallVec<[Target; 2]>) {
        let (db, lang) = (self.cx.db, self.cx.lang);
        if !short(args) {
            return self.unseen(Unseen::TooLong);
        }
        if Some(f) == lang.DropInPlace || Some(f) == lang.DropGlue {
            return self.drop_glue(args.type_at(0), &mut HashSet::default(), out);
        }
        match intrinsic_of(db, f) {
            Ok(Some(Intrinsic::ConstEvalSelect)) => {
                // `const_eval_select(args, const_fn, runtime_fn)` runs `runtime_fn`.
                if let Some(TyKind::FnDef(def, fn_args)) = args.types().nth(2).map(|it| it.kind())
                    && let CallableDefId::FunctionId(g) = def.0
                {
                    self.call(g, fn_args.skip_binder(), out);
                }
                return;
            }
            // No other intrinsic calls code.
            Ok(Some(_)) | Err(_) => return,
            Ok(None) => {}
        }
        if let ItemContainerId::TraitId(trait_) = f.loc(db).container {
            if [lang.Fn, lang.FnMut, lang.FnOnce].contains(&Some(trait_)) {
                return self.fn_trait(f, args, args.type_at(0), out);
            }
            if [lang.AsyncFn, lang.AsyncFnMut, lang.AsyncFnOnce].contains(&Some(trait_)) {
                return self.unseen(Unseen::AsyncClosure);
            }
        }
        self.resolved(f, args, out)
    }

    /// `Fn*::call*` for `Self` = `self_ty`.
    fn fn_trait(&mut self, f: FunctionId, args: GenericArgs<'db>, self_ty: Ty<'db>, out: &mut SmallVec<[Target; 2]>) {
        match self.cx.normalize(self_ty).kind() {
            TyKind::Ref(_, inner, _) => self.fn_trait(f, args, inner, out),
            TyKind::Closure(id, closure_args) => {
                let parent = GenericArgs::new_from_slice(closure_args.as_closure().parent_args());
                self.walk(Def::Closure(id.0.as_id()), parent, out);
            }
            TyKind::FnDef(def, fn_args) => {
                if let CallableDefId::FunctionId(g) = def.0 {
                    self.call(g, fn_args.skip_binder(), out);
                }
            }
            // Through a pointer or `dyn Fn*`: the promise of `Fn*`'s unmarked methods, checked
            // where the pointer or vtable is made.
            TyKind::FnPtr(..) | TyKind::Dynamic(..) => {}
            TyKind::CoroutineClosure(..) => self.unseen(Unseen::AsyncClosure),
            _ => self.resolved(f, args, out),
        }
    }

    /// A call of `f` past the dispatch on intrinsics, drop glue and `Fn*`.
    fn resolved(&mut self, f: FunctionId, args: GenericArgs<'db>, out: &mut SmallVec<[Target; 2]>) {
        let cx = self.cx;
        let db = cx.db;
        if Some(f) == cx.lang.FuturePoll
            && let TyKind::Coroutine(id, coroutine_args) = cx.normalize(args.type_at(0)).kind()
        {
            let coroutine = coroutine_args.as_coroutine();
            let def = match is_by_move_coroutine(db, id.0, coroutine) {
                true => Def::CoroutineByMove(id.0.as_id()),
                false => Def::Coroutine(id.0.as_id()),
            };
            let parent = GenericArgs::new_from_slice(coroutine.parent_args());
            return self.walk(def, parent, out);
        }
        if is_dyn_method(cx.interner, cx.env.param_env, f, args).is_some() {
            if AttrFlags::query(db, f.into()).contains(AttrFlags::MAY_SUSPEND) {
                out.push(Target::Marked(Marked::Method(f)));
            }
            return;
        }
        let (callee, callee_args) = db.lookup_impl_method(cx.env, f, args);
        let Either::Left(g) = callee else {
            return self.derived(f, args, out);
        };
        if g == f && let ItemContainerId::TraitId(trait_) = f.loc(db).container {
            if Some(trait_) == cx.lang.Clone {
                return self.clone_builtin(f, args.type_at(0), out);
            }
            if Some(trait_) == cx.lang.FnPtrTrait || !FunctionSignature::of(db, f).has_body() {
                return;
            }
        }
        self.function(g, callee_args, out)
    }

    /// A call of the function `g` itself.
    fn function(&mut self, g: FunctionId, args: GenericArgs<'db>, out: &mut SmallVec<[Target; 2]>) {
        let db = self.cx.db;
        if self.cx.roots.contains(&g) {
            return out.push(Target::Root(g));
        }
        // A foreign function calls Rust only through pointers it is handed, made and checked here.
        if matches!(g.loc(db).container, ItemContainerId::ExternBlockId(_))
            || !FunctionSignature::of(db, g).has_body()
        {
            return;
        }
        self.walk(Def::Fn(g), args, out)
    }

    /// A builtin derive's method `f` (`Clone`, `PartialEq`, `Hash`, `Debug`, ...) for `Self`:
    /// the same method on each field, `Self` replaced by the field's type.
    fn derived(&mut self, f: FunctionId, args: GenericArgs<'db>, out: &mut SmallVec<[Target; 2]>) {
        if self.derive_depth >= DERIVE_DEPTH {
            return self.unseen(Unseen::TooDeep);
        }
        let cx = self.cx;
        let self_ty = cx.normalize(args.type_at(0));
        let TyKind::Adt(def, adt_args) = self_ty.kind() else { return };
        self.derive_depth += 1;
        for variant in variants(cx.db, def.def_id()) {
            for (_, field) in cx.db.field_types(variant).iter() {
                let field = cx.normalize(field.ty().instantiate(cx.interner, adt_args).skip_norm_wip());
                // alloc: inline up to four arguments, a derive's call per field
                let replaced: SmallVec<[GenericArg<'db>; 4]> = args
                    .iter()
                    .map(|arg| match arg.ty() {
                        Some(ty) if cx.normalize(ty) == self_ty => GenericArg::from(field),
                        _ => arg,
                    })
                    .collect();
                self.call(f, GenericArgs::new_from_slice(&replaced), out);
            }
        }
        self.derive_depth -= 1;
    }

    /// `Clone::clone` of a type whose impl the compiler supplies: tuples, closures and arrays
    /// clone their parts; the rest is `Copy`.
    fn clone_builtin(&mut self, clone: FunctionId, ty: Ty<'db>, out: &mut SmallVec<[Target; 2]>) {
        let ty = self.cx.normalize(ty);
        let mut part = |this: &mut Self, part: Ty<'db>| {
            this.call(clone, GenericArgs::new_from_slice(&[GenericArg::from(part)]), out);
        };
        match ty.kind() {
            TyKind::Tuple(fields) => fields.iter().for_each(|it| part(self, it)),
            TyKind::Closure(..) | TyKind::CoroutineClosure(..) => {
                if let Some(TyKind::Tuple(fields)) = ty.closure_upvars_ty().map(|it| it.kind()) {
                    fields.iter().for_each(|it| part(self, it));
                }
            }
            TyKind::Array(element, _) => part(self, element),
            _ => {}
        }
    }

    /// Drop glue of `ty`: its `Drop` impl's `drop`, then its fields'.
    fn drop_glue(&mut self, ty: Ty<'db>, seen: &mut HashSet<Ty<'db>>, out: &mut SmallVec<[Target; 2]>) {
        let cx = self.cx;
        let db = cx.db;
        let ty = cx.normalize(ty);
        if !seen.insert(ty) || has_drop_glue(&cx.infcx, ty, cx.env.param_env) == DropGlue::None {
            return;
        }
        match ty.kind() {
            TyKind::Adt(def, args) => {
                let adt = def.def_id();
                if destructor(db, adt).is_some()
                    && let Some(drop) = cx
                        .lang
                        .Drop
                        .and_then(|it| it.trait_items(db).method_by_name(&Name::new_symbol_root(sym::drop)))
                {
                    self.call(drop, GenericArgs::new_from_slice(&[GenericArg::from(ty)]), out);
                }
                if let AdtId::StructId(it) = adt
                    && cx.lang.OwnedBox == Some(it)
                {
                    self.drop_glue(args.type_at(0), seen, out);
                }
                for variant in variants(db, adt) {
                    for (_, field) in db.field_types(variant).iter() {
                        self.drop_glue(field.ty().instantiate(cx.interner, args).skip_norm_wip(), seen, out);
                    }
                }
            }
            TyKind::Tuple(fields) => fields.iter().for_each(|it| self.drop_glue(it, seen, out)),
            TyKind::Array(element, _) | TyKind::Slice(element) => self.drop_glue(element, seen, out),
            TyKind::Closure(_, args) => self.drop_glue(args.as_closure().tupled_upvars_ty(), seen, out),
            // A `dyn`'s drop is its vtable's, checked where the vtable is made.
            _ => {}
        }
    }

    fn rvalue(&mut self, place: &'a StoredPlace, rvalue: &Rvalue, span: MirSpan) {
        let Rvalue::Cast(kind, operand, to) = rvalue else { return };
        match kind {
            CastKind::PointerCoercion(PointerCast::ReifyFnPointer) => {
                if let Some(TyKind::FnDef(def, args)) = self.operand_ty(operand).map(|it| it.kind())
                    && let CallableDefId::FunctionId(f) = def.0
                {
                    let mut targets = SmallVec::new();
                    self.call(f, args.skip_binder(), &mut targets);
                    self.reified(place, span, targets);
                }
            }
            CastKind::PointerCoercion(PointerCast::ClosureFnPointer(_)) => {
                if let Some(TyKind::Closure(id, args)) = self.operand_ty(operand).map(|it| it.kind()) {
                    let parent = GenericArgs::new_from_slice(args.as_closure().parent_args());
                    let mut targets = SmallVec::new();
                    self.walk(Def::Closure(id.0.as_id()), parent, &mut targets);
                    self.reified(place, span, targets);
                }
            }
            CastKind::PointerCoercion(PointerCast::Unsize) => {
                if let Some(from) = self.operand_ty(operand) {
                    self.unsize(from, to.as_ref(), span);
                }
            }
            CastKind::DynStar => self.unseen(Unseen::DynStar),
            _ => {}
        }
    }

    fn reified(&mut self, place: &StoredPlace, span: MirSpan, targets: SmallVec<[Target; 2]>) {
        if targets.is_empty() {
            return;
        }
        let into_marked = match place.projection.as_slice() {
            [] => only_into_marked(self.cx, self.body, place.local),
            _ => self.marked_field(place).is_some(),
        };
        self.out.reified.push(Reified { span, targets: targets.into_iter().collect(), into_marked });
    }

    /// The vtables an unsizing coercion of `from` to `to` makes.
    fn unsize(&mut self, from: Ty<'db>, to: Ty<'db>, span: MirSpan) {
        let cx = self.cx;
        let (from, to) = (cx.normalize(from), cx.normalize(to));
        if from == to {
            return;
        }
        match (from.kind(), to.kind()) {
            (
                TyKind::RawPtr(from, _) | TyKind::Ref(_, from, _),
                TyKind::RawPtr(to, _) | TyKind::Ref(_, to, _),
            ) => self.unsize_pointee(from, to, span),
            (TyKind::Pat(from, _), TyKind::Pat(to, _)) => self.unsize(from, to, span),
            (TyKind::Adt(def, args), TyKind::Adt(to_def, to_args)) if def.def_id() == to_def.def_id() => {
                let AdtId::StructId(id) = def.def_id() else { return };
                for (_, field) in cx.db.field_types(id.into()).iter() {
                    let from = field.ty().instantiate(cx.interner, args).skip_norm_wip();
                    let to = field.ty().instantiate(cx.interner, to_args).skip_norm_wip();
                    self.unsize(from, to, span);
                }
            }
            _ => {}
        }
    }

    fn unsize_pointee(&mut self, from: Ty<'db>, to: Ty<'db>, span: MirSpan) {
        let cx = self.cx;
        let (from, to) = (cx.normalize(from), cx.normalize(to));
        if from == to {
            return;
        }
        match (from.kind(), to.kind()) {
            // An upcast: the vtable it starts from was checked where it was made.
            (TyKind::Dynamic(..), TyKind::Dynamic(..)) | (TyKind::Array(..), TyKind::Slice(_)) => {}
            (_, TyKind::Dynamic(..)) => self.vtable(from, to, span),
            (TyKind::Adt(def, args), TyKind::Adt(to_def, to_args)) if def.def_id() == to_def.def_id() => {
                let AdtId::StructId(id) = def.def_id() else { return };
                let fields = cx.db.field_types(id.into());
                let Some((_, last)) = fields.iter().last() else { return };
                let from = last.ty().instantiate(cx.interner, args).skip_norm_wip();
                let to = last.ty().instantiate(cx.interner, to_args).skip_norm_wip();
                self.unsize_pointee(from, to, span);
            }
            _ => {}
        }
    }

    /// The vtable of `concrete` for `dyn_ty`: its drop, and every method of the principal
    /// trait and its supertraits that a trait object can call.
    fn vtable(&mut self, concrete: Ty<'db>, dyn_ty: Ty<'db>, span: MirSpan) {
        let cx = self.cx;
        let mut drop = SmallVec::new();
        self.drop_glue(concrete, &mut HashSet::default(), &mut drop);
        if !drop.is_empty() {
            self.out.slots.push(Slot { span, concrete, method: None, targets: drop.into_iter().collect() });
        }
        let TyKind::Dynamic(bounds, _) = dyn_ty.kind() else { return };
        let Some(principal) = bounds.principal() else { return };
        let trait_ref =
            cx.interner.instantiate_bound_regions_with_erased(principal.with_self_ty(cx.interner, concrete));
        let mut seen = HashSet::default();
        self.trait_methods(trait_ref, concrete, span, &mut seen);
    }

    fn trait_methods(
        &mut self,
        trait_ref: TraitRef<'db>,
        concrete: Ty<'db>,
        span: MirSpan,
        seen: &mut HashSet<TraitRef<'db>>,
    ) {
        if !seen.insert(trait_ref) {
            return;
        }
        let cx = self.cx;
        let (db, interner) = (cx.db, cx.interner);
        let supertraits: SmallVec<[TraitRef<'db>; 4]> = interner
            .explicit_super_clauses_of(trait_ref.def_id)
            .iter_instantiated(interner, trait_ref.args.as_slice())
            .filter_map(|clause| clause.skip_norm_wip().0.as_trait_clause())
            .map(|pred| interner.instantiate_bound_regions_with_erased(pred).trait_ref)
            .collect(); // alloc: inline up to four supertraits, all read before any is walked
        for supertrait in supertraits {
            self.trait_methods(supertrait, concrete, span, seen);
        }
        let trait_items = trait_ref.def_id.0.trait_items(db);
        for &(_, item) in trait_items.items.iter() {
            let AssocItemId::FunctionId(func) = item else { continue };
            if generics_require_sized_self(db, func.into()) {
                continue;
            }
            let mut generic = false;
            let args = GenericArgs::fill_rest(interner, func.into(), trait_ref.args.iter(), |_, id, _| {
                generic |= !matches!(id, GenericParamId::LifetimeParamId(_));
                Region::new_erased(interner).into()
            });
            // A method with type or const parameters of its own has no slot.
            if generic {
                continue;
            }
            let mut targets = SmallVec::new();
            self.resolved(func, args, &mut targets);
            if !targets.is_empty() {
                self.out.slots.push(Slot { span, concrete, method: Some(func), targets: targets.into_iter().collect() });
            }
        }
    }
}

/// The field marked `#[blaze::may_suspend]` that `place` ends in, if it ends in one.
pub(crate) fn marked_field<'db>(cx: &Cx<'db>, body: &MirBody<'db>, place: &StoredPlace) -> Option<FieldId> {
    let (ProjectionElem::Field(field), parent) = place.projection.as_slice().split_last()? else {
        return None;
    };
    let parent = PlaceTy::from_ty(body.locals[place.local].ty.as_ref()).multi_projection_ty(
        &cx.infcx,
        cx.env.param_env,
        parent,
    );
    let variant = match parent.variant_id {
        Some(variant) => variant,
        None => match cx.normalize(parent.ty).kind() {
            TyKind::Adt(def, _) => VariantId::from_non_enum(def.def_id())?,
            _ => return None,
        },
    };
    let id = FieldId { parent: variant, local_id: field.to_local_field_id() };
    AttrFlags::query_field(cx.db, id).contains(AttrFlags::MAY_SUSPEND).then_some(id)
}

/// Whether every use of `local`, a function pointer just made, puts it into a marked field: a
/// struct literal's or an assignment's.
fn only_into_marked<'db>(cx: &Cx<'db>, body: &MirBody<'db>, local: LocalId) -> bool {
    let is_local = |operand: &Operand| {
        matches!(&operand.kind, OperandKind::Copy(it) | OperandKind::Move(it)
            if it.local == local && it.projection.as_slice().is_empty())
    };
    let mut used = false;
    for (_, block) in body.basic_blocks.iter() {
        for statement in &block.statements {
            let StatementKind::Assign(place, rvalue) = &statement.kind else { continue };
            if place.local == local && place.projection.as_slice().is_empty() {
                continue;
            }
            match rvalue {
                Rvalue::Aggregate(hir_ty_mir::mir::AggregateKind::Adt(variant, _), operands) => {
                    for (at, operand) in operands.iter().enumerate() {
                        if !is_local(operand) {
                            continue;
                        }
                        used = true;
                        let id = FieldId {
                            parent: *variant,
                            local_id: hir_ty_mir::mir::FieldIndex(at as u32).to_local_field_id(),
                        };
                        if !AttrFlags::query_field(cx.db, id).contains(AttrFlags::MAY_SUSPEND) {
                            return false;
                        }
                    }
                }
                Rvalue::Use(operand) if is_local(operand) => {
                    used = true;
                    if marked_field(cx, body, place).is_none() {
                        return false;
                    }
                }
                _ => {
                    let mut other = false;
                    rvalue_operands(rvalue, |operand| other |= is_local(operand));
                    if other {
                        return false;
                    }
                }
            }
        }
        if let Some(terminator) = &block.terminator {
            let mut other = false;
            terminator.kind.for_each_operand(&mut |operand| other |= is_local(operand));
            if other {
                return false;
            }
        }
    }
    used
}

fn rvalue_operands(rvalue: &Rvalue, mut each: impl FnMut(&Operand)) {
    for operand in hir_ty_mir::mir::purity::operands(rvalue) {
        each(operand);
    }
}

/// The variants of `adt`: its one for a struct or union.
pub(crate) fn variants(db: &Db, adt: AdtId) -> SmallVec<[VariantId; 4]> {
    match adt {
        AdtId::StructId(it) => SmallVec::from_slice(&[VariantId::StructId(it)]),
        AdtId::UnionId(it) => SmallVec::from_slice(&[VariantId::UnionId(it)]),
        AdtId::EnumId(it) => it
            .enum_variants(db)
            .variants
            .values()
            .map(|&(id, _)| VariantId::EnumVariantId(id))
            .collect(), // alloc: inline up to four variants
    }
}
