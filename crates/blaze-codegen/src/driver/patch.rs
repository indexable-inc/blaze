//! A walk made by patching the walk kept of the same program
//! (docs/v2/plan/design/incremental-walk.md): the work of an edit, not of the program.
//!
//! The kept walk ([`Walked`]) is the whole result of a walk that had no error, with what
//! finds the changed set and what a change touches ([`Keep`]): every *item* the walk
//! described (a function as some origin's code asks for it, a static) with its describe, the
//! graph of what each refers to and the two sets of it that the roots reach
//! ([`support`]: everything described, and what is built), and which functions are asked
//! again.
//!
//! A build patches it when all of this holds, each checked, and walks whole otherwise,
//! saying which did not ([`super::Walk::whole`]):
//!
//! 1. Its inputs are the kept walk's: roots, static roots, check roots, policy.
//! 2. The front end closes no facts of the whole program ([`Frontend::closes`]) and its
//!    durable view is the kept walk's ([`Frontend::durable_view`]).
//! 3. Each changed function keeps its *frame* ([`same_frame`]: its item, origin, symbol,
//!    linkage, address and what its slot is decided by), is one describe's key, is built
//!    into no other, and takes no key the walk holds; each new function takes a key of its
//!    own; a changed static is the same static under the same symbol.
//! 4. Nothing the whole walk would refuse came of it: no describe failed, the recursion
//!    limit's check passes ([`recursion::claims_past`]), no symbol is defined twice, no
//!    dependency's code refers to the workspace's, nothing builds in a patchable function.
//!
//! Why the patched walk is then the whole walk's result. A whole walk with no refusal
//! describes exactly what its check's roots and its own reach (`recursion::claims_past`) and
//! builds what its own roots reach, and each node is a function of its describe, the policy
//! and what it refers to. The patch asks again every function and static whose describe or
//! count may be other ([`Frontend::durable`], [`Frontend::group`]), sets each changed one's
//! edges, describes what newly enters, and lets [`support`] say what the roots reach now:
//! the same items. Then each rule below is evaluated again for every node one of its inputs
//! changed for, found through the graph's reverse edges:
//!
//! - a node's unit, key and settings: its describe and the policy (the changed and the new);
//! - its slot ([`super::rebind`]): its describe, its callees' `patchable`, and the slots of
//!   what it builds in (the changed and the new; one built into another gives way);
//! - its places: its references, and each callee's key, slot and weak symbol (the changed,
//!   the new, and every referrer of a node whose key or slot changed);
//! - its built-in closure: its places at its built-in references, and their closures (the
//!   changed and the new; a changed function is built into no other);
//! - the symbols' and the owners' checks: the nodes added and removed, and the referrers of
//!   a node whose owner changed.

use std::{
    alloc::Global,
    collections::{BTreeMap, BTreeSet, HashMap},
    hash::Hash,
    sync::Arc,
};

use blaze_graph::par;
use blaze_ids::ContentId;

use super::{
    Build, CheckRoot, FrontInstance, FrontStatic, Node, Planted, Sorted, StaticId, StaticOf, Walk,
    Work,
    callee, kept, recursion,
    support::{self, ALL, BUILT},
};
use crate::{
    InstanceKey, Linkage, Owner, Target,
    backend::{Counted, Described, DescribedStatic, FrontRef, Frontend, Instantiates, Prepare},
    plan::Origin,
};

/// When a function's describe and count are asked again.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Hangs {
    /// Never while the durable view is the kept one ([`Frontend::durable`]).
    Durable,
    /// When an input of this group was written ([`Frontend::group`]).
    Group(u64),
    /// Each build.
    Loose,
}

/// A function of a kept walk, as one origin's code asks for it.
struct FunctionItem<I, S> {
    asked: (I, Origin),
    /// The walk's own describe of it, never read back from the describes kept for every
    /// program, which another program's build may have replaced since.
    described: Arc<Described<I, S>>,
    key: InstanceKey,
    counted: Counted,
    hangs: Hangs,
}

/// A static of a kept walk.
struct StaticItem<I, S> {
    value: S,
    described: DescribedStatic<I, S>,
    id: StaticId,
}

enum Item<I, S> {
    Function(FunctionItem<I, S>),
    Static(StaticItem<I, S>),
}

/// What a kept walk holds beside its nodes: what finds the changed set and what a change
/// touches.
pub(super) struct Keep<I, S> {
    /// Every item the walk described, by its place in `graph`; `None` for a place given up.
    items: Vec<Option<Item<I, S>>>,
    functions: HashMap<(I, Origin), u32>,
    values: HashMap<S, u32>,
    /// Each static described, by which static it is.
    ids: HashMap<StaticId, u32>,
    /// How many describes each key is of.
    askers: blaze_hash::HashMap<InstanceKey, u32, Global>,
    /// What each item refers to, and what the roots reach of it.
    graph: support::Graph,
    /// The functions asked again each build, sorted.
    loose: Vec<u32>,
    /// The functions asked again when their group is written, by group, each list sorted.
    // BTreeMap: the groups are asked in one order every build.
    groups: BTreeMap<u64, Vec<u32>>,
    /// Every static, sorted: asked again each build.
    statics: Vec<u32>,
    /// The check's roots, in its order ([`recursion`]).
    check_roots: Vec<u32>,
    /// Every function some node builds in. A function taken out stays: it then gives way
    /// where it need not, never the other way.
    built_into: blaze_hash::HashSet<InstanceKey, Global>,
    /// How many functions instantiate each body, and whether it is drop glue.
    bodies: HashMap<ContentId, (u32, bool)>,
    /// How many functions are held to each limit.
    // BTreeMap: the least limit is its first key.
    limits: BTreeMap<u32, u32>,
    /// How many bodies have more instances than the least limit admits on one path
    /// ([`hot`]): with none, no order of claims places an instance past a limit.
    hot: usize,
    /// How many nodes and statics define each symbol: one each, in a walk with no error.
    symbols: HashMap<String, u32>,
}

