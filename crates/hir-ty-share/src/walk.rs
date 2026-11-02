//! The share key of an instance, in two passes over its generic body.
//!
//! - [`sites`], once per generic body and options, walks the body and lists, in walk order,
//!   each value an instance's records are read from (every local's type, and each type or
//!   constant where the body names a generic parameter) with the ops that make records of them.
//! - Per instance, the listed values are substituted at once by `hir-ty-mir`'s
//!   `monomorphized_args`, the fold `monomorphized` fills a body with, so a value here and the
//!   same value in the monomorphized body the code generator compiles are one. The ops then
//!   replay over them. A value `monomorphized` leaves as it is (a `Yield`'s and a
//!   `DropAndReplace`'s) is kept as the generic body has it.
//!
//! Where a type or constant of the generic body names a parameter, the records read what an
//! instance has in its place:
//!
//! - a local's type: its fact; each type a `Deref` of the local's place reaches: its fact;
//! - a constant: its value, evaluated as the code generator's `eval_const` does; its type:
//!   its fact; a promoted constant's value, as `promoted_constant` makes it;
//! - a call: the callee by its own share key, or by what the code generator's walk asks for
//!   it (an intrinsic by [`classify`], drop glue by [`drop::key`], a `dyn` call, a builtin
//!   impl's method, `Future::poll` of a coroutine, a builtin derive by full identity);
//! - an unsizing coercion, a function or closure turned into a pointer and a `dyn*` cast:
//!   both types' identities (the vtable or shim it makes is named by them); another cast:
//!   the target's fact;
//! - a drop: its drop key; an aggregate: its type's fact; an `asm!` `sym`: its identity.
//!
//! Whether something is read depends only on the generic body, so the records of two
//! instances line up one for one, and equal records mean equal monomorphized bodies up to the
//! facts the code generator reads. Each record is a tag byte and a 32-byte content ID.
//!
//! A call of a generic instance of the body itself is recorded as such: direct recursion
//! shares. Any other cycle of instances gives [`Unshared::Recursive`] to all in it.

use std::hash::Hash as _;

use base_db::Db;
use blaze_hash::HashSet;
use blaze_ids::{ContentId, Hasher};
use blaze_items::{ItemId, closure_code, code_hash, coroutine_code, interface_hash, item_key, key};
use blaze_query::db::{AsId, FromId};
use either::Either;
use hir_def::{
    CallableDefId, FunctionId, HasModule, ItemContainerId, Lookup, hir::ExprId,
    signatures::FunctionSignature,
};
use hir_ty::{
    InferBodyId, MemoryMap, PointerCast,
    db::{HirDatabase, InternedClosureId, InternedCoroutineId},
    intrinsic::{Intrinsic, intrinsic_of},
    method_resolution::{is_dyn_method, lookup_impl_const},
    mir::{MirOptions, MirOwner, MirStage},
    next_solver::{
        AliasConst, AliasConstKind, AllocationData, Const, ConstKind, FreeConstAliasId, GenericArg,
        GenericArgs, InherentAssocConstId, StoredGenericArgs, TraitAssocConstId, Ty, TyKind,
    },
    traits::StoredParamEnvAndCrate,
};
use hir_ty_mir::mir::{
    AggregateKind, AsmOperand, BasicBlock, CastKind, InlineAsm, MirBody, Operand, OperandKind,
    PlaceElem, PlaceTy, ProjectionElem, Rvalue, Statement, StatementKind, StoredPlace,
    TerminatorKind, monomorphized_args, monomorphized_promoted_mir_body_query,
    runtime_promoted_mir_body_query,
};
use rustc_type_ir::{
    GenericArgKind, TypeFlags, TypeVisitableExt,
    inherent::{GenericArgs as _, IntoKind},
};
use smallvec::SmallVec;

use crate::{
    BodyOptions, CALLEE_DOMAIN, Class, OptionsSet, SHARE_DOMAIN, ShareDef, ShareInstance, ShareKey,
    Unshared, VALUE_DOMAIN, WALK_DOMAIN, classify, drop, fact::Cx, identity,
};

/// A generic parameter, of a type or a constant. A lifetime is erased before codegen.
const PARAMS: TypeFlags = TypeFlags::HAS_TY_PARAM.union(TypeFlags::HAS_CT_PARAM);

/// What a record of the walk is of.
mod tag {
    pub(super) const LOCAL: u8 = 1;
    pub(super) const DEREF: u8 = 2;
    pub(super) const CONST_TY: u8 = 3;
    pub(super) const CONST: u8 = 4;
    pub(super) const ALLOCATION_TY: u8 = 5;
    pub(super) const CAST: u8 = 6;
    pub(super) const COERCE_FROM: u8 = 7;
    pub(super) const COERCE_TO: u8 = 8;
    pub(super) const REPEAT: u8 = 9;
    pub(super) const AGGREGATE: u8 = 10;
    pub(super) const CALL: u8 = 11;
    pub(super) const DROP: u8 = 12;
    pub(super) const PROMOTED: u8 = 13;
    pub(super) const PROMOTED_NONE: u8 = 14;
    pub(super) const SYM: u8 = 15;

    pub(super) const DEF_FN: u8 = 1;
    pub(super) const DEF_CLOSURE: u8 = 2;
    pub(super) const DEF_COROUTINE: u8 = 3;
    pub(super) const DEF_COROUTINE_BY_MOVE: u8 = 4;

    pub(super) const CALLEE_FN: u8 = 1;
    pub(super) const CALLEE_SELF: u8 = 2;
    pub(super) const CALLEE_SHARED: u8 = 3;
    pub(super) const CALLEE_ARGS: u8 = 4;
    pub(super) const CALLEE_INTRINSIC: u8 = 5;
    pub(super) const CALLEE_DROP: u8 = 6;
    pub(super) const CALLEE_FN_TRAIT: u8 = 7;
    pub(super) const CALLEE_CLOSURE: u8 = 8;
    pub(super) const CALLEE_IDENTITY: u8 = 9;
    pub(super) const CALLEE_POINTER: u8 = 10;

    pub(super) const ARG_TY: u8 = 1;
    pub(super) const ARG_CONST: u8 = 2;
}

fn has_params<'db>(ty: Ty<'db>) -> bool {
    ty.has_type_flags(PARAMS)
}

