//! [`code_id`]: behaviour identity, what running an instance can depend on: its own semantic
//! part and every instance's it can run, transitively, as a Merkle graph ([`crate::scc`]).
//!
//! The walk is closed-world over [`dispatch`], as the code generator dispatches: calls, `Fn*`
//! of closures and function items, drop glue, reified pointers, the vtables an unsizing makes,
//! and the function pointers and vtables a constant or static holds. It stops at three leaves,
//! each by its key and interface: a `#[blaze::pure(trusted = "..")]` function (and its own
//! code: it is the step's, run unwalked), a method of a `#[blaze::pure(trusted)]` trait (the
//! kernel's `ctx` door, whose impls are the kernel's and are not the step's code), and a
//! foreign function. Each instance's part is its semantic identity: its code, arguments,
//! whether its arithmetic is checked, and the facts of its monomorphized runtime body, every
//! static by its value. The MIR options are not in it: a step built by a debug and by a release
//! blaze has one CodeId.
//!
//! Each instance's identity is its own query ([`node_id`]), so an instance is hashed once and
//! shared by every root that reaches it; a recursion is hashed as one component.

use base_db::Db;
use blaze_hash::HashSet;
use blaze_ids::{ContentId, Hasher};
use blaze_items::{ItemId, code_hash, item_key};
use hir_def::{FunctionId, ItemContainerId, attrs::AttrFlags, signatures::FunctionSignature};
use hir_ty::{
    db::HirDatabase as _,
    next_solver::{GenericArg, GenericArgs, StoredGenericArgs, Ty, TyKind},
    traits::StoredParamEnvAndCrate,
};
use hir_ty_mir::mir::{
    dispatch::{
        self, Body, Call, Callee, DropStep, LONGEST, clone_parts, coroutine_poll, vtable_methods,
        within_longest,
    },
    overflow,
};
use hir_ty_share::identity as share;
use rustc_type_ir::inherent::{BoundExistentialPredicates as _, GenericArgs as _, IntoKind as _};
use smallvec::SmallVec;
use std::alloc::Global;

use crate::{
    BEHAVIOUR_DOMAIN, CODE_ID_DOMAIN, CodeId, Instance, LEAF_DOMAIN, NAME_DOMAIN, SELF_READ,
    Subject, Unidentified,
    facts::{Cx, Edge, Facts, body_key, fn_path, interface, trusted, trusted_trait_method},
    instance::{body_tag, own_code},
    name_of,
    scc::{self, Graph, Local},
};

/// The most instances of one recursion: past it the walk stops with an error naming the
/// instance it was at (rule 18).
const INSTANCES: usize = 1 << 20;

/// Why the walk stops at a function.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum LeafKind {
    /// `#[blaze::pure(trusted = "..")]`.
    Trusted = 0,
    /// A method of a `#[blaze::pure(trusted)]` trait: the kernel's door.
    Door = 1,
    /// A function of an `extern` block.
    Foreign = 2,
}

/// One instance of a step's walk.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Node {
    Walk(Body, StoredGenericArgs),
    Leaf(FunctionId, StoredGenericArgs, LeafKind),
}

/// One instance's part of a step's identity, and what it runs, in site order.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Summary {
    content: ContentId,
    callees: Box<[Node]>,
}

/// The CodeId of the compute step whose root is `root`, a function, closure or `async` block
/// with no parameter left, compiled in `env`'s crate: its `-C overflow-checks` says whether the
/// arithmetic it runs checks overflow. The same root gives the same CodeId in every process, on
/// every machine and in every checkout; a change to what the step can run changes it, and a
/// change to the kernel past its doors does not. A crate of the toolchain has no setting of its
/// own (its bodies take their callers'), so a root compiled in one is refused, naming the crate:
/// the key never guesses the bit.
pub fn code_id(
    db: &Db,
    root: &Instance,
    env: &StoredParamEnvAndCrate,
) -> Result<CodeId, Unidentified> {
    let Some(checked) = overflow::crate_checks_overflow(db, env.krate) else {
        let krate = env.krate.named(db);
        return Err(Unidentified::of(
            db,
            root,
            // alloc: error path
            format!(
                "it is compiled in `{krate}`, a crate of the toolchain, which has no \
                 `-C overflow-checks` of its own to say whether its arithmetic checks overflow"
            ),
        ));
    };
    // clone: the query's keys, reference counts; an error's text, once
    blaze_query::db::attach(db, || code_id_of(db, root.clone(), env.clone(), checked).clone())
}

