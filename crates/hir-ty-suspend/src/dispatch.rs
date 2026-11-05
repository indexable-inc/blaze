//! What a monomorphized body can run, site by site, as the code generator dispatches it
//! (`hir_ty_mir::mir::dispatch`): each call and drop to its instances (the impl's method of a
//! trait method with a concrete `Self`, `Fn*` of a closure or function, drop glue, the
//! `Future::poll` of an `async` block, a builtin derive's calls on the fields), each vtable made
//! to the impls in its slots, and each function pointer made to what it points at.
//!
//! A call through `dyn` or a function pointer goes nowhere here: what it can reach is judged by
//! the marker `#[blaze::may_suspend]` on the trait method, or on the struct field the pointer
//! was loaded from, and the vtables and pointers made are where the promise of an unmarked one
//! is checked (`crate::check`).

use base_db::Db;
use blaze_hash::{HashMap, HashSet};
use blaze_query::db::AsId;
use hir_def::{
    CallableDefId, FieldId, FunctionId, GenericParamId, ItemContainerId, VariantId,
    attrs::AttrFlags,
    lang_item::{LangItems, lang_items},
    signatures::FunctionSignature,
};
use hir_ty::{
    ParamEnvAndCrate, PointerCast,
    db::HirDatabase as _,
    next_solver::{
        DbInterner, GenericArg, GenericArgs, Region, RegionExt as _, StoredGenericArgs, Ty, TyKind,
        TypingMode,
        infer::{DbInternerInferExt as _, InferCtxt},
    },
    traits::StoredParamEnvAndCrate,
};
use hir_ty_mir::mir::{
    BasicBlockId, CastKind, LocalId, MirBody, MirSpan, Operand, OperandKind, PlaceTy,
    ProjectionElem, Rvalue, StatementKind, StoredPlace, TerminatorKind,
    dispatch::{
        self, Body, Call, Callee, DropStep, UnsizeStep, clone_parts, variants, vtable_methods,
        within_longest,
    },
    normalize_or_keep,
};
use rustc_type_ir::inherent::{GenericArgs as _, IntoKind as _};
use smallvec::SmallVec;
use std::alloc::Global;

/// How deep a builtin derive's calls on its fields are followed inline: past it, a chain of
/// derived types nested that deep is reported.
const DERIVE_DEPTH: u32 = 64;

/// An instance: the body it runs and its generic arguments (a closure's or coroutine's are its
/// parent's).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Instance {
    pub(crate) def: Body,
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
    /// Generic arguments past [`dispatch::LONGEST`] type nodes.
    TooLong,
    /// Builtin derives nested past [`DERIVE_DEPTH`].
    TooDeep,
    /// A call of a trait method that no impl's method is found for, where one should be.
    NoImplMethod,
}

impl Unseen {
    pub fn said(self) -> &'static str {
        match self {
            Unseen::NoMir => "its body does not lower to MIR",
            Unseen::AsyncClosure => "it calls an `async` closure, which the walk does not follow",
            Unseen::DynStar => "it makes a `dyn*`, which the walk does not follow",
            Unseen::TooLong => "it reaches an instance whose type arguments grow without end",
            Unseen::TooDeep => "it reaches builtin derives nested past 64 types",
            Unseen::NoImplMethod => "it calls a trait method that no impl's method is found for",
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
        let roots: &[FunctionId] = &crate::root::roots(db, env.krate).found;
        Cx { db, interner, infcx, env, stored, lang: lang_items(db, env.krate), roots }
    }

    pub(crate) fn normalize(&self, ty: Ty<'db>) -> Ty<'db> {
        normalize_or_keep(&self.infcx, self.env.param_env, ty)
    }
}

/// The arguments of the function `func` with every lifetime erased; `None` when it has a type
/// or const parameter, its impl's or trait's included.
pub(crate) fn concrete_args<'db>(
    interner: DbInterner<'db>,
    func: FunctionId,
) -> Option<GenericArgs<'db>> {
    let mut generic = false;
    let args = GenericArgs::fill_rest(interner, func.into(), std::iter::empty(), |_, id, _| {
        generic |= !matches!(id, GenericParamId::LifetimeParamId(_));
        Region::new_erased(interner).into()
    });
    (!generic).then_some(args)
}

