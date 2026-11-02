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
    AdtId, CallableDefId, FunctionId, ItemContainerId, StaticId, VariantId,
    attrs::AttrFlags,
    signatures::{StaticFlags, StaticSignature},
};
use hir_ty::{
    ComplexMemoryMap, MemoryMap, ParamEnvAndCrate, PointerCast,
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
            walk.walk(&value.memory, 0, ty)?;
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
                // FIXME(comptime): one made a `dyn` in a union or a coroutine's capture is hashed
                // as a pointer only, as its `dyn` is at no place the walk types; refuse it or
                // find its trait there.
                None if function => {
                    self.h.update(&[2]).update(&0u64.to_le_bytes());
                }
                // An id no word of the value holds: a vtable a temporary of the evaluation made
                // (const eval keeps its whole table), which nothing can emit.
                None if !holds_word(value, map, word, id) => {
                    self.h.update(&[3]);
                }
                // A vtable a word of the value holds at no place whose type names its trait:
                // one in a union, a coroutine's capture, an unsafe binder or a `DynMetadata` of
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

/// The `dyn` each vtable in a constant's value is made for, found by walking the value with its
/// type: a vtable is the metadata of a pointer to `dyn`, and is for the `dyn` that pointer's type
/// names at its own place, as the code generator's `relocate` (hir-ty-codegen) pairs them. The
/// constant's own type says nothing of a `dyn` behind another `dyn` (a `&dyn Debug` holding a
/// `W(&'static dyn Display)`), and const eval's table of vtables names each type only.
///
/// It follows what `relocate` follows: references and raw pointers (a raw one's pointee only
/// where its memory holds all of it), the concrete value behind a `dyn` and at a struct's `dyn`
/// tail, arrays, slices, tuples, a struct's fields, an enum's fields in the variant its bytes
/// hold, a closure's captures, and `DynMetadata`; a vtable found for two traits is hashed once
/// for each. A union's words, a coroutine's captures and memory a static owns are not walked
/// (a static is its own facts). A vtable left unpaired is refused by the caller, unless no word
/// of the value holds it (a dead temporary's) or it is a function item's or closure's, which is
/// hashed as a function pointer only (the FIXME in `allocation`).
struct VtableWalk<'a, 'db> {
    cx: &'a Cx<'db>,
    map: &'a MemoryMap<'db>,
    dl: &'db TargetDataLayout,
    word: usize,
    /// Each vtable id's `dyn` types, deeply normalized with their free regions erased, in the
    /// order the walk first met them, which the value and its type fix.
    found: HashMap<usize, SmallVec<[Ty<'db>; 1]>, Global>,
    /// The places walked behind a pointer: the allocation's start, the offset in it, the
    /// pointee's type and a slice's length. A value that points at one place many times (a
    /// DAG of references) is walked there once.
    walked: HashSet<(usize, usize, Ty<'db>, usize), Global>,
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
            walked: HashSet::new_in(Global),
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
        let word = bytes.get(offset..offset + self.word).ok_or_else(|| {
            format!("a `{ty:?}` at offset {offset} is past its memory's {} bytes", bytes.len()) // alloc: error path
        })?;
        Ok(word.iter().rev().fold(0, |value, &byte| value << 8 | usize::from(byte)))
    }

    /// The value of `ty` at `offset` in `bytes`. One level per level of the value, as deep as
    /// the program built it (rule 17).
    fn walk(&mut self, bytes: &[u8], offset: usize, ty: Ty<'db>) -> Result<(), String> {
        stdx::ensure_sufficient_stack(|| self.walk_level(bytes, offset, ty))
    }

    fn walk_level(&mut self, bytes: &[u8], offset: usize, ty: Ty<'db>) -> Result<(), String> {
        let ty = self.cx.normalize(ty);
        if !self.holds_pointer(ty) {
            return Ok(());
        }
        self.places += 1;
        if self.places > PLACES {
            return Err(format!("its value has more than {PLACES} places to look for a vtable in, at a `{ty:?}`")); // alloc: error path
        }
        let (db, lang) = (self.cx.db, self.cx.interner.lang_items());
        match ty.kind() {
            TyKind::Pat(base, _) => self.walk(bytes, offset, base)?,
            TyKind::Ref(_, pointee, _) | TyKind::RawPtr(pointee, _) => self.pointer(bytes, offset, ty, pointee)?,
            TyKind::Adt(def, args) if lang.DynMetadata.is_some_and(|it| def.def_id() == AdtId::StructId(it)) => {
                // `_vtable_ptr`, its first field.
                let at = offset + self.layout(ty)?.fields.offset(0).bytes_usize();
                let id = self.word_at(bytes, at, ty)?;
                let dyn_ty = self.cx.normalize(args.type_at(0));
                if matches!(dyn_ty.kind(), TyKind::Dynamic(..)) && self.map.vtable_ty(id).is_ok() {
                    self.pair(id, dyn_ty)?;
                }
            }
            TyKind::Adt(def, args) => {
                let layout = self.layout(ty)?;
                let (variant, variant_layout): (VariantId, Cow<'_, Layout>) = match def.def_id() {
                    AdtId::StructId(id) => (id.into(), Cow::Borrowed(layout)),
                    // Nothing says which of its fields a union's bytes are.
                    AdtId::UnionId(_) => return Ok(()),
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
                for ((_, field), at) in db.field_types(variant).iter().zip(offsets) {
                    self.walk(bytes, offset + at, field.ty().instantiate(self.cx.interner, args).skip_norm_wip())?;
                }
            }
            TyKind::Tuple(fields) => {
                let layout = self.layout(ty)?;
                for (i, field) in fields.iter().enumerate() {
                    self.walk(bytes, offset + layout.fields.offset(i).bytes_usize(), field)?;
                }
            }
            // A closure's captures, as a struct's fields, as `relocate` walks them.
            TyKind::Closure(_, args) => {
                let layout = self.layout(ty)?;
                let upvars = self.cx.normalize(args.as_closure().tupled_upvars_ty());
                for (i, field) in upvars.tuple_fields().iter().enumerate() {
                    self.walk(bytes, offset + layout.fields.offset(i).bytes_usize(), field)?;
                }
            }
            TyKind::Array(element, len) => {
                let Some(len) = try_const_usize(db, len) else {
                    return Err(format!("`{ty:?}` has a length that does not evaluate")); // alloc: error path
                };
                let size = self.layout(element)?.size.bytes_usize();
                for i in 0..len as usize {
                    self.walk(bytes, offset + i * size, element)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// A pointer of type `ty` to `pointee` at `offset`: a pointer to `dyn` pairs its vtable with
    /// that `dyn`, and the memory it points to is walked as the pointee's (the vtable's type, for
    /// a `dyn`). A static's memory is the static's own facts.
    fn pointer(&mut self, bytes: &[u8], offset: usize, ty: Ty<'db>, pointee: Ty<'db>) -> Result<(), String> {
        let addr = self.word_at(bytes, offset, ty)?;
        let mut pointee = self.cx.normalize(pointee);
        // The pointer's metadata is its pointee's struct tail's, found as the code generator's
        // `relocate_pointer` finds it: a `&Wrap<dyn Handler>` carries the vtable of the
        // concrete type at its tail for `dyn Handler`.
        let tail = hir_ty::layout::struct_tail_erasing_lifetimes(self.cx.db, pointee, |it| self.cx.normalize(it));
        let wide = matches!(tail.kind(), TyKind::Str | TyKind::Slice(_) | TyKind::Dynamic(..));
        // The concrete value at a struct's `dyn` tail, walked by its type (as `walk_allocation`
        // walks it), at its offset in the struct.
        let mut at_tail = None;
        if matches!(tail.kind(), TyKind::Dynamic(..)) {
            let id = self.word_at(bytes, offset + self.word, ty)?;
            // A raw pointer's metadata can be any id (a dangling `*const dyn Tr`).
            let Ok(concrete) = self.map.vtable_ty(id) else { return Ok(()) };
            self.pair(id, tail)?;
            // Only a pointer to a `dyn` itself points at the concrete value; one to a struct
            // with a `dyn` tail points at the struct, walked as its own type (as `relocate`
            // walks it).
            if matches!(pointee.kind(), TyKind::Dynamic(..)) {
                pointee = concrete;
            } else if self.holds_pointer(concrete) {
                let align = self.layout(concrete)?.align.abi.bytes_usize();
                let place = hir_ty::layout::dyn_tail_place(
                    self.cx.db,
                    pointee,
                    align,
                    |it| Ok(self.cx.normalize(it)),
                    |it| self.layout(it),
                )?;
                at_tail = place.map(|(offset, _)| (offset, concrete));
            }
        } else if matches!(ty.kind(), TyKind::RawPtr(..))
            && !wide
            && self.map.vtable_ty(addr).is_ok_and(|it| matches!(it.kind(), TyKind::FnDef(..) | TyKind::Closure(..)))
        {
            // A function cast to a thin raw pointer keeps its id.
            return Ok(());
        }
        if self.map.static_at(addr).is_some() {
            return Ok(());
        }
        let Some((start, memory)) = self.map.memory_at(addr) else { return Ok(()) };
        let at = addr - start;
        // A raw pointer need not point at a whole value (one past the end, a slice of a length
        // made up): what it points at is followed only where the memory holds all of it, as
        // nothing reads through it in the constant.
        let raw = matches!(ty.kind(), TyKind::RawPtr(..));
        match pointee.kind() {
            TyKind::Str => {}
            TyKind::Slice(element) => {
                let len = self.word_at(bytes, offset + self.word, ty)?;
                if !self.holds_pointer(self.cx.normalize(element)) {
                    return Ok(());
                }
                let size = self.layout(element)?.size.bytes_usize();
                if raw && size.checked_mul(len).is_none_or(|it| it > memory.len().saturating_sub(at)) {
                    return Ok(());
                }
                if !self.walked.insert((start, at, pointee, len)) {
                    return Ok(());
                }
                for i in 0..len {
                    self.walk(memory, at + i * size, element)?;
                }
            }
            _ => {
                let fits = || self.layout(pointee).is_ok_and(|it| it.size.bytes_usize() <= memory.len().saturating_sub(at));
                if (!raw || fits()) && self.walked.insert((start, at, pointee, 0)) {
                    self.walk(memory, at, pointee)?;
                }
            }
        }
        if let Some((offset, concrete)) = at_tail {
            let size = self.layout(concrete)?.size.bytes_usize();
            let fits = at.checked_add(offset).and_then(|it| it.checked_add(size)).is_some_and(|end| end <= memory.len());
            if (!raw || fits) && self.walked.insert((start, at + offset, concrete, 0)) {
                self.walk(memory, at + offset, concrete)?;
            }
        }
        Ok(())
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