/// [`code_id`] of the function item or closure type `callable`, as const eval answers
/// `core::reflect::fn_info::<F>().code_id` with it: a trait method by the impl's method its
/// arguments select.
pub fn fn_code_id<'db>(
    db: &'db Db,
    callable: Ty<'db>,
    env: &StoredParamEnvAndCrate,
) -> Result<CodeId, Unidentified> {
    blaze_query::db::attach(db, || {
        let cx = Cx::new(db, env);
        // alloc: error path
        let refuse = |what: String| Unidentified::named(format!("`{callable:?}`"), what);
        let (body, args) = match cx.normalize(callable).kind() {
            TyKind::Closure(id, args) => (
                Body::Closure(blaze_query::db::AsId::as_id(&id.0)),
                GenericArgs::new_from_slice(args.as_closure().parent_args()),
            ),
            TyKind::FnDef(def, args) => {
                let hir_def::CallableDefId::FunctionId(f) = def.0 else {
                    return Err(refuse(
                        "it is a constructor, which has no code of its own".to_owned(),
                    )); // alloc: error path
                };
                let args = args.skip_binder();
                match f.loc(db).container {
                    ItemContainerId::TraitId(_) => match dispatch::call(&cx.infcx, cx.env, f, args)
                    {
                        Call::Method(Ok(Callee::Fn(g, g_args))) => (Body::Fn(g), g_args),
                        _ => {
                            return Err(refuse(format!(
                                "it is `{}`, which its arguments select no method with a body of",
                                fn_path(db, f)
                            )));
                        } // alloc: error path
                    },
                    _ => (Body::Fn(f), args),
                }
            }
            _ => return Err(refuse(
                "it is not a function item or closure: only those have code known when compiling"
                    .to_owned(),
            )), // alloc: error path
        };
        if !within_longest(args) {
            return Err(refuse(format!("its type arguments are past {LONGEST} types long"))); // alloc: error path
        }
        code_id(db, &Instance { subject: Subject::Body(body), args: args.store() }, env)
    })
}

#[blaze_query::tracked(returns(ref), cycle_result = code_id_cycle)]
#[expect(
    clippy::needless_pass_by_value,
    reason = "a tracked query's arguments are its key, which `blaze_query::tracked` hands the body by value"
)]
fn code_id_of(
    db: &Db,
    root: Instance,
    env: StoredParamEnvAndCrate,
    checked: bool,
) -> Result<CodeId, Unidentified> {
    let Subject::Body(body) = root.subject else {
        // alloc: error path
        return Err(Unidentified::of(
            db,
            &root,
            "a compute step's root is a function, closure or `async` block".to_owned(),
        ));
    };
    root_params(db, &env, body, &root.args).map_err(|what| Unidentified::of(db, &root, what))?;
    // clone: the query's keys, reference counts; an error's text, once
    node_id(db, env.clone(), Node::Walk(body, root.args.clone()), checked).clone().map(CodeId)
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "the `cycle_result` signature `blaze_query::tracked` calls, with the query's arguments by value"
)]
fn code_id_cycle(
    db: &Db,
    _: blaze_query::db::Id,
    root: Instance,
    _: StoredParamEnvAndCrate,
    _: bool,
) -> Result<CodeId, Unidentified> {
    Err(Unidentified::of(db, &root, SELF_READ.to_owned())) // alloc: error path
}

/// The behaviour identity of `node`: of the step rooted at it.
#[blaze_query::tracked(returns(ref), cycle_result = node_id_cycle)]
#[expect(
    clippy::needless_pass_by_value,
    reason = "a tracked query's arguments are its key, which `blaze_query::tracked` hands the body by value"
)]
fn node_id(
    db: &Db,
    env: StoredParamEnvAndCrate,
    node: Node,
    checked: bool,
) -> Result<ContentId, Unidentified> {
    scc::identity(&Behaviour { db, env: &env, checked }, CODE_ID_DOMAIN, &node, INSTANCES)
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "the `cycle_result` signature `blaze_query::tracked` calls, with the query's arguments by value"
)]
fn node_id_cycle(
    db: &Db,
    _: blaze_query::db::Id,
    _: StoredParamEnvAndCrate,
    node: Node,
    _: bool,
) -> Result<ContentId, Unidentified> {
    Err(Unidentified::named(node_name(db, &node), SELF_READ.to_owned())) // alloc: error path
}

