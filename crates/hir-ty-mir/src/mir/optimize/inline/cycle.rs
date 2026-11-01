//! rustc's `mir_callgraph_cyclic` and `mir_inliner_callees`
//! (`compiler/rustc_mir_transform/src/inline/cycle.rs`): the bodies of the caller's crate that
//! may call the caller back, following each call as it resolves once the generic arguments of
//! the body it is in are known. The inliner inlines none of them into the caller, so the
//! optimized body of a function never asks for itself.

use std::hash::Hash;

use base_db::Db;
use blaze_hash::{HashMap, HashSet};
use blaze_query::db::AsId;
use hir_def::{FunctionId, HasModule, nameres::crate_def_map, signatures::FunctionSignature};
use rustc_type_ir::inherent::GenericArgs as _;
use smallvec::SmallVec;

use crate::{
    db::{HirDatabase, InternedClosureId},
    intrinsic::Intrinsic,
    mir::{InlinedBody, MirBody, TerminatorKind, transform::Ctx},
    next_solver::{GenericArgs, StoredGenericArgs},
    traits::ParamEnvAndCrate,
};

use super::{Attrs, fn_def, instantiate, is_generic, resolve, spans::owner_of};

/// The functions `body` calls by name, each once with its generic arguments, in the order it
/// first calls them: rustc's `mir_inliner_callees`, of the runtime body. Of
/// `const_eval_select` it is the function called at runtime.
fn callees_of(body: &MirBody<'_>) -> Vec<(FunctionId, StoredGenericArgs)> {
    let mut seen = HashSet::default();
    let mut callees = Vec::new(); // alloc: the memoized query's stored result, once per body
    for (_, block) in body.basic_blocks.iter() {
        let Some(TerminatorKind::Call { func, args, intrinsic, .. }) =
            block.terminator.as_ref().map(|it| &it.kind)
        else {
            continue;
        };
        let call = if *intrinsic == Some(Intrinsic::ConstEvalSelect) {
            args.get(2).and_then(|it| fn_def(body, it))
        } else {
            fn_def(body, func)
        };
        let Some(call) = call else { continue };
        if seen.insert(call) {
            callees.push((call.0, call.1.store()));
        }
    }
    callees
}

#[blaze_query::tracked(returns(ref))]
pub(super) fn inliner_callees_of_fn(
    db: &Db,
    func: FunctionId,
) -> Vec<(FunctionId, StoredGenericArgs)> {
    match db.runtime_mir_body(func.into()) {
        Ok(body) => callees_of(body),
        Err(_) => Vec::new(), // alloc: empty, allocates nothing
    }
}

#[blaze_query::tracked(returns(ref))]
pub(super) fn inliner_callees_of_closure<'db>(
    db: &'db Db,
    closure: InternedClosureId<'db>,
) -> Vec<(FunctionId, StoredGenericArgs)> {
    match db.runtime_mir_body_for_closure(closure) {
        Ok(body) => callees_of(body),
        Err(_) => Vec::new(), // alloc: empty, allocates nothing
    }
}

fn inliner_callees<'db>(
    db: &'db Db,
    body: InlinedBody<'db>,
) -> &'db [(FunctionId, StoredGenericArgs)] {
    match body {
        InlinedBody::Fn(func) => inliner_callees_of_fn(db, func),
        InlinedBody::Closure(closure) => inliner_callees_of_closure(db, closure),
    }
}

/// What [`walk`] found of the bodies it keeps.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Walked<B: Eq + Hash> {
    /// rustc's `mir_callgraph_cyclic`: the bodies a path back to the root goes through.
    pub(super) cyclic: HashSet<B>,
    /// The bodies the walk followed.
    pub(super) reached: HashSet<B>,
}

/// [`Walked`] of the bodies of the root's crate, each list sorted by [`key`].
#[derive(Debug, PartialEq, Eq, blaze_query::Value)]
pub(super) struct Callgraph<'db> {
    cyclic: Vec<InlinedBody<'db>>,
    reached: Vec<InlinedBody<'db>>,
}

fn key(body: &InlinedBody<'_>) -> (bool, blaze_query::db::Id) {
    match *body {
        InlinedBody::Fn(func) => (false, func.as_id()),
        InlinedBody::Closure(closure) => (true, closure.as_id()),
    }
}

impl<'db> Callgraph<'db> {
    fn new(walked: Walked<InlinedBody<'db>>) -> Self {
        let sorted = |set: HashSet<InlinedBody<'db>>| {
            // alloc: the memoized query's stored result, once per root
            let mut list: Vec<_> = set.into_iter().collect();
            list.sort_unstable_by_key(key);
            list
        };
        Callgraph { cyclic: sorted(walked.cyclic), reached: sorted(walked.reached) }
    }

    /// `body` may call the root back.
    pub(super) fn is_cyclic(&self, body: InlinedBody<'db>) -> bool {
        self.cyclic.binary_search_by_key(&key(&body), key).is_ok()
    }

    /// The walk followed `body`. A body of the crate the inliner finds called that the walk
    /// did not follow is one whose call resolves only in the instantiated callee it is in (its
    /// opaque types revealed): the walk never looked at what it calls.
    pub(super) fn reached(&self, body: InlinedBody<'db>) -> bool {
        self.reached.binary_search_by_key(&key(&body), key).is_ok()
    }
}

#[blaze_query::tracked(returns(ref))]
pub(super) fn callgraph_cyclic_fn<'db>(
    db: &'db Db,
    root: FunctionId,
) -> Option<Callgraph<'db>> {
    cyclic(db, InlinedBody::Fn(root))
}