/// The value of a constant of the monomorphized body: a value's identity, or what an
/// unevaluated one evaluates to, as the code generator's `eval_const` evaluates it.
pub(crate) fn value<'db>(cx: &Cx<'db>, konst: Const<'db>) -> Result<ContentId, Unshared> {
    let ConstKind::Alias(_, AliasConst { kind, args, .. }) = konst.kind() else {
        return identity::konst(cx.db, konst);
    };
    let db = cx.db;
    let allocation = match kind {
        AliasConstKind::Projection { def_id: TraitAssocConstId(id) }
        | AliasConstKind::InherentImpl { def_id: InherentAssocConstId(id) }
        | AliasConstKind::Free { def_id: FreeConstAliasId(id) } => {
            let (id, args) = lookup_impl_const(&cx.infcx, cx.env.param_env, id, args);
            db.const_eval(id, args, Some(cx.env))
        }
        AliasConstKind::Anon { def_id } => db.anon_const_eval(def_id.0, args, Some(cx.env)),
        AliasConstKind::InherentSelf { .. } => {
            return Err(Unshared::ConstEval(format!( // alloc: error path
                "`{konst:?}`: an inherent const in Self form after type check"
            )));
        }
    }
    .map_err(|error| Unshared::ConstEval(format!("`{konst:?}`: {error:?}")))?; // alloc: error path
    allocation_value(db, &allocation)
}

/// An evaluated constant's bytes and the memory it points to. Its `TypeId`s are numbers of the
/// evaluation, which the code generator replaces by their types' identities: so are they here.
fn allocation_value(db: &Db, allocation: &AllocationData<'_>) -> Result<ContentId, Unshared> {
    let mut h = Hasher::new(VALUE_DOMAIN);
    h.part(&allocation.memory);
    match &allocation.memory_map {
        MemoryMap::Empty => {
            h.update(&[0]);
        }
        MemoryMap::Simple(pointee) => {
            h.update(&[1]).part(pointee);
        }
        // A static is named by the database's own id, which is no content; the rest of the
        // map is bytes, addresses and types.
        MemoryMap::Complex(map) if !map.statics.is_empty() => {
            return Err(Unshared::NoContent(format!( // alloc: error path
                "a constant of `{:?}` that points to a static",
                allocation.ty
            )));
        }
        MemoryMap::Complex(map) => {
            // The map's own `Hash` reads every field; the types of its `TypeId`s and of its
            // function pointers and vtables are taken out first and hashed by their identity.
            // A function pointer's or vtable's bytes are its id, `FIRST_ID` plus its type's
            // place in the table, so the table is hashed in that order.
            // clone: once per constant with a `TypeId` or more pieces, on the query's miss
            let mut bare = (**map).clone();
            let types = std::mem::take(&mut bare.type_ids);
            let pointees = std::mem::take(&mut bare.vtable);
            bare.hash(&mut Fed(h.update(&[2])));
            h.update(&(types.len() as u64).to_le_bytes());
            for ty in types {
                match ty {
                    Some(ty) => h.update(&[1]).id(&identity::ty(db, ty)?),
                    None => h.update(&[0]),
                };
            }
            h.update(&(pointees.tys().len() as u64).to_le_bytes());
            for ty in pointees.tys() {
                h.id(&identity::ty(db, ty)?);
            }
        }
    };
    Ok(h.finish())
}

/// A [`std::hash::Hasher`] into a content hash: every byte it is given is hashed.
struct Fed<'a>(&'a mut Hasher);

impl std::hash::Hasher for Fed<'_> {
    fn finish(&self) -> u64 {
        unreachable!("a content hash is finished by its owner")
    }

    fn write(&mut self, bytes: &[u8]) {
        self.0.update(bytes);
    }
}