/// Whether `node` is on a cycle of what instances run: `true` from its fallback, which
/// blaze_query gives every member of a query cycle ([`crate::scc`]).
#[blaze_query::tracked(returns(copy), cycle_result = on_cycle)]
fn behaviour_cyclic(db: &Db, env: StoredParamEnvAndCrate, node: Node, checked: bool) -> bool {
    // clone: the query's keys, reference counts
    if let Ok(it) = summary(db, env.clone(), node, checked) {
        for callee in it.callees.iter() {
            // Every callee, never stopping early: a member that does not ask is not one.
            let _ = behaviour_cyclic(db, env.clone(), callee.clone(), checked); // clone: as above
        }
    }
    false
}

fn on_cycle(_: &Db, _: blaze_query::db::Id, _: StoredParamEnvAndCrate, _: Node, _: bool) -> bool {
    true
}

/// The graph of what instances run, under one environment and one overflow setting.
struct Behaviour<'a> {
    db: &'a Db,
    env: &'a StoredParamEnvAndCrate,
    checked: bool,
}

impl<'a> Graph<'a> for Behaviour<'a> {
    type Node = Node;
    type Error = Unidentified;

    fn local(&self, node: &Node) -> Result<Local<'a, Node>, Unidentified> {
        // clone: the query's keys, reference counts
        match summary(self.db, self.env.clone(), node.clone(), self.checked) {
            Ok(it) => Ok(Local { content: it.content, callees: &it.callees }),
            Err(what) => Err(Unidentified::named(node_name(self.db, node), what.to_string())), // alloc: error path
        }
    }

    fn cyclic(&self, node: &Node) -> bool {
        behaviour_cyclic(self.db, self.env.clone(), node.clone(), self.checked) // clone: as above
    }

    fn id(&self, node: &Node) -> Result<ContentId, Unidentified> {
        // clone: as above; an error's text, once
        node_id(self.db, self.env.clone(), node.clone(), self.checked).clone()
    }

    fn name(&self, node: &Node) -> Result<ContentId, Unidentified> {
        let db = self.db;
        let mut h = Hasher::new(NAME_DOMAIN);
        let args = match node {
            Node::Walk(body, args) => {
                let (tag, key) = body_key(db, *body);
                h.update(&[0, tag]).id(&key);
                args
            }
            Node::Leaf(func, args, kind) => {
                h.update(&[1, *kind as u8]).id(&item_key(db, ItemId::Function(*func)).0);
                args
            }
        };
        let args = share::args(db, args.as_ref()).map_err(|it| {
            Unidentified::named(
                node_name(db, node),
                format!("its arguments have no identity: {it}"),
            )
        })?; // alloc: error path
        Ok(h.id(&args).finish())
    }

    fn through(&self, node: &Node, error: Unidentified) -> Unidentified {
        error.through(node_name(self.db, node))
    }

    fn exhausted(&self, node: &Node, fuel: usize) -> Unidentified {
        // alloc: error path
        Unidentified::named(
            node_name(self.db, node),
            format!("its recursion has more than {fuel} instances"),
        )
    }
}

fn node_name(db: &Db, node: &Node) -> String {
    match node {
        Node::Walk(body, _) => name_of(db, Subject::Body(*body)),
        Node::Leaf(func, ..) => format!("`{}`", fn_path(db, *func)), // alloc: error path
    }
}

/// A root whose parameters hold code made outside the step: a function pointer, or a trait
/// object of a trait that is not the kernel's door. What it runs through them is not in the
/// walk, so the step has no CodeId.
fn root_params(
    db: &Db,
    env: &StoredParamEnvAndCrate,
    body: Body,
    args: &StoredGenericArgs,
) -> Result<(), String> {
    // clone: the query's keys, reference counts
    let mir = dispatch::body_of(db, env.clone(), body, args.clone())
        .map_err(|error| format!("its MIR does not lower: {error:?}"))?; // alloc: error path
    let cx = Cx::new(db, env);
    let mut seen = HashSet::new_in(Global);
    for &local in &mir.param_locals {
        let ty = mir.locals[local].ty.as_ref();
        if let Some(what) = outside_code(&cx, ty, &mut seen) {
            return Err(format!("it takes `{ty:?}`, which holds {what}")); // alloc: error path
        }
    }
    Ok(())
}

