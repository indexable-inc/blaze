//! A walk made by patching the walk kept of the same program
//! (docs/v2/plan/design/incremental-walk.md), for the edit that changes what functions are
//! and not what they refer to: a body edit.
//!
//! The kept walk ([`Walked`]) is the whole result of a walk that had no error, with what
//! finds the changed set and what a change touches. A build patches it when all of this holds,
//! each checked, and walks whole otherwise, saying which did not ([`super::Walk::whole`]):
//!
//! 1. Its inputs are the kept walk's: roots, static roots, check roots, policy.
//! 2. The front end closes no facts of the whole program ([`Frontend::closes`]) and its
//!    durable view is the kept walk's ([`Frontend::durable_view`]): every function that was
//!    durable ([`Frontend::durable`]) describes and counts as it did, unasked.
//! 3. Every other function's count is the kept one, every static's describe is the kept one,
//!    and each function whose describe changed keeps its *shape* ([`same_shape`]): its item,
//!    origin, references, built-in places, symbol, linkage and what its slot is decided by.
//! 4. A changed function is one describe's key, is built into no other, and its new key is
//!    none the walk holds.
//!
//! Why the patched walk is then the whole walk's result. The whole walk is a function of each
//! reached function's describe and count and each static's describe. By 2 and 3 the counts,
//! the statics and every reference list are the kept walk's, so the graph, the levels, the
//! recursion limit's claims (which read counts and references only) and what the build's roots
//! reach are the kept walk's, and it had no error. A node's slot ([`super::rebind`]) reads its
//! policy choice, `editable`, `over_editable`, and the slots and built-in places of what it
//! refers to: all kept by 3. A node's places read its references and each callee's key, slot
//! and weak symbol: only a changed callee's key differs, and every referrer of it is renamed
//! through the reverse edges. A node's built-in closure names keys of built-in callees: by 4
//! no changed key is among them. So the patch is: each changed node takes its new describe,
//! unit and key, at its place in key order, and every place that named its old key names the
//! new one.

use std::{alloc::Global, collections::HashMap, sync::Arc};

use blaze_graph::par;
use blaze_ids::ContentId;

use super::{
    Build, CheckRoot, FnNode, FrontInstance, FrontStatic, Node, Sorted, StaticId, StaticNode, Walk,
    callee,
};
use crate::{
    InstanceKey, Target,
    backend::{Counted, Described, DescribedStatic, Frontend, Prepare},
    plan::Origin,
    settings::Spec,
};

/// What refers to a function, by its place in the walk.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Referrer {
    Function(InstanceKey),
    Static(StaticId),
}

/// A static of a kept walk: [`StaticNode`] without its backend's name.
pub(super) struct KeptStatic<I, S> {
    id: StaticId,
    value: S,
    described: DescribedStatic<I, S>,
    spec: Spec,
    refs: Vec<Target>,
}

/// What a kept walk holds beside its nodes: what finds the changed set and what a change
/// touches.
pub(super) struct Keep<I, S> {
    /// Every function the walk described, by whose code asked for it, with its key: those
    /// built, and those described for the recursion limit's check alone.
    asked: HashMap<(I, Origin), InstanceKey>,
    /// How many describes each key is of.
    askers: blaze_hash::HashMap<InstanceKey, u32, Global>,
    /// The functions that were not durable, each with its count and the describe the walk
    /// holds of it: asked again each build. The describe is the walk's own, never read back
    /// from the describes kept for every program, which another program's build may have
    /// replaced since.
    volatile: Vec<(I, Origin)>,
    counted: Vec<Counted>,
    was: Vec<Arc<Described<I, S>>>,
    /// Every static the walk described, with its describe: asked again each build.
    statics: Vec<(S, DescribedStatic<I, S>)>,
    /// What refers to each function: the reverse of every node's and static's places.
    referrers: blaze_hash::HashMap<InstanceKey, Vec<Referrer>, Global>,
    /// Every function some node builds in.
    built_into: blaze_hash::HashSet<InstanceKey, Global>,
}