#[blaze_query::tracked(returns(ref), cycle_result = share_cycle)]
pub(crate) fn share(
    db: &Db,
    instance: ShareInstance,
    env: StoredParamEnvAndCrate,
    options: BodyOptions,
) -> Result<ShareKey, Unshared> {
    let _on = OnChain::enter(db, &instance, env.krate)?;
    let cx = Cx::new(db, env);
    let mut key = Hasher::new(SHARE_DOMAIN);
    let bodies = bodies(db, instance.def, options, &mut key)?;
    // The code generator reads an instance's const arguments itself (a SIMD lane).
    for konst in instance.args.as_ref().iter().filter_map(|arg| arg.konst()) {
        key.id(&value(&cx, konst)?);
    }
    // Options that give one body (every options without UB checks give a coroutine its
    // runtime body) walk it once.
    let mut walked: SmallVec<[(&MirBody<'_>, ContentId); 4]> = SmallVec::new();
    for (asked, generic) in bodies {
        let records = match walked.iter().find(|(body, _)| std::ptr::eq(*body, generic)) {
            Some(&(_, records)) => records,
            None => {
                let sites = sites(db, instance.def, asked).as_ref().map_err(Clone::clone)?;
                let records = Walk::records(&cx, &instance, options, generic.owner, sites)?;
                walked.push((generic, records));
                records
            }
        };
        key.update(&[OptionsSet::index(asked) as u8]).id(&records);
    }
    Ok(ShareKey(key.finish()))
}

/// The instances whose share a job is walking, outermost first, each with the crate its chain
/// started in.
type Chain = Vec<(ShareInstance, base_db::Crate)>;

thread_local! {
    /// The running job's [`Chain`]: a share walks its callees' shares inline, so this is the
    /// chain of calls rustc's collector counts a body's recursion depth along. A callee's share
    /// may wait and resume the job on another thread, so the chain moves with the job
    /// ([`carry_chain`]), and it is read only in `#[inline(never)]` functions that end before
    /// any wait.
    static CHAIN: std::cell::RefCell<Chain> = const { std::cell::RefCell::new(Vec::new()) };
}

/// The chains of jobs switched out with one, by the slot [`take_chain`] gave (its index + 1;
/// 0 is an empty chain, which moves without a slot). Held only to move one chain in or out.
static PARKED: parking_lot::Mutex<Vec<Option<Chain>>> = parking_lot::Mutex::new(Vec::new());

/// Has [`CHAIN`] moved with each job across its waits. Once a process, before the first chain
/// is made.
fn carry_chain() {
    static CARRIED: std::sync::Once = std::sync::Once::new();
    CARRIED.call_once(|| blaze_query::carry(blaze_query::Carried { take: take_chain, put: put_chain }));
}

/// Moves the switching job's chain off this thread, leaving an empty one.
#[inline(never)]
fn take_chain() -> usize {
    let chain = CHAIN.with_borrow_mut(std::mem::take);
    if chain.is_empty() {
        return 0;
    }
    let mut parked = PARKED.lock();
    let at = match parked.iter().position(Option::is_none) {
        Some(at) => at,
        None => {
            parked.push(None);
            parked.len() - 1
        }
    };
    parked[at] = Some(chain);
    at + 1
}

/// Puts the chain [`take_chain`] moved off back, on the thread the job resumes on.
///
/// # Panics
///
/// If the thread's own chain is not empty, or `taken` names no parked chain.
#[inline(never)]
fn put_chain(taken: usize) {
    let chain = match taken {
        0 => Vec::new(),
        at => PARKED.lock()[at - 1].take().expect("a parked chain is put back once"),
    };
    CHAIN.with_borrow_mut(|it| {
        assert!(it.is_empty(), "a job resumed on a thread whose share chain holds {} instances", it.len());
        *it = chain;
    });
}

/// An instance on [`CHAIN`] while its share is walked.
struct OnChain;

impl OnChain {
    /// `instance` onto the chain, or [`Unshared::RecursionLimit`] when its body is on it as
    /// many times as the `#![recursion_limit]` of the crate the chain started in (`krate`
    /// for an outermost one) allows. The limit is a query, which may wait, so each read of
    /// the chain is a function of its own.
    fn enter(db: &Db, instance: &ShareInstance, krate: base_db::Crate) -> Result<OnChain, Unshared> {
        carry_chain();
        let (root, depth) = chain_depth(instance.def, krate);
        let limit = hir_def::nameres::crate_def_map(db, root).recursion_limit();
        if depth > limit {
            return Err(recursion_limit(db, &chain_now(), instance, root, depth, limit));
        }
        chain_push(instance, root);
        Ok(OnChain)
    }
}

/// The crate the running job's chain started in (`krate` when it is empty), and how many
/// times `def` would be on it with one more.
#[inline(never)]
fn chain_depth(def: ShareDef, krate: base_db::Crate) -> (base_db::Crate, u32) {
    CHAIN.with_borrow(|chain| {
        let depth = 1 + chain.iter().filter(|it| it.0.def == def).count() as u32;
        (chain.first().map_or(krate, |it| it.1), depth)
    })
}

/// The running job's chain, for an error.
#[inline(never)]
fn chain_now() -> Chain {
    CHAIN.with_borrow(Clone::clone) // clone: an error's chain, once
}

#[inline(never)]
fn chain_push(instance: &ShareInstance, root: base_db::Crate) {
    // clone: the query's key, a reference count
    CHAIN.with_borrow_mut(|chain| chain.push((instance.clone(), root)));
}

impl Drop for OnChain {
    #[inline(never)]
    fn drop(&mut self) {
        CHAIN.with_borrow_mut(|chain| chain.pop());
    }
}

/// How many instances of a chain past the recursion limit its error names from its start,
/// where the arguments begin to grow, and from its end.
const CHAIN_HEAD: usize = 10;
const CHAIN_TAIL: usize = 3;

/// The error of `instance`, `depth` times its body on `chain`, past `root`'s `limit`.
fn recursion_limit(
    db: &Db,
    chain: &[(ShareInstance, base_db::Crate)],
    instance: &ShareInstance,
    root: base_db::Crate,
    depth: u32,
    limit: u32,
) -> Unshared {
    let named = |it: &ShareInstance| -> String {
        let spelled = match it.def {
            ShareDef::Fn(func) => {
                hir_ty_mir::mir::type_name::rustc_function_name(db, func, it.args.as_ref().as_slice())
            }
            _ => None,
        };
        spelled.unwrap_or_else(|| format!("{it:?}")) // alloc: the error's message
    };
    let body = match instance.def {
        ShareDef::Fn(func) => FunctionSignature::of(db, func).name.as_str().to_owned(), // alloc: the error's message
        def => format!("{def:?}"), // alloc: the error's message
    };
    let krate = (root.extra_data(db).display_name.as_ref())
        .map_or_else(|| format!("{root:?}"), |it| it.to_string()); // alloc: the error's message
    let ends: Vec<String> = if chain.len() <= CHAIN_HEAD + CHAIN_TAIL {
        chain.iter().map(|it| named(&it.0)).collect() // alloc: the error's message
    } else {
        let (head, tail) = (&chain[..CHAIN_HEAD], &chain[chain.len() - CHAIN_TAIL..]);
        let between = format!("({} more)", chain.len() - CHAIN_HEAD - CHAIN_TAIL); // alloc: the error's message
        // alloc: the error's message, its two ends
        head.iter().map(|it| named(&it.0)).chain([between]).chain(tail.iter().map(|it| named(&it.0))).collect()
    };
    Unshared::RecursionLimit { krate, body, depth, limit, instance: named(instance), chain: ends }
}

fn share_cycle(
    _: &Db,
    _: blaze_query::db::Id,
    _: ShareInstance,
    _: StoredParamEnvAndCrate,
    _: BodyOptions,
) -> Result<ShareKey, Unshared> {
    Err(Unshared::Recursive)
}

type Bodies<'db> = SmallVec<[(MirOptions, &'db MirBody<'db>); 4]>;

/// The generic bodies the code generator may compile `def` from, one per options, with what
/// names the body hashed into `key`.
fn bodies<'db>(
    db: &'db Db,
    def: ShareDef,
    options: BodyOptions,
    key: &mut Hasher,
) -> Result<Bodies<'db>, Unshared> {
    let owner = match def {
        ShareDef::Fn(func) => {
            let item = ItemId::Function(func);
            let code = code_hash(db, item).map_err(identity::unhashable(db))?.ok_or_else(|| {
                Unshared::NoMir(format!("`{}` has no body", FunctionSignature::of(db, func).name.as_str())) // alloc: error path
            })?;
            key.update(&[tag::DEF_FN]).id(&item_key(db, item).0).id(&code.0);
            InferBodyId::DefWithBodyId(func.into())
        }
        ShareDef::Closure(id) => {
            let closure = InternedClosureId::from_id(id);
            let code = closure_code(db, closure).map_err(identity::unhashable(db))?;
            key.update(&[tag::DEF_CLOSURE]).id(&code.0);
            closure.loc(db).owner
        }
        ShareDef::Coroutine(id) | ShareDef::CoroutineByMove(id) => {
            let coroutine = InternedCoroutineId::from_id(id);
            let code = coroutine_code(db, coroutine).map_err(identity::unhashable(db))?;
            let what = match def {
                ShareDef::CoroutineByMove(_) => tag::DEF_COROUTINE_BY_MOVE,
                _ => tag::DEF_COROUTINE,
            };
            key.update(&[what]).id(&code.0);
            coroutine.loc(db).owner
        }
        ShareDef::DropGlue => return Err(Unshared::Shim("drop glue")),
        ShareDef::CallOnceShim => return Err(Unshared::Shim("`FnOnce::call_once` shim")),
        ShareDef::VtableShim(_) => return Err(Unshared::Shim("vtable shim")),
        ShareDef::FnPointerShim => return Err(Unshared::Shim("function pointer shim")),
        ShareDef::ReifyShim { .. } => return Err(Unshared::Shim("reify shim")),
    };
    let set = if owner.krate(db).data(db).origin.is_lang() { options.lang } else { options.other };
    if set.is_empty() {
        return Err(Unshared::NoOptions);
    }
    let mut bodies = Bodies::new();
    for asked in set.iter() {
        bodies.push((asked, body_of(db, def, asked)?));
    }
    Ok(bodies)
}