/// The walk kept of one program: its inputs, its result, and its [`Keep`].
pub(super) struct Walked<I, S> {
    roots_in: Vec<(I, Origin)>,
    static_roots_in: Vec<S>,
    check_in: Vec<CheckRoot<I, S>>,
    policy: ContentId,
    /// The front end's durable view and its view ([`Frontend::view`]) from before the walk.
    view: u64,
    seen: Option<u64>,
    /// Shared with the build's `Later`: a patch copies the chunks it changes.
    nodes: Arc<Sorted<InstanceKey, super::FnNode<I, S>>>,
    statics: Sorted<StaticId, super::StaticNode<I, S>>,
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

/// What a patched walk changed of the walk before it: what every pass after the walk does
/// again, and nothing else ([`super::lists`]). Each list sorted, each key once; no key is in
/// two.
#[derive(Default, Debug)]
pub(super) struct Touched {
    /// Functions the walk holds whose node is another: made again, new, or its places name
    /// another key.
    pub(super) functions: Vec<InstanceKey>,
    /// Keys the walk held and no longer holds.
    pub(super) removed: Vec<InstanceKey>,
    pub(super) statics: Vec<StaticId>,
    pub(super) removed_statics: Vec<StaticId>,
}

/// A walk's result with what keeps it.
pub(super) struct Patched<B: Prepare> {
    pub(super) nodes: Sorted<InstanceKey, Node<B>>,
    pub(super) statics: Sorted<StaticId, StaticOf<B>>,
    pub(super) roots: Vec<InstanceKey>,
    pub(super) walk: Walk,
    pub(super) keep: Keep<FrontInstance<B>, FrontStatic<B>>,
    pub(super) touched: Touched,
    pub(super) work: Work,
}

/// Whether a function described as `now` is in the walk what one described as `was` is, but
/// for its code, its key and what it refers to: every field its slot, its callers' places
/// and the symbols are made from. What may differ is what its own node and row hold: its
/// code hashes, arguments, anchors, name, owner, identity, bound, coldness, and its
/// references and built-in places, whose edges the patch sets again.
fn same_frame<I: PartialEq, S: PartialEq>(was: &Described<I, S>, now: &Described<I, S>) -> bool {
    was.item == now.item
        && was.origin == now.origin
        && was.editable == now.editable
        && was.over_editable == now.over_editable
        && was.overflow_checks == now.overflow_checks
        && was.address == now.address
        && was.symbol == now.symbol
        && was.linkage == now.linkage
}

/// Whether a body of `count` instances can be past the limit `least` on some path: an
/// instance's depth is how many instances of its body are above it on its path, at most all
/// the others, and drop glue counts a quarter (`recursion::Paths::place`).
fn hot(count: u32, drop_glue: bool, least: Option<u32>) -> bool {
    let Some(least) = least else { return false };
    let depth = count.saturating_sub(1);
    (if drop_glue { depth / 4 } else { depth }) > least
}

fn sorted_insert(list: &mut Vec<u32>, item: u32) {
    if let Err(at) = list.binary_search(&item) {
        list.insert(at, item);
    }
}

fn sorted_remove(list: &mut Vec<u32>, item: u32) {
    if let Ok(at) = list.binary_search(&item) {
        list.remove(at);
    }
}

impl<I: Clone + Eq + Hash, S: Clone + Eq + Hash> Keep<I, S> {
    fn function(&self, item: u32) -> Option<&FunctionItem<I, S>> {
        match self.items.get(item as usize)?.as_ref()? {
            Item::Function(it) => Some(it),
            Item::Static(_) => None,
        }
    }

    fn least(&self) -> Option<u32> {
        self.limits.keys().next().copied()
    }

    fn tally(&mut self, body: Instantiates, add: bool) {
        let held = self.bodies.entry(body.body).or_insert((0, body.drop_glue));
        held.0 = if add { held.0 + 1 } else { held.0.saturating_sub(1) };
        if held.0 == 0 {
            self.bodies.remove(&body.body);
        }
    }

    /// Counts a function of `counted` in (`add`) or out of the bodies and limits, keeping
    /// [`Self::hot`].
    fn count(&mut self, counted: &Counted, add: bool) {
        let before = self.least();
        if let Some(limit) = counted.limit {
            let held = self.limits.entry(limit.value).or_insert(0);
            *held = if add { *held + 1 } else { held.saturating_sub(1) };
            if *held == 0 {
                self.limits.remove(&limit.value);
            }
        }
        let least = self.least();
        let is_hot = |bodies: &HashMap<ContentId, (u32, bool)>, body: &ContentId| {
            bodies.get(body).is_some_and(|(count, glue)| hot(*count, *glue, least))
        };
        match counted.body {
            // Another least limit moves every body's bound: counted again, once a change of
            // the least `#![recursion_limit]` of the program.
            _ if least != before => {
                if let Some(body) = counted.body {
                    self.tally(body, add);
                }
                self.hot = self.bodies.values().filter(|it| hot(it.0, it.1, least)).count();
            }
            Some(body) => {
                let was = is_hot(&self.bodies, &body.body);
                self.tally(body, add);
                let is = is_hot(&self.bodies, &body.body);
                self.hot = self.hot + usize::from(is) - usize::from(was);
            }
            None => {}
        }
    }

    /// Files function `item` under when it is asked again, or takes it out.
    fn hang(&mut self, item: u32, hangs: Hangs, add: bool) {
        let list = match hangs {
            Hangs::Durable => return,
            Hangs::Loose => &mut self.loose,
            // alloc: once per group of inputs a walk's functions hang on
            Hangs::Group(group) => self.groups.entry(group).or_default(),
        };
        match add {
            true => sorted_insert(list, item),
            false => sorted_remove(list, item),
        }
    }

