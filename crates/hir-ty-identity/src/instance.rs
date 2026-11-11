//! [`instance_identity`]: compile identity, what one instance's generated code depends on,
//! by one walk of its generic body in two passes. Two instances of one body with one
//! identity compile to one body: this is what the code generator's ICF folds by
//! (`docs/v2/plan/design/icf.md`), and what replaces an instance's arguments in its
//! instance key.
//!
//! - [`sites`], once per generic body and options, walks the body and lists, in walk order,
//!   each value an instance's records are read from (every local's type, and each type or
//!   constant where the body names a generic parameter) with the ops that make records of them,
//!   and the code of every body MIR inlining built into it.
//! - Per instance, the listed values are substituted at once by `hir-ty-mir`'s
//!   `monomorphized_args`, the fold `monomorphized` fills a body with, so a value here and the
//!   same value in the monomorphized body the code generator compiles are one. The ops then
//!   replay over them. A value `monomorphized` leaves as it is (a `Yield`'s and a
//!   `DropAndReplace`'s) is kept as the generic body has it.
//!
//! The identity holds the body's item key and code hash ([`blaze_items`]: each item it refers
//! to by interface, never a position or a path), the code of each body inlined into it, each
//! fact of another item its passes relied on (`Inlined::relied`), and,
//! where a type or constant of the generic body names a parameter, what an instance has in its
//! place:
//!
//! - a local's type: its fact ([`hir_ty_share::Cx::fact`]: its layout and shape, never its
//!   name, so `Vec<&A>` and `Vec<&B>` are one); each type a `Deref` of the local's place
//!   reaches: its fact;
//! - a constant: its value, evaluated as the code generator's `eval_const` does; its type:
//!   its fact; a promoted constant's value, as `promoted_constant` makes it;
//! - a call: the callee by its own compile identity (Merkle), or by what the code generator's
//!   walk asks for it (an intrinsic by [`classify`], drop glue by [`drop::key`], a `dyn` call, a
//!   builtin impl's method, `Future::poll` of a coroutine, a builtin derive by full identity);
//! - an unsizing coercion, a function or closure turned into a pointer and a `dyn*` cast:
//!   both types' identities (the vtable or shim it makes is named by them); another cast:
//!   the target's fact;
//! - a drop: its drop key; an aggregate: its type's fact; an `asm!` `sym`: its identity.
//!
//! Whether something is read depends only on the generic body, so the records of two
//! instances line up one for one, and equal records mean equal monomorphized bodies up to the
//! facts the code generator reads. Each record is a tag byte and a 32-byte content ID.
//!
//! Left to the unit's template key, not read here: the unit's settings (overflow checks, the
//! target, the backend; `Settings`), the callees the code generator builds in and each inlined
//! callee's instance (`Described::built_in`, by their instance keys), and the program's dyn
//! facts a devirtualized body was built against (`Inlined::devirtualized`; every options here
//! has no world, [`OptionsSet`]).
//!
//! A call of the instance itself is recorded as such: direct recursion shares. A callee's
//! identity is asked only where a chain of asks is bounded by its keys alone ([`Walk::asks`]):
//! its arguments nest strictly shallower than the instance's, or, for another body, they are an
//! order-preserving sub-list of the instance's own arguments. Lifetimes play no part: every key,
//! a root's and a callee's, has its free regions erased and its binders anonymized
//! ([`erased`]) before it is read, so two calls that differ only in lifetimes are one key. Every
//! other callee is recorded by its arguments without asking. Along a chain of asks the depth never
//! rises; while it holds, the number of arguments never rises; and while both hold the arguments
//! are the same, so only the body changes, and a body that comes back closes a cycle. So a
//! chain is at most (`SHARED_DEPTH` + 1) × A × B asks long, for A the most type and constant
//! parameters of any body and B the number of bodies; every key asked is an instance its asker
//! calls, which the build collects too. A cycle of instances gives
//! [`Unshared::Recursive`] to all in it, and arguments nested past [`SHARED_DEPTH`] (only a root's
//! can be, as no ask goes deeper) give [`Unshared::TooDeep`]; the code generator compiles both
//! per instance and a caller records them by their arguments. It never hashes a cycle as one
//! component, as behaviour identity does ([`crate::scc`]). Each of these reads only the
//! instance's own key: an identity never depends on which root asked first.

use base_db::Db;
use blaze_hash::{Blake3Hasher, HashSet};
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
    mir::{InlinedBody, MirLowerError, MirOptions, MirOwner, MirStage},
    next_solver::DbInterner,
    next_solver::{
        AliasConst, AliasConstKind, AllocationData, Const, ConstKind, FreeConstAliasId, GenericArg,
        GenericArgs, InherentAssocConstId, Region, RegionExt as _, RegionKind, StoredGenericArgs,
        TraitAssocConstId, Ty, TyKind,
    },
    traits::StoredParamEnvAndCrate,
};
use hir_ty_mir::mir::{
    AggregateKind, AsmOperand, BasicBlock, CastKind, InlineAsm, MirBody, Operand, OperandKind,
    PlaceElem, PlaceTy, ProjectionElem, Rvalue, Statement, StatementKind, StoredPlace,
    TerminatorKind, dispatch::Body, location::mentions_location, monomorphized_args,
    monomorphized_promoted_mir_body_query, owner_of, runtime_promoted_mir_body_query,
};
use rustc_type_ir::{
    GenericArgKind, TypeFlags, TypeFoldable, TypeFolder, TypeSuperFoldable, TypeSuperVisitable,
    TypeVisitableExt, TypeVisitor,
    inherent::{GenericArgs as _, IntoKind},
};
use smallvec::SmallVec;
use std::alloc::Global;

use hir_ty_share::{Class, Cx, Through, Unshared, classify, code, drop, identity};