/// The generic body the code generator compiles `def` from under `options`, as
/// [`MirStage::compiled`] picks it for both.
fn body_of<'db>(
    db: &'db Db,
    def: ShareDef,
    options: MirOptions,
) -> Result<&'db MirBody<'db>, Unshared> {
    let owner = match def {
        ShareDef::Fn(func) => MirOwner::Body(InferBodyId::DefWithBodyId(func.into())),
        ShareDef::Closure(id) => MirOwner::Closure(InternedClosureId::from_id(id)),
        ShareDef::Coroutine(id) => MirOwner::Coroutine(InternedCoroutineId::from_id(id)),
        ShareDef::CoroutineByMove(id) => {
            MirOwner::CoroutineByMove(InternedCoroutineId::from_id(id))
        }
        ShareDef::DropGlue
        | ShareDef::CallOnceShim
        | ShareDef::VtableShim(_)
        | ShareDef::FnPointerShim
        | ShareDef::ReifyShim { .. } => return Err(Unshared::Shim("a shim, which has no generic body")),
    };
    db.mir(owner, MirStage::compiled(owner, options))
        .map_err(|error| Unshared::NoMir(format!("{error:?}"))) // alloc: error path
}

/// What the instances of one generic body under one options read their records from.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Sites {
    /// The type of each local, by its index, then each value a site reads that `monomorphized`
    /// fills: substituted per instance by one fold.
    pub(crate) filled: StoredGenericArgs,
    /// The values at sites `monomorphized` leaves as they are, read as the generic body has
    /// them.
    kept: StoredGenericArgs,
    /// What the records are made of, in walk order.
    ops: Box<[Op]>,
}

/// Where a site's value is: its index in [`Sites::filled`] or [`Sites::kept`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum At {
    Filled(u32),
    Kept(u32),
}

/// What an operand's type is read from.
#[derive(Debug, PartialEq, Eq)]
enum TyOf {
    /// The place's local's type, projected.
    Place(StoredPlace),
    /// A constant's type, normalized.
    Constant(At),
    Allocation(At),
    Static,
}

/// One step of an instance's walk, in the order of the generic body.
#[derive(Debug, PartialEq, Eq)]
enum Op {
    /// The tag, then the fact of the type at the site.
    Fact(u8, At),
    /// The tag, then the value of the constant at the site.
    Value(u8, At),
    /// An ADT aggregate's arguments: `len` filled values from `start`.
    AggregateArgs { start: u32, len: u32 },
    /// A promoted constant whose body names a parameter.
    Promoted(ExprId),
    Cast { kind: CastKind, from: TyOf, to: At },
    /// The fact at each `Deref` of a place whose local's type names a parameter.
    Derefs(StoredPlace),
    Drop(StoredPlace),
    Sym(TyOf),
    Call { func: TyOf, intrinsic: Option<Intrinsic> },
}

/// The sites of `def`'s generic body under `options`. It reads no argument, so every instance
/// of the body shares one.
#[blaze_query::tracked(returns(ref))]
pub(crate) fn sites(db: &Db, def: ShareDef, options: MirOptions) -> Result<Sites, Unshared> {
    let body = body_of(db, def, options)?;
    let mut collect = Collect {
        db,
        body,
        // alloc: once per generic body and options, this query's miss
        filled: Vec::with_capacity(body.locals.len() + 16),
        kept: Vec::new(), // alloc: once per generic body and options, this query's miss
        ops: Vec::new(), // alloc: once per generic body and options, this query's miss
        derefs: HashSet::default(),
    };
    collect.body();
    Ok(Sites {
        filled: GenericArgs::new_from_slice(&collect.filled).store(),
        kept: GenericArgs::new_from_slice(&collect.kept).store(),
        ops: collect.ops.into_boxed_slice(),
    })
}

/// The first pass: a generic body's sites, read from the body alone.
struct Collect<'a, 'db> {
    db: &'db Db,
    body: &'a MirBody<'db>,
    filled: Vec<GenericArg<'db>>,
    kept: Vec<GenericArg<'db>>,
    ops: Vec<Op>,
    /// The places whose `Deref`s are recorded, once each.
    derefs: HashSet<&'a StoredPlace>,
}

impl<'a, 'db> Collect<'a, 'db> {
    fn push(&mut self, arg: GenericArg<'db>, filled: bool) -> At {
        match filled {
            true => {
                self.filled.push(arg);
                At::Filled(self.filled.len() as u32 - 1)
            }
            false => {
                self.kept.push(arg);
                At::Kept(self.kept.len() as u32 - 1)
            }
        }
    }