/// The sites of `body`, every block's (cleanup blocks too), in block order.
pub(crate) fn sites<'db>(cx: &Cx<'db>, body: &MirBody<'db>) -> Sites<'db> {
    let mut scan = Scan {
        cx,
        body,
        out: Sites { calls: Vec::new(), slots: Vec::new(), reified: Vec::new(), unseen: None },
        loads: HashMap::new_in(Global),
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
                scan.drop_glue(ty, &mut HashSet::new_in(Global), &mut targets);
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
    loads: HashMap<LocalId, &'a StoredPlace, Global>,
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
                    Rvalue::Use(operand)
                    | Rvalue::Cast(
                        CastKind::PointerCoercion(PointerCast::UnsafeFnPointer),
                        operand,
                        _,
                    ) => operand,
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

    fn walk(&mut self, def: Body, args: GenericArgs<'db>, out: &mut SmallVec<[Target; 2]>) {
        if !within_longest(args) {
            return self.unseen(Unseen::TooLong);
        }
        out.push(Target::Walk(Instance { def, args: args.store() }));
    }

    /// What calling `f` with `args` runs.
    fn call(&mut self, f: FunctionId, args: GenericArgs<'db>, out: &mut SmallVec<[Target; 2]>) {
        if !within_longest(args) {
            return self.unseen(Unseen::TooLong);
        }
        match dispatch::call(&self.cx.infcx, self.cx.env, f, args) {
            Call::DropGlue(ty) => self.drop_glue(ty, &mut HashSet::new_in(Global), out),
            Call::ConstEvalSelect(Some((CallableDefId::FunctionId(g), g_args))) => {
                self.call(g, g_args, out)
            }
            // No other intrinsic calls code.
            Call::ConstEvalSelect(_) | Call::Intrinsic(_) | Call::UnknownIntrinsic(_) => {}
            Call::Closure(def, parent) => self.walk(def, parent, out),
            Call::FnItem(CallableDefId::FunctionId(g), g_args) => self.call(g, g_args, out),
            // Through a pointer or `dyn Fn*`: the promise of `Fn*`'s unmarked methods, checked
            // where the pointer or vtable is made.
            Call::FnItem(..) | Call::Indirect => {}
            Call::AsyncClosure => self.unseen(Unseen::AsyncClosure),
            Call::Method(Err(_)) => self.unseen(Unseen::NoImplMethod),
            Call::Method(Ok(callee)) => self.method(f, args, callee, out),
        }
    }

    /// A call of `f` past the dispatch on intrinsics, drop glue and `Fn*`.
    fn method(
        &mut self,
        f: FunctionId,
        args: GenericArgs<'db>,
        callee: Callee<'db>,
        out: &mut SmallVec<[Target; 2]>,
    ) {
        match callee {
            Callee::Poll(def, parent) => self.walk(def, parent, out),
            Callee::Virtual => {
                if AttrFlags::query(self.cx.db, f.into()).contains(AttrFlags::MAY_SUSPEND) {
                    out.push(Target::Marked(Marked::Method(f)));
                }
            }
            Callee::Derived(..) => self.derived(f, args, out),
            Callee::CloneBuiltin(_) => self.clone_builtin(f, args.type_at(0), out),
            Callee::FnPtr(_) => {}
            Callee::Fn(g, g_args) => self.function(g, g_args, out),
        }
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
        self.walk(Body::Fn(g), args, out)
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
                let field =
                    cx.normalize(field.ty().instantiate(cx.interner, adt_args).skip_norm_wip());
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

    /// `Clone::clone` of a type whose impl the compiler supplies: each part cloned, and the ones
    /// cloned before dropped when one unwinds.
    fn clone_builtin(&mut self, clone: FunctionId, ty: Ty<'db>, out: &mut SmallVec<[Target; 2]>) {
        let Some(parts) = clone_parts(&self.cx.infcx, self.cx.env, ty) else { return };
        for part in parts {
            self.call(clone, GenericArgs::new_from_slice(&[GenericArg::from(part)]), out);
            self.drop_glue(part, &mut HashSet::new_in(Global), out);
        }
    }

    /// Drop glue of `ty`: its `Drop` impl's `drop`, then its fields'.
    fn drop_glue(
        &mut self,
        ty: Ty<'db>,
        seen: &mut HashSet<Ty<'db>, Global>,
        out: &mut SmallVec<[Target; 2]>,
    ) {
        let mut steps = SmallVec::new();
        dispatch::drop_glue(&self.cx.infcx, self.cx.env, ty, seen, &mut steps);
        for step in steps {
            // A `dyn`'s drop is its vtable's, checked where the vtable is made.
            if let DropStep::Drop(drop, args) = step {
                self.call(drop, args, out);
            }
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
                if let Some(TyKind::Closure(id, args)) =
                    self.operand_ty(operand).map(|it| it.kind())
                {
                    let parent = GenericArgs::new_from_slice(args.as_closure().parent_args());
                    let mut targets = SmallVec::new();
                    self.walk(Body::Closure(id.0.as_id()), parent, &mut targets);
                    self.reified(place, span, targets);
                }
            }
            CastKind::PointerCoercion(PointerCast::Unsize) => {
                if let Some(from) = self.operand_ty(operand) {
                    let mut steps = SmallVec::new();
                    dispatch::unsized_vtables(
                        &self.cx.infcx,
                        self.cx.env,
                        from,
                        to.as_ref(),
                        &mut steps,
                    );
                    for step in steps {
                        if let UnsizeStep::Vtable(concrete, dyn_ty) = step {
                            self.vtable(concrete, dyn_ty, span);
                        }
                    }
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
        self.out.reified.push(Reified {
            span,
            targets: targets.into_iter().collect(),
            into_marked,
        });
    }

    /// The vtable of `concrete` for `dyn_ty`: its drop, and every method of the principal
    /// trait and its supertraits that a trait object can call.
    fn vtable(&mut self, concrete: Ty<'db>, dyn_ty: Ty<'db>, span: MirSpan) {
        let mut drop = SmallVec::new();
        self.drop_glue(concrete, &mut HashSet::new_in(Global), &mut drop);
        if !drop.is_empty() {
            self.out.slots.push(Slot {
                span,
                concrete,
                method: None,
                targets: drop.into_iter().collect(),
            });
        }
        for (func, args) in vtable_methods(self.cx.interner, concrete, dyn_ty) {
            let mut targets = SmallVec::new();
            self.call(func, args, &mut targets);
            if !targets.is_empty() {
                self.out.slots.push(Slot {
                    span,
                    concrete,
                    method: Some(func),
                    targets: targets.into_iter().collect(),
                });
            }
        }
    }
}

/// The field marked `#[blaze::may_suspend]` that `place` ends in, if it ends in one.
pub(crate) fn marked_field<'db>(
    cx: &Cx<'db>,
    body: &MirBody<'db>,
    place: &StoredPlace,
) -> Option<FieldId> {
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