use crate::{
    CALLEE_DOMAIN, COMPILE_DOMAIN, CompileIdentity, Compiled, INLINED_DOMAIN, Instance, OptionsSet,
    Subject, VALUE_DOMAIN, WALK_DOMAIN, name_of,
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
    pub(super) const INLINED_PROMOTED: u8 = 16;

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
fn value<'db>(cx: &Cx<'db>, konst: Const<'db>) -> Result<ContentId, Unshared> {
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
            return Err(Unshared::ConstEval(format!(
                // alloc: error path
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
            return Err(Unshared::NoContent(format!(
                // alloc: error path
                "a constant of `{:?}` that points to a static",
                allocation.ty
            )));
        }
        MemoryMap::Complex(map) => {
            // The map's content (`ComplexMemoryMap::hash_content`, which code identity's
            // `Facts::allocation` hashes too): pieces, alignments and provenance. Then the types
            // of its `TypeId`s and of its function pointers and vtables, by their identity. A
            // function pointer's or vtable's bytes are its id, `FIRST_ID` plus its type's place
            // in the table, so the table is hashed in that order, each vtable with the `dyn`
            // type it is for, which the code generator emits it for.
            h.update(&[2]);
            map.hash_content(&mut Blake3Hasher(&mut h));
            h.update(&(map.type_ids.len() as u64).to_le_bytes());
            for ty in &map.type_ids {
                match ty {
                    Some(ty) => h.update(&[1]).id(&identity::ty(db, *ty)?),
                    None => h.update(&[0]),
                };
            }
            let pointees = &map.vtable;
            h.update(&(pointees.entries().len() as u64).to_le_bytes());
            for (ty, dyn_ty) in pointees.entries() {
                h.id(&identity::ty(db, ty)?);
                match dyn_ty {
                    Some(dyn_ty) => h.update(&[1]).id(&identity::ty(db, dyn_ty)?),
                    None => h.update(&[0]),
                };
            }
        }
    };
    Ok(h.finish())
}

/// The compile identity of `instance` in `env`, with its body under each options of
/// `compiled`: equal identities of two instances of one body mean their code is the same.
pub fn instance_identity<'db>(
    db: &'db Db,
    instance: &Instance,
    env: &StoredParamEnvAndCrate,
    compiled: Compiled,
) -> &'db Result<CompileIdentity, Unshared> {
    blaze_query::db::attach(db, || {
        // Free regions erased, bound ones kept, as every callee's key is
        // (`Walk::instance_callee`): two instances equal but for their regions are one key, and
        // a call of the instance itself is `CALLEE_SELF` whatever regions it was asked with.
        let interner = DbInterner::new_with(db, env.as_ref(db).krate);
        let args = erased(interner, instance.args.as_ref());
        let instance = Instance { subject: instance.subject, args: args.store() };
        // clone: the query's key, a reference count
        identity_of(db, instance, env.clone(), compiled)
    })
}

#[blaze_query::tracked(returns(ref), cycle_result = identity_cycle)]
#[expect(
    clippy::needless_pass_by_value,
    reason = "a tracked query's arguments are its key, which `blaze_query::tracked` hands the body by value"
)]
fn identity_of(
    db: &Db,
    instance: Instance,
    env: StoredParamEnvAndCrate,
    compiled: Compiled,
) -> Result<CompileIdentity, Unshared> {
    #[cfg(test)]
    MADE.with(|made| made.set(made.get() + 1));
    let depth = nesting(instance.args.as_ref());
    if depth > SHARED_DEPTH {
        return Err(Unshared::TooDeep { depth, limit: SHARED_DEPTH });
    }
    let cx = Cx::new(db, env);
    let mut key = Hasher::new(COMPILE_DOMAIN);
    let bodies = bodies(db, instance.subject, compiled, &mut key)?;
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
                let sites = sites(db, instance.subject, asked).as_ref().map_err(Clone::clone)?;
                let records = Walk::records(&cx, &instance, compiled, generic, sites, depth)?;
                walked.push((generic, records));
                records
            }
        };
        key.update(&[OptionsSet::index(asked) as u8]).id(&records);
    }
    Ok(CompileIdentity(key.finish()))
}

#[cfg(test)]
thread_local! {
    /// How many compile identities this thread has made, each one execution of the query: what
    /// the tests bound the keys a chain of asks reaches by.
    pub(crate) static MADE: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// How deep an instance's arguments may nest for its compile identity to be made: past it, the
/// instance is [`Unshared::TooDeep`], compiled for its arguments, and a caller names it by them
/// without asking. No identity asks a callee deeper than itself ([`Walk::asks`]), so only an
/// instance asked from outside (a root of the code generator's) can be past it; and the depth a
/// chain of asks starts from bounds how many steps down it can take.
/// It reads only the instance's own key, so the answer is the same whichever root asks first
/// (rules 21 and 25). It is no limit on what builds: rustc's recursion limit, which counts how
/// often one body repeats along the collector's path of instances (`check_recursion_limit`), is
/// the build's walk's (`blaze_codegen`'s `driver/recursion.rs`). A type written
/// this deep by hand, with no recursion, builds: it is only compiled for its arguments.
pub(crate) const SHARED_DEPTH: u32 = 128;

/// How deep `args` nest: a type or constant is one deeper than the deepest it holds. Each
/// distinct type or constant is measured once, so a type that names one part twice at each
/// level (`(&T, &T)`, rust#37311) costs its distinct parts, never its size as a tree.
fn nesting(args: GenericArgs<'_>) -> u32 {
    // alloc: once per instance's identity, this query's miss; one entry per distinct part
    let mut memo = blaze_hash::HashMap::new_in(Global);
    args.iter().map(|arg| depth(arg, &mut memo)).max().unwrap_or(0)
}

fn depth<'db>(
    arg: GenericArg<'db>,
    memo: &mut blaze_hash::HashMap<GenericArg<'db>, u32, Global>,
) -> u32 {
    if let Some(&known) = memo.get(&arg) {
        return known;
    }
    let mut parts = Parts(SmallVec::new());
    match arg.kind() {
        GenericArgKind::Type(ty) => ty.super_visit_with(&mut parts),
        GenericArgKind::Const(konst) => konst.super_visit_with(&mut parts),
        GenericArgKind::Lifetime(_) => return 0,
    }
    // A part is as deep as the type was built, which only a chain of instances makes deep.
    let below =
        parts.0.into_iter().map(|part| stdx::ensure_sufficient_stack(|| depth(part, memo))).max();
    let depth = 1 + below.unwrap_or(0);
    memo.insert(arg, depth);
    depth
}