    fn local_ty(&self, place: &StoredPlace) -> Ty<'db> {
        self.body.locals[place.local].ty.as_ref()
    }

    fn body(&mut self) {
        let body = self.body;
        for (_, local) in body.locals.iter() {
            self.filled.push(local.ty.as_ref().into());
        }
        for (local, data) in body.locals.iter() {
            if has_params(data.ty.as_ref()) {
                self.ops.push(Op::Fact(tag::LOCAL, At::Filled(local.into_raw().into_u32())));
            }
        }
        for &(_, expr) in &body.promoted_locals {
            let Ok(promoted) = runtime_promoted_mir_body_query(self.db, body.owner, expr) else {
                continue;
            };
            if mentions_params(promoted) {
                self.ops.push(Op::Promoted(expr));
            }
        }
        for (_, block) in body.basic_blocks.iter() {
            self.block(block);
        }
    }

    fn block(&mut self, block: &'a BasicBlock) {
        for statement in &block.statements {
            self.statement(statement);
        }
        if let Some(terminator) = &block.terminator {
            self.terminator(&terminator.kind);
        }
    }

    fn statement(&mut self, statement: &'a Statement) {
        match &statement.kind {
            StatementKind::Assign(place, rvalue) => {
                self.place(place);
                self.rvalue(rvalue);
            }
            StatementKind::FakeRead(place) | StatementKind::Deinit(place) => self.place(place),
            StatementKind::InlineAsm(asm) => self.inline_asm(asm),
            StatementKind::StorageLive(_) | StatementKind::StorageDead(_) | StatementKind::Nop => {}
        }
    }

    fn rvalue(&mut self, rvalue: &'a Rvalue) {
        match rvalue {
            Rvalue::Use(operand) | Rvalue::UnaryOp(_, operand) => self.operand(operand, true),
            Rvalue::Repeat(operand, len) => {
                self.operand(operand, true);
                if len.as_ref().has_type_flags(PARAMS) {
                    let at = self.push(len.as_ref().into(), true);
                    self.ops.push(Op::Value(tag::REPEAT, at));
                }
            }
            Rvalue::Ref(_, place)
            | Rvalue::Len(place)
            | Rvalue::Discriminant(place)
            | Rvalue::CopyForDeref(place) => self.place(place),
            Rvalue::Cast(kind, operand, to) => {
                self.operand(operand, true);
                if self.operand_has_params(operand) || has_params(to.as_ref()) {
                    let from = self.ty_of(operand);
                    let to = self.push(to.as_ref().into(), true);
                    // clone: a cast's kind, a few words, on this query's miss
                    self.ops.push(Op::Cast { kind: kind.clone(), from, to });
                }
            }
            Rvalue::BinaryOp(_, left, right) | Rvalue::CheckedBinaryOp(_, left, right) => {
                self.operand(left, true);
                self.operand(right, true);
            }
            Rvalue::Aggregate(kind, operands) => {
                self.aggregate(kind);
                for operand in operands.iter() {
                    self.operand(operand, true);
                }
            }
            Rvalue::ThreadLocalRef(never) | Rvalue::AddressOf(never) | Rvalue::NullaryOp(never) => {
                match *never {}
            }
        }
    }

    fn aggregate(&mut self, kind: &AggregateKind) {
        match kind {
            AggregateKind::Array(ty) | AggregateKind::Tuple(ty) | AggregateKind::Closure(ty) => {
                if has_params(ty.as_ref()) {
                    let at = self.push(ty.as_ref().into(), true);
                    self.ops.push(Op::Fact(tag::AGGREGATE, at));
                }
            }
            AggregateKind::Adt(_, args) => {
                let args = args.as_ref();
                if args.has_type_flags(PARAMS) {
                    let start = self.filled.len() as u32;
                    self.filled.extend(args.iter());
                    self.ops.push(Op::AggregateArgs { start, len: args.len() as u32 });
                }
            }
            AggregateKind::Union(..) => {}
        }
    }

    fn inline_asm(&mut self, asm: &'a InlineAsm) {
        for operand in asm.operands.iter() {
            match operand {
                AsmOperand::Reg { input, output, .. } => {
                    if let Some(input) = input {
                        self.operand(input, true);
                    }
                    if let Some(output) = output {
                        self.place(output);
                    }
                }
                AsmOperand::Const(operand) => self.operand(operand, true),
                AsmOperand::Sym(operand) => {
                    self.operand(operand, true);
                    if self.operand_has_params(operand) {
                        let of = self.ty_of(operand);
                        self.ops.push(Op::Sym(of));
                    }
                }
                AsmOperand::Label => {}
            }
        }
        if let Some(taken) = &asm.taken {
            self.place(taken);
        }
    }

    fn terminator(&mut self, kind: &'a TerminatorKind) {
        match kind {
            TerminatorKind::Goto { .. }
            | TerminatorKind::UnwindResume
            | TerminatorKind::Abort
            | TerminatorKind::Return
            | TerminatorKind::Unreachable
            | TerminatorKind::CoroutineDrop
            | TerminatorKind::FalseEdge { .. }
            | TerminatorKind::FalseUnwind { .. } => {}
            TerminatorKind::SwitchInt { discr, .. } => self.operand(discr, true),
            // `monomorphized` leaves the value of these two as the generic body has it.
            TerminatorKind::Yield { value, resume_arg, .. } => {
                self.operand(value, false);
                self.place(resume_arg);
            }
            TerminatorKind::Drop { place, .. } => self.drop(place),
            TerminatorKind::DropAndReplace { place, value, .. } => {
                self.drop(place);
                self.operand(value, false);
            }
            TerminatorKind::Assert { cond, msg, .. } => {
                self.operand(cond, true);
                let mut any = false;
                msg.for_each_operand(&mut |it| any |= self.operand_has_params(it));
                if any {
                    let mut operands = SmallVec::<[Operand; 2]>::new();
                    // clone: an assertion's message operands, a few handles, on this query's miss
                    msg.for_each_operand(&mut |it| operands.push(it.clone()));
                    for operand in &operands {
                        self.owned_operand(operand);
                    }
                }
            }
            TerminatorKind::Call { func, args, destination, intrinsic, .. } => {
                self.operand(func, true);
                if self.operand_has_params(func) {
                    let func = self.ty_of(func);
                    self.ops.push(Op::Call { func, intrinsic: *intrinsic });
                }
                for operand in args.iter() {
                    self.operand(operand, true);
                }
                self.place(destination);
            }
            // A `Call` whose destination is the caller's return place.
            TerminatorKind::TailCall { func, args, .. } => {
                self.operand(func, true);
                if self.operand_has_params(func) {
                    let func = self.ty_of(func);
                    self.ops.push(Op::Call { func, intrinsic: None });
                }
                for operand in args.iter() {
                    self.operand(operand, true);
                }
            }
        }
    }

    fn drop(&mut self, place: &'a StoredPlace) {
        self.place(place);
        if has_params(self.local_ty(place)) {
            // clone: a place, a local and an interned projection, on this query's miss
            self.ops.push(Op::Drop(place.clone()));
        }
    }

    /// An operand at a site `monomorphized` fills or, with `filled` false, leaves as it is.
    fn operand(&mut self, operand: &'a Operand, filled: bool) {
        match &operand.kind {
            OperandKind::Copy(place) | OperandKind::Move(place) => self.place(place),
            _ => self.constant(operand, filled),
        }
    }

    /// [`Self::operand`] of operands the walk holds copies of, whose places it does not keep.
    fn owned_operand(&mut self, operand: &Operand) {
        match &operand.kind {
            OperandKind::Copy(place) | OperandKind::Move(place) => {
                if self.has_derefs(place) {
                    // clone: a place, a local and an interned projection, on this query's miss
                    self.ops.push(Op::Derefs(place.clone()));
                }
            }
            _ => self.constant(operand, true),
        }
    }

    fn constant(&mut self, operand: &Operand, filled: bool) {
        match &operand.kind {
            OperandKind::Constant { konst, ty } => {
                if has_params(ty.as_ref()) {
                    let at = self.push(ty.as_ref().into(), filled);
                    self.ops.push(Op::Fact(tag::CONST_TY, at));
                }
                if konst.as_ref().has_type_flags(PARAMS) {
                    let at = self.push(konst.as_ref().into(), filled);
                    self.ops.push(Op::Value(tag::CONST, at));
                }
            }
            OperandKind::Allocation { allocation } => {
                let ty = allocation.as_ref().ty;
                if has_params(ty) {
                    let at = self.push(ty.into(), filled);
                    self.ops.push(Op::Fact(tag::ALLOCATION_TY, at));
                }
            }
            OperandKind::Static(_) | OperandKind::Copy(_) | OperandKind::Move(_) => {}
        }
    }

    /// Whether a place has a `Deref` and its local's type names a parameter.
    fn has_derefs(&self, place: &StoredPlace) -> bool {
        place.projection.as_slice().contains(&ProjectionElem::Deref)
            && has_params(self.local_ty(place))
    }

    /// A place's `Deref`s, once per place of the generic body.
    fn place(&mut self, place: &'a StoredPlace) {
        if self.has_derefs(place) && self.derefs.insert(place) {
            // clone: a place, a local and an interned projection, on this query's miss
            self.ops.push(Op::Derefs(place.clone()));
        }
    }

    /// Where the type of an operand at a filled site is read from.
    fn ty_of(&mut self, operand: &Operand) -> TyOf {
        match &operand.kind {
            // clone: a place, a local and an interned projection, on this query's miss
            OperandKind::Copy(place) | OperandKind::Move(place) => TyOf::Place(place.clone()),
            OperandKind::Constant { ty, .. } => TyOf::Constant(self.push(ty.as_ref().into(), true)),
            OperandKind::Allocation { allocation } => {
                TyOf::Allocation(self.push(allocation.as_ref().ty.into(), true))
            }
            OperandKind::Static(_) => TyOf::Static,
        }
    }

    /// Whether the generic body's `operand` is of a type that names a parameter.
    fn operand_has_params(&self, operand: &Operand) -> bool {
        match &operand.kind {
            OperandKind::Copy(place) | OperandKind::Move(place) => has_params(self.local_ty(place)),
            OperandKind::Constant { konst, ty } => {
                has_params(ty.as_ref()) || konst.as_ref().has_type_flags(PARAMS)
            }
            OperandKind::Allocation { allocation } => has_params(allocation.as_ref().ty),
            OperandKind::Static(_) => false,
        }
    }
}