/// The walk kept of one program: its inputs, its result, and its [`Keep`].
pub(super) struct Walked<I, S> {
    roots_in: Vec<(I, Origin)>,
    static_roots_in: Vec<S>,
    check_in: Vec<CheckRoot<I, S>>,
    policy: ContentId,
    view: u64,
    /// Shared with the build's `Later`: patched only once that build is gone.
    nodes: Arc<Sorted<InstanceKey, FnNode<I, S>>>,
    statics: Vec<KeptStatic<I, S>>,
    roots: Vec<InstanceKey>,
    keep: Keep<I, S>,
}

/// What a whole walk hands the build beside its result, for the walk to be kept
/// ([`Build::keep_of`]): made only by a walk with no error.
pub(super) struct Side<I, S> {
    pub(super) asked: HashMap<(I, Origin), InstanceKey>,
    pub(super) counted: Vec<((I, Origin), Counted)>,
    /// Every describe the walk's last round was given, by who asked.
    pub(super) described: Vec<((I, Origin), Arc<Described<I, S>>)>,
    pub(super) statics: Vec<(S, DescribedStatic<I, S>)>,
}

/// A walk's result with what keeps it.
pub(super) type Patched<B> = (
    Sorted<InstanceKey, Node<B>>,
    Sorted<StaticId, StaticNode<B>>,
    Vec<InstanceKey>,
    Walk,
    Keep<FrontInstance<B>, FrontStatic<B>>,
);

/// Whether a function described as `now` is in the walk what one described as `was` is, but
/// for its code and key: every field the graph, the slots and the places are made from. What
/// may differ is what only the node itself holds: its code hashes, arguments, anchors, name,
/// owner, identity, bound and coldness.
fn same_shape<I: PartialEq, S: PartialEq>(was: &Described<I, S>, now: &Described<I, S>) -> bool {
    was.item == now.item
        && was.origin == now.origin
        && was.editable == now.editable
        && was.over_editable == now.over_editable
        && was.overflow_checks == now.overflow_checks
        && was.address == now.address
        && was.symbol == now.symbol
        && was.linkage == now.linkage
        && was.refs == now.refs
        && was.built_in == now.built_in
}

/// Every place of `refs` that names the function `old` names `new`.
fn rename(refs: &mut [Target], old: InstanceKey, new: InstanceKey) {
    for target in refs {
        if let Target::Function(key) | Target::Direct(key) = target
            && *key == old
        {
            *key = new;
        }
    }
}

impl<'a, B: Prepare> Build<'a, B> {
    /// The walk of `program` (these roots, check roots and policy) patched from the one
    /// kept; `Err` says why the walk is to run whole. The kept walk is taken either way: a
    /// build that ends well keeps its own.
    pub(super) fn patched(
        &self,
        program: u64,
        roots: &[(FrontInstance<B>, Origin)],
        static_roots: &[FrontStatic<B>],
        check: &[CheckRoot<FrontInstance<B>, FrontStatic<B>>],
        policy: ContentId,
        view: Option<u64>,
    ) -> Result<Patched<B>, &'static str> {
        let Some(kept) = self.kept else { return Err("the build keeps no walk") };
        if self.frontend.closes() {
            return Err("the program may close dyn facts, which are made from a whole walk");
        }
        let Some(view) = view else { return Err("the front end tells no durable view") };
        let Some(walked) = kept.take_walk(program) else {
            return Err("no walk of this program is kept");
        };
        if walked.roots_in != roots
            || walked.static_roots_in != static_roots
            || walked.check_in != check
            || walked.policy != policy
        {
            return Err("the kept walk is of other roots or another policy");
        }
        if walked.view != view {
            return Err("a durable input was written since the kept walk");
        }
        let Walked { nodes, mut statics, mut roots, mut keep, .. } = walked;
        let Ok(mut nodes) = Arc::try_unwrap(nodes) else {
            return Err("the kept walk is still held by the build that made it");
        };
        if self.frontend.stopped() {
            return Err("the build stopped before its walk");
        }