/// The types and constants one level below what is visited.
struct Parts<'db>(SmallVec<[GenericArg<'db>; 4]>);

impl<'db> TypeVisitor<DbInterner<'db>> for Parts<'db> {
    type Result = ();

    fn visit_ty(&mut self, ty: Ty<'db>) {
        self.0.push(ty.into());
    }

    fn visit_const(&mut self, konst: Const<'db>) {
        self.0.push(konst.into());
    }
}

fn identity_cycle(
    _: &Db,
    _: blaze_query::db::Id,
    _: Instance,
    _: StoredParamEnvAndCrate,
    _: Compiled,
) -> Result<CompileIdentity, Unshared> {
    Err(Unshared::Recursive)
}

type Bodies<'db> = SmallVec<[(MirOptions, &'db MirBody<'db>); 4]>;

/// The generic bodies the code generator may compile `subject` from, one per options, with
/// what names the body hashed into `key`.
fn bodies<'db>(
    db: &'db Db,
    subject: Subject,
    compiled: Compiled,
    key: &mut Hasher,
) -> Result<Bodies<'db>, Unshared> {
    let body = match subject {
        Subject::Body(body) => body,
        Subject::Shim(shim) => return Err(Unshared::Shim(shim.what())),
    };
    let owner = match body {
        Body::Fn(func) => {
            let item = ItemId::Function(func);
            let code = code_hash(db, item).map_err(unhashable_body(db))?.ok_or_else(|| {
                Unshared::NoMir(format!(
                    "`{}` has no body",
                    FunctionSignature::of(db, func).name.as_str()
                )) // alloc: error path
            })?;
            key.update(&[tag::DEF_FN]).id(&item_key(db, item).0).id(&code.0);
            InferBodyId::DefWithBodyId(func.into())
        }
        Body::Closure(id) => {
            let closure = InternedClosureId::from_id(id);
            let lowered = || db.mir_body_for_closure(closure).err();
            let code =
                closure_code(db, closure).map_err(unhashable_nested(db, subject, lowered))?;
            key.update(&[tag::DEF_CLOSURE]).id(&code.0);
            closure.loc(db).owner
        }
        Body::Coroutine(id) | Body::CoroutineByMove(id) => {
            let coroutine = InternedCoroutineId::from_id(id);
            let lowered = || db.mir_body_for_coroutine(coroutine).err();
            let code =
                coroutine_code(db, coroutine).map_err(unhashable_nested(db, subject, lowered))?;
            let what = match body {
                Body::CoroutineByMove(_) => tag::DEF_COROUTINE_BY_MOVE,
                _ => tag::DEF_COROUTINE,
            };
            key.update(&[what]).id(&code.0);
            coroutine.loc(db).owner
        }
    };
    let set =
        if owner.krate(db).data(db).origin.is_lang() { compiled.lang } else { compiled.other };
    if set.is_empty() {
        return Err(Unshared::NoOptions);
    }
    let mut bodies = Bodies::new();
    for asked in set.iter() {
        bodies.push((asked, body_of(db, subject, asked)?));
    }
    Ok(bodies)
}

/// A body with no code hash, as the error of its identity. MIR that failed to lower at a call of
/// an intrinsic missing from the table is [`Unshared::UnknownIntrinsic`], the reason the walk
/// gives a call of one, naming the body that calls it; a caller that asks it names itself the
/// callee it went through ([`Walk::instance_callee`]). Any other miss is what blaze-items says,
/// naming the item.
fn unhashable_body(db: &Db) -> impl Fn(blaze_items::Unhashable) -> Unshared + '_ {
    move |it| match it.mir_lowering_error(db) {
        Some((item, within, MirLowerError::UnknownIntrinsic(name))) => Unshared::UnknownIntrinsic {
            name: name.as_str().to_owned(), // alloc: error path
            // The body that failed, a closure or an `async` block in the item where it is one
            // (review 7, C2): the item's code covers theirs, but it is theirs that calls it.
            body: match within {
                None => format!("`{}`", key::path(db, item)), // alloc: error path
                Some(blaze_items::Nested::Closure(closure)) => {
                    name_of(db, Subject::Body(Body::Closure(closure.as_id())))
                }
                Some(blaze_items::Nested::Coroutine(coroutine)) => {
                    name_of(db, Subject::Body(Body::Coroutine(coroutine.as_id())))
                }
            },
            through: match within {
                None => Through::Own,
                Some(_) => Through::Nested,
            },
        },
        _ => identity::unhashable(db)(it),
    }
}

/// A closure's or coroutine's code with no hash, as the error of its identity (review 6, C3).
/// blaze-items names its owner item, whose first failing body may be another closure's, so a
/// miss at MIR is read from the body's own lowering (`lowered`): an intrinsic missing from the
/// table is [`Unshared::UnknownIntrinsic`] naming this body, any other error is this body's own.
/// Where its own MIR lowered the miss is elsewhere, and blaze-items' description names the owner.
fn unhashable_nested<'db>(
    db: &'db Db,
    subject: Subject,
    lowered: impl FnOnce() -> Option<MirLowerError<'db>>,
) -> impl FnOnce(blaze_items::Unhashable) -> Unshared {
    move |it| match it.missed_mir().then(lowered).flatten() {
        Some(MirLowerError::UnknownIntrinsic(name)) => Unshared::UnknownIntrinsic {
            name: name.as_str().to_owned(), // alloc: error path
            body: name_of(db, subject),
            through: Through::Own,
        },
        Some(error) => {
            // alloc: error path
            let mut said = format!(
                "blaze-items: the code of {} has no hash: MIR that failed to lower (",
                name_of(db, subject)
            );
            blaze_items::lowering::write(db, &error, &mut said);
            said.push(')');
            Unshared::Unhashable(said)
        }
        None => identity::unhashable(db)(it),
    }
}