#[blaze_query::tracked(returns(ref))]
pub(super) fn callgraph_cyclic_closure<'db>(
    db: &'db Db,
    root: InternedClosureId<'db>,
) -> Option<Callgraph<'db>> {
    cyclic(db, InlinedBody::Closure(root))
}

/// The walk of the bodies of `root`'s crate that a call of `root`'s body may lead to; `None`
/// where it gave up at the recursion limit, as rustc's does, and nothing of the crate may then
/// be inlined into `root`.
pub(super) fn callgraph_cyclic<'db>(
    db: &'db Db,
    root: InlinedBody<'db>,
) -> Option<&'db Callgraph<'db>> {
    match root {
        InlinedBody::Fn(func) => callgraph_cyclic_fn(db, func).as_ref(),
        InlinedBody::Closure(closure) => callgraph_cyclic_closure(db, closure).as_ref(),
    }
}

/// Whether the walk looks into `body` instantiated with `args`: rustc's `should_recurse`,
/// which follows an instance whose MIR is available. Of a crate other than `krate`, that is
/// what rustc keeps of it: the MIR of a generic or `#[inline]` function, or a closure's.
fn has_mir<'db>(
    db: &'db Db,
    krate: base_db::Crate,
    body: InlinedBody<'db>,
    args: GenericArgs<'db>,
) -> bool {
    match body {
        InlinedBody::Fn(func) => {
            FunctionSignature::of(db, func).has_body()
                && !FunctionSignature::is_intrinsic(db, func)
                && (func.krate(db) == krate || is_generic(args) || Attrs::of_fn(db, func).hinted())
        }
        InlinedBody::Closure(_) => true,
    }
}

fn cyclic<'db>(db: &'db Db, root: InlinedBody<'db>) -> Option<Callgraph<'db>> {
    let owner = owner_of(db, root);
    let krate = owner.krate(db);
    blaze_mem::frame::task(blaze_mem::Tag::Mir, |frame| {
        let ctx = Ctx::new(db, owner, frame);
        let env = ParamEnvAndCrate { param_env: ctx.env, krate };
        let generic_def = owner.generic_def(db).into();
        let identity = GenericArgs::identity_for_item(ctx.infcx.interner, generic_def);
        let limit = crate_def_map(db, krate).recursion_limit() as usize / 8;
        let callees = |(body, args): (InlinedBody<'db>, GenericArgs<'db>)| {
            let mut out: SmallVec<[((InlinedBody<'db>, GenericArgs<'db>), bool); 8]> =
                SmallVec::new();
            for (func, callee_args) in inliner_callees(db, body) {
                let Some(callee_args) = instantiate(&ctx, callee_args.as_ref(), args) else {
                    continue;
                };
                let Some(callee) = resolve(&ctx, env, *func, callee_args) else { continue };
                out.push((callee, has_mir(db, krate, callee.0, callee.1)));
            }
            out
        };
        let local = |body: InlinedBody<'db>| owner_of(db, body).krate(db) == krate;
        walk((root, identity), root, limit, |node| node.0, callees, local).map(Callgraph::new)
    })
}

struct Frame<N> {
    node: N,
    callees: SmallVec<[(N, bool); 8]>,
    next: usize,
    reaches: bool,
}

/// rustc's `process` from `start`, without recursion. A node is an instance, `body_of` its
/// body; `callees` gives the instances an instance calls, each with whether it has MIR to
/// follow. Of the bodies `local` says to keep, it gives those of the instances a path from
/// `start` to `target` goes through, and those followed. Each instance is followed once, one
/// met again while it is being followed taken not to reach `target`; a body followed more
/// than `limit` times ends the walk with `None`.
pub(super) fn walk<N, B, C>(
    start: N,
    target: B,
    limit: usize,
    body_of: impl Fn(N) -> B,
    mut callees: impl FnMut(N) -> C,
    mut local: impl FnMut(B) -> bool,
) -> Option<Walked<B>>
where
    N: Copy + Eq + Hash,
    B: Copy + Eq + Hash,
    C: Into<SmallVec<[(N, bool); 8]>>,
{
    let mut seen: HashMap<N, bool> = HashMap::default();
    let mut recursion: HashMap<B, usize> = HashMap::default();
    let mut graph = Walked { cyclic: HashSet::default(), reached: HashSet::default() };
    let mut stack =
        // alloc: one stack per memoized walk of a root, not per node
        vec![Frame { node: start, callees: callees(start).into(), next: 0, reaches: false }];
    while let Some(frame) = stack.last_mut() {
        let Some(&(callee, has_mir)) = frame.callees.get(frame.next) else {
            let done = stack.pop()?;
            let Some(caller) = stack.last_mut() else { break };
            seen.insert(done.node, done.reaches);
            if done.reaches {
                let body = body_of(done.node);
                if local(body) {
                    graph.cyclic.insert(body);
                }
                caller.reaches = true;
            }
            continue;
        };
        frame.next += 1;
        let body = body_of(callee);
        if body == target {
            frame.reaches = true;
            seen.insert(callee, true);
            continue;
        }
        if !has_mir {
            continue;
        }
        match seen.get(&callee) {
            Some(&reaches) => {
                if reaches {
                    frame.reaches = true;
                    if local(body) {
                        graph.cyclic.insert(body);
                    }
                }
            }
            None => {
                seen.insert(callee, false);
                let count = recursion.entry(body).or_default();
                *count += 1;
                if *count > limit {
                    return None;
                }
                if local(body) {
                    graph.reached.insert(body);
                }
                let callees = callees(callee).into();
                stack.push(Frame { node: callee, callees, next: 0, reaches: false });
            }
        }
    }
    Some(graph)
}