        // The changed set: of the functions that were not durable, those whose describe is
        // another. A describe that fails is the whole walk's to report.
        let asked_again = par::map(0..keep.volatile.len(), |at| {
            let (asked, was) = (&keep.volatile[at], &keep.was[at]);
            if !self.asks {
                return Ok(None);
            }
            match self.frontend.redescribe(&asked.0, &asked.1, was) {
                Ok(None) => Ok(None),
                Ok(Some(now)) => {
                    // alloc: once per describe, shared by the walk's node and the walk kept
                    let now = Arc::new(now);
                    kept.keep(0, asked, now.clone()); // clone: a reference count
                    Ok(Some(now))
                }
                Err(_) => Err("a describe failed"),
            }
        });
        if self.frontend.counted(&keep.volatile) != keep.counted {
            return Err("a function's recursion count changed");
        }
        for (value, was) in &keep.statics {
            match self.frontend.describe_static(value) {
                Ok(now) if now == *was => {}
                _ => return Err("a static's describe changed"),
            }
        }

        // alloc: per build, the functions it changed
        let mut changed = Vec::new();
        let mut visited = 0;
        for (at, again) in asked_again.into_iter().enumerate() {
            let Some(now) = again? else { continue };
            if self.checks_shape && !same_shape(&keep.was[at], &now) {
                return Err("a changed function refers to other things or is another's");
            }
            keep.was[at] = now.clone(); // clone: a reference count, once a change
            let asked = &keep.volatile[at];
            let Some(&old) = keep.asked.get(asked) else {
                return Err("a kept walk's function has no key");
            };
            let Ok(made) = self.node(&asked.0, asked.1, now) else {
                return Err("a changed function has no unit under the policy");
            };
            let new = made.unit.instance.key();
            if keep.askers.get(&old) != Some(&1) {
                return Err("a key of two describes changed");
            }
            if keep.built_into.contains(&old) {
                return Err("a changed function is built into another");
            }
            if new != old && keep.askers.contains_key(&new) {
                return Err("a changed function took a key the walk holds");
            }
            visited += 1;
            keep.asked.insert(asked.clone(), new); // clone: the walk's handle, once a change
            keep.askers.remove(&old);
            keep.askers.insert(new, 1);
            changed.push(asked.clone()); // clone: the walk's handle, once a change
            // Described for the recursion limit's check alone: no node, and nothing names it.
            if nodes.get(&old).is_none() {
                continue;
            }
            if new != old && !nodes.rekey(&old, new) {
                return Err("a changed function took a key the walk holds");
            }
            let node = nodes.get_mut(&new).expect("its node, under its new key");
            if node.patchable != made.patchable || node.then.is_some() != made.then.is_some() {
                return Err("a changed function is reached another way");
            }
            // Its places, its built-in closure and its slot are the kept ones: its shape is.
            (node.unit, node.spec, node.then) = (made.unit, made.spec, made.then);
            (node.described, node.name_digest) = (made.described, made.name_digest);
            if new == old {
                continue;
            }
            for root in roots.iter_mut().filter(|it| **it == old) {
                *root = new;
            }
            // Every place that named it, its own among them, names its new key.
            let mut referrers = keep.referrers.remove(&old).unwrap_or_default();
            for referrer in &mut referrers {
                if *referrer == Referrer::Function(old) {
                    *referrer = Referrer::Function(new);
                }
            }
            if self.renames {
                visited += referrers.len();
                // The statics are in the kept walk's order, which is by their ids.
                for referrer in &referrers {
                    match referrer {
                        Referrer::Function(key) => {
                            let node = nodes.get_mut(key).expect("a referrer is a node");
                            rename(&mut node.refs, old, new);
                        }
                        Referrer::Static(id) => {
                            let Ok(at) = statics.binary_search_by_key(id, |it| it.id) else {
                                return Err("a kept walk's referrer is no static of it");
                            };
                            rename(&mut statics[at].refs, old, new);
                        }
                    }
                }
            }
            keep.referrers.insert(new, referrers);
            // It refers under its new key.
            let node = nodes.get(&new).expect("its node");
            for callee in node.refs.iter().filter_map(callee).filter(|it| *it != new) {
                for referrer in keep.referrers.get_mut(&callee).into_iter().flatten() {
                    if *referrer == Referrer::Function(old) {
                        *referrer = Referrer::Function(new);
                    }
                }
            }
        }
        // The statics as the build holds them.
        let statics = Sorted::of_sorted(statics.into_iter().map(|it| {
            let node: StaticNode<B> =
                StaticNode { value: it.value, described: it.described, spec: it.spec, refs: it.refs };
            (it.id, node)
        }));
        self.frontend.reached(&changed, &[]);
        let walk = Walk {
            described: changed.len(),
            verified: keep.volatile.len() - changed.len(),
            reached: nodes.len(),
            dropped: 0,
            fresh: None,
            whole: None,
            visited,
        };
        Ok((nodes, statics, roots, walk, keep))
    }

    /// What keeps a whole walk that had no error: who asked for what, which functions are
    /// asked again, and the reverse of every place. Linear in the walk, once per whole walk.
    pub(super) fn keep_of(
        &self,
        nodes: &Sorted<InstanceKey, Node<B>>,
        statics: &Sorted<StaticId, StaticNode<B>>,
        side: Side<FrontInstance<B>, FrontStatic<B>>,
    ) -> Keep<FrontInstance<B>, FrontStatic<B>> {
        let mut askers = blaze_hash::HashMap::with_capacity_in(side.asked.len(), Global);
        for key in side.asked.values() {
            *askers.entry(*key).or_insert(0) += 1;
        }
        let mut referrers: blaze_hash::HashMap<InstanceKey, Vec<Referrer>, Global> =
            blaze_hash::HashMap::with_capacity_in(nodes.len(), Global);
        let mut refer = |refs: &[Target], by: Referrer| {
            for callee in refs.iter().filter_map(callee) {
                // alloc: per whole walk, one per reference
                let list = referrers.entry(callee).or_default();
                if list.last() != Some(&by) {
                    list.push(by);
                }
            }
        };
        for (key, node) in nodes.iter() {
            refer(&node.refs, Referrer::Function(*key));
        }
        for (id, node) in statics.iter() {
            refer(&node.refs, Referrer::Static(*id));
        }
        let mut built_into = blaze_hash::HashSet::new_in(Global);
        for key in nodes.values().flat_map(|node| &node.built_in) {
            built_into.insert(*key);
        }
        // Asked once a function, after its describe: which are asked again each build.
        // alloc: per whole walk, the functions that are not durable
        // alloc: per whole walk, one per describe, freed with the pass
        let described: HashMap<_, _> =
            side.described.iter().map(|(asked, it)| (asked, it)).collect();
        let asked_again = par::filter_map(&side.counted, |(asked, counted)| {
            if self.frontend.durable(&asked.0, &asked.1) {
                return None;
            }
            let was =
                described.get(asked).expect("a walk with no error described what it counted");
            // clone: the walk's handle of a function that is asked again each build
            Some((asked.clone(), *counted, Arc::clone(*was)))
        });
        // alloc: per whole walk, the functions that are not durable
        let (mut volatile, mut counted, mut was) = (Vec::new(), Vec::new(), Vec::new());
        for (asked, count, described) in asked_again {
            volatile.push(asked);
            counted.push(count);
            was.push(described);
        }
        Keep {
            asked: side.asked,
            askers,
            volatile,
            counted,
            was,
            statics: side.statics,
            referrers,
            built_into,
        }
    }

    /// Keeps a walk that ended a build well as the walk of `program`, for the next build to
    /// patch. `view` is the front end's durable view from before the walk.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn keep_walk(
        &self,
        program: u64,
        roots: &[(FrontInstance<B>, Origin)],
        static_roots: &[FrontStatic<B>],
        check: &[CheckRoot<FrontInstance<B>, FrontStatic<B>>],
        policy: ContentId,
        view: u64,
        nodes: Arc<Sorted<InstanceKey, Node<B>>>,
        statics: Sorted<StaticId, StaticNode<B>>,
        root_keys: Vec<InstanceKey>,
        keep: Keep<FrontInstance<B>, FrontStatic<B>>,
    ) {
        let Some(kept) = self.kept else { return };
        let statics = (statics.keys.into_iter().zip(statics.values))
            .map(|(id, node)| KeptStatic {
                id,
                value: node.value,
                described: node.described,
                spec: node.spec,
                refs: node.refs,
            })
            // alloc: per build that keeps its walk, its statics
            .collect();
        let walked = Walked {
            // clone: per build that keeps its walk, its roots
            roots_in: roots.to_vec(),
            static_roots_in: static_roots.to_vec(), // clone: as above
            check_in: check.to_vec(),               // clone: as above
            policy,
            view,
            nodes,
            statics,
            roots: root_keys,
            keep,
        };
        kept.keep_walk(program, walked);
    }
}