    /// Counts a definition of `symbol` in or out; `false` when it is then defined twice.
    fn define(&mut self, symbol: Option<&str>, add: bool) -> bool {
        let Some(symbol) = symbol else { return true };
        match (add, self.symbols.get_mut(symbol)) {
            (true, Some(held)) => {
                *held += 1;
                false
            }
            (true, None) => {
                // alloc: once per symbol a walk defines
                self.symbols.insert(symbol.to_owned(), 1);
                true
            }
            (false, Some(held)) if *held > 1 => {
                *held -= 1;
                true
            }
            (false, _) => {
                self.symbols.remove(symbol);
                true
            }
        }
    }

    /// The items `refs`, of code asked for by `origin`, name, in order. One the walk holds
    /// no item of gets a place in the graph and goes to `fresh`, to be described.
    fn outs(
        &mut self,
        refs: &[FrontRef<I, S>],
        origin: Origin,
        fresh: &mut Vec<(u32, Fresh<I, S>)>,
    ) -> Vec<u32> {
        // alloc: per item whose edges are set, one per reference
        let mut outs = Vec::with_capacity(refs.len());
        for it in refs {
            match it {
                FrontRef::Function(callee) => {
                    // clone: the walk's handle of a callee; who asks for it is a copy
                    let asked = (callee.clone(), origin);
                    let item = match self.functions.get(&asked) {
                        Some(item) => *item,
                        None => {
                            let item = self.place();
                            // clone: the handle of a function new to the walk, its map's key
                            self.functions.insert(asked.clone(), item);
                            fresh.push((item, Fresh::Function(asked)));
                            item
                        }
                    };
                    outs.push(item);
                }
                FrontRef::Static(value) => {
                    let item = match self.values.get(value) {
                        Some(item) => *item,
                        None => {
                            let item = self.place();
                            // clone: the handle of a static new to the walk, its map's key
                            self.values.insert(value.clone(), item);
                            fresh.push((item, Fresh::Static(value.clone()))); // clone: as above
                            item
                        }
                    };
                    outs.push(item);
                }
                FrontRef::Extern(_) | FrontRef::Weak(_) => {}
            }
        }
        outs
    }

    /// A place in the graph with no item yet.
    fn place(&mut self) -> u32 {
        let item = self.graph.add();
        if self.items.len() <= item as usize {
            self.items.resize_with(item as usize + 1, || None);
        }
        item
    }
}

/// An item new to a walk, not yet described.
enum Fresh<I, S> {
    Function((I, Origin)),
    Static(S),
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
        if self.frontend.stopped() {
            return Err("the build stopped before its walk");
        }
        let Walked { nodes, mut statics, mut keep, seen, .. } = walked;
        // clone: the directory of the kept walk's nodes, when the build that made it lives
        let mut nodes = Arc::try_unwrap(nodes).unwrap_or_else(|shared| (*shared).clone());
        let mut work = Work::default();

