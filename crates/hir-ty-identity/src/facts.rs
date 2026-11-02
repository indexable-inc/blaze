//! The facts one monomorphized body reads past its own code, streamed into a hash in body
//! order: every type its locals and places have (each type's identity the first time, its
//! index among the types seen after), the types an ADT's fields hold after normalization (a
//! field `<T as Tr>::Out` is the impl's type, which no ADT identity reads), each constant's
//! value and the memory it points to, each static it names, and each call's target as the code
//! generator's dispatch resolves it, by item key and arguments, as its symbol names it. The
//! same walk lists the body's [`Edge`]s, what it can run beyond its own code, in body order.

use base_db::Db;
use blaze_hash::{HashMap, HashSet};
use blaze_ids::Hasher;
use blaze_items::{ItemId, interface_hash, item_key, key};
use blaze_query::db::FromId;
use hir_def::{
    AdtId, CallableDefId, FunctionId, ItemContainerId, StaticId, UnionId, VariantId,
    attrs::AttrFlags,
    signatures::{StaticFlags, StaticSignature},
};
use hir_ty::{
    ComplexMemoryMap, MemoryMap, ParamEnvAndCrate, PointerAt, PointerCast,
    consteval::try_const_usize,
    layout::Layout,
    mir::VTableMap,
    utils::detect_variant_from_bytes,
    db::{HirDatabase, InternedClosureId, InternedCoroutineId},
    method_resolution::lookup_impl_const,
    next_solver::{
        AliasConst, AliasConstKind, Allocation, AllocationData, Const, ConstKind, DbInterner,
        FreeConstAliasId, GenericArgs, InherentAssocConstId, TraitAssocConstId, Ty, TyKind,
        TypingMode,
        infer::{DbInternerInferExt as _, InferCtxt},
    },
    traits::StoredParamEnvAndCrate,
};
use hir_ty_mir::mir::{
    AggregateKind, AsmOperand, CastKind, MirBody, Operand, OperandKind, PlaceTy, Rvalue,
    StatementKind, StoredPlace, TerminatorKind,
    dispatch::{self, Body, Call, Callee, LONGEST, UnsizeStep, variants, vtable_methods, within_longest},
    normalize_or_keep,
};
use hir_ty_share::identity as share;
use rustc_type_ir::inherent::{GenericArgs as _, IntoKind as _, Ty as _};
use rustc_abi::TargetDataLayout;
use smallvec::SmallVec;
use std::{alloc::Global, borrow::Cow};

/// The most types one body's facts may hold: past it the walk stops with an error naming the
/// body, never runs on (rule 15).
const TYPES: u32 = 1 << 20;

/// What the queries of one environment share.
pub(crate) struct Cx<'db> {
    pub(crate) db: &'db Db,
    pub(crate) interner: DbInterner<'db>,
    pub(crate) infcx: InferCtxt<'db>,
    pub(crate) env: ParamEnvAndCrate<'db>,
}