fn outside_code<'db>(
    cx: &Cx<'db>,
    ty: Ty<'db>,
    seen: &mut HashSet<Ty<'db>, Global>,
) -> Option<&'static str> {
    let ty = cx.normalize(ty);
    if !seen.insert(ty) {
        return None;
    }
    match ty.kind() {
        TyKind::FnPtr(..) => Some("a function pointer, whose code the step's walk does not see"),
        TyKind::Dynamic(bounds, _) => match bounds.principal() {
            Some(principal) if trusted(cx.db, principal.skip_binder().def_id.0.into()) => None,
            _ => Some(
                "a trait object of a trait not `#[blaze::pure(trusted)]`, whose vtable the step's walk does not see",
            ),
        },
        TyKind::RawPtr(pointee, _) | TyKind::Ref(_, pointee, _) | TyKind::Pat(pointee, _) => {
            outside_code(cx, pointee, seen)
        }
        TyKind::Array(element, _) | TyKind::Slice(element) => outside_code(cx, element, seen),
        TyKind::Tuple(fields) => fields.iter().find_map(|it| outside_code(cx, it, seen)),
        TyKind::Closure(_, args) => outside_code(cx, args.as_closure().tupled_upvars_ty(), seen),
        TyKind::Adt(def, args) => {
            for variant in dispatch::variants(cx.db, def.def_id()) {
                for (_, field) in cx.db.field_types(variant).iter() {
                    let field = field.ty().instantiate(cx.interner, args).skip_norm_wip();
                    if let Some(what) = outside_code(cx, field, seen) {
                        return Some(what);
                    }
                }
            }
            None
        }
        _ => None,
    }
}

/// One instance's part of a step's identity and the instances it runs, in site order. Its
/// constants are evaluated here, so a body whose constant reads the CodeId of a step that runs
/// it re-enters this query: [`SELF_READ`].
#[blaze_query::tracked(returns(ref), cycle_result = summary_cycle)]
#[expect(
    clippy::needless_pass_by_value,
    reason = "a tracked query's arguments are its key, which `blaze_query::tracked` hands the body by value"
)]
fn summary(
    db: &Db,
    env: StoredParamEnvAndCrate,
    node: Node,
    checked: bool,
) -> Result<Summary, Box<str>> {
    let cx = Cx::new(db, &env);
    let mut follow = Follow {
        cx: &cx,
        callees: Vec::new(), // alloc: one instance's callees, this query's miss, memoized after
        seen: HashSet::new_in(Global),
        dropped: HashSet::new_in(Global),
        failed: None,
    };
    let content = match &node {
        Node::Leaf(func, args, kind) => {
            let item = ItemId::Function(*func);
            let mut h = Hasher::new(LEAF_DOMAIN);
            h.update(&[*kind as u8]).id(&item_key(db, item).0).id(&interface(db, item)?);
            match kind {
                LeafKind::Trusted => {
                    match code_hash(db, item).map_err(|it| it.describe(db))? {
                        Some(code) => h.update(&[1]).id(&code.0),
                        None => h.update(&[0]),
                    };
                    // A trusted function runs the closures and functions it is given.
                    for ty in args.as_ref().types() {
                        follow.given(ty);
                    }
                }
                LeafKind::Foreign => {
                    match AttrFlags::link_name(db, (*func).into()) {
                        Some(link) => h.update(&[1]).part(link.as_str().as_bytes()),
                        None => h.update(&[0]),
                    };
                }
                LeafKind::Door => {}
            }
            let args = share::args(db, args.as_ref())
                .map_err(|it| format!("its arguments have no identity: {it}"))?; // alloc: error path
            h.id(&args);
            h.finish()
        }
        Node::Walk(body, args) => {
            // clone: the query's keys, reference counts
            let mir = dispatch::body_of(db, env.clone(), *body, args.clone())
                .map_err(|error| format!("its MIR does not lower: {error:?}"))?; // alloc: error path
            // Its arithmetic checks overflow when the crate it is written in keeps the checks
            // (or it takes its callers') and the crate compiling the step runs them, as rustc's
            // code generator drops an optional overflow check its crate does not run.
            let effective = overflow::body_written_checked(db, mir) && checked;
            let mut h = Hasher::new(BEHAVIOUR_DOMAIN);
            h.update(&[body_tag(*body), effective as u8]).id(&own_code(db, *body)?);
            let mut facts = Facts::new(&cx, h);
            facts.args(args.as_ref());
            facts.body(mir);
            let Facts { h, edges, failed, .. } = facts;
            if let Some(what) = failed {
                return Err(what.into_boxed_str());
            }
            for edge in edges {
                follow.edge(edge);
            }
            h.finish()
        }
    };
    if let Some(what) = follow.failed {
        return Err(what.into_boxed_str());
    }
    Ok(Summary { content, callees: follow.callees.into_boxed_slice() })
}