/// The second pass: one instance's records, from its body's sites with its arguments put in.
struct Walk<'a, 'db> {
    cx: &'a Cx<'db>,
    instance: &'a ShareInstance,
    options: BodyOptions,
    owner: InferBodyId<'db>,
    filled: &'db [GenericArg<'db>],
    kept: &'db [GenericArg<'db>],
    /// The records, hashed as they are made.
    out: Hasher,
}

impl<'a, 'db> Walk<'a, 'db> {
    /// The records of `instance` over `sites`: every filled value substituted by one fold of
    /// `monomorphized`'s, then each op in order.
    fn records(
        cx: &'a Cx<'db>,
        instance: &'a ShareInstance,
        options: BodyOptions,
        owner: InferBodyId<'db>,
        sites: &Sites,
    ) -> Result<ContentId, Unshared> {
        let filled = monomorphized_args(cx.db, sites.filled.as_ref(), &instance.args, &cx.stored)
            .map_err(|error| Unshared::NoMir(format!("monomorphized: {error:?}")))?; // alloc: error path
        let mut walk = Walk {
            cx,
            instance,
            options,
            owner,
            filled: filled.as_slice(),
            kept: sites.kept.as_ref().as_slice(),
            out: Hasher::new(WALK_DOMAIN),
        };
        for op in &sites.ops {
            walk.op(op)?;
        }
        Ok(walk.out.finish())
    }