        // Who is asked: the functions asked each build, and those of each group an input
        // of which was written since the kept walk.
        // clone: per build, the functions it asks of
        let mut asks = keep.loose.clone();
        for (group, members) in &keep.groups {
            work.groups += 1;
            if seen.is_none_or(|seen| self.frontend.written_since(*group, seen)) {
                asks.extend_from_slice(members);
            }
        }
        asks.sort_unstable();
        work.asked = asks.len() + keep.statics.len();
        // Each one's describe and count, on all cores. A describe that fails is the whole
        // walk's to report.
        type Answer<I, S> = Option<(Option<Arc<Described<I, S>>>, Counted)>;
        let answers: Vec<Result<Answer<_, _>, &'static str>> = par::map(&asks, |item| {
            let Some(it) = keep.function(*item) else {
                return Err("a kept walk asks of a function it does not hold");
            };
            let now = match self.asks {
                false => None,
                true => match self.frontend.redescribe(&it.asked.0, &it.asked.1, &it.described) {
                    Ok(None) => None,
                    Ok(Some(now)) => {
                        // alloc: once per describe, shared by the walk's node and the walk kept
                        let now = Arc::new(now);
                        kept.keep(0, &it.asked, now.clone()); // clone: a reference count
                        Some(now)
                    }
                    Err(_) => return Err("a describe failed"),
                },
            };
            let counted = self.frontend.counted(std::slice::from_ref(&it.asked));
            let &[counted] = counted.as_slice() else {
                return Err("the front end gave no count of a function");
            };
            Ok((now.is_some() || counted != it.counted).then_some((now, counted)))
        });
        // alloc: per build, the statics whose describe is another
        let mut changed_statics = Vec::new();
        for &item in &keep.statics {
            let Some(Some(Item::Static(it))) = keep.items.get(item as usize) else {
                return Err("a kept walk asks of a static it does not hold");
            };
            match self.frontend.describe_static(&it.value) {
                Ok(now) if now == it.described => {}
                Ok(now) => changed_statics.push((item, now)),
                Err(_) => return Err("a static's describe failed"),
            }
        }

        // The changed set. Each changed item takes its new describe and its edges are set
        // again; what it newly refers to goes to `fresh`.
        // alloc: per build, what it changed
        let mut fresh: Vec<(u32, Fresh<FrontInstance<B>, FrontStatic<B>>)> = Vec::new();
        // Each changed function with its old key and its node made again.
        let mut remade: Vec<(u32, InstanceKey, Node<B>)> = Vec::new();
        // The items whose own node is made: changed, or new to the build.
        let mut own: BTreeSet<u32> = BTreeSet::new();
        // Items whose node is to be made again or resolved again.
        // BTreeSet: the passes below go over them in one order every build.
        let mut dirty: BTreeSet<u32> = BTreeSet::new();
        // Items whose owner is another: their referrers are checked again.
        let mut owner_changed: Vec<u32> = Vec::new();
        // alloc: per build, the functions it tells the front end it reached
        let mut told: Vec<(FrontInstance<B>, Origin)> = Vec::new();
        let (mut structural, mut recount) = (false, false);
        // Kept describes the front end said changed.
        let mut redescribed = 0;
        for (item, answer) in asks.iter().copied().zip(answers) {
            let Some((now, counted)) = answer? else { continue };
            let Some(Some(Item::Function(it))) = keep.items.get(item as usize) else {
                return Err("a kept walk asks of a function it does not hold");
            };
            let (was, old, was_counted) = (it.described.clone(), it.key, it.counted); // clone: a count
            let asked = it.asked.clone(); // clone: the walk's handle, once a change
            if counted != was_counted {
                keep.count(&was_counted, false);
                keep.count(&counted, true);
                recount = true;
            }
            let Some(now) = now else {
                if let Some(Some(Item::Function(it))) = keep.items.get_mut(item as usize) {
                    it.counted = counted;
                }
                continue;
            };
            work.described += 1;
            redescribed += 1;
            if !same_frame(&was, &now) {
                return Err("a changed function is another's or is defined another way");
            }
            let Ok(made) = self.node(&asked.0, asked.1, now.clone()) else {
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
            keep.askers.remove(&old);
            keep.askers.insert(new, 1);
            if was.owner != now.owner {
                owner_changed.push(item);
            }
            // The planted bug of a control keeps a changed function's old edges and places.
            if self.checks_shape && (was.refs != now.refs || was.built_in != now.built_in) {
                let outs = keep.outs(&now.refs, now.origin, &mut fresh);
                keep.graph.set_outs(item, outs);
                structural = true;
            }
            if let Some(Some(Item::Function(it))) = keep.items.get_mut(item as usize) {
                (it.described, it.key, it.counted) = (now, new, counted);
            }
            told.push(asked);
            remade.push((item, old, made));
            dirty.insert(item);
            own.insert(item);
        }
        for (item, now) in changed_statics {
            let Some(Some(Item::Static(it))) = keep.items.get(item as usize) else {
                return Err("a kept walk asks of a static it does not hold");
            };
            let was = &it.described;
            if (was.item, was.instance, was.origin, &was.symbol, was.linkage)
                != (now.item, now.instance, now.origin, &now.symbol, now.linkage)
            {
                return Err("a changed static is another or is defined another way");
            }
            if was.owner != now.owner {
                owner_changed.push(item);
            }
            let refs_changed = was.refs != now.refs;
            work.described += 1;
            if refs_changed {
                let outs = keep.outs(&now.refs, now.origin, &mut fresh);
                keep.graph.set_outs(item, outs);
                structural = true;
            }
            if let Some(Some(Item::Static(it))) = keep.items.get_mut(item as usize) {
                it.described = now;
            }
            dirty.insert(item);
        }

        // What newly enters is described, a round at a time, each round on all cores: what
        // a new item refers to may be new too.
        let counts = kept::Counts::default();
        // Each new function's node, until the sets say whether it is built.
        let mut made_new: HashMap<u32, Node<B>> = HashMap::new();
        while !fresh.is_empty() {
            if self.frontend.stopped() {
                return Err("the build stopped in its walk");
            }
            let round = std::mem::take(&mut fresh);
            // alloc: per round of new items, their handles
            let functions: Vec<(u32, (FrontInstance<B>, Origin))> = (round.iter())
                .filter_map(|(item, it)| match it {
                    // clone: the walk's handle of a new function
                    Fresh::Function(asked) => Some((*item, asked.clone())),
                    Fresh::Static(_) => None,
                })
                .collect();
            let described = par::map(&functions, |(_, asked)| {
                let described = self.described(asked, 0, &counts)?;
                let node = self.node(&asked.0, asked.1, described)?;
                let counted = self.frontend.counted(std::slice::from_ref(asked));
                let hangs = match self.frontend.durable(&asked.0, &asked.1) {
                    true => Hangs::Durable,
                    false => match self.frontend.group(&asked.0, &asked.1) {
                        Some(group) => Hangs::Group(group),
                        None => Hangs::Loose,
                    },
                };
                Ok::<_, crate::CodegenError>((node, counted, hangs))
            });
            for ((item, asked), made) in functions.into_iter().zip(described) {
                let Ok((node, counted, hangs)) = made else {
                    return Err("a describe of a new function failed");
                };
                let &[counted] = counted.as_slice() else {
                    return Err("the front end gave no count of a new function");
                };
                let key = node.unit.instance.key();
                if keep.askers.contains_key(&key) {
                    return Err("a new function has a key the walk holds");
                }
                keep.askers.insert(key, 1);
                keep.count(&counted, true);
                keep.hang(item, hangs, true);
                let described = node.described.clone(); // clone: a reference count
                let outs = keep.outs(&described.refs, described.origin, &mut fresh);
                keep.graph.set_outs(item, outs);
                told.push(asked.clone()); // clone: the walk's handle of a new function
                keep.items[item as usize] =
                    Some(Item::Function(FunctionItem { asked, described, key, counted, hangs }));
                made_new.insert(item, node);
                work.described += 1;
            }
            for (item, it) in round {
                let Fresh::Static(value) = it else { continue };
                let Ok(described) = self.frontend.describe_static(&value) else {
                    return Err("a describe of a new static failed");
                };
                let id = (described.item, described.instance);
                if keep.ids.insert(id, item).is_some() {
                    return Err("a new static is one the walk holds under another handle");
                }
                let outs = keep.outs(&described.refs, described.origin, &mut fresh);
                keep.graph.set_outs(item, outs);
                sorted_insert(&mut keep.statics, item);
                keep.items[item as usize] = Some(Item::Static(StaticItem { value, described, id }));
                work.described += 1;
            }
            structural = true;
        }
        self.frontend.reached(&told, &[]);

        // What the roots reach now.
        let visited = keep.graph.visited;
        let settled = keep.graph.settle();
        work.graph = keep.graph.visited - visited;
        if structural || recount {
            // The recursion limit: with no body of more instances than the least limit
            // admits, no order of claims refuses one; else rustc's walk, over the whole
            // graph, which is all described. The planted bug of a control runs none.
            if keep.hot > 0 && self.planted != Some(Planted::SkippedClaims) {
                let graph = &keep.graph;
                let (past, claims) = recursion::claims_past(
                    keep.items.len(),
                    &keep.check_roots,
                    |item| graph.outs(item),
                    |item| keep.function(item).map(|it| it.counted),
                );
                work.claims = claims;
                if past {
                    return Err("the recursion limit's check may refuse an instance");
                }
            }
        }

        // The nodes: what left the build goes, what a change made again is made again, and
        // what entered the build is made.
        let mut touched = Touched::default();
        // Each changed function that is still built, with whether it was rebound.
        let mut slots_before: Vec<(u32, bool)> = Vec::new();
        let old_key = |remade: &[(u32, InstanceKey, Node<B>)], item: u32| {
            remade.iter().find(|it| it.0 == item).map(|it| it.1)
        };
        for &item in &settled.left[BUILT] {
            made_new.remove(&item);
            match keep.items.get(item as usize).and_then(Option::as_ref) {
                Some(Item::Function(it)) => {
                    let key = old_key(&remade, item).unwrap_or(it.key);
                    // A changed one's count of describes is under its new key already.
                    if keep.askers.get(&it.key) != Some(&1) {
                        return Err("a function of a key of two describes left the build");
                    }
                    if let Some(node) = nodes.remove(&key) {
                        keep.define(node.described.symbol.as_deref(), false);
                        touched.removed.push(key);
                    }
                }
                Some(Item::Static(it)) => {
                    let id = it.id;
                    if let Some(node) = statics.remove(&id) {
                        keep.define(node.described.symbol.as_deref(), false);
                        touched.removed_statics.push(id);
                    }
                }
                None => {}
            }
            dirty.remove(&item);
        }
        for &item in &settled.left[ALL] {
            match keep.items.get_mut(item as usize).and_then(Option::take) {
                Some(Item::Function(it)) => {
                    // Another describe of its key stays: which of them the key's node holds
                    // is the whole walk's to say.
                    if keep.askers.remove(&it.key) != Some(1) {
                        return Err("a function of a key of two describes left the walk");
                    }
                    keep.functions.remove(&it.asked);
                    keep.count(&it.counted, false);
                    keep.hang(item, it.hangs, false);
                }
                Some(Item::Static(it)) => {
                    keep.values.remove(&it.value);
                    keep.ids.remove(&it.id);
                    sorted_remove(&mut keep.statics, item);
                }
                None => {}
            }
        }
        for (item, old, made) in remade {
            // Described for the recursion limit's check alone, or no longer built: no node.
            if !keep.graph.is_member(BUILT, item) || settled.entered[BUILT].contains(&item) {
                made_new.insert(item, made);
                continue;
            }
            let Some(was) = nodes.remove(&old) else {
                return Err("a kept walk's function has no node");
            };
            if was.patchable != made.patchable
                || was.then.is_some() != made.then.is_some()
                || was.rebound_own != made.rebound_own
            {
                return Err("a changed function is reached another way");
            }
            let new = made.unit.instance.key();
            // Until the passes below say otherwise, what it builds in and its places are the
            // kept ones. Its slot is decided again, from what its own describe says.
            let mut node = made;
            slots_before.push((item, was.rebound));
            (node.built_in, node.refs) = (was.built_in, was.refs);
            node.least_origin = node.described.origin;
            if nodes.insert(new, node).is_some() {
                return Err("a changed function took a key the walk holds");
            }
            if new != old {
                touched.removed.push(old);
                // Every place that named it names its new key: its referrers' places are
                // made again. The planted bug of a control leaves them.
                if self.renames {
                    dirty.extend(keep.graph.ins(item).iter().copied());
                }
            }
        }
        for &item in &settled.entered[BUILT] {
            match keep.items.get(item as usize).and_then(Option::as_ref) {
                Some(Item::Function(it)) => {
                    let node = match made_new.remove(&item) {
                        Some(node) => node,
                        // Described before, for the check alone: built from now on.
                        // clone: a reference count
                        None => match self.node(&it.asked.0, it.asked.1, it.described.clone()) {
                            Ok(node) => node,
                            Err(_) => return Err("a function has no unit under the policy"),
                        },
                    };
                    let symbol = node.described.symbol.clone(); // clone: a new node's name
                    if nodes.insert(it.key, node).is_some() {
                        return Err("a function entered the build under a key the walk holds");
                    }
                    if !keep.define(symbol.as_deref(), true) {
                        return Err("a symbol is defined twice");
                    }
                }
                Some(Item::Static(it)) => {
                    // clone: the describe of a static new to the build, its node's
                    let Ok(node) = self.static_node(&it.value, it.described.clone()) else {
                        return Err("a static has no settings under the policy");
                    };
                    let symbol = node.described.symbol.clone(); // clone: a new static's name
                    statics.insert(it.id, node);
                    if !keep.define(symbol.as_deref(), true) {
                        return Err("a symbol is defined twice");
                    }
                }
                None => return Err("an item entered the build undescribed"),
            }
            dirty.insert(item);
            own.insert(item);
        }
        // Only what is built has a node to make again.
        dirty.retain(|item| keep.graph.is_member(BUILT, *item));
        // A changed static's node takes its new describe.
        for &item in &dirty {
            if let Some(Some(Item::Static(it))) = keep.items.get(item as usize)
                && let Some(node) = statics.get_mut(&it.id)
                && node.described != it.described
            {
                node.described = it.described.clone(); // clone: a changed static's describe
            }
        }

        // The slots ([`super::rebind`]'s rule) of the nodes made again and the new ones, each
        // from what its own describe says: a round marks one more or is the last, so the
        // rounds are at most one a node and one more. Every other node's slot is the kept
        // one: it reads its callees' `patchable`, which a changed function keeps, and the
        // slot of what it builds in, which gives way below when it is another.
        let mut resolve: BTreeSet<u32> = dirty.clone(); // clone: per build, its changed set
        for round in 0..=own.len() {
            let mut moved = false;
            for &item in &own {
                let Some(it) = keep.function(item) else { continue };
                let Some(node) = nodes.get(&it.key) else { continue };
                work.classified += 1;
                if node.patchable || node.rebound || !self.rebinds {
                    continue;
                }
                let key_of = |place: usize| -> Option<InstanceKey> {
                    let FrontRef::Function(callee) = node.described.refs.get(place)? else {
                        return None;
                    };
                    // clone: the walk's handle of a callee; who asks for it is a copy
                    let asked = (callee.clone(), node.described.origin);
                    keep.function(*keep.functions.get(&asked)?).map(|it| it.key)
                };
                let refers = (0..node.described.refs.len()).any(|place| {
                    key_of(place).and_then(|key| nodes.get(&key)).is_some_and(|it| it.patchable)
                }) || node.described.built_in.iter().any(|place| {
                    key_of(*place as usize).and_then(|key| nodes.get(&key)).is_some_and(|it| it.rebound)
                });
                if !refers {
                    continue;
                }
                let key = it.key;
                nodes.get_mut(&key).expect("looked up above").rebound = true;
                moved = true;
            }
            if !moved {
                break;
            }
            if round == own.len() {
                return Err("the slots of the changed functions found no fixed point");
            }
        }
        for (item, was) in slots_before {
            let Some(it) = keep.function(item) else { continue };
            if nodes.get(&it.key).is_none_or(|node| node.rebound == was) {
                continue;
            }
            // What builds it in reads its slot, at any depth: the whole walk's to say.
            if keep.built_into.contains(&it.key) {
                return Err("a function built into another is reached another way");
            }
            // Its callers reach it another way: their places are made again.
            resolve.extend(keep.graph.ins(item).iter().copied());
        }
        resolve.retain(|item| keep.graph.is_member(BUILT, *item));

        // The places of every node whose references, or a callee's key or slot, changed.
        for &item in &resolve {
            let (refs, origin) = match keep.items.get(item as usize).and_then(Option::as_ref) {
                Some(Item::Function(it)) => (&it.described.refs, it.described.origin),
                Some(Item::Static(it)) => (&it.described.refs, it.described.origin),
                None => return Err("a kept walk refers from an item it does not hold"),
            };
            work.resolved += 1;
            let places: Option<Vec<Target>> = (refs.iter())
                .map(|it| match it {
                    FrontRef::Function(callee) => {
                        // clone: the walk's handle of a callee; who asks for it is a copy
                        let asked = (callee.clone(), origin);
                        let key = keep.function(*keep.functions.get(&asked)?)?.key;
                        let node = nodes.get(&key)?;
                        Some(match (&node.described.symbol, node.described.linkage) {
                            // clone: a reference to a weakly defined function
                            (Some(name), Linkage::Weak) => Target::Extern(name.clone()),
                            _ if node.slotted() || node.then.is_some() => Target::Function(key),
                            _ => Target::Direct(key),
                        })
                    }
                    FrontRef::Static(value) => {
                        let Some(Item::Static(it)) =
                            keep.items.get(*keep.values.get(value)? as usize)?.as_ref()
                        else {
                            return None;
                        };
                        let node = statics.get(&it.id)?;
                        Some(match (&node.described.symbol, node.described.linkage) {
                            // clone: a reference to a weakly defined static
                            (Some(name), Linkage::Weak) => Target::Extern(name.clone()),
                            _ => Target::Static { item: it.id.0, instance: it.id.1 },
                        })
                    }
                    // clone: the item's own target, kept by its node
                    FrontRef::Extern(name) => Some(Target::Extern(name.clone())),
                    // clone: the item's own target, kept by its node
                    FrontRef::Weak(name) => Some(Target::Weak(name.clone())),
                })
                // alloc: per item whose places are made again, its places
                .collect();
            let Some(places) = places else {
                return Err("a patched item refers to what the walk does not build");
            };
            match keep.items.get(item as usize).and_then(Option::as_ref) {
                Some(Item::Function(it)) => {
                    let Some(node) = nodes.get_mut(&it.key) else {
                        return Err("a kept walk's function has no node");
                    };
                    if !self.checks_shape && node.refs.len() == places.len() {
                        // The planted bug: only the keys of its old places are renamed.
                        for (place, now) in node.refs.iter_mut().zip(places) {
                            if callee(place).is_some() && callee(&now).is_some() {
                                *place = now;
                            }
                        }
                    } else {
                        node.refs = places;
                    }
                    // The planted bug of a control keeps the function a referrer was made
                    // of, though a place of it names another key.
                    if self.planted != Some(Planted::KeptTemplates) || own.contains(&item) {
                        touched.functions.push(it.key);
                    }
                }
                Some(Item::Static(it)) => {
                    let Some(node) = statics.get_mut(&it.id) else {
                        return Err("a kept walk's static has no node");
                    };
                    node.refs = places;
                    touched.statics.push(it.id);
                }
                None => {}
            }
        }

        // What each changed or new function builds in, at any depth: its places at its
        // built-in references, then theirs.
        let direct = |nodes: &Sorted<InstanceKey, Node<B>>, key: &InstanceKey| {
            let node = nodes.get(key)?;
            (node.described.built_in.iter())
                .map(|at| node.refs.get(*at as usize).and_then(callee))
                // alloc: per function whose built-in closure is made, what it builds in
                .collect::<Option<Vec<InstanceKey>>>()
        };
        for &item in &dirty {
            let Some(it) = keep.function(item) else { continue };
            let key = it.key;
            let Some(node) = nodes.get(&key) else { continue };
            if node.described.built_in.is_empty() && node.built_in.is_empty() {
                continue;
            }
            work.built_in += 1;
            // alloc: per function that builds code in, its closure
            let (mut seen, mut queue): (Vec<InstanceKey>, Vec<InstanceKey>) = (Vec::new(), Vec::new());
            let Some(first) = direct(&nodes, &key) else {
                return Err("a function builds in a place that is no function of its references");
            };
            queue.extend(first);
            while let Some(at) = queue.pop() {
                if at == key || seen.contains(&at) {
                    continue;
                }
                if nodes.get(&at).is_none_or(|it| it.patchable) {
                    return Err("a function builds in one an edit patches alone");
                }
                seen.push(at);
                let Some(more) = direct(&nodes, &at) else {
                    return Err("a function builds in a place that is no function of its references");
                };
                queue.extend(more);
            }
            seen.sort_unstable();
            keep.built_into.extend(seen.iter().copied());
            nodes.get_mut(&key).expect("looked up above").built_in = seen;
        }

        // A dependency's code never refers to the workspace's (`owners`): checked again for
        // each node made again and each referrer of one whose owner is another.
        // alloc: per build, the items the owners' check looks at again
        let mut checked: BTreeSet<u32> = resolve.clone(); // clone: per build, its changed set
        for item in owner_changed {
            checked.extend(keep.graph.ins(item).iter().copied());
        }
        for &item in &checked {
            if !keep.graph.is_member(BUILT, item) {
                continue;
            }
            work.owners += 1;
            let (owner, refs) = match keep.items.get(item as usize).and_then(Option::as_ref) {
                Some(Item::Function(it)) => match nodes.get(&it.key) {
                    Some(node) => (node.described.owner, &node.refs),
                    None => continue,
                },
                Some(Item::Static(it)) => match statics.get(&it.id) {
                    Some(node) => (node.described.owner, &node.refs),
                    None => continue,
                },
                None => continue,
            };
            if owner != Owner::Dependency {
                continue;
            }
            let workspace = refs.iter().any(|target| match target {
                Target::Function(key) | Target::Direct(key) => {
                    nodes.get(key).is_some_and(|it| it.described.owner == Owner::Workspace)
                }
                Target::Static { item, instance } => (statics.get(&(*item, *instance)))
                    .is_some_and(|it| it.described.owner == Owner::Workspace),
                _ => false,
            });
            if workspace {
                return Err("a dependency's code refers to the workspace's");
            }
        }

        // A function whose node was made again and whose places were not: its row is
        // another all the same.
        for &item in &dirty {
            if let Some(it) = keep.function(item)
                && nodes.contains(&it.key)
                && (self.planted != Some(Planted::KeptTemplates) || own.contains(&item))
            {
                touched.functions.push(it.key);
            }
            if let Some(Some(Item::Static(it))) = keep.items.get(item as usize)
                && statics.contains(&it.id)
            {
                touched.statics.push(it.id);
            }
        }
        touched.functions.sort_unstable();
        touched.functions.dedup();
        touched.statics.sort_unstable();
        touched.statics.dedup();
        touched.removed.retain(|key| !nodes.contains(key));
        touched.removed.sort_unstable();
        touched.removed.dedup();
        touched.removed_statics.retain(|id| !statics.contains(id));
        touched.removed_statics.sort_unstable();
        touched.removed_statics.dedup();

        let Some(roots) = (roots.iter())
            .map(|asked| keep.function(*keep.functions.get(asked)?).map(|it| it.key))
            // alloc: per build, sized by its roots
            .collect::<Option<Vec<InstanceKey>>>()
        else {
            return Err("a kept walk holds no item of a root");
        };
        let (described, verified) = counts.get();
        let visited = touched.functions.len() + touched.statics.len();
        let walk = Walk {
            described: redescribed + described,
            verified: asks.len() - redescribed + verified,
            reached: nodes.len(),
            dropped: 0,
            fresh: None,
            whole: None,
            visited,
        };
        Ok(Patched { nodes, statics, roots, walk, keep, touched, work })
    }

    /// What keeps a whole walk that had no error: every item it described with the graph of
    /// what each refers to, what the roots reach of it, and when each function is asked
    /// again. Linear in the walk, once per whole walk. `None` when the walk's own tables do
    /// not agree with what its roots reach: such a walk is not kept, and the next runs whole.
    pub(super) fn keep_of(
        &self,
        nodes: &Sorted<InstanceKey, Node<B>>,
        statics: &Sorted<StaticId, StaticOf<B>>,
        side: Side<FrontInstance<B>, FrontStatic<B>>,
        roots: &[(FrontInstance<B>, Origin)],
        static_roots: &[FrontStatic<B>],
        check: &[CheckRoot<FrontInstance<B>, FrontStatic<B>>],
    ) -> Option<Keep<FrontInstance<B>, FrontStatic<B>>> {
        let mut keep = Keep {
            // alloc: per whole walk, one per item it described
            items: Vec::with_capacity(side.described.len() + side.statics.len()),
            functions: HashMap::with_capacity(side.described.len()),
            values: HashMap::with_capacity(side.statics.len()),
            ids: HashMap::with_capacity(side.statics.len()),
            askers: blaze_hash::HashMap::with_capacity_in(side.asked.len(), Global),
            graph: support::Graph::default(),
            loose: Vec::new(), // alloc: per whole walk, the functions asked each build
            groups: BTreeMap::new(),
            statics: Vec::new(), // alloc: per whole walk, its statics
            check_roots: Vec::new(), // alloc: per whole walk, its check's roots
            built_into: blaze_hash::HashSet::new_in(Global),
            bodies: HashMap::new(),
            limits: BTreeMap::new(),
            hot: 0,
            symbols: HashMap::new(),
        };
        // alloc: per whole walk, one per count
        let counted: HashMap<&(FrontInstance<B>, Origin), Counted> =
            side.counted.iter().map(|(asked, counted)| (asked, *counted)).collect();
        // Asked once a function, after its describe: when it is asked again.
        let hangs = par::map(&side.described, |(asked, _)| {
            match self.frontend.durable(&asked.0, &asked.1) {
                true => Hangs::Durable,
                false => match self.frontend.group(&asked.0, &asked.1) {
                    Some(group) => Hangs::Group(group),
                    None => Hangs::Loose,
                },
            }
        });
        for ((asked, described), hangs) in side.described.iter().zip(hangs) {
            let item = keep.place();
            let key = *side.asked.get(asked)?;
            let counted = *counted.get(asked)?;
            *keep.askers.entry(key).or_insert(0) += 1;
            keep.count(&counted, true);
            keep.hang(item, hangs, true);
            // clone: the walk's handle of the function, its map's key and its item's
            keep.functions.insert(asked.clone(), item);
            keep.items[item as usize] = Some(Item::Function(FunctionItem {
                asked: asked.clone(),           // clone: as above
                described: described.clone(),   // clone: a reference count
                key,
                counted,
                hangs,
            }));
        }
        for (value, described) in side.statics {
            let item = keep.place();
            let id = (described.item, described.instance);
            // Two handles of one static: the whole walk keeps the first, and so does it.
            if keep.ids.insert(id, item).is_some() {
                return None;
            }
            keep.values.insert(value.clone(), item); // clone: the walk's handle of the static
            keep.statics.push(item);
            keep.items[item as usize] = Some(Item::Static(StaticItem { value, described, id }));
        }
        // The edges, then the roots: each root brings in what it reaches.
        let mut fresh = Vec::new();
        for item in 0..keep.items.len() as u32 {
            let outs = match keep.items[item as usize].as_ref()? {
                Item::Function(it) => {
                    // clone: a reference count, so its references outlive the borrow
                    let described = it.described.clone();
                    keep.outs(&described.refs, described.origin, &mut fresh)
                }
                Item::Static(it) => {
                    // clone: per whole walk, a static's references
                    let (refs, origin) = (it.described.refs.clone(), it.described.origin);
                    keep.outs(&refs, origin, &mut fresh)
                }
            };
            keep.graph.set_outs(item, outs);
        }
        // A walk with no error described everything it refers to.
        if !fresh.is_empty() {
            return None;
        }
        let function = |keep: &Keep<_, _>, asked: &(FrontInstance<B>, Origin)| {
            keep.functions.get(asked).copied()
        };
        for root in check {
            let item = match root {
                // clone: the walk's handle of a check root, to look it up
                CheckRoot::Function(instance, origin) => function(&keep, &(instance.clone(), *origin))?,
                CheckRoot::Static(value) => *keep.values.get(value)?,
            };
            keep.check_roots.push(item);
        }
        for root in roots {
            let item = function(&keep, root)?;
            keep.check_roots.push(item);
            keep.graph.root(BUILT, item);
        }
        for root in static_roots {
            let item = *keep.values.get(root)?;
            keep.check_roots.push(item);
            keep.graph.root(BUILT, item);
        }
        // Each root once, at its first place: the check's order (`recursion::Graph::start`).
        let mut seen = blaze_hash::HashSet::new_in(Global);
        keep.check_roots.retain(|item| seen.insert(*item));
        for at in 0..keep.check_roots.len() {
            let root = keep.check_roots[at];
            keep.graph.root(ALL, root);
        }
        keep.graph.settle();
        keep.graph.visited = 0;
        // The walk's nodes are what the build's roots reach, and everything described is
        // what the roots reach.
        for item in 0..keep.items.len() as u32 {
            if !keep.graph.is_member(ALL, item) {
                return None;
            }
            let held = match keep.items[item as usize].as_ref()? {
                Item::Function(it) => nodes.contains(&it.key),
                Item::Static(it) => statics.contains(&it.id),
            };
            if keep.graph.is_member(BUILT, item) != held {
                // One key of two describes, one of them the check's alone: its node is
                // built, and the patch gives way at it (`askers`).
                if !held || !matches!(keep.items[item as usize], Some(Item::Function(_))) {
                    return None;
                }
            }
        }
        for node in nodes.values() {
            keep.built_into.extend(node.built_in.iter().copied());
            keep.define(node.described.symbol.as_deref(), true);
        }
        for node in statics.values() {
            keep.define(node.described.symbol.as_deref(), true);
        }
        Some(keep)
    }

    /// Keeps a walk that ended a build well as the walk of `program`, for the next build to
    /// patch. `view` and `seen` are the front end's durable view and view from before the
    /// walk.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn keep_walk(
        &self,
        program: u64,
        roots: &[(FrontInstance<B>, Origin)],
        static_roots: &[FrontStatic<B>],
        check: &[CheckRoot<FrontInstance<B>, FrontStatic<B>>],
        policy: ContentId,
        view: u64,
        seen: Option<u64>,
        nodes: Arc<Sorted<InstanceKey, Node<B>>>,
        statics: Sorted<StaticId, StaticOf<B>>,
        keep: Keep<FrontInstance<B>, FrontStatic<B>>,
    ) {
        let Some(kept) = self.kept else { return };
        let walked = Walked {
            // clone: per build that keeps its walk, its roots
            roots_in: roots.to_vec(),
            static_roots_in: static_roots.to_vec(), // clone: as above
            check_in: check.to_vec(),               // clone: as above
            policy,
            view,
            seen,
            nodes,
            statics,
            keep,
        };
        kept.keep_walk(program, walked);
    }
}