fn summary_cycle(
    _: &Db,
    _: blaze_query::db::Id,
    _: StoredParamEnvAndCrate,
    _: Node,
    _: bool,
) -> Result<Summary, Box<str>> {
    Err(SELF_READ.into())
}

/// What one instance's edges run, as dispatch resolves them: the walk's next instances.
struct Follow<'a, 'db> {
    cx: &'a Cx<'db>,
    callees: Vec<Node>,
    seen: HashSet<Node, Global>,
    dropped: HashSet<Ty<'db>, Global>,
    failed: Option<String>,
}

impl<'db> Follow<'_, 'db> {
    fn fail(&mut self, what: impl FnOnce() -> String) {
        if self.failed.is_none() {
            self.failed = Some(what());
        }
    }

    fn push(&mut self, node: Node) {
        if !self.seen.contains(&node) {
            self.seen.insert(node.clone()); // clone: a reference count
            self.callees.push(node);
        }
    }

    fn walk(&mut self, body: Body, args: GenericArgs<'db>) {
        if !within_longest(args) {
            return self.fail(|| {
                format!("reaches an instance whose type arguments are past {LONGEST} types long")
            }); // alloc: error path
        }
        self.push(Node::Walk(body, args.store()));
    }

    fn leaf(&mut self, func: FunctionId, args: GenericArgs<'db>, kind: LeafKind) {
        self.push(Node::Leaf(func, args.store(), kind));
    }

    fn edge(&mut self, edge: Edge<'db>) {
        match edge {
            Edge::Call(f, args) => self.call(f, args),
            Edge::Closure(body, parent) => self.walk(body, parent),
            Edge::Drop(ty) => self.drop_glue(ty),
            Edge::Vtable(concrete, dyn_ty) => self.vtable(concrete, dyn_ty),
        }
    }

    /// What calling `f` with `args` runs, one dispatch step at a time.
    fn call(&mut self, f: FunctionId, args: GenericArgs<'db>) {
        let cx = self.cx;
        let db = cx.db;
        if let ItemContainerId::TraitId(_) = f.loc(db).container
            && trusted_trait_method(db, f)
        {
            return self.leaf(f, args, LeafKind::Door);
        }
        if trusted(db, f.into()) {
            return self.leaf(f, args, LeafKind::Trusted);
        }
        match dispatch::call(&cx.infcx, cx.env, f, args) {
            Call::DropGlue(ty) => self.drop_glue(ty),
            Call::ConstEvalSelect(Some((hir_def::CallableDefId::FunctionId(g), g_args))) => {
                self.call(g, g_args)
            }
            Call::FnItem(hir_def::CallableDefId::FunctionId(g), g_args) => self.call(g, g_args),
            // A constructor, the code generator's own code, or a pointer made where it was.
            Call::ConstEvalSelect(Some(_))
            | Call::Intrinsic(_)
            | Call::FnItem(..)
            | Call::Indirect => {}
            Call::ConstEvalSelect(None) => {
                self.fail(|| {
                    "calls `const_eval_select` with a run-time function not known".to_owned()
                }) // alloc: error path
            }
            Call::UnknownIntrinsic(unknown) => {
                self.fail(|| format!("calls an intrinsic not known: {unknown}"))
            } // alloc: error path
            Call::Closure(body, parent) => self.walk(body, parent),
            Call::AsyncClosure => {
                self.fail(|| "calls an `async` closure, which the walk does not follow".to_owned())
            } // alloc: error path
            Call::Method(Err(why)) => self.fail(|| {
                format!(
                    "calls `{}` for `{:?}`, which no impl's method is found for: {}",
                    fn_path(db, f),
                    args.type_at(0),
                    why.describe()
                ) // alloc: error path
            }),
            Call::Method(Ok(callee)) => match callee {
                Callee::Poll(body, parent) => self.walk(body, parent),
                // Made where the vtable or the pointer was: an edge there, or the root's refusal.
                Callee::Virtual | Callee::FnPtr(_) => {}
                Callee::Derived(..) => {
                    self.fail(|| {
                        format!("calls a derived `{}`, which blaze does not build", fn_path(db, f))
                    }) // alloc: error path
                }
                Callee::CloneBuiltin(_) => self.clone_builtin(f, args.type_at(0)),
                Callee::Fn(g, g_args) => self.function(f, g, g_args),
            },
        }
    }

    /// A call of `f` that resolved to the function `g` itself.
    fn function(&mut self, f: FunctionId, g: FunctionId, args: GenericArgs<'db>) {
        let db = self.cx.db;
        if trusted(db, g.into()) {
            return self.leaf(g, args, LeafKind::Trusted);
        }
        if trusted_trait_method(db, g) {
            return self.leaf(g, args, LeafKind::Door);
        }
        if let ItemContainerId::ExternBlockId(_) = g.loc(db).container {
            return self.leaf(g, args, LeafKind::Foreign);
        }
        if !FunctionSignature::of(db, g).has_body() {
            return self.fail(|| {
                match g == f && matches!(f.loc(db).container, ItemContainerId::TraitId(_)) {
                    true => format!(
                        "calls `{}` for `{:?}`, which no impl gives a body",
                        fn_path(db, f),
                        args.type_at(0)
                    ), // alloc: error path
                    false => format!("calls `{}`, which has no body", fn_path(db, g)), // alloc: error path
                }
            });
        }
        self.walk(Body::Fn(g), args);
    }

    /// The closures and functions a trusted function is given, as its type arguments: it can
    /// call or drop them.
    fn given(&mut self, ty: Ty<'db>) {
        match self.cx.normalize(ty).kind() {
            TyKind::Closure(id, args) => {
                let parent = GenericArgs::new_from_slice(args.as_closure().parent_args());
                self.walk(Body::Closure(blaze_query::db::AsId::as_id(&id.0)), parent);
                self.drop_glue(ty);
            }
            TyKind::FnDef(def, args) => {
                if let hir_def::CallableDefId::FunctionId(f) = def.0 {
                    self.call(f, args.skip_binder());
                }
            }
            TyKind::CoroutineClosure(..) => {
                self.fail(|| {
                    "gives a trusted function an `async` closure, which the walk does not follow"
                        .to_owned()
                }) // alloc: error path
            }
            _ => {}
        }
    }

    /// `Clone::clone` of a type whose impl the compiler supplies: its parts' clones, and their
    /// drops when one panics.
    fn clone_builtin(&mut self, clone: FunctionId, ty: Ty<'db>) {
        let Some(parts) = clone_parts(&self.cx.infcx, self.cx.env, ty) else {
            return self.fail(|| "clones a closure whose captures are not known".to_owned()); // alloc: error path
        };
        for part in parts {
            self.call(clone, GenericArgs::new_from_slice(&[GenericArg::from(part)]));
            self.drop_glue(part);
        }
    }

    fn drop_glue(&mut self, ty: Ty<'db>) {
        let cx = self.cx;
        let mut steps = SmallVec::new();
        dispatch::drop_glue(&cx.infcx, cx.env, ty, &mut self.dropped, &mut steps);
        for step in steps {
            match step {
                DropStep::Drop(drop, args) => self.call(drop, args),
                DropStep::NoDropTrait => {
                    self.fail(|| "drops a value, but the toolchain has no `Drop::drop`".to_owned())
                } // alloc: error path
                // An `async` block's drop glue drops what its `poll` holds: its body's.
                DropStep::Unknown(ty) => match coroutine_poll(cx.db, ty) {
                    Some((body, parent)) => self.walk(body, parent),
                    None => self
                        .fail(|| format!("drops `{ty:?}`, whose drop glue the walk does not know")), // alloc: error path
                },
            }
        }
    }

    fn vtable(&mut self, concrete: Ty<'db>, dyn_ty: Ty<'db>) {
        self.drop_glue(concrete);
        for (method, args) in vtable_methods(self.cx.interner, concrete, dyn_ty) {
            self.call(method, args);
        }
    }
}