    fn db(&self) -> &'db Db {
        self.cx.db
    }

    fn record(&mut self, tag: u8, id: ContentId) {
        self.out.update(&[tag]).id(&id);
    }

    fn arg(&self, at: At) -> GenericArg<'db> {
        match at {
            At::Filled(index) => self.filled[index as usize],
            At::Kept(index) => self.kept[index as usize],
        }
    }

    fn ty(&self, at: At) -> Ty<'db> {
        self.arg(at).ty().expect("a type's site holds a type")
    }

    fn konst(&self, at: At) -> Const<'db> {
        self.arg(at).konst().expect("a constant's site holds a constant")
    }

    fn op(&mut self, op: &Op) -> Result<(), Unshared> {
        match op {
            Op::Fact(tag, at) => {
                let ty = self.ty(*at);
                // A fact asks the solver, whose frames below a deep share go past the stack.
                let fact = stdx::ensure_sufficient_stack(|| self.cx.fact(ty))?;
                self.record(*tag, fact);
            }
            Op::Value(tag, at) => {
                let value = value(self.cx, self.konst(*at))?;
                self.record(*tag, value);
            }
            Op::AggregateArgs { start, len } => {
                let args = &self.filled[*start as usize..(*start + *len) as usize];
                let facts = self.args_facts(args.iter().copied())?;
                self.record(tag::AGGREGATE, facts);
            }
            Op::Promoted(expr) => self.promoted(*expr)?,
            Op::Cast { kind, from, to } => {
                let from = self.ty_of(from)?;
                let to = self.cx.normalize(self.ty(*to));
                self.cast(kind, from, to)?;
            }
            Op::Derefs(place) => self.derefs(place)?,
            Op::Drop(place) => {
                let key = drop::key(self.cx, self.place_ty(place)?)?;
                self.record(tag::DROP, key);
            }
            Op::Sym(of) => {
                let sym = identity::ty(self.db(), self.ty_of(of)?)?;
                self.record(tag::SYM, sym);
            }
            Op::Call { func, intrinsic } => {
                let callee = self.callee(self.ty_of(func)?, *intrinsic)?;
                self.record(tag::CALL, callee);
            }
        }
        Ok(())
    }

    /// A promoted constant by its value.
    fn promoted(&mut self, expr: ExprId) -> Result<(), Unshared> {
        let db = self.db();
        let evaluated = monomorphized_promoted_mir_body_query(
            db,
            self.owner,
            expr,
            self.instance.args.clone(), // clone: the query's key, a reference count
            self.cx.stored.clone(),     // clone: the query's key, a reference count
        )
        .ok()
        .and_then(|body| db.interpret_mir(body, Some(self.cx.env)).ok());
        match evaluated {
            Some(allocation) => {
                let value = allocation_value(db, &allocation)?;
                self.record(tag::PROMOTED, value);
            }
            // The code generator does not promote a constant it can't evaluate.
            None => self.record(tag::PROMOTED_NONE, ContentId::default()),
        }
        Ok(())
    }

    /// The fact at each `Deref` of `place`, up to its last.
    fn derefs(&mut self, place: &StoredPlace) -> Result<(), Unshared> {
        let projection = place.projection.as_slice();
        let Some(last) = projection.iter().rposition(|it| *it == ProjectionElem::Deref) else {
            return Ok(());
        };
        let mut ty = PlaceTy::from_ty(self.cx.normalize(self.local_ty(place)));
        for elem in &projection[..=last] {
            ty = self.project(ty, elem)?;
            if let ProjectionElem::Deref = elem {
                let fact = self.cx.fact(ty.ty)?;
                self.record(tag::DEREF, fact);
            }
        }
        Ok(())
    }

    /// The instance's type of a place's local: the locals' types lead [`Sites::filled`].
    fn local_ty(&self, place: &StoredPlace) -> Ty<'db> {
        self.ty(At::Filled(place.local.into_raw().into_u32()))
    }

    fn place_ty(&self, place: &StoredPlace) -> Result<Ty<'db>, Unshared> {
        let mut ty = PlaceTy::from_ty(self.cx.normalize(self.local_ty(place)));
        for elem in place.projection.as_slice() {
            ty = self.project(ty, elem)?;
        }
        Ok(ty.ty)
    }

    fn ty_of(&self, of: &TyOf) -> Result<Ty<'db>, Unshared> {
        match of {
            TyOf::Place(place) => self.place_ty(place),
            TyOf::Constant(at) => Ok(self.cx.normalize(self.ty(*at))),
            TyOf::Allocation(at) => Ok(self.ty(*at)),
            TyOf::Static => {
                Err(Unshared::NoContent("the type of a static's operand".to_owned())) // alloc: error path
            }
        }
    }

    /// The facts of a list of arguments' types and the values of its constants.
    fn args_facts(
        &self,
        args: impl IntoIterator<Item = GenericArg<'db>>,
    ) -> Result<ContentId, Unshared> {
        let mut h = Hasher::new(CALLEE_DOMAIN);
        for arg in args {
            match arg.kind() {
                GenericArgKind::Type(ty) => h.update(&[tag::ARG_TY]).id(&self.cx.fact(ty)?),
                GenericArgKind::Const(konst) => {
                    h.update(&[tag::ARG_CONST]).id(&value(self.cx, konst)?)
                }
                GenericArgKind::Lifetime(_) => continue,
            };
        }
        Ok(h.finish())
    }

    /// A cast whose source or target names a parameter in the generic body.
    fn cast(&mut self, kind: &CastKind, from: Ty<'db>, to: Ty<'db>) -> Result<(), Unshared> {
        let coerces = match kind {
            CastKind::PointerCoercion(PointerCast::Unsize) => !is_array_to_slice(from, to),
            CastKind::PointerCoercion(
                PointerCast::ReifyFnPointer | PointerCast::ClosureFnPointer(_),
            )
            | CastKind::DynStar => true,
            CastKind::PointerCoercion(
                PointerCast::UnsafeFnPointer
                | PointerCast::MutToConstPointer
                | PointerCast::ArrayToPointer,
            )
            | CastKind::PointerExposeAddress
            | CastKind::PointerFromExposedAddress
            | CastKind::PtrToPtr
            | CastKind::IntToInt
            | CastKind::FloatToInt
            | CastKind::FloatToFloat
            | CastKind::IntToFloat
            | CastKind::FnPtrToPtr
            | CastKind::Transmute => false,
        };
        if coerces {
            let (from, to) = (identity::ty(self.db(), from)?, identity::ty(self.db(), to)?);
            self.record(tag::COERCE_FROM, from);
            self.record(tag::COERCE_TO, to);
        } else {
            let (from, to) = (self.cx.fact(from)?, self.cx.fact(to)?);
            self.record(tag::CAST, from);
            self.record(tag::CAST, to);
        }
        Ok(())
    }

    fn project(&self, ty: PlaceTy<'db>, elem: &PlaceElem) -> Result<PlaceTy<'db>, Unshared> {
        if ty.variant_id.is_some() && !matches!(ty.ty.kind(), TyKind::Adt(..)) {
            return Err(Unshared::Shape("a field of a coroutine's variant"));
        }
        let mut ty = ty.projection_ty(&self.cx.infcx, elem, self.cx.env.param_env);
        ty.ty = self.cx.normalize(ty.ty);
        Ok(ty)
    }

    /// What the code generator's walk asks for a call of `fn_ty`.
    fn callee(&self, fn_ty: Ty<'db>, intrinsic: Option<Intrinsic>) -> Result<ContentId, Unshared> {
        let fn_ty = self.cx.normalize(fn_ty);
        match fn_ty.kind() {
            // Called through the pointer, whose type's fact its place has.
            TyKind::FnPtr(..) => Ok(ContentId::of(CALLEE_DOMAIN, &[tag::CALLEE_POINTER])),
            TyKind::FnDef(def, args) => {
                let args = args.no_bound_vars().unwrap();
                match def.0 {
                    CallableDefId::FunctionId(func) => self.call_fn(fn_ty, func, args, intrinsic),
                    CallableDefId::StructId(_) | CallableDefId::EnumVariantId(_) => {
                        self.identity_callee(fn_ty)
                    }
                }
            }
            _ => self.identity_callee(fn_ty),
        }
    }

    fn identity_callee(&self, fn_ty: Ty<'db>) -> Result<ContentId, Unshared> {
        let mut h = Hasher::new(CALLEE_DOMAIN);
        h.update(&[tag::CALLEE_IDENTITY]).id(&identity::ty(self.db(), fn_ty)?);
        Ok(h.finish())
    }

    fn call_fn(
        &self,
        fn_ty: Ty<'db>,
        func: FunctionId,
        args: GenericArgs<'db>,
        intrinsic: Option<Intrinsic>,
    ) -> Result<ContentId, Unshared> {
        let db = self.db();
        let intrinsic = match intrinsic {
            Some(intrinsic) => Some(intrinsic),
            None => intrinsic_of(db, func)
                .map_err(|unknown| Unshared::UnknownIntrinsic(unknown.0.as_str().to_owned()))?, // alloc: error path
        };
        if let Some(intrinsic) = intrinsic {
            return self.intrinsic(fn_ty, func, args, intrinsic);
        }
        let lang = self.cx.interner.lang_items();
        if [lang.DropInPlace, lang.DropGlue].contains(&Some(func)) {
            let mut h = Hasher::new(CALLEE_DOMAIN);
            h.update(&[tag::CALLEE_DROP]).id(&drop::key(self.cx, args.type_at(0))?);
            return Ok(h.finish());
        }
        if [lang.Fn_call, lang.FnMut_call_mut, lang.FnOnce_call_once].contains(&Some(func)) {
            return self.fn_trait_call(fn_ty, func, args);
        }
        if Some(func) == lang.FuturePoll
            || is_dyn_method(self.cx.interner, self.cx.env.param_env, func, args).is_some()
        {
            return self.identity_callee(fn_ty);
        }
        let (callee, callee_args) = db.lookup_impl_method(self.cx.env, func, args);
        let Either::Left(callee) = callee else { return self.identity_callee(fn_ty) };
        // A method of a builtin impl (`Clone` of a tuple, `FnPtr::addr`), which the code
        // generator makes from the type.
        if callee == func
            && let ItemContainerId::TraitId(_) = func.lookup(db).container
            && !FunctionSignature::of(db, func).has_body()
        {
            return self.identity_callee(fn_ty);
        }
        self.instance_callee(ShareDef::Fn(callee), callee_args, tag::CALLEE_FN, Some(callee))
    }

    /// A call of `def` with `args`: the code generator names it by its instance key, which is
    /// its share key, or its arguments' hash for an instance with none.
    fn instance_callee(
        &self,
        def: ShareDef,
        args: GenericArgs<'db>,
        what: u8,
        func: Option<FunctionId>,
    ) -> Result<ContentId, Unshared> {
        let db = self.db();
        let mut h = Hasher::new(CALLEE_DOMAIN);
        h.update(&[what]);
        if let Some(func) = func {
            let item = ItemId::Function(func);
            h.id(&item_key(db, item).0).id(&interface_hash(db, item).map_err(identity::unhashable(db))?.0);
        }
        // Which closure, as its parent's arguments are all a closure of a body with no
        // generics has: two closures of one signature are two callees.
        if let ShareDef::Closure(id) = def {
            h.id(&key::closure_key(db, InternedClosureId::from_id(id)).0);
        }
        let generic = args.iter().any(|arg| !matches!(arg.kind(), GenericArgKind::Lifetime(_)));
        if !generic {
            return Ok(h.finish());
        }
        let instance = ShareInstance { def, args: args.store() };
        if instance == *self.instance {
            h.update(&[tag::CALLEE_SELF]);
            return Ok(h.finish());
        }
        // A callee's share walks its own callees: the chain is as deep as the call graph,
        // each level a query's frames, past a codegen thread's stack.
        let stored = self.cx.stored.clone(); // clone: the query's key, a reference count
        match stdx::ensure_sufficient_stack(|| share(db, instance, stored, self.options)) {
            Ok(key) => h.update(&[tag::CALLEE_SHARED]).id(&key.0),
            Err(Unshared::Recursive) => return Err(Unshared::Recursive),
            Err(why @ Unshared::RecursionLimit { .. }) => return Err(why.clone()),
            Err(_) => h.update(&[tag::CALLEE_ARGS]).id(&identity::args(db, args)?),
        };
        Ok(h.finish())
    }

    fn intrinsic(
        &self,
        fn_ty: Ty<'db>,
        func: FunctionId,
        args: GenericArgs<'db>,
        intrinsic: Intrinsic,
    ) -> Result<ContentId, Unshared> {
        match classify(intrinsic) {
            Class::Layout => {
                let db = self.db();
                let mut h = Hasher::new(CALLEE_DOMAIN);
                h.update(&[tag::CALLEE_INTRINSIC]).update(&intrinsic.index().to_le_bytes());
                h.id(&self.args_facts(args.iter())?);
                // An intrinsic the code generator does not make inline calls its fallback body.
                if FunctionSignature::of(db, func).has_body() {
                    h.id(&self.instance_callee(ShareDef::Fn(func), args, tag::CALLEE_FN, Some(func))?);
                }
                Ok(h.finish())
            }
            Class::Identity => self.identity_callee(fn_ty),
            Class::PerInstance => Err(Unshared::PerInstance(intrinsic.to_string())), // alloc: error path
        }
    }

    /// `Fn::call`, `FnMut::call_mut` or `FnOnce::call_once` of a closure or a function item
    /// by its body's key; of anything else by identity.
    fn fn_trait_call(
        &self,
        fn_ty: Ty<'db>,
        method: FunctionId,
        args: GenericArgs<'db>,
    ) -> Result<ContentId, Unshared> {
        let db = self.db();
        let self_ty = self.cx.normalize(args.type_at(0));
        let mut h = Hasher::new(CALLEE_DOMAIN);
        h.update(&[tag::CALLEE_FN_TRAIT]).id(&item_key(db, ItemId::Function(method)).0);
        h.id(&self.cx.fact(args.type_at(1))?);
        match self_ty.kind() {
            TyKind::Closure(id, closure_args) => {
                let parent = GenericArgs::new_from_slice(closure_args.as_closure().parent_args());
                let def = ShareDef::Closure(id.0.as_id());
                h.id(&self.instance_callee(def, parent, tag::CALLEE_CLOSURE, None)?);
                // A closure passed by value to a body that borrows it is the caller's to drop.
                h.id(&drop::key(self.cx, self_ty)?);
            }
            TyKind::FnDef(def, fn_args) => {
                let fn_args = fn_args.no_bound_vars().unwrap();
                match def.0 {
                    CallableDefId::FunctionId(func)
                        if intrinsic_of(db, func).ok().flatten().is_none() =>
                    {
                        h.id(&self.call_fn(self_ty, func, fn_args, None)?);
                    }
                    _ => return self.identity_callee(fn_ty),
                }
            }
            _ => return self.identity_callee(fn_ty),
        }
        Ok(h.finish())
    }
}