impl<'db> Cx<'db> {
    pub(crate) fn new(db: &'db Db, stored: &StoredParamEnvAndCrate) -> Cx<'db> {
        let env = stored.as_ref(db);
        let interner = DbInterner::new_with(db, env.krate);
        let infcx = interner.infer_ctxt().build(TypingMode::Codegen);
        Cx { db, interner, infcx, env }
    }

    pub(crate) fn normalize(&self, ty: Ty<'db>) -> Ty<'db> {
        normalize_or_keep(&self.infcx, self.env.param_env, ty)
    }

    /// `ty` with every alias in it normalized, or why it can't be
    /// ([`hir_ty_share::deeply_normalize`]): what a type is spelled from.
    pub(crate) fn deeply_normalize(&self, ty: Ty<'db>) -> Result<Ty<'db>, hir_ty_share::Unshared> {
        hir_ty_share::deeply_normalize(&self.infcx, self.env.param_env, ty)
    }
}

/// What a body can run beyond its own code, in body order: the edges of a compute step's walk.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Edge<'db> {
    /// A call, a function item made a pointer, a function pointer a constant holds, or a
    /// `sym` operand: what calling the function with the arguments runs, as dispatch says.
    Call(FunctionId, GenericArgs<'db>),
    /// A closure made a function pointer, or one a constant points to: its body.
    Closure(Body, GenericArgs<'db>),
    /// A value of the type dropped: its drop glue.
    Drop(Ty<'db>),
    /// A vtable of the concrete type (the first) for the `dyn` type (the second).
    Vtable(Ty<'db>, Ty<'db>),
}

/// One body's facts and edges.
pub(crate) struct Facts<'a, 'db> {
    pub(crate) cx: &'a Cx<'db>,
    pub(crate) h: Hasher,
    tys: HashMap<Ty<'db>, u32, Global>,
    seen_statics: HashSet<StaticId, Global>,
    pub(crate) edges: Vec<Edge<'db>>,
    /// The first fact that has no identity, which stops the walk.
    pub(crate) failed: Option<String>,
}

impl<'a, 'db> Facts<'a, 'db> {
    pub(crate) fn new(cx: &'a Cx<'db>, h: Hasher) -> Facts<'a, 'db> {
        Facts {
            cx,
            h,
            tys: HashMap::new_in(Global),
            seen_statics: HashSet::new_in(Global),
            edges: Vec::new(), // alloc: one body's edges, this query's miss, memoized after
            failed: None,
        }
    }

    fn fail(&mut self, what: impl FnOnce() -> String) {
        if self.failed.is_none() {
            self.failed = Some(what());
        }
    }

    /// Every fact of `body`, in its order: its locals' types, then each block's statements and
    /// terminator, each with the types of the places it names, the values of its operands and
    /// the targets of what it calls, makes a pointer of, unsizes or drops.
    pub(crate) fn body(&mut self, body: &MirBody<'db>) {
        for (_, local) in body.locals.iter() {
            self.ty(local.ty.as_ref());
        }
        for (_, block) in body.basic_blocks.iter() {
            for statement in &block.statements {
                MirBody::visit_statement_places(&statement.kind, &mut |place| self.place(body, place));
                statement.kind.for_each_operand(&mut |operand| self.operand(operand));
                match &statement.kind {
                    StatementKind::Assign(_, rvalue) => self.rvalue(body, rvalue),
                    StatementKind::InlineAsm(asm) => {
                        for operand in &asm.operands {
                            if let AsmOperand::Sym(operand) = operand {
                                self.h.update(&[10]);
                                self.call_site(body, operand);
                            }
                        }
                    }
                    StatementKind::FakeRead(_)
                    | StatementKind::Deinit(_)
                    | StatementKind::StorageLive(_)
                    | StatementKind::StorageDead(_)
                    | StatementKind::Nop => {}
                }
            }
            let Some(terminator) = &block.terminator else { continue };
            MirBody::visit_terminator_places(&terminator.kind, &mut |place| self.place(body, place));
            terminator.kind.for_each_operand(&mut |operand| self.operand(operand));
            match &terminator.kind {
                TerminatorKind::Call { func, .. } | TerminatorKind::TailCall { func, .. } => {
                    self.h.update(&[11]);
                    self.call_site(body, func);
                }
                TerminatorKind::Drop { place, .. } | TerminatorKind::DropAndReplace { place, .. } => {
                    let ty = self.place_ty(body, place);
                    self.drop(ty);
                }
                _ => {}
            }
        }
    }

    /// The type of every place a projection of `place` passes through: the code generator
    /// reads each one's layout to find the next.
    fn place(&mut self, body: &MirBody<'db>, place: &StoredPlace) {
        let place = place.as_ref();
        let mut ty = PlaceTy::from_ty(body.locals[place.local].ty.as_ref());
        for elem in place.projection.as_slice() {
            ty = ty.projection_ty(&self.cx.infcx, elem, self.cx.env.param_env);
            self.ty(ty.ty);
        }
    }

    fn place_ty(&self, body: &MirBody<'db>, place: &StoredPlace) -> Ty<'db> {
        let ty = place.as_ref().ty(body, &self.cx.infcx, self.cx.env.param_env).ty;
        self.cx.normalize(ty)
    }

    fn operand_ty(&self, body: &MirBody<'db>, operand: &Operand) -> Option<Ty<'db>> {
        match &operand.kind {
            OperandKind::Copy(place) | OperandKind::Move(place) => Some(self.place_ty(body, place)),
            OperandKind::Constant { ty, .. } => Some(ty.as_ref()),
            OperandKind::Allocation { allocation } => Some(allocation.as_ref().ty),
            OperandKind::Static(_) => None,
        }
    }

    fn rvalue(&mut self, body: &MirBody<'db>, rvalue: &Rvalue) {
        match rvalue {
            Rvalue::Cast(kind, operand, to) => {
                let to = to.as_ref();
                self.ty(to);
                match kind {
                    CastKind::PointerCoercion(PointerCast::ReifyFnPointer) => {
                        self.h.update(&[12]);
                        match self.operand_ty(body, operand).map(|it| it.kind()) {
                            Some(TyKind::FnDef(def, args)) => self.callable(def.0, args.skip_binder()),
                            _ => self.fail(|| "makes a function pointer of what is not a function item".to_owned()), // alloc: error path
                        }
                    }
                    CastKind::PointerCoercion(PointerCast::ClosureFnPointer(_)) => {
                        self.h.update(&[13]);
                        match self.operand_ty(body, operand).map(|it| it.kind()) {
                            Some(TyKind::Closure(id, args)) => {
                                let parent = GenericArgs::new_from_slice(args.as_closure().parent_args());
                                self.closure(Body::Closure(blaze_query::db::AsId::as_id(&id.0)), parent);
                            }
                            _ => self.fail(|| "makes a function pointer of a closure not known".to_owned()), // alloc: error path
                        }
                    }
                    CastKind::PointerCoercion(PointerCast::Unsize) => match self.operand_ty(body, operand) {
                        Some(from) => self.unsize(from, to),
                        None => self.fail(|| "unsizes the address of a static".to_owned()), // alloc: error path
                    },
                    CastKind::DynStar => self.fail(|| "makes a `dyn*`, which has no identity".to_owned()), // alloc: error path
                    _ => {}
                }
            }
            Rvalue::Aggregate(AggregateKind::Array(ty) | AggregateKind::Tuple(ty) | AggregateKind::Closure(ty), _) => {
                self.ty(ty.as_ref());
            }
            _ => {}
        }
    }

    fn operand(&mut self, operand: &Operand) {
        match &operand.kind {
            OperandKind::Copy(_) | OperandKind::Move(_) => {}
            OperandKind::Static(id) => self.static_use(*id),
            OperandKind::Constant { konst, ty } => {
                let (konst, ty) = (konst.as_ref(), ty.as_ref());
                self.ty(ty);
                if matches!(konst.kind(), ConstKind::Alias(..)) {
                    match evaluate(self.cx, konst) {
                        Ok(value) => {
                            self.h.update(&[2]);
                            self.allocation(&value, ty, Holder::Const(konst));
                        }
                        Err(what) => self.fail(|| what),
                    }
                } else {
                    match share::konst(self.cx.db, konst) {
                        Ok(id) => {
                            self.h.update(&[3]).id(&id);
                        }
                        Err(it) => self.fail(|| format!("the constant `{konst:?}` has no identity: {it}")), // alloc: error path
                    }
                }
            }
            OperandKind::Allocation { allocation } => {
                let value = allocation.as_ref();
                self.ty(value.ty);
                self.h.update(&[4]);
                self.allocation(&value, value.ty, Holder::Body);
            }
        }
    }

    /// A type: its identity the first time, its index among the types seen after, and the
    /// types its fields hold. A pointer's pointee is not walked: its layout is read where the
    /// pointer is dereferenced, a place whose type is walked there. The type is normalized all
    /// through first, as the code generator has it: a field instantiated with its ADT's
    /// arguments can hold a projection inside (addr2line's
    /// `Load<<MappedLookup<..> as LookupContinuation>::Buf>`). One that does not normalize
    /// stops the walk, naming it. Its free regions are erased, error regions too (inference
    /// leaves one wherever it pinned no lifetime): no fact reads a lifetime, so `&'{region
    /// error} T` and `&'static T` are one type here, as in the code generator's vtables.
    pub(crate) fn ty(&mut self, ty: Ty<'db>) {
        let ty = match self.cx.deeply_normalize(ty) {
            Ok(ty) => crate::instance::erased(self.cx.interner, ty),
            Err(why) => return self.fail(|| format!("the type `{ty:?}` has no identity: {why}")), // alloc: error path
        };
        if let Some(&index) = self.tys.get(&ty) {
            self.h.update(&[0]).update(&index.to_le_bytes());
            return;
        }
        let index = self.tys.len() as u32;
        if index == TYPES {
            return self.fail(|| format!("its facts hold more than {TYPES} types")); // alloc: error path
        }
        self.tys.insert(ty, index);
        match share::ty(self.cx.db, ty) {
            Ok(id) => {
                self.h.update(&[1]).id(&id);
            }
            Err(it) => return self.fail(|| format!("the type `{ty:?}` has no identity: {it}")), // alloc: error path
        }
        let (db, interner) = (self.cx.db, self.cx.interner);
        match ty.kind() {
            TyKind::Adt(def, args) => {
                for variant in variants(db, def.def_id()) {
                    for (_, field) in db.field_types(variant).iter() {
                        self.ty(field.ty().instantiate(interner, args).skip_norm_wip());
                    }
                }
            }
            TyKind::Tuple(fields) => fields.iter().for_each(|it| self.ty(it)),
            TyKind::Array(element, _) | TyKind::Slice(element) | TyKind::Pat(element, _) => self.ty(element),
            TyKind::Closure(_, args) => self.ty(args.as_closure().tupled_upvars_ty()),
            TyKind::CoroutineClosure(_, args) => self.ty(args.as_coroutine_closure().tupled_upvars_ty()),
            TyKind::Coroutine(_, args) => self.ty(args.as_coroutine().tupled_upvars_ty()),
            _ => {}
        }
    }

    /// A static: its key, its flags and its value, which a compute step reads: the code the
    /// value points to is the step's too.
    pub(crate) fn static_use(&mut self, id: StaticId) {
        if !self.seen_statics.insert(id) {
            return;
        }
        let db = self.cx.db;
        self.h.update(&[5]).id(&item_key(db, ItemId::Static(id)).0);
        let signature = StaticSignature::of(db, id);
        let thread_local = AttrFlags::query(db, id.into()).contains(AttrFlags::THREAD_LOCAL);
        self.h.update(&[signature.flags.bits(), thread_local as u8]);
        if signature.flags.contains(StaticFlags::EXTERN) {
            let link = AttrFlags::link_name(db, id.into()).unwrap_or_else(|| signature.name.symbol().clone()); // clone: a handle of an interned name
            self.h.part(link.as_str().as_bytes());
            return;
        }
        match db.const_eval_static(id) {
            Ok(value) => {
                let ty = db.type_for_static(id).skip_binder();
                self.ty(ty);
                self.allocation(&value, ty, Holder::Static(id));
            }
            Err(error) => self.fail(|| {
                format!("reads `{}`, whose value does not evaluate: {error:?}", signature.name.as_str()) // alloc: error path
            }),
        }
    }

    /// An evaluated value of `ty`, which `holder` holds: its bytes and every piece of memory,
    /// function pointer, vtable, type id and static it holds.
    pub(crate) fn allocation(&mut self, value: &AllocationData<'db>, ty: Ty<'db>, holder: Holder<'db>) {
        self.h.part(&value.memory);
        let map = match &value.memory_map {
            MemoryMap::Empty => {
                self.h.update(&[0]);
                return;
            }
            MemoryMap::Simple(bytes) => {
                self.h.update(&[1]).part(bytes);
                return;
            }
            MemoryMap::Complex(map) => map,
        };
        self.h.update(&[2]).update(&(map.memory.iter().len() as u64).to_le_bytes());
        for (address, bytes) in map.memory.iter() {
            self.h.update(&(*address as u64).to_le_bytes()).part(bytes);
        }
        self.h.update(&(map.type_ids.len() as u64).to_le_bytes());
        for type_id in &map.type_ids {
            match type_id {
                Some(it) => {
                    self.h.update(&[1]);
                    self.ty(*it);
                }
                None => {
                    self.h.update(&[0]);
                }
            }
        }
        self.h.update(&(map.statics.len() as u64).to_le_bytes());
        for memory in &map.statics {
            self.h.update(&(memory.start as u64).to_le_bytes()).update(&(memory.size as u64).to_le_bytes());
            self.static_use(memory.id);
        }
        let pointees = map.vtable.tys();
        self.h.update(&(pointees.len() as u64).to_le_bytes());
        if pointees.len() == 0 {
            return;
        }
        // One table holds function pointers and vtables, and a function item's or closure's id
        // is both its pointer and its vtable (`&index as &dyn Handler` stores `FnDef(index)`,
        // as `index as fn(_)` does), so every value with a pointee is walked for the `dyn`s
        // its ids are vtables of.
        let walked = VtableWalk::new(self.cx, &value.memory_map).and_then(|mut walk| {
            walk.walk(&value.memory, At { kept: None, raw: false }, 0, ty)?;
            walk.ids_left(&value.memory, ty)?;
            Ok((walk.found, walk.word))
        });
        let (found, word) = match walked {
            Ok(it) => it,
            Err(why) => {
                let db = self.cx.db;
                return self.fail(|| format!("uses {}, whose vtables can't be looked for: {why}", holder.describe(db, ty))); // alloc: error path
            }
        };
        for (index, pointee) in pointees.enumerate() {
            let id = VTableMap::FIRST_ID + index;
            let function = matches!(pointee.kind(), TyKind::FnDef(..) | TyKind::Closure(..));
            if function {
                // What its pointer runs, used as a pointer or not.
                self.pointee(pointee);
            }
            match found.get(&id) {
                Some(dyns) => {
                    self.h.update(&[2]).update(&(dyns.len() as u64).to_le_bytes());
                    for &dyn_ty in dyns {
                        self.vtable(pointee, dyn_ty);
                    }
                }
                // A function item or closure no `dyn` names: a function pointer only.
                // FIXME(comptime): one made a `dyn` in a union of two non-empty fields or a
                // coroutine's capture is hashed as a pointer only, as its `dyn` is at no place
                // the walk types; refuse it or find its trait there.
                None if function => {
                    self.h.update(&[2]).update(&0u64.to_le_bytes());
                }
                // An id no word of the value holds: a vtable a temporary of the evaluation made
                // (const eval keeps its whole table), which nothing can emit.
                None if !holds_word(value, map, word, id) => {
                    self.h.update(&[3]);
                }
                // A vtable a word of the value holds at no place whose type names its trait:
                // one in a union of two non-empty fields, a coroutine's capture, an unsafe binder or a `DynMetadata` of
                // no `dyn`. Its trait is not known, so the code its slots run
                // is not either (rule 3).
                None => {
                    let db = self.cx.db;
                    self.fail(|| {
                        // alloc: error path
                        format!(
                            "uses {} holding a vtable of `{pointee:?}` at no place whose type names its trait",
                            holder.describe(db, ty)
                        )
                    })
                }
            }
        }
    }

    /// A function or closure a constant's table of pointees holds: what its pointer runs.
    fn pointee(&mut self, pointee: Ty<'db>) {
        match pointee.kind() {
            TyKind::FnDef(def, args) => {
                self.h.update(&[0]);
                self.callable(def.0, args.skip_binder());
            }
            TyKind::Closure(id, args) => {
                self.h.update(&[1]);
                let parent = GenericArgs::new_from_slice(args.as_closure().parent_args());
                self.closure(Body::Closure(blaze_query::db::AsId::as_id(&id.0)), parent);
            }
            _ => unreachable!("`pointee` is asked of a function or a closure only"),
        }
    }

    /// A call terminator's or `sym` operand's callee: a function by its dispatched target, a
    /// constructor by its key, a pointer by its type, which is a local's.
    fn call_site(&mut self, body: &MirBody<'db>, func: &Operand) {
        let callee = func.fn_def().or_else(|| match self.operand_ty(body, func)?.kind() {
            TyKind::FnDef(def, args) => Some((def.0, args.skip_binder())),
            _ => None,
        });
        match callee {
            Some((def, args)) => self.callable(def, args),
            None => {
                self.h.update(&[0xff]);
            }
        }
    }

    fn callable(&mut self, def: CallableDefId, args: GenericArgs<'db>) {
        match def {
            CallableDefId::FunctionId(f) => {
                self.edges.push(Edge::Call(f, args));
                self.name(f, args);
            }
            CallableDefId::StructId(it) => {
                self.h.update(&[20]).id(&item_key(self.cx.db, ItemId::Adt(it.into())).0);
                self.args(args);
            }
            CallableDefId::EnumVariantId(it) => {
                self.h.update(&[21]).id(&key::variant_key(self.cx.db, it).0);
                self.args(args);
            }
        }
    }

    fn closure(&mut self, body: Body, parent: GenericArgs<'db>) {
        self.edges.push(Edge::Closure(body, parent));
        self.body_name(body);
        self.args(parent);
    }

    /// What calling `f` with `args` runs, as the code generator dispatches it, named as its
    /// symbol names it: the item's key and the arguments' identity.
    fn name(&mut self, f: FunctionId, args: GenericArgs<'db>) {
        let cx = self.cx;
        let db = cx.db;
        if !within_longest(args) {
            return self.fail(|| format!("calls `{}` with type arguments past {LONGEST} types long", fn_path(db, f))); // alloc: error path
        }
        match dispatch::call(&cx.infcx, cx.env, f, args) {
            Call::DropGlue(ty) => {
                self.h.update(&[30]);
                self.ty(ty);
            }
            Call::ConstEvalSelect(Some((def, args))) => {
                self.h.update(&[31]);
                match def {
                    CallableDefId::FunctionId(g) => self.name(g, args),
                    _ => self.callable(def, args),
                }
            }
            Call::ConstEvalSelect(None) => {
                self.fail(|| "calls `const_eval_select` with a run-time function not known".to_owned()) // alloc: error path
            }
            Call::Intrinsic(intrinsic) => {
                // By name: its index is its rank among the toolchain's names and shifts when one
                // is added, so a stored hash would come to name another intrinsic (rule 18).
                self.h.update(&[32]).part(intrinsic.symbol().as_str().as_bytes());
                self.args(args);
                // The code generator writes an intrinsic's code in place, from its types.
                args.types().for_each(|it| self.ty(it));
            }
            Call::UnknownIntrinsic(unknown) => {
                self.fail(|| format!("calls an intrinsic not known: {unknown}")) // alloc: error path
            }
            Call::Closure(body, parent) => {
                self.h.update(&[33]);
                self.body_name(body);
                self.args(parent);
            }
            Call::FnItem(def, args) => {
                self.h.update(&[34]);
                match def {
                    CallableDefId::FunctionId(g) => self.name(g, args),
                    _ => self.callable(def, args),
                }
            }
            Call::Indirect => {
                self.h.update(&[35]);
            }
            // The future an `async` closure's call gives is its type's, in the arguments.
            Call::AsyncClosure => {
                self.h.update(&[36]);
                self.item(ItemId::Function(f));
                self.args(args);
            }
            Call::Method(Err(why)) => self.fail(|| {
                format!("calls `{}` for `{:?}`, which no impl's method is found for: {}", fn_path(db, f), args.type_at(0), why.describe()) // alloc: error path
            }),
            Call::Method(Ok(callee)) => self.method(f, args, callee),
        }
    }

    fn method(&mut self, f: FunctionId, args: GenericArgs<'db>, callee: Callee<'db>) {
        let db = self.cx.db;
        match callee {
            Callee::Poll(body, parent) => {
                self.h.update(&[40]);
                self.body_name(body);
                self.args(parent);
            }
            // The slot is the method's place among those of the trait and its supertraits.
            Callee::Virtual => {
                self.h.update(&[41]);
                self.item(ItemId::Function(f));
                self.args(args);
                let dyn_ty = args.type_at(0);
                let slots = vtable_methods(self.cx.interner, dyn_ty, dyn_ty);
                self.h.update(&(slots.len() as u64).to_le_bytes());
                for (method, _) in slots {
                    self.item(ItemId::Function(method));
                }
            }
            Callee::Derived(..) => {
                self.fail(|| format!("calls a derived `{}`, which blaze does not build", fn_path(db, f))) // alloc: error path
            }
            Callee::CloneBuiltin(found) => {
                self.h.update(&[42]);
                self.item(ItemId::Function(f));
                self.args(found);
                self.ty(found.type_at(0));
            }
            Callee::FnPtr(found) => {
                self.h.update(&[43]);
                self.item(ItemId::Function(f));
                self.args(found);
            }
            Callee::Fn(g, found) => {
                self.h.update(&[44]);
                self.item(ItemId::Function(g));
                self.args(found);
            }
        }
    }

    /// The vtables an unsizing of `from` to `to` makes.
    fn unsize(&mut self, from: Ty<'db>, to: Ty<'db>) {
        let mut steps = SmallVec::new();
        dispatch::unsized_vtables(&self.cx.infcx, self.cx.env, from, to, &mut steps);
        for step in steps {
            match step {
                UnsizeStep::Vtable(concrete, dyn_ty) => self.vtable(concrete, dyn_ty),
                UnsizeStep::NotStruct(from) => self.fail(|| format!("unsizes `{from:?}`, which is not a struct")), // alloc: error path
                UnsizeStep::NoFields(from) => self.fail(|| format!("unsizes `{from:?}`, a struct without fields")), // alloc: error path
                UnsizeStep::Unfollowed(from, to) => {
                    self.fail(|| format!("unsizes `{from:?}` to `{to:?}`, which dispatch does not follow")) // alloc: error path
                }
            }
        }
    }

    /// A vtable of `concrete` for `dyn_ty`: the types, and the target of each slot.
    fn vtable(&mut self, concrete: Ty<'db>, dyn_ty: Ty<'db>) {
        self.edges.push(Edge::Vtable(concrete, dyn_ty));
        self.h.update(&[50]);
        self.ty(concrete);
        self.ty(dyn_ty);
        let slots = vtable_methods(self.cx.interner, concrete, dyn_ty);
        self.h.update(&(slots.len() as u64).to_le_bytes());
        for (method, args) in slots {
            self.name(method, args);
        }
    }

    fn drop(&mut self, ty: Ty<'db>) {
        self.edges.push(Edge::Drop(ty));
        self.h.update(&[51]);
        self.ty(ty);
    }

    fn item(&mut self, item: ItemId) {
        self.h.id(&item_key(self.cx.db, item).0);
    }

    pub(crate) fn args(&mut self, args: GenericArgs<'db>) {
        match share::args(self.cx.db, args) {
            Ok(id) => {
                self.h.id(&id);
            }
            Err(it) => self.fail(|| format!("the arguments `{args:?}` have no identity: {it}")), // alloc: error path
        }
    }

    /// A body by its key, as its symbol names it.
    fn body_name(&mut self, body: Body) {
        let (tag, key) = body_key(self.cx.db, body);
        self.h.update(&[tag]).id(&key);
    }
}

/// A body by its key, as its symbol names it: its kind, and its item's, closure's or
/// coroutine's key, none of which reads a path.
pub(crate) fn body_key(db: &Db, body: Body) -> (u8, blaze_ids::ContentId) {
    let key = match body {
        Body::Fn(f) => item_key(db, ItemId::Function(f)),
        Body::Closure(id) => key::closure_key(db, InternedClosureId::from_id(id)),
        Body::Coroutine(id) | Body::CoroutineByMove(id) => {
            key::coroutine_key(db, InternedCoroutineId::from_id(id))
        }
    };
    let tag = match body {
        Body::Fn(_) => 0,
        Body::Closure(_) => 1,
        Body::Coroutine(_) => 2,
        Body::CoroutineByMove(_) => 3,
    };
    (tag, key.0)
}

/// A named constant's value.
pub(crate) fn evaluate<'db>(cx: &Cx<'db>, konst: Const<'db>) -> Result<Allocation<'db>, String> {
    let ConstKind::Alias(_, AliasConst { kind, args, .. }) = konst.kind() else {
        return Err(format!("`{konst:?}` is not a named constant")); // alloc: error path
    };
    match kind {
        AliasConstKind::Projection { def_id: TraitAssocConstId(id) }
        | AliasConstKind::InherentImpl { def_id: InherentAssocConstId(id) }
        | AliasConstKind::Free { def_id: FreeConstAliasId(id) } => {
            let (id, args) = lookup_impl_const(&cx.infcx, cx.env.param_env, id, args);
            cx.db.const_eval(id, args, Some(cx.env))
        }
        AliasConstKind::Anon { def_id } => cx.db.anon_const_eval(def_id.0, args, Some(cx.env)),
        AliasConstKind::InherentSelf { .. } => {
            return Err(format!("`{konst:?}`: an inherent constant in `Self` form after type check")); // alloc: error path
        }
    }
    .map_err(|error| format!("the constant `{konst:?}` does not evaluate: {error:?}")) // alloc: error path
}

/// What holds a value [`Facts::allocation`] reads, for its refusals to name (rule 16).
#[derive(Clone, Copy)]
pub(crate) enum Holder<'db> {
    /// A named constant, by its use.
    Const(Const<'db>),
    /// A static.
    Static(StaticId),
    /// A value the body holds in place (a promoted constant).
    Body,
}

impl<'db> Holder<'db> {
    fn describe(self, db: &Db, ty: Ty<'db>) -> String {
        match self {
            Holder::Const(konst) => {
                let ConstKind::Alias(_, AliasConst { kind, args, .. }) = konst.kind() else {
                    return format!("the constant `{konst:?}` of `{ty:?}`"); // alloc: error path
                };
                let id = match kind {
                    AliasConstKind::Projection { def_id: TraitAssocConstId(id) }
                    | AliasConstKind::InherentImpl { def_id: InherentAssocConstId(id) }
                    | AliasConstKind::Free { def_id: FreeConstAliasId(id) } => id,
                    AliasConstKind::Anon { .. } => {
                        return format!("an anonymous constant of `{ty:?}`"); // alloc: error path
                    }
                    // Not reached: `evaluate` refuses it before any value is read.
                    AliasConstKind::InherentSelf { .. } => {
                        return format!("the constant `{konst:?}` of `{ty:?}`"); // alloc: error path
                    }
                };
                let path = key::path(db, ItemId::Const(id));
                match args.is_empty() {
                    true => format!("the constant `{path}` of `{ty:?}`"), // alloc: error path
                    false => format!("the constant `{path}` with `{args:?}` of `{ty:?}`"), // alloc: error path
                }
            }
            Holder::Static(id) => format!("the static `{}` of `{ty:?}`", key::path(db, ItemId::Static(id))), // alloc: error path
            Holder::Body => format!("a value of `{ty:?}` in its body"), // alloc: error path
        }
    }
}

/// Whether `id` is a word, at any offset, of the value's own bytes or of a piece of memory it
/// points to: where the code generator could find it to emit a vtable.
fn holds_word(value: &AllocationData<'_>, map: &ComplexMemoryMap<'_>, word: usize, id: usize) -> bool {
    let full = id.to_le_bytes();
    let bytes = &full[..word];
    std::iter::once(&value.memory[..])
        .chain(map.memory.iter().map(|(_, it)| &it[..]))
        .any(|memory| memory.windows(word).any(|it| it == bytes))
}

/// The most places [`VtableWalk`] looks at in one value: past it the walk stops with an error,
/// never runs on (rule 15).
const PLACES: u32 = 1 << 22;

/// Where the bytes a walk goes through were in const eval, and whether a type says what they
/// are, as the code generator's `Walk` (hir-ty-codegen's `relocate`) has it.
#[derive(Clone, Copy)]
struct At {
    /// The address of the memory const eval kept they are, or `None` for the value's own bytes.
    kept: Option<usize>,
    /// Reached through a raw pointer or in a union, whose type may be a cast's: a word that type
    /// says is a pointer is one only with provenance, and its vtable one only without.
    raw: bool,
}

impl At {
    /// Where const eval's provenance of the word at `offset` is.
    fn word(self, offset: usize) -> PointerAt {
        match self.kept {
            Some(start) => PointerAt::Memory(start + offset),
            None => PointerAt::Value(offset),
        }
    }
}

/// How a pointer reaches memory const eval kept, as the code generator's `Reach` says: the type
/// the memory is walked as (a `dyn`'s concrete type), the pointer's pointee, the offset, whether
/// a type there can be trusted, a slice's or `str`'s length, and the concrete type of a struct's
/// `dyn` tail.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct Reach<'db> {
    ty: Ty<'db>,
    pointee: Ty<'db>,
    at: usize,
    raw: bool,
    len: Option<usize>,
    tail: Option<Ty<'db>>,
}

/// What a walk by type says of one byte of a value's memory, as the code generator's walk
/// finds it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Leaf {
    /// A number (an integer, a float, a `bool`, a `char`, a slice's length) from the first
    /// offset to the second.
    Number(usize, usize),
    /// A word from that offset the walk relocates by its type: a vtable, a function, a
    /// `TypeId`'s.
    Word(usize),
    /// Nothing a type says: padding, a union's bytes, memory behind a raw pointer.
    None,
}

/// Whether a value of `ty` is a number, with no pointer in it, as the code generator's
/// `is_number` says.
fn is_number(ty: Ty<'_>) -> bool {
    matches!(ty.kind(), TyKind::Int(_) | TyKind::Uint(_) | TyKind::Float(_) | TyKind::Bool | TyKind::Char)
}

/// The memory const eval kept that holds `addr`, with the address it starts at, or that `addr`
/// is one past the end of, as the code generator's `memory_holding` finds it for a raw pointer.
fn memory_holding<'m>(map: &'m MemoryMap<'_>, addr: usize) -> Option<(usize, &'m [u8])> {
    map.memory_at(addr).or_else(|| {
        let (start, memory) = map.memory_at(addr.checked_sub(1)?)?;
        (addr - start == memory.len()).then_some((start, memory))
    })
}

/// The `dyn` each vtable in a constant's value is made for, found by walking the value with its
/// type: a vtable is the metadata of a pointer to `dyn`, and is for the `dyn` that pointer's type
/// names at its own place, as the code generator's `relocate` (hir-ty-codegen) pairs them. The
/// constant's own type says nothing of a `dyn` behind another `dyn` (a `&dyn Debug` holding a
/// `W(&'static dyn Display)`), and const eval's table of vtables names each type only.
///
/// It follows what `relocate` follows, under its rules: references and raw pointers, each place
/// a pointer reaches walked once as the type it reaches it as (a raw one's pointee only where its
/// memory holds all of it, or, where the memory is a whole number of the pointee and the pointer
/// is at one, as `as_ptr` makes it, every element of it), the concrete value behind a `dyn` and
/// at a struct's `dyn` tail, the elements of a struct's slice tail, arrays, slices, tuples, a
/// struct's fields, an enum's fields in the variant its bytes hold, a closure's captures, the one
/// field of a union whose other fields are empty, and `DynMetadata`. Behind a raw pointer or in a
/// union, where the type may be a cast's, a pointer without provenance is a number and a vtable
/// with it no vtable. A union of two or more non-empty fields, a coroutine's captures and memory
/// a static owns are not walked (a static is its own facts). A vtable found for two traits is
/// hashed once for each.
///
/// After the walk, [`VtableWalk::ids_left`] refuses what the code generator's `refuse_ids_left`
/// refuses: a word holding a vtable's or function's id that a type reaching it does not say is
/// one. A vtable left unpaired is refused by the caller, unless no word of the value holds it (a
/// dead temporary's) or it is a function item's or closure's, which is hashed as a function
/// pointer only (the FIXME in `allocation`).
struct VtableWalk<'a, 'db> {
    cx: &'a Cx<'db>,
    map: &'a MemoryMap<'db>,
    dl: &'db TargetDataLayout,
    word: usize,
    /// Each vtable id's `dyn` types, deeply normalized with their free regions erased, in the
    /// order the walk first met them, which the value and its type fix.
    found: HashMap<usize, SmallVec<[Ty<'db>; 1]>, Global>,
    /// The places walked behind a pointer, by the start of the memory they are in. A value that
    /// points at one place many times (a DAG of references) is walked there once.
    reached: HashSet<(usize, Reach<'db>), Global>,
    /// The same, in the order the walk met them, with their memory, for
    /// [`VtableWalk::ids_left`].
    reaches: Vec<(usize, &'a [u8], Reach<'db>)>,
    /// Whether a value of each type can hold a pointer at all.
    pointers: HashMap<Ty<'db>, bool, Global>,
    places: u32,
}

impl<'a, 'db> VtableWalk<'a, 'db> {
    fn new(cx: &'a Cx<'db>, map: &'a MemoryMap<'db>) -> Result<Self, String> {
        let dl = cx
            .db
            .target_data_layout(cx.env.krate)
            .map_err(|error| format!("its crate has no target data layout: {error}"))?; // alloc: error path
        Ok(VtableWalk {
            cx,
            map,
            dl,
            word: dl.pointer_size().bytes_usize(),
            // alloc: one value's vtables, dropped with the walk
            found: HashMap::new_in(Global),
            // alloc: one value's places behind pointers, dropped with the walk
            reached: HashSet::new_in(Global),
            reaches: Vec::new(), // alloc: one value's places behind pointers, dropped with the walk
            // alloc: one entry per distinct type in the value, dropped with the walk
            pointers: HashMap::new_in(Global),
            places: 0,
        })
    }

    fn layout(&self, ty: Ty<'db>) -> Result<&'static Layout, String> {
        self.cx
            .db
            .layout_of_ty(ty.store(), self.cx.env.store())
            .map(|it| &it.get().0)
            .map_err(|error| format!("`{ty:?}` has no layout: {error:?}")) // alloc: error path
    }

    fn word_at(&self, bytes: &[u8], offset: usize, ty: Ty<'db>) -> Result<usize, String> {
        let word = offset.checked_add(self.word).and_then(|end| bytes.get(offset..end)).ok_or_else(|| {
            format!("a `{ty:?}` at offset {offset} is past its memory's {} bytes", bytes.len()) // alloc: error path
        })?;
        Ok(word.iter().rev().fold(0, |value, &byte| value << 8 | usize::from(byte)))
    }

    /// Whether the word at `at` is a pointer with provenance. A map of a literal's reference
    /// says nothing of provenance, and its pointers are what its type says.
    fn provenance(&self, at: PointerAt) -> bool {
        match self.map {
            MemoryMap::Complex(_) => self.map.has_provenance(at),
            MemoryMap::Empty | MemoryMap::Simple(_) => true,
        }
    }

    /// Whether the map says which words have provenance, as const eval's always does.
    fn tracks_provenance(&self) -> bool {
        matches!(self.map, MemoryMap::Complex(_))
    }

    fn count_place(&mut self, ty: Ty<'db>) -> Result<(), String> {
        self.places += 1;
        if self.places > PLACES {
            return Err(format!("its value has more than {PLACES} places to look for a vtable in, at a `{ty:?}`")); // alloc: error path
        }
        Ok(())
    }

    /// The value of `ty` at `offset` in `bytes`. One level per level of the value, as deep as
    /// the program built it (rule 17).
    fn walk(&mut self, bytes: &[u8], at: At, offset: usize, ty: Ty<'db>) -> Result<(), String> {
        stdx::ensure_sufficient_stack(|| self.walk_level(bytes, at, offset, ty))
    }

    fn walk_level(&mut self, bytes: &[u8], at: At, offset: usize, ty: Ty<'db>) -> Result<(), String> {
        let ty = self.cx.normalize(ty);
        if !self.holds_pointer(ty) {
            return Ok(());
        }
        self.count_place(ty)?;
        let (db, lang) = (self.cx.db, self.cx.interner.lang_items());
        match ty.kind() {
            TyKind::Pat(base, _) => self.walk(bytes, at, offset, base)?,
            TyKind::Ref(_, pointee, _) | TyKind::RawPtr(pointee, _) => self.pointer(bytes, at, offset, ty, pointee)?,
            TyKind::Adt(def, args) if lang.DynMetadata.is_some_and(|it| def.def_id() == AdtId::StructId(it)) => {
                // `_vtable_ptr`, its first field.
                let place = offset + self.layout(ty)?.fields.offset(0).bytes_usize();
                let id = self.word_at(bytes, place, ty)?;
                let dyn_ty = self.cx.normalize(args.type_at(0));
                if matches!(dyn_ty.kind(), TyKind::Dynamic(..)) && self.map.vtable_ty(id).is_ok() {
                    self.pair(id, dyn_ty)?;
                }
            }
            TyKind::Adt(def, args) => {
                let layout = self.layout(ty)?;
                let (variant, variant_layout): (VariantId, Cow<'_, Layout>) = match def.def_id() {
                    AdtId::StructId(id) => (id.into(), Cow::Borrowed(layout)),
                    AdtId::UnionId(id) => {
                        if let Some((field_at, field)) = self.union_field(ty, id, args)? {
                            self.walk(bytes, At { raw: true, ..at }, offset + field_at, field)?;
                        }
                        return Ok(());
                    }
                    AdtId::EnumId(id) => {
                        let Some(value) = bytes.get(offset..offset + layout.size.bytes_usize()) else {
                            return Err(format!("a `{ty:?}` at offset {offset} is past its memory's {} bytes", bytes.len())); // alloc: error path
                        };
                        let Some((variant, layout)) = detect_variant_from_bytes(layout, db, self.dl, value, id) else {
                            return Ok(());
                        };
                        (variant.into(), layout)
                    }
                };
                let offsets = (0..variant_layout.fields.count()).map(|i| variant_layout.fields.offset(i).bytes_usize());
                for ((_, field), field_at) in db.field_types(variant).iter().zip(offsets) {
                    self.walk(bytes, at, offset + field_at, field.ty().instantiate(self.cx.interner, args).skip_norm_wip())?;
                }
            }
            TyKind::Tuple(fields) => {
                let layout = self.layout(ty)?;
                for (i, field) in fields.iter().enumerate() {
                    self.walk(bytes, at, offset + layout.fields.offset(i).bytes_usize(), field)?;
                }
            }
            // A closure's captures, as a struct's fields, as `relocate` walks them.
            TyKind::Closure(_, args) => {
                let layout = self.layout(ty)?;
                let upvars = self.cx.normalize(args.as_closure().tupled_upvars_ty());
                for (i, field) in upvars.tuple_fields().iter().enumerate() {
                    self.walk(bytes, at, offset + layout.fields.offset(i).bytes_usize(), field)?;
                }
            }
            TyKind::Array(element, len) => {
                let Some(len) = try_const_usize(db, len) else {
                    return Err(format!("`{ty:?}` has a length that does not evaluate")); // alloc: error path
                };
                let size = self.layout(element)?.size.bytes_usize();
                for i in 0..len as usize {
                    self.walk(bytes, at, offset + i * size, element)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// The field a union `ty` holds, if all its other fields are empty (`MaybeUninit<T>`'s
    /// `value`, besides `()`), with its offset, as the code generator's `relocate_union` finds
    /// it: nothing says which of two non-empty fields its bytes are.
    fn union_field(
        &self,
        ty: Ty<'db>,
        id: UnionId,
        args: GenericArgs<'db>,
    ) -> Result<Option<(usize, Ty<'db>)>, String> {
        let layout = self.layout(ty)?;
        let mut held = None;
        for (i, (_, field)) in self.cx.db.field_types(id.into()).iter().enumerate() {
            let field = self.cx.normalize(field.ty().instantiate(self.cx.interner, args).skip_norm_wip());
            if self.layout(field)?.size.bytes() == 0 {
                continue;
            }
            if held.is_some() {
                return Ok(None);
            }
            held = Some((layout.fields.offset(i).bytes_usize(), field));
        }
        Ok(held)
    }

    /// A pointer of type `ty` to `pointee` at `offset`, as the code generator's
    /// `relocate_pointer` takes it: a pointer to `dyn` pairs its vtable with that `dyn`, and the
    /// memory it points to is walked as the pointee's (the vtable's type, for a `dyn`). A
    /// static's memory is the static's own facts.
    fn pointer(&mut self, bytes: &[u8], at: At, offset: usize, ty: Ty<'db>, pointee: Ty<'db>) -> Result<(), String> {
        let addr = self.word_at(bytes, offset, ty)?;
        let raw = matches!(ty.kind(), TyKind::RawPtr(..));
        let provenance = self.provenance(at.word(offset));
        // Behind a raw pointer the type may be a cast's: a word without provenance is a number,
        // its metadata too.
        if at.raw && !provenance {
            return Ok(());
        }
        let pointee = self.cx.normalize(pointee);
        // The pointer's metadata is its pointee's struct tail's, found as the code generator's
        // `relocate_pointer` finds it: a `&Wrap<dyn Handler>` carries the vtable of the
        // concrete type at its tail for `dyn Handler`.
        let tail = hir_ty::layout::struct_tail_erasing_lifetimes(self.cx.db, pointee, |it| self.cx.normalize(it));
        let wide = matches!(tail.kind(), TyKind::Str | TyKind::Slice(_) | TyKind::Dynamic(..));
        let mut concrete = None;
        if matches!(tail.kind(), TyKind::Dynamic(..)) {
            // Behind a raw pointer, a metadata word with provenance (a pointer, a function) is
            // no vtable.
            if at.raw && self.tracks_provenance() && self.provenance(at.word(offset + self.word)) {
                return Ok(());
            }
            let id = self.word_at(bytes, offset + self.word, ty)?;
            // A raw pointer's metadata can be any id (a dangling `*const dyn Tr`).
            let Ok(of) = self.map.vtable_ty(id) else { return Ok(()) };
            self.pair(id, tail)?;
            concrete = Some(of);
        } else if raw
            && !wide
            && provenance
            && self.map.vtable_ty(addr).is_ok_and(|it| matches!(it.kind(), TyKind::FnDef(..) | TyKind::Closure(..)))
        {
            // A function cast to a thin raw pointer keeps its id.
            return Ok(());
        }
        if self.map.static_at(addr).is_some() {
            return Ok(());
        }
        // What is walked by type: the concrete type behind a `dyn`, else the pointee.
        let walked = match (pointee.kind(), concrete) {
            (TyKind::Dynamic(..), Some(concrete)) => concrete,
            _ => pointee,
        };
        let len = match tail.kind() {
            TyKind::Str | TyKind::Slice(_) => Some(self.word_at(bytes, offset + self.word, ty)?),
            _ => None,
        };
        let holding = if raw { memory_holding(self.map, addr) } else { self.map.memory_at(addr) };
        let Some((start, memory)) = holding else { return Ok(()) };
        // A reference to nothing (no bytes, or an empty slice) is walked by no type: the code
        // generator makes it its alignment, or points it into the memory by provenance.
        if !raw && (memory.is_empty() || (matches!(walked.kind(), TyKind::Str | TyKind::Slice(_)) && len == Some(0))) {
            return Ok(());
        }
        let reach = Reach {
            ty: walked,
            pointee,
            at: addr - start,
            raw: at.raw || raw,
            len,
            // Only a pointer to a `dyn` itself points at the concrete value; one to a struct
            // with a `dyn` tail points at the struct, its tail walked as the concrete type.
            tail: concrete.filter(|_| !matches!(pointee.kind(), TyKind::Dynamic(..))),
        };
        if !self.reached.insert((start, reach)) {
            return Ok(());
        }
        self.reaches.push((start, memory, reach));
        self.reach(start, memory, reach)
    }

    /// The memory at `start` as `reach` walks it, as the code generator's `walk_allocation`
    /// walks it by type: the pointee, or a slice's elements; a raw pointer's pointee only where
    /// it fits, or at every element of memory that is a whole number of it; then a struct's
    /// slice tail's elements, and its `dyn` tail as the concrete type.
    fn reach(&mut self, start: usize, memory: &'a [u8], reach: Reach<'db>) -> Result<(), String> {
        let at = At { kept: Some(start), raw: reach.raw };
        let place = reach.at;
        let fits = |size: usize| place.checked_add(size).is_some_and(|end| end <= memory.len());
        match reach.ty.kind() {
            TyKind::Str => {}
            TyKind::Slice(element) => {
                let element = self.cx.normalize(element);
                if self.holds_pointer(element) {
                    let size = self.layout(element)?.size.bytes_usize();
                    let len = reach.len.unwrap_or(0);
                    if !reach.raw || size.checked_mul(len).is_some_and(fits) {
                        for i in 0..len {
                            self.walk(memory, at, place + i * size, element)?;
                        }
                    }
                }
            }
            _ if !reach.raw => self.walk(memory, at, place, reach.ty)?,
            _ => match self.stride(memory, reach) {
                Some(size) => {
                    for pos in (0..memory.len()).step_by(size) {
                        self.walk(memory, at, pos, reach.ty)?;
                    }
                }
                None if self.sized(reach.ty).is_some_and(fits) => self.walk(memory, at, place, reach.ty)?,
                None => {}
            },
        }
        // A struct or tuple whose tail is a slice: its elements, after the sized fields.
        if let Some(len) = reach.len
            && !matches!(reach.ty.kind(), TyKind::Str | TyKind::Slice(_))
            && let Some((tail, element)) = self.slice_tail(reach.pointee)?
            && self.holds_pointer(element)
        {
            let size = self.layout(element)?.size.bytes_usize();
            let whole = size.checked_mul(len).and_then(|it| it.checked_add(tail));
            if !reach.raw || whole.is_some_and(fits) {
                for i in 0..len {
                    self.walk(memory, at, place + tail + i * size, element)?;
                }
            }
        }
        // A struct or tuple with a `dyn` tail: the tail is its concrete type, placed by the rule
        // const eval and the code generator share.
        if let Some(concrete) = reach.tail
            && self.holds_pointer(concrete)
            && let Some((tail, size)) = self.dyn_tail(reach.pointee, concrete)?
            && (!reach.raw || fits(tail + size))
        {
            self.walk(memory, at, place + tail, concrete)?;
        }
        Ok(())
    }

    /// The size of a value of `ty`, if it has a layout and is sized.
    fn sized(&self, ty: Ty<'db>) -> Option<usize> {
        self.layout(ty).ok().filter(|it| it.is_sized()).map(|it| it.size.bytes_usize())
    }

    /// The stride a raw pointer's pointee is walked at over all of its memory: where the memory
    /// is a whole number of the pointee and the pointer is at one of them, as `as_ptr` makes
    /// it; `None` where only the pointee at the pointer is walked, if it fits.
    fn stride(&self, memory: &[u8], reach: Reach<'db>) -> Option<usize> {
        let size = self.sized(reach.ty)?;
        (size > 0 && !is_number(reach.ty) && reach.at % size == 0 && memory.len() % size == 0).then_some(size)
    }

    /// Where the slice at the end of `ty`'s chain of last fields (through structs and tuples)
    /// starts in a value of it, and its element type, as the code generator's `slice_tail`
    /// finds it; `None` when the tail is no slice.
    fn slice_tail(&self, ty: Ty<'db>) -> Result<Option<(usize, Ty<'db>)>, String> {
        let (mut at, mut offset) = (ty, 0);
        // A type's chain of last fields is as deep as the type: fuel for a cycle a bug makes.
        for _ in 0..256 {
            let (index, last) = match at.kind() {
                TyKind::Slice(element) => return Ok(Some((offset, self.cx.normalize(element)))),
                TyKind::Adt(def, args) => {
                    let AdtId::StructId(id) = def.def_id() else { return Ok(None) };
                    let fields = self.cx.db.field_types(id.into());
                    let Some((_, last)) = fields.iter().next_back() else { return Ok(None) };
                    (fields.iter().count() - 1, last.ty().instantiate(self.cx.interner, args).skip_norm_wip())
                }
                TyKind::Tuple(tys) => match tys.iter().next_back() {
                    Some(last) => (tys.len() - 1, last),
                    None => return Ok(None),
                },
                _ => return Ok(None),
            };
            offset += self.layout(at)?.fields.offset(index).bytes_usize();
            at = self.cx.normalize(last);
        }
        Err(format!("the tail of `{ty:?}` is more than 256 fields deep")) // alloc: error path
    }

    /// Where the `dyn` tail of `pointee` holding a `concrete` starts in a value of it, and the
    /// concrete value's size: the rule const eval and the code generator share.
    fn dyn_tail(&self, pointee: Ty<'db>, concrete: Ty<'db>) -> Result<Option<(usize, usize)>, String> {
        let layout = self.layout(concrete)?;
        let place = hir_ty::layout::dyn_tail_place(
            self.cx.db,
            pointee,
            layout.align.abi.bytes_usize(),
            |it| Ok(self.cx.normalize(it)),
            |it| self.layout(it),
        )?;
        Ok(place.map(|(offset, _)| (offset, layout.size.bytes_usize())))
    }

    /// `dyn_ty` is a trait the vtable `id` is for. Kept once per `dyn`, its lifetimes aside: two
    /// spellings of one `dyn` (`dyn Debug + 'static`, `dyn Debug + '_`) are one vtable.
    fn pair(&mut self, id: usize, dyn_ty: Ty<'db>) -> Result<(), String> {
        let normalized = self
            .cx
            .deeply_normalize(dyn_ty)
            .map_err(|why| format!("`{dyn_ty:?}` does not normalize: {why}"))?; // alloc: error path
        let dyn_ty = crate::instance::erased(self.cx.interner, normalized);
        let dyns = self.found.entry(id).or_default();
        if !dyns.contains(&dyn_ty) {
            dyns.push(dyn_ty);
        }
        Ok(())
    }

    /// Refuses a word left equal to one of const eval's ids of a function or a vtable, as the
    /// code generator's `refuse_ids_left` refuses it: a word without provenance in the value's
    /// bytes, or in memory a pointer reaches, that the walk by type there (the value's own type,
    /// or each type a pointer reaches the memory as) does not relocate, and that no type of that
    /// memory says is a number. It is a vtable behind a pointer cast to another type (`&&dyn Tr
    /// as *const _ as *const u8`), or one in a union of two non-empty fields: const eval's ids
    /// name its type only, so its trait, and the code its slots run, is not known, and the code
    /// generator stops there too. Memory that only a word's provenance reaches is not looked at:
    /// a vtable in it is paired at no place, which the caller refuses.
    fn ids_left(&mut self, value: &[u8], ty: Ty<'db>) -> Result<(), String> {
        if !self.tracks_provenance() {
            return Ok(());
        }
        let own = Reach { ty, pointee: ty, at: 0, raw: false, len: None, tail: None };
        let reaches = std::mem::take(&mut self.reaches);
        let mut groups: Vec<(Option<usize>, &[u8], SmallVec<[Reach<'db>; 2]>)> = Vec::new(); // alloc: the memories a value's pointers reach, once per value with a vtable
        groups.push((None, value, SmallVec::from_slice(&[own])));
        for &(start, memory, reach) in &reaches {
            match groups.iter_mut().find(|it| it.0 == Some(start)) {
                Some(group) => group.2.push(reach),
                None => groups.push((Some(start), memory, SmallVec::from_slice(&[reach]))),
            }
        }
        for (kept, memory, of) in &groups {
            let at = At { kept: *kept, raw: false };
            let candidates = (0..memory.len().saturating_sub(self.word - 1)).step_by(self.word).filter(|&x| {
                let id = memory[x..x + self.word].iter().rev().fold(0, |value, &byte| value << 8 | usize::from(byte));
                id >= VTableMap::FIRST_ID && !self.provenance(at.word(x)) && self.map.vtable_ty(id).is_ok()
            });
            // alloc: the words to look at, almost always none
            let candidates: SmallVec<[usize; 4]> = candidates.collect();
            for x in candidates {
                for &reach in of {
                    if self.leaf_reach(*kept, memory, reach, x)? == Leaf::Word(x) || self.number(*kept, memory, of, x)? {
                        continue;
                    }
                    let id = self.word_at(memory, x, reach.ty)?;
                    let pointee = self.map.vtable_ty(id).map_err(|error| format!("{error:?}"))?; // alloc: error path
                    let place = match kept {
                        Some(start) => format!("the memory const eval kept at {start:#x}, reached as `{:?}`", reach.ty), // alloc: error path
                        None => format!("its value, of `{ty:?}`"), // alloc: error path
                    };
                    let behind = if reach.raw { " behind a raw pointer" } else { "" };
                    return Err(format!(
                        // alloc: error path
                        "the word at {x} of {place}{behind} is const eval's id of `{pointee:?}` (a function or a \
                         vtable), which no type there says is a pointer: a vtable behind a pointer cast to \
                         another type (`&&dyn Tr as *const _ as *const u8`) needs const eval's vtable ids \
                         to name the trait, as the code generator says"
                    ));
                }
            }
        }
        Ok(())
    }

    /// Whether every byte of the word at `x` of `memory` is a number by some type the memory is
    /// reached as, as the code generator's `Relocations::is_number` says over the numbers its
    /// walks noted there.
    fn number(&mut self, kept: Option<usize>, memory: &[u8], of: &[Reach<'db>], x: usize) -> Result<bool, String> {
        let mut pos = x;
        // Each step moves `pos` to the end of a number that holds it, at least a byte on.
        for _ in 0..=self.word {
            if pos >= x + self.word {
                return Ok(true);
            }
            let mut end = None;
            for &reach in of {
                if let Leaf::Number(start, to) = self.leaf_reach(kept, memory, reach, pos)?
                    && start <= pos
                    && pos < to
                {
                    end = Some(to);
                    break;
                }
            }
            match end {
                Some(to) => pos = to,
                None => return Ok(false),
            }
        }
        Ok(pos >= x + self.word)
    }

    /// What `reach`'s walk of `memory` (kept at `kept`) says of the byte at `pos`, as
    /// [`VtableWalk::reach`] walks it.
    fn leaf_reach(&mut self, kept: Option<usize>, memory: &[u8], reach: Reach<'db>, pos: usize) -> Result<Leaf, String> {
        let at = At { kept, raw: reach.raw };
        let place = reach.at;
        let fits = |size: usize| place.checked_add(size).is_some_and(|end| end <= memory.len());
        let main = match reach.ty.kind() {
            TyKind::Str => Leaf::None,
            TyKind::Slice(element) => {
                let element = self.cx.normalize(element);
                let size = self.layout(element)?.size.bytes_usize();
                let len = reach.len.unwrap_or(0);
                match size.checked_mul(len).and_then(|whole| Some((whole, place.checked_add(whole)?))) {
                    Some((whole, end)) if size > 0 && (!reach.raw || fits(whole)) && (place..end).contains(&pos) => {
                        self.leaf(memory, at, place + (pos - place) / size * size, element, pos)?
                    }
                    _ => Leaf::None,
                }
            }
            _ if !reach.raw => self.leaf(memory, at, place, reach.ty, pos)?,
            _ => match self.stride(memory, reach) {
                Some(size) => self.leaf(memory, at, pos / size * size, reach.ty, pos)?,
                None if self.sized(reach.ty).is_some_and(fits) => self.leaf(memory, at, place, reach.ty, pos)?,
                None => Leaf::None,
            },
        };
        if main != Leaf::None {
            return Ok(main);
        }
        if let Some(len) = reach.len
            && !matches!(reach.ty.kind(), TyKind::Str | TyKind::Slice(_))
            && let Some((tail, element)) = self.slice_tail(reach.pointee)?
        {
            let size = self.layout(element)?.size.bytes_usize();
            let begin = place + tail;
            if let Some(whole) = size.checked_mul(len)
                && let Some(end) = begin.checked_add(whole)
                && size > 0
                && (begin..end).contains(&pos)
            {
                if is_number(element) {
                    return Ok(if reach.raw { Leaf::None } else { Leaf::Number(begin, end) });
                }
                if !reach.raw || whole.checked_add(tail).is_some_and(fits) {
                    return self.leaf(memory, at, begin + (pos - begin) / size * size, element, pos);
                }
            }
        }
        if let Some(concrete) = reach.tail
            && let Some((tail, size)) = self.dyn_tail(reach.pointee, concrete)?
            && (!reach.raw || fits(tail + size))
        {
            return self.leaf(memory, at, place + tail, concrete, pos);
        }
        Ok(Leaf::None)
    }

    /// What a value of `ty` at `base` in `bytes` says of the byte at `pos`, as the code
    /// generator's `relocate` walks it: one level per level of the type, as deep as the program
    /// built it (rule 17).
    fn leaf(&mut self, bytes: &[u8], at: At, base: usize, ty: Ty<'db>, pos: usize) -> Result<Leaf, String> {
        stdx::ensure_sufficient_stack(|| self.leaf_level(bytes, at, base, ty, pos))
    }

    fn leaf_level(&mut self, bytes: &[u8], at: At, base: usize, ty: Ty<'db>, pos: usize) -> Result<Leaf, String> {
        let ty = self.cx.normalize(ty);
        if pos < base {
            return Ok(Leaf::None);
        }
        self.count_place(ty)?;
        let (db, lang) = (self.cx.db, self.cx.interner.lang_items());
        let word = self.word;
        let in_word = |start: usize| start <= pos && pos < start + word;
        // Where the field of a value of `layout` at `base` holding `pos` is, among `fields`: a
        // sized one that holds it, or else the unsized one at the end, which holds what follows.
        let field_at = |this: &Self,
                        layout: &Layout,
                        fields: &mut dyn Iterator<Item = Ty<'db>>|
         -> Result<Option<(usize, Ty<'db>)>, String> {
            let mut unsized_tail = None;
            for (i, field) in fields.enumerate() {
                let start = base + layout.fields.offset(i).bytes_usize();
                match this.layout(this.cx.normalize(field)) {
                    Ok(it) if it.is_sized() => {
                        if start <= pos && pos < start + it.size.bytes_usize() {
                            return Ok(Some((start, field)));
                        }
                    }
                    Ok(_) => unsized_tail = Some((start, field)),
                    Err(error) => return Err(error),
                }
            }
            Ok(unsized_tail.filter(|&(start, _)| start <= pos))
        };
        let leaf = match ty.kind() {
            TyKind::Pat(inner, _) => return self.leaf(bytes, at, base, inner, pos),
            // The data word is relocated by its provenance only; the metadata by type.
            TyKind::Ref(_, pointee, _) | TyKind::RawPtr(pointee, _) => {
                let meta = base + word;
                if !in_word(meta) || (at.raw && !self.provenance(at.word(base))) {
                    Leaf::None
                } else {
                    let pointee = self.cx.normalize(pointee);
                    let tail =
                        hir_ty::layout::struct_tail_erasing_lifetimes(db, pointee, |it| self.cx.normalize(it));
                    match tail.kind() {
                        TyKind::Dynamic(..) => {
                            let cast = at.raw && self.tracks_provenance() && self.provenance(at.word(meta));
                            let id = self.word_at(bytes, meta, ty)?;
                            if !cast && self.map.vtable_ty(id).is_ok() { Leaf::Word(meta) } else { Leaf::None }
                        }
                        TyKind::Str | TyKind::Slice(_) if !at.raw => Leaf::Number(meta, meta + word),
                        _ => Leaf::None,
                    }
                }
            }
            TyKind::FnPtr(..) if in_word(base) => {
                let id = self.word_at(bytes, base, ty)?;
                let function =
                    self.map.vtable_ty(id).is_ok_and(|it| matches!(it.kind(), TyKind::FnDef(..) | TyKind::Closure(..)));
                if function && (!at.raw || self.provenance(at.word(base))) { Leaf::Word(base) } else { Leaf::None }
            }
            TyKind::Adt(def, args) if lang.DynMetadata.is_some_and(|it| def.def_id() == AdtId::StructId(it)) => {
                let place = base + self.layout(ty)?.fields.offset(0).bytes_usize();
                let dyn_ty = self.cx.normalize(args.type_at(0));
                let paired = in_word(place)
                    && matches!(dyn_ty.kind(), TyKind::Dynamic(..))
                    && self.map.vtable_ty(self.word_at(bytes, place, ty)?).is_ok();
                if paired { Leaf::Word(place) } else { Leaf::None }
            }
            // A `TypeId`'s words hold const eval's id of its type, which the code generator
            // writes rustc's hash over.
            TyKind::Adt(def, _) if lang.TypeId.is_some_and(|it| def.def_id() == AdtId::StructId(it)) => {
                let id = bytes.get(base..base + 16).and_then(|it| it.try_into().ok()).map(u128::from_le_bytes);
                match id.and_then(|it| self.map.type_id_ty(it)) {
                    Some(_) if pos < base + 16 => Leaf::Word(base + (pos - base) / word * word),
                    _ => Leaf::None,
                }
            }
            TyKind::Adt(def, args) => {
                let layout = self.layout(ty)?;
                if layout.is_sized() && pos >= base + layout.size.bytes_usize() {
                    return Ok(Leaf::None);
                }
                let (variant, variant_layout): (VariantId, Cow<'_, Layout>) = match def.def_id() {
                    AdtId::StructId(id) => (id.into(), Cow::Borrowed(layout)),
                    AdtId::UnionId(id) => {
                        return match self.union_field(ty, id, args)? {
                            Some((field_at, field)) => {
                                self.leaf(bytes, At { raw: true, ..at }, base + field_at, field, pos)
                            }
                            None => Ok(Leaf::None),
                        };
                    }
                    AdtId::EnumId(id) => {
                        let Some(value) = bytes.get(base..base + layout.size.bytes_usize()) else {
                            return Ok(Leaf::None);
                        };
                        let Some((variant, layout)) = detect_variant_from_bytes(layout, db, self.dl, value, id) else {
                            return Ok(Leaf::None);
                        };
                        (variant.into(), layout)
                    }
                };
                let interner = self.cx.interner;
                let mut fields = db.field_types(variant).iter().map(|(_, it)| it.ty().instantiate(interner, args).skip_norm_wip());
                match field_at(self, &*variant_layout, &mut fields)? {
                    Some((start, field)) => return self.leaf(bytes, at, start, field, pos),
                    None => Leaf::None,
                }
            }
            TyKind::Tuple(fields) => {
                let layout = self.layout(ty)?;
                match field_at(self, layout, &mut fields.iter())? {
                    Some((start, field)) => return self.leaf(bytes, at, start, field, pos),
                    None => Leaf::None,
                }
            }
            TyKind::Closure(_, args) => {
                let layout = self.layout(ty)?;
                let upvars = self.cx.normalize(args.as_closure().tupled_upvars_ty());
                match field_at(self, layout, &mut upvars.tuple_fields().iter())? {
                    Some((start, field)) => return self.leaf(bytes, at, start, field, pos),
                    None => Leaf::None,
                }
            }
            TyKind::Array(element, len) => {
                let Some(len) = try_const_usize(db, len) else {
                    return Err(format!("`{ty:?}` has a length that does not evaluate")); // alloc: error path
                };
                let element = self.cx.normalize(element);
                let size = self.layout(element)?.size.bytes_usize();
                let end = size.checked_mul(len as usize).and_then(|it| base.checked_add(it));
                match end {
                    Some(end) if size > 0 && pos < end => match is_number(element) {
                        true if at.raw => Leaf::None,
                        true => Leaf::Number(base, end),
                        false => return self.leaf(bytes, at, base + (pos - base) / size * size, element, pos),
                    },
                    _ => Leaf::None,
                }
            }
            _ if is_number(ty) && !at.raw => {
                let end = base + self.layout(ty)?.size.bytes_usize();
                if pos < end { Leaf::Number(base, end) } else { Leaf::None }
            }
            _ => Leaf::None,
        };
        Ok(leaf)
    }

    /// Whether a value of `ty`, normalized at its top, can hold a pointer, and so a vtable. A
    /// type met again while it is being looked at (only an ill-formed one, as a type that holds
    /// itself without a pointer has no size) counts as holding one, so the walk looks.
    fn holds_pointer(&mut self, ty: Ty<'db>) -> bool {
        if let Some(&known) = self.pointers.get(&ty) {
            return known;
        }
        self.pointers.insert(ty, true);
        let holds = stdx::ensure_sufficient_stack(|| self.holds_pointer_level(ty));
        self.pointers.insert(ty, holds);
        holds
    }

    fn holds_pointer_level(&mut self, ty: Ty<'db>) -> bool {
        let (db, lang) = (self.cx.db, self.cx.interner.lang_items());
        match ty.kind() {
            TyKind::Ref(..) | TyKind::RawPtr(..) => true,
            TyKind::Adt(def, _) if lang.DynMetadata.is_some_and(|it| def.def_id() == AdtId::StructId(it)) => true,
            TyKind::Adt(def, args) => variants(db, def.def_id()).into_iter().any(|variant| {
                db.field_types(variant).iter().any(|(_, field)| {
                    let field = field.ty().instantiate(self.cx.interner, args).skip_norm_wip();
                    let field = self.cx.normalize(field);
                    self.holds_pointer(field)
                })
            }),
            TyKind::Tuple(fields) => fields.iter().any(|it| {
                let it = self.cx.normalize(it);
                self.holds_pointer(it)
            }),
            TyKind::Closure(_, args) => {
                let upvars = self.cx.normalize(args.as_closure().tupled_upvars_ty());
                self.holds_pointer(upvars)
            }
            TyKind::Array(element, _) | TyKind::Slice(element) | TyKind::Pat(element, _) => {
                let element = self.cx.normalize(element);
                self.holds_pointer(element)
            }
            // What the walk does not follow, or a value with no pointer in it.
            _ => false,
        }
    }
}

/// A function as an error names it.
pub(crate) fn fn_path(db: &Db, func: FunctionId) -> String {
    key::path(db, ItemId::Function(func))
}

/// Whether `func` is a method of a trait whose impls are a leaf: `#[blaze::pure(trusted)]`.
pub(crate) fn trusted_trait_method(db: &Db, func: FunctionId) -> bool {
    match func.loc(db).container {
        ItemContainerId::TraitId(it) => trusted(db, it.into()),
        ItemContainerId::ImplId(it) => db
            .impl_trait(it)
            .is_some_and(|trait_ref| trusted(db, trait_ref.skip_binder().def_id.0.into())),
        _ => false,
    }
}

/// Whether the item is `#[blaze::pure(trusted = "..")]`.
pub(crate) fn trusted(db: &Db, owner: hir_def::AttrDefId) -> bool {
    matches!(AttrFlags::pure_attr(db, owner), Some(hir_def::attrs::PureAttr::Trusted(_)))
}

/// The interface hash of `item`, or why it has none.
pub(crate) fn interface(db: &Db, item: ItemId) -> Result<blaze_ids::ContentId, String> {
    interface_hash(db, item).map(|it| it.0).map_err(|it| it.describe(db))
}