/// The generic body the code generator compiles `subject` from under `options`, as
/// [`MirStage::compiled`] picks it for both.
fn body_of<'db>(
    db: &'db Db,
    subject: Subject,
    options: MirOptions,
) -> Result<&'db MirBody<'db>, Unshared> {
    let owner = match subject {
        Subject::Body(body) => mir_owner(db, body),
        Subject::Shim(shim) => return Err(Unshared::Shim(shim.what())),
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
    /// The code of every body MIR inlining built into this one ([`inlined_code`]).
    inlined: ContentId,
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
    AggregateArgs {
        start: u32,
        len: u32,
    },
    /// A promoted constant held by `local`, whose body names a parameter.
    Promoted {
        local: u32,
        expr: ExprId,
    },
    /// A promoted constant held by `local` of the body MIR inlining built in as `callee`
    /// (`Inlined::promoted`), whose body names the callee's parameters where the callee's
    /// arguments name this body's: evaluated, as the code generator does, in the callee's
    /// owner with the callee's arguments, `len` filled values from `start`.
    InlinedPromoted {
        local: u32,
        callee: u32,
        expr: ExprId,
        start: u32,
        len: u32,
    },
    /// A promoted constant held by `local` (of the body MIR inlined as `callee`, or its own)
    /// whose MIR does not build: the code generator skips it where `local` is zero-sized in the
    /// instance, and refuses it elsewhere, as this identity does.
    UnbuiltPromoted {
        local: u32,
        callee: Option<u32>,
        expr: ExprId,
    },
    Cast {
        kind: CastKind,
        from: TyOf,
        to: At,
    },
    /// The fact at each `Deref` of a place whose local's type names a parameter.
    Derefs(StoredPlace),
    Drop(StoredPlace),
    Sym(TyOf),
    Call {
        func: TyOf,
        intrinsic: Option<Intrinsic>,
    },
}

/// The sites of `subject`'s generic body under `options`. It reads no argument, so every
/// instance of the body shares one.
#[blaze_query::tracked(returns(ref))]
pub(crate) fn sites(db: &Db, subject: Subject, options: MirOptions) -> Result<Sites, Unshared> {
    let body = body_of(db, subject, options)?;
    if body.has_comptime_loop() {
        // Its expansion depends on the domain's fields (their types, names and attributes),
        // which no fact of a local reads (`Fields<T>` is a ZST for every `T`): kept per
        // instance, keyed by the arguments, whose identity covers each of them (an ADT's
        // interface form hashes its fields' and variants' names, types and attributes).
        return Err(Unshared::PerInstance(
            // alloc: once per generic body with a `for const` loop, this query's miss
            "a `for const` loop: its expansion reads the instance's fields".to_owned(),
        ));
    }
    let inlined = inlined_code(db, body)?;
    let mut collect = Collect {
        db,
        body,
        // alloc: once per generic body and options, this query's miss
        filled: Vec::with_capacity(body.locals.len() + 16),
        kept: Vec::new(), // alloc: once per generic body and options, this query's miss
        ops: Vec::new(),  // alloc: once per generic body and options, this query's miss
        derefs: HashSet::new_in(Global),
    };
    collect.body()?;
    Ok(Sites {
        filled: GenericArgs::new_from_slice(&collect.filled).store(),
        kept: GenericArgs::new_from_slice(&collect.kept).store(),
        ops: collect.ops.into_boxed_slice(),
        inlined,
    })
}

/// The code of every body MIR inlining built into `body`, in the order it inlined them: which
/// body ([`code`]: its code hash, its bindings' names and its span-table indices) and whether
/// its arithmetic is checked. A callee's body edit changes it only where it was inlined: a
/// plain call names its callee by interface, and by each fact of it a pass relied on
/// (`Inlined::relied`), hashed after the callees. What the inlined code does with an instance's
/// arguments is in the body's sites, as the rest of it is; which instance of the callee it is,
/// the unit's template key holds (`Described::built_in`).
fn inlined_code(db: &Db, body: &MirBody<'_>) -> Result<ContentId, Unshared> {
    let callees = &body.inlined.callees;
    let mut h = Hasher::new(INLINED_DOMAIN);
    h.update(&(callees.len() as u64).to_le_bytes());
    for callee in callees {
        let (tag, code) = match callee.body {
            InlinedBody::Fn(f) => (tag::DEF_FN, code::of_item(db, f.into())),
            InlinedBody::Closure(it) => (tag::DEF_CLOSURE, code::of_closure(db, it)),
        };
        h.update(&[tag, callee.written_checked as u8]).id(code.as_ref().map_err(Clone::clone)?);
    }
    // The facts of other items the body's passes relied on (`Inlined::relied`,
    // docs/v2/plan/design/facts.md), in the order first relied on: the body is the code
    // they changed. A fact the body did not rely on is not here, so proving more of a
    // callee, or less of what no pass spent, changes no identity.
    let relied = &body.inlined.relied;
    h.update(&(relied.len() as u64).to_le_bytes());
    for digest in relied {
        h.update(digest);
    }
    Ok(h.finish())
}

/// The first pass: a generic body's sites, read from the body alone.
struct Collect<'a, 'db> {
    db: &'db Db,
    body: &'a MirBody<'db>,
    filled: Vec<GenericArg<'db>>,
    kept: Vec<GenericArg<'db>>,
    ops: Vec<Op>,
    /// The places whose `Deref`s are recorded, once each.
    derefs: HashSet<&'a StoredPlace, Global>,
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

    fn body(&mut self) -> Result<(), Unshared> {
        let body = self.body;
        for (_, local) in body.locals.iter() {
            self.filled.push(local.ty.as_ref().into());
        }
        for (local, data) in body.locals.iter() {
            if has_params(data.ty.as_ref()) {
                self.ops.push(Op::Fact(tag::LOCAL, At::Filled(local.into_raw().into_u32())));
            }
        }
        // As the code generator's `promoted_constants`: a constant that takes its value from
        // where it is in the source is made from its expression, never from MIR; one whose MIR
        // does not build is refused where the code generator would build it (`UnbuiltPromoted`),
        // never left out.
        let mir = |owner: InferBodyId<'db>, expr| match owner {
            InferBodyId::DefWithBodyId(def) if mentions_location(self.db, def, expr) => None,
            _ => Some(runtime_promoted_mir_body_query(self.db, owner, expr).ok()),
        };
        for &(local, expr) in &body.promoted_locals {
            let local = local.into_raw().into_u32();
            match mir(body.owner, expr) {
                None => {}
                Some(None) => self.ops.push(Op::UnbuiltPromoted { local, callee: None, expr }),
                Some(Some(promoted)) if mentions_params(promoted) => {
                    self.ops.push(Op::Promoted { local, expr })
                }
                Some(Some(_)) => {}
            }
        }
        for promoted in &body.inlined.promoted {
            let callee = &body.inlined.callees[promoted.callee as usize];
            let (local, expr) = (promoted.local.into_raw().into_u32(), promoted.expr);
            let args = callee.args.as_ref();
            match mir(owner_of(self.db, callee.body), expr) {
                None => {}
                Some(None) => self.ops.push(Op::UnbuiltPromoted {
                    local,
                    callee: Some(promoted.callee),
                    expr,
                }),
                // The same arguments for every instance: the value is the body's alone.
                Some(Some(mir)) if args.has_type_flags(PARAMS) && mentions_params(mir) => {
                    let start = self.filled.len() as u32;
                    self.filled.extend(args.iter());
                    self.ops.push(Op::InlinedPromoted {
                        local,
                        callee: promoted.callee,
                        expr,
                        start,
                        len: args.len() as u32,
                    });
                }
                Some(Some(_)) => {}
            }
        }
        for (_, block) in body.basic_blocks.iter() {
            self.block(block);
        }
        Ok(())
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
            // `sites` refuses a body with one before the walk.
            TerminatorKind::ComptimeFor { .. } | TerminatorKind::ComptimeNext { .. } => {}
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

/// What [`erased`] changes: a free region, and a binder's variables, which it numbers afresh
/// with no names. A caller that caches [`erased`] skips what has none of these.
pub const ERASES: TypeFlags = TypeFlags::HAS_FREE_REGIONS.union(TypeFlags::HAS_BINDER_VARS);

/// `value` with its free regions erased, error regions included, and each binder's variables
/// anonymized, as rustc's `erase_and_anonymize_regions`: compile identity reads no region
/// (`Spell::args` writes a lifetime as a placeholder and a binder by its count, facts ignore
/// regions), an impl's arguments can name `'static` where the instance's are erased, and
/// `fn(&str)` and `for<'a> fn(&'a str)` are one type whose binder names its variable or not.
/// Each type and constant is guarded on its own, as a callee's arguments can nest past
/// [`SHARED_DEPTH`], and folded once, as one that names a part twice at each level (`(&T, &T)`,
/// rust#37311) is 2^depth as a tree.
///
/// The code generator keys its instances (`erase_regions`) and vtables (`fn vtable`) with it
/// too, so a change here moves those keys, and every key of compile identity and code identity
/// (`COMPILE_DOMAIN`, `CODE_ID_DOMAIN`).
pub fn erased<'db, T: TypeFoldable<DbInterner<'db>>>(interner: DbInterner<'db>, value: T) -> T {
    if !value.has_type_flags(ERASES) {
        return value;
    }
    // alloc: per erasure that has a region to erase; one entry per distinct part
    let mut folder = EraseFree {
        interner,
        tys: blaze_hash::HashMap::new_in(Global),
        consts: blaze_hash::HashMap::new_in(Global),
    };
    value.fold_with(&mut folder)
}

/// [`erased`]'s folder, with each distinct type and constant's result. A result depends on the
/// part alone: a binder is anonymized before its contents are folded, so a part inside it
/// names only variables already numbered afresh.
struct EraseFree<'db> {
    interner: DbInterner<'db>,
    tys: blaze_hash::HashMap<Ty<'db>, Ty<'db>, Global>,
    consts: blaze_hash::HashMap<Const<'db>, Const<'db>, Global>,
}

impl<'db> TypeFolder<DbInterner<'db>> for EraseFree<'db> {
    fn cx(&self) -> DbInterner<'db> {
        self.interner
    }

    fn fold_binder<T: TypeFoldable<DbInterner<'db>>>(
        &mut self,
        binder: rustc_type_ir::Binder<DbInterner<'db>, T>,
    ) -> rustc_type_ir::Binder<DbInterner<'db>, T> {
        rustc_type_ir::Interner::anonymize_bound_vars(self.interner, binder).super_fold_with(self)
    }

    fn fold_ty(&mut self, ty: Ty<'db>) -> Ty<'db> {
        if !ty.has_type_flags(ERASES) {
            return ty;
        }
        if let Some(&done) = self.tys.get(&ty) {
            return done;
        }
        let done = stdx::ensure_sufficient_stack(|| ty.super_fold_with(self));
        self.tys.insert(ty, done);
        done
    }

    fn fold_const(&mut self, konst: Const<'db>) -> Const<'db> {
        if !konst.has_type_flags(ERASES) {
            return konst;
        }
        if let Some(&done) = self.consts.get(&konst) {
            return done;
        }
        let done = stdx::ensure_sufficient_stack(|| konst.super_fold_with(self));
        self.consts.insert(konst, done);
        done
    }

    fn fold_region(&mut self, region: Region<'db>) -> Region<'db> {
        match region.kind() {
            RegionKind::ReBound(..) => region,
            _ => Region::new_erased(self.interner),
        }
    }
}

/// The second pass: one instance's records, from its body's sites with its arguments put in.
struct Walk<'a, 'db> {
    cx: &'a Cx<'db>,
    instance: &'a Instance,
    compiled: Compiled,
    owner: InferBodyId<'db>,
    /// The generic body the sites are of: what it inlined.
    sites_body: &'db MirBody<'db>,
    filled: &'db [GenericArg<'db>],
    kept: &'db [GenericArg<'db>],
    /// The records, hashed as they are made.
    out: Hasher,
    /// How deep the instance's arguments nest ([`nesting`]).
    nesting: u32,
}