/// An unsizing of a pointer to an array to one to a slice: its metadata is the length, which
/// the source's fact has.
fn is_array_to_slice<'db>(from: Ty<'db>, to: Ty<'db>) -> bool {
    match (from.builtin_deref(true), to.builtin_deref(true)) {
        (Some(from), Some(to)) => {
            matches!((from.kind(), to.kind()), (TyKind::Array(..), TyKind::Slice(_)))
        }
        _ => false,
    }
}

/// Whether a promoted constant's body names a parameter: in a local's type or a constant.
fn mentions_params(body: &MirBody<'_>) -> bool {
    if body.locals.iter().any(|(_, local)| has_params(local.ty.as_ref())) {
        return true;
    }
    let mut in_operand = false;
    let mut visit = |operand: &Operand| {
        in_operand |= match &operand.kind {
            OperandKind::Constant { konst, ty } => {
                has_params(ty.as_ref()) || konst.as_ref().has_type_flags(PARAMS)
            }
            OperandKind::Allocation { allocation } => has_params(allocation.as_ref().ty),
            OperandKind::Copy(_) | OperandKind::Move(_) | OperandKind::Static(_) => false,
        };
    };
    let mut in_length = false;
    for (_, block) in body.basic_blocks.iter() {
        for statement in &block.statements {
            statement.kind.for_each_operand(&mut visit);
            if let StatementKind::Assign(_, Rvalue::Repeat(_, len)) = &statement.kind {
                in_length |= len.as_ref().has_type_flags(PARAMS);
            }
        }
        if let Some(terminator) = &block.terminator {
            terminator.kind.for_each_operand(&mut visit);
        }
    }
    in_operand || in_length
}