impl<'a, 'db> Walk<'a, 'db> {
    /// The records of `instance` over `sites`: every filled value substituted by one fold of
    /// `monomorphized`'s, then each op in order.
    fn records(
        cx: &'a Cx<'db>,
        instance: &'a Instance,
        compiled: Compiled,
        sites_body: &'db MirBody<'db>,
        sites: &'db Sites,
        nesting: u32,
    ) -> Result<ContentId, Unshared> {
        let filled = monomorphized_args(cx.db, sites.filled.as_ref(), &instance.args, &cx.stored)
            .map_err(|error| Unshared::NoMir(format!("monomorphized: {error:?}")))?; // alloc: error path
        let mut out = Hasher::new(WALK_DOMAIN);
        out.id(&sites.inlined);
        let mut walk = Walk {
            cx,
            instance,
            compiled,
            owner: sites_body.owner,
            sites_body,
            filled: filled.as_slice(),
            kept: sites.kept.as_ref().as_slice(),
            out,
            nesting,
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
                // A fact asks the solver, whose frames below a deep identity go past the stack;
                // `Cx::fact` makes sure of its stack itself.
                let fact = self.cx.fact(ty)?;
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
            // The code generator promotes no zero-sized constant (`promoted_constant`).
            Op::Promoted { local, .. }
            | Op::InlinedPromoted { local, .. }
            | Op::UnbuiltPromoted { local, .. }
                if self.zero_sized(*local)? =>
            {
                self.record(tag::PROMOTED_NONE, ContentId::default());
            }
            Op::Promoted { expr, .. } => {
                let args = self.instance.args.clone(); // clone: the query's key, a reference count
                self.promoted(tag::PROMOTED, self.owner, *expr, args)?;
            }
            Op::InlinedPromoted { callee, expr, start, len, .. } => {
                let body = self.sites_body.inlined.callees[*callee as usize].body;
                let args = &self.filled[*start as usize..(*start + *len) as usize];
                let args = GenericArgs::new_from_slice(args).store();
                self.promoted(tag::INLINED_PROMOTED, owner_of(self.db(), body), *expr, args)?;
            }
            Op::UnbuiltPromoted { callee, expr, .. } => {
                let owner = match callee {
                    Some(callee) => {
                        owner_of(self.db(), self.sites_body.inlined.callees[*callee as usize].body)
                    }
                    None => self.owner,
                };
                let error = runtime_promoted_mir_body_query(self.db(), owner, *expr).err();
                return Err(no_promoted_mir(self.db(), owner, *expr, error.as_ref()));
            }
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
                let sym = self.spelled(self.ty_of(of)?)?;
                self.record(tag::SYM, sym);
            }
            Op::Call { func, intrinsic } => {
                let callee = self.callee(self.ty_of(func)?, *intrinsic)?;
                self.record(tag::CALL, callee);
            }
        }
        Ok(())
    }

    /// Whether `local` of the generic body is zero-sized in this instance: the code generator
    /// promotes no such constant (`promoted_constant`).
    fn zero_sized(&self, local: u32) -> Result<bool, Unshared> {
        let ty = self.cx.normalize(self.ty(At::Filled(local)));
        Ok(self.cx.layout(ty)?.size.bytes() == 0)
    }

    /// Whether a callee, `subject` with `args`, is asked for its identity rather than named by
    /// its arguments: where its arguments nest strictly shallower than the instance's, or where
    /// it is another body and its type and constant arguments are an order-preserving sub-list of
    /// the instance's own. The comparison is by value, up to regions (both keys have their free
    /// regions erased, [`instance_identity`] and [`Self::instance_callee`]): a literal or a
    /// projection's value passes only where it equals one of the instance's arguments, kept in
    /// order, and two arguments that differ only in their lifetimes are one.
    ///
    /// That bounds every chain of asks by its keys alone (rules 18 and 21). Along a chain the
    /// depth never rises, so it falls at most [`SHARED_DEPTH`] times. Between two falls each ask
    /// is a sub-list, so the number of arguments never rises; while it holds too, the arguments
    /// are equal, so each ask is of another body with the same arguments, and a body that comes
    /// back closes a cycle ([`Unshared::Recursive`]). A chain is so at most
    /// (`SHARED_DEPTH` + 1) × A × B asks long, for A the most type and constant parameters of any
    /// body and B the number of bodies. What it still asks: a shallower callee
    /// (`<Vec<Vec<A>> as Clone>::clone` calling `<Vec<A> as Clone>::clone`), one that passes the
    /// caller's parameters on (`Vec<T, A>::push` calling `RawVec<T, A>::grow_one` and
    /// `ptr::write::<T>`), and a closure of the caller (its parent's arguments are the caller's).
    /// What it names by its arguments:
    /// - a callee deeper than the caller (`HashMap<K, V>` calling `RawTable<(K, V)>`, any call
    ///   that passes a closure of the caller), which could grow forever through two bodies
    ///   (`f::<T>` calling `g::<(T, u8)>` and `g::<(T, u16)>`, `g::<T>` calling `f::<T>`: 2^127
    ///   keys before [`SHARED_DEPTH`]);
    /// - one as deep with anything new in its arguments: a literal (`foo::<T, u8>`) or a
    ///   projection's value that is none of the instance's arguments, an argument moved. By these
    ///   a chain can count at a fixed depth: a
    ///   binary counter `a::<N>` calling `b::<N::Out>`, and back, takes 2^d steps at depth d; and
    ///   one whose digits are separate parameters, each the value of a projection or an impl's
    ///   choice, with a deep argument beside them that holds every digit's value, takes 2^k steps
    ///   for k digits. Round 5's rule (every argument one of the instance's arguments' parts)
    ///   asked all of those.
    fn asks(&self, subject: Subject, args: GenericArgs<'db>) -> bool {
        let depth = nesting(args);
        depth < self.nesting || (subject != self.instance.subject && self.passes_on(args, depth))
    }

    /// Whether the type and constant arguments of `args` (`depth` deep) are an order-preserving
    /// sub-list of the instance's own. Both have their free regions erased
    /// ([`instance_identity`], [`Self::instance_callee`]), so they compare up to regions. A
    /// sub-list is never deeper than the instance's arguments, so a deeper `args` is no
    /// sub-list, without comparing anything.
    fn passes_on(&self, args: GenericArgs<'db>, depth: u32) -> bool {
        if depth > self.nesting {
            return false;
        }
        let not_lifetime =
            |arg: &GenericArg<'db>| !matches!(arg.kind(), GenericArgKind::Lifetime(_));
        let mut own = self.instance.args.as_ref().iter().filter(not_lifetime);
        args.iter().filter(not_lifetime).all(|arg| own.any(|it| it == arg))
    }

    /// A promoted constant by its value: `expr` of `owner`'s store with `args`, as the code
    /// generator's `promoted_constant` evaluates it.
    fn promoted(
        &mut self,
        tag: u8,
        owner: InferBodyId<'db>,
        expr: ExprId,
        args: StoredGenericArgs,
    ) -> Result<(), Unshared> {
        let db = self.db();
        // clone: the query's key, a reference count
        let body =
            monomorphized_promoted_mir_body_query(db, owner, expr, args, self.cx.stored.clone())
                // The code generator refuses it too (`no_promoted_mir`).
                .map_err(|error| no_promoted_mir(db, owner, expr, Some(&error)))?;
        let evaluated = db.interpret_mir(body, Some(self.cx.env)).ok();
        match evaluated {
            Some(allocation) => {
                let value = allocation_value(db, &allocation)?;
                self.record(tag, value);
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
            // `from` can be a field's type, which holds a projection where the field's declared
            // type does (`Box<Load<L::Buf>>` unsized to `Box<dyn Tr>`).
            let (from, to) = (self.spelled(from)?, self.spelled(to)?);
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
        h.update(&[tag::CALLEE_IDENTITY]).id(&self.spelled(fn_ty)?);
        Ok(h.finish())
    }

    /// The full identity of `ty`, normalized all through first: a type read from a place is
    /// normalized only at its top ([`Self::project`]), and a field's type can hold a projection
    /// inside that only resolves with its ADT's arguments. A type with no alias is spelled as
    /// it is, so this changes no identity a type had before; one that does not normalize is
    /// refused, naming it.
    fn spelled(&self, ty: Ty<'db>) -> Result<ContentId, Unshared> {
        // Normalizing recurses once per level of `ty`, as deep as the program built it.
        stdx::ensure_sufficient_stack(|| identity::ty(self.db(), self.cx.deeply_normalize(ty)?))
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
            None => intrinsic_of(db, func).map_err(|unknown| Unshared::UnknownIntrinsic {
                name: unknown.0.as_str().to_owned(), // alloc: error path
                body: name_of(db, self.instance.subject),
                through: Through::Own,
            })?,
        };
        if let Some(intrinsic) = intrinsic {
            return self.intrinsic(fn_ty, func, args, intrinsic);
        }
        // The code generator writes what these answer for their type argument into the caller
        // (`hir_ty_mir::mir::answered_by_codegen`): the code of the closure or function it
        // names, which its fact does not say. Keyed by the argument's full identity.
        if hir_ty_mir::mir::answered_by_codegen(db, func) {
            return self.identity_callee(fn_ty);
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
        self.instance_callee(Body::Fn(callee), callee_args, tag::CALLEE_FN, Some(callee))
    }

    /// A call of `body` with `args`: the code generator names it by its instance key, which
    /// holds its compile identity, or its arguments' hash for an instance compiled per instance.
    fn instance_callee(
        &self,
        body: Body,
        args: GenericArgs<'db>,
        what: u8,
        func: Option<FunctionId>,
    ) -> Result<ContentId, Unshared> {
        let db = self.db();
        // Free regions erased, bound ones kept, before anything reads `args`: calls equal but
        // for their lifetimes (`'static` or a reordering of the caller's early-bound lifetimes,
        // or of `&u8` type arguments that differ only in region) are one key, so they are one
        // `CALLEE_SELF`, one ask and one `Instance`, and a chain of asks is bounded by its keys
        // up to regions as [`Self::asks`] says (review 6, C1; rules 18 and 21). Compile identity
        // reads no region, so this only merges keys.
        let args = erased(self.cx.interner, args);
        let mut h = Hasher::new(CALLEE_DOMAIN);
        h.update(&[what]);
        if let Some(func) = func {
            let item = ItemId::Function(func);
            h.id(&item_key(db, item).0)
                .id(&interface_hash(db, item).map_err(identity::unhashable(db))?.0);
        }
        // Which closure, as its parent's arguments are all a closure of a body with no
        // generics has: two closures of one signature are two callees.
        if let Body::Closure(id) = body {
            h.id(&key::closure_key(db, InternedClosureId::from_id(id)).0);
        }
        let generic = args.iter().any(|arg| !matches!(arg.kind(), GenericArgKind::Lifetime(_)));
        if !generic {
            return Ok(h.finish());
        }
        let instance = Instance { subject: Subject::Body(body), args: args.store() };
        if instance == *self.instance {
            h.update(&[tag::CALLEE_SELF]);
            return Ok(h.finish());
        }
        // Named by its arguments' full identity, which names one instance and so the one symbol
        // it is called by: always exact, and finer than its identity. Taken, without asking its
        // identity, wherever a chain of asks would not be bounded by its keys ([`Self::asks`]:
        // deeper arguments, or as deep and not a sub-list of the instance's own). A call with
        // shallower arguments (`<Vec<Vec<A>> as Clone>::clone` calling `<Vec<A> as
        // Clone>::clone`), or of another body passing the instance's arguments on
        // (`Vec<T, A>::push` calling `RawVec<T, A>::grow_one`), asks, so instances that differ
        // only in what their facts don't read share again.
        if !self.asks(instance.subject, args) {
            h.update(&[tag::CALLEE_ARGS]).id(&identity::args(db, args)?);
            return Ok(h.finish());
        }
        // A callee's identity walks its own callees: the chain is as deep as the call graph,
        // each level a query's frames, past a codegen thread's stack.
        let stored = self.cx.stored.clone(); // clone: the query's key, a reference count
        match stdx::ensure_sufficient_stack(|| identity_of(db, instance, stored, self.compiled)) {
            Ok(key) => h.update(&[tag::CALLEE_SHARED]).id(&key.0),
            // A callee compiled for its arguments alone by what it is (the code generator's
            // `per_instance`) is named by them. `Recursive` is the answer of every instance of
            // its cycle, whichever was asked first (`CycleKind::Result`), and `TooDeep` reads
            // only its key, so neither depends on who asks. Any other reason is something its
            // identity could not read, which is this identity's too.
            Err(why) if why.is_per_instance() => {
                h.update(&[tag::CALLEE_ARGS]).id(&identity::args(db, args)?)
            }
            // Rule 19: the caller's error names the callee it went through, not only the body
            // that calls the intrinsic. The callee calls it itself only where the body is the
            // callee's own; one in a closure of the callee, or further on, is reached.
            Err(Unshared::UnknownIntrinsic { name, body: at, through }) => {
                return Err(Unshared::UnknownIntrinsic {
                    name: name.clone(), // alloc: error path
                    body: at.clone(),   // alloc: error path
                    through: match through {
                        Through::Own => Through::Calls,
                        Through::Nested | Through::Calls | Through::Reaches(_) => {
                            Through::Reaches(name_of(db, Subject::Body(body)))
                        }
                    },
                });
            }
            Err(why) => return Err(why.clone()),
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
                // By name, never by index: the index shifts when an intrinsic is added (rule 21).
                h.update(&[tag::CALLEE_INTRINSIC]).part(intrinsic.symbol().as_str().as_bytes());
                h.id(&self.args_facts(args.iter())?);
                // An intrinsic the code generator does not make inline calls its fallback body.
                if FunctionSignature::of(db, func).has_body() {
                    h.id(&self.instance_callee(
                        Body::Fn(func),
                        args,
                        tag::CALLEE_FN,
                        Some(func),
                    )?);
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
                let body = Body::Closure(id.0.as_id());
                h.id(&self.instance_callee(body, parent, tag::CALLEE_CLOSURE, None)?);
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

pub(crate) fn body_tag(body: Body) -> u8 {
    match body {
        Body::Fn(_) => 0,
        Body::Closure(_) => 1,
        Body::Coroutine(_) => 2,
        Body::CoroutineByMove(_) => 3,
    }
}

/// A body's own code, anonymous: its code hash (each callee by interface) and its span-table
/// indices ([`hir_ty_share::code`]); never its key, a position or a binding's name (a
/// debugger reads those from the database at link).
pub(crate) fn own_code(db: &Db, body: Body) -> Result<ContentId, String> {
    let code = match body {
        Body::Fn(f) => code::of_item(db, f.into()),
        Body::Closure(id) => code::of_closure(db, InternedClosureId::from_id(id)),
        Body::Coroutine(id) => code::of_coroutine(db, InternedCoroutineId::from_id(id)),
        Body::CoroutineByMove(id) => {
            code::of_coroutine_by_move(db, InternedCoroutineId::from_id(id))
        }
    };
    code.as_ref().copied().map_err(|it| format!("its code has no hash: {it}")) // alloc: error path
}

pub(crate) fn mir_owner<'db>(_: &'db Db, body: Body) -> MirOwner<'db> {
    match body {
        Body::Fn(f) => MirOwner::Body(InferBodyId::from(f)),
        Body::Closure(id) => MirOwner::Closure(InternedClosureId::from_id(id)),
        Body::Coroutine(id) => MirOwner::Coroutine(InternedCoroutineId::from_id(id)),
        Body::CoroutineByMove(id) => MirOwner::CoroutineByMove(InternedCoroutineId::from_id(id)),
    }
}

/// The error of a promoted constant whose MIR does not build, naming it and the body it is of,
/// as the code generator's `no_promoted_mir` does.
fn no_promoted_mir<E: std::fmt::Debug>(
    db: &Db,
    owner: InferBodyId<'_>,
    expr: ExprId,
    error: Option<&E>,
) -> Unshared {
    let owner = match owner {
        InferBodyId::DefWithBodyId(def) => format!("`{}`", key::path(db, ItemId::of_body(db, def))), // alloc: error path
        InferBodyId::AnonConstId(_) => "an anonymous constant".to_owned(), // alloc: error path
    };
    // alloc: error path
    Unshared::NoMir(blaze_query::db::attach(db, || {
        format!("the promoted constant {expr:?} of {owner} has no MIR: {error:?}")
    }))
}
