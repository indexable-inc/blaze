//! What each crate's code does at the toolchain's edge, the facts of blaze-lints' trust lints
//! (docs/v2/plan/design/trust.md): the instances a set of roots reaches outside the toolchain,
//! each with what its own body does and what each call it makes into the toolchain does
//! ([`Done`]), and the calls between them ([`Reach`]). A capability is then the crate's whose
//! code makes the call into the toolchain, or does the thing itself: never the caller's.
//!
//! Read off the one walk's summaries ([`summary_at`], under [`Proofs::None`], the summaries a
//! tier-0 build of a library reads): no new walk. Two tracked queries keep it per instance,
//! keyed as the summary is (the environment and the instance): [`edge`], one instance's
//! [`Done`]s and the instances it goes on to, and [`toolchain_call`], what one call into the
//! toolchain does below it and the code outside the toolchain it calls back (a closure given
//! to `Iterator::map`, a `Display` impl `format!` calls). An edit of a workspace body reruns
//! that body's summary and its own [`edge`]; a dependency's are read back, never redone.
//!
//! What a summary cannot say is said as unseen, never dropped: a body that does not lower or a
//! call the walk can't resolve ([`Rule::Unchecked`]), a foreign function no table knows
//! ([`Rule::ForeignUnknown`]), a call through a `dyn` ([`Rule::Dyn`]), and a toolchain call
//! whose callees pass [`TOOLCHAIN_DESCENT`] instances. The lints count an unseen thing as every
//! capability it could be.
//!
//! The toolchain (core, alloc, std and their kin, and bstd, [`toolchain_crate`]) is trusted:
//! what it does inside is said as the one call into it the crate made.

use std::alloc::Global;

use base_db::{Crate, CrateOrigin, Db};
use blaze_hash::{HashMap, HashSet};
use hir_def::{FunctionId, ItemContainerId, signatures::FunctionSignature};
use hir_ty::{facts::Proofs, traits::StoredParamEnvAndCrate};
use hir_ty_mir::mir::dispatch::Body;

use crate::{
    effects::{EffectSet, Rule},
    function::generic,
    walk::{Def, How, Instance, Summary, def_crate, fn_item_path, name_of, summary_at},
};

/// The libraries outside the toolchain's own crates that are granted as the toolchain is, by
/// crate name: bstd, blaze's standard library, which a blaze-edition crate's `std` is.
pub const TOOLCHAIN_LIBRARIES: &[&str] = &["bstd"];

/// The instances below one call into the toolchain read to say what it does: past this many,
/// the rest is unseen. A bound so a call whose closure over std is the whole of std (a
/// `fmt::Display` over every type std formats) ends, not a guess at a size: std's file and
/// socket calls reach a few hundred instances.
pub const TOOLCHAIN_DESCENT: usize = 1 << 13;

/// The instances one [`reach`] visits: past this many, the instance it stopped at says the rest
/// is unseen. A bound so a graph a generic recursion makes without end ends.
pub const REACHED: usize = 1 << 20;

/// What an instance does at the toolchain's edge: its body itself, or one call it makes into
/// the toolchain (or of a row a table keeps for its callee).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Done {
    /// The kinds it does: what the body does itself, or every kind under the call.
    pub kinds: EffectSet,
    /// The toolchain function called, as a refusal names it (`std::fs::read`): `None` for what
    /// the body does itself.
    pub leaf: Option<Box<str>>,
    /// What it does, said: "calls `std::fs::read`", "reads `COUNT`, a `static mut`".
    pub what: Box<str>,
}

/// One instance a [`reach`] visited.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Node {
    /// As a refusal names it, in backticks: `` `left_pad::phone_home` ``.
    pub name: Box<str>,
    /// The crate whose code it is.
    pub krate: Crate,
    pub done: Box<[Done]>,
    /// The nodes it goes on to (its calls outside the toolchain, and the code a toolchain call
    /// of it calls back), by index in [`Reach::nodes`], in the order of its sites, each once.
    pub calls: Box<[u32]>,
}

/// The instances a set of roots reaches outside the toolchain: the roots first, then the rest
/// in the order a breadth-first walk from them meets them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reach {
    /// How many of [`Reach::nodes`] are roots (the first ones).
    pub roots: u32,
    pub nodes: Box<[Node]>,
}

/// Whether `krate` is the toolchain's or bstd's: what it does is said at the call into it.
pub fn toolchain_crate(db: &Db, krate: Crate) -> bool {
    if matches!(krate.data(db).origin, CrateOrigin::Lang(_)) {
        return true;
    }
    let Some(display) = krate.extra_data(db).display_name.as_ref() else { return false };
    TOOLCHAIN_LIBRARIES.contains(&display.crate_name().as_str())
}

/// The instances `roots` (each in its own where-clauses) reach outside the toolchain, with what
/// each does at the edge. With `within`, only that crate's instances are visited: what each of a
/// crate's functions does itself, whether anything calls it or not.
///
/// Caller: blaze-lints' trust facts (`blaze::trust::facts`), with a workspace crate's functions
/// for what the workspace reaches, and with a dependency's for what its code holds unreached.
pub fn reach(db: &Db, roots: &[FunctionId], within: Option<Crate>) -> Reach {
    let mut nodes: Vec<Node> = Vec::new_in(Global);
    let mut walked: Vec<(StoredParamEnvAndCrate, Instance)> = Vec::new_in(Global);
    let mut index: HashMap<Instance, u32, Global> = HashMap::new_in(Global);
    for &func in roots {
        let foreign = matches!(func.loc(db).container, ItemContainerId::ExternBlockId(_));
        if foreign || !FunctionSignature::of(db, func).has_body() {
            continue;
        }
        let (env, args) = generic(db, func);
        let instance = Instance { def: Def::Body(Body::Fn(func)), args };
        if index.contains_key(&instance) {
            continue;
        }
        let at = u32::try_from(nodes.len()).expect("fewer roots than `u32::MAX`");
        // clone: the index's key, reference counts
        index.insert(instance.clone(), at);
        nodes.push(node_of(db, &instance));
        walked.push((env, instance));
    }
    let roots = u32::try_from(nodes.len()).expect("fewer roots than `u32::MAX`");
    let mut next = 0;
    while next < walked.len() {
        let at = next;
        next += 1;
        if walked.len() >= REACHED {
            let done = Done {
                kinds: EffectSet::one(Rule::Unchecked),
                leaf: None,
                what: format!("is past the first {REACHED} instances blaze follows").into(),
            };
            nodes[at].done = std::iter::once(done).collect();
            continue;
        }
        let (env, instance) = &walked[at];
        // clone: the query's key, reference counts
        let edge = edge(db, env.clone(), instance.clone());
        nodes[at].done = edge.done.clone();
        // clone: the environment each callee is asked in, reference counts
        let env = env.clone();
        let mut calls = Vec::with_capacity_in(edge.next.len(), Global);
        for callee in &edge.next {
            if within.is_some_and(|krate| def_crate(db, callee.def) != krate) {
                continue;
            }
            let to = match index.get(callee) {
                Some(&to) => to,
                None => {
                    let to = u32::try_from(nodes.len()).expect("fewer nodes than `REACHED`");
                    // clone: the index's key and the walk's, reference counts
                    index.insert(callee.clone(), to);
                    nodes.push(node_of(db, callee));
                    walked.push((env.clone(), callee.clone()));
                    to
                }
            };
            if !calls.contains(&to) {
                calls.push(to);
            }
        }
        nodes[at].calls = calls.into_boxed_slice();
    }
    Reach { roots, nodes: nodes.into_boxed_slice() }
}

/// `func` as a refusal names it, without backticks (`left_pad::phone_home`): what a finding
/// about an item of a crate, not an instance, says.
pub fn function_path(db: &Db, func: FunctionId) -> Box<str> {
    fn_item_path(db, func).into()
}

/// A node of `instance` before its edge is read.
fn node_of(db: &Db, instance: &Instance) -> Node {
    Node {
        name: name_of(db, instance).into(),
        krate: def_crate(db, instance.def),
        done: Box::default(),
        calls: Box::default(),
    }
}

/// One instance's edge: what it does and the instances outside the toolchain it goes on to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Edge {
    done: Box<[Done]>,
    next: Box<[Instance]>,
}

/// The kinds that say the walk could not see: the lints count each as every capability.
pub const UNSEEN: EffectSet = EffectSet::of(&[Rule::Unchecked, Rule::ForeignUnknown, Rule::Dyn]);

/// What `instance` (in `env`) does at the toolchain's edge ([`Done`]) and the instances outside
/// the toolchain it calls, makes a pointer of, puts in a vtable or gives a toolchain function
/// that calls it back. A tracked query keyed as the summary is: an edit reruns the edges of the
/// bodies whose summaries changed.
#[blaze_query::tracked(returns(ref))]
#[expect(
    clippy::needless_pass_by_value,
    reason = "a tracked query's arguments are its key, which `blaze_query::tracked` hands the body by value"
)]
pub(crate) fn edge(db: &Db, env: StoredParamEnvAndCrate, instance: Instance) -> Edge {
    // clone: the query's key, reference counts
    let summary = summary_at(db, Proofs::None, env.clone(), instance.clone());
    let mut done: Vec<Done> = Vec::new_in(Global);
    let mut next: Vec<Instance> = Vec::new_in(Global);
    let mut seen: HashSet<Instance, Global> = HashSet::new_in(Global);
    let rows = site_rows(&summary);
    let mut explained = EffectSet::EMPTY;
    for (at, site) in summary.sites.iter().enumerate() {
        let row = rows.get(&at).copied();
        let callee = &site.instance;
        // clone: the query's key, reference counts
        let below = summary_at(db, Proofs::None, env.clone(), callee.clone()).kinds;
        explained = explained.union(below);
        let in_toolchain = toolchain(db, callee);
        if let Some((kinds, said)) = row {
            explained = explained.union(kinds);
            // A row of a function outside the toolchain is said as this body's own doing.
            let leaf = if in_toolchain { leaf_path(db, callee) } else { None };
            done.push(Done { kinds, leaf, what: said.into() });
        }
        // clone: the seen set's key, reference counts
        if !seen.insert(callee.clone()) {
            continue;
        }
        if !in_toolchain {
            next.push(callee.clone()); // clone: the edge's value, reference counts
            continue;
        }
        // clone: the query's key, reference counts
        let call = toolchain_call(db, env.clone(), callee.clone());
        let path = leaf_path(db, callee);
        let shown = path.as_deref().map_or_else(|| name_of(db, callee), |it| format!("`{it}`"));
        if !call.kinds.is_empty() {
            let what = format!("calls {shown}").into();
            done.push(Done { kinds: call.kinds, leaf: path.clone(), what });
        }
        if call.cut {
            done.push(Done {
                kinds: EffectSet::one(Rule::Unchecked),
                leaf: None,
                what: format!(
                    "calls {shown}, past whose first {TOOLCHAIN_DESCENT} instances blaze did not look"
                )
                .into(),
            });
        }
        for back in &call.escapes {
            // clone: the seen set's key, reference counts
            if seen.insert(back.clone()) {
                next.push(back.clone()); // clone: the edge's value, reference counts
            }
        }
    }
    // A pointer made or a vtable filled is no site: what it reaches is its own instance's.
    for callee in &summary.callees {
        // clone: the query's key, reference counts
        let below = summary_at(db, Proofs::None, env.clone(), callee.clone()).kinds;
        explained = explained.union(below);
        // clone: the seen set's key, reference counts
        if !toolchain(db, callee) && seen.insert(callee.clone()) {
            next.push(callee.clone()); // clone: the edge's value, reference counts
        }
    }
    own_done(&summary, explained, &mut done);
    Edge { done: done.into_boxed_slice(), next: next.into_boxed_slice() }
}

/// What a summary's own body does: each kind its witnesses say it does itself, with their text,
/// and each kind it has that no site's summary, row or own witness accounts for, said as the
/// kind says itself.
fn own_done(summary: &Summary, explained: EffectSet, done: &mut Vec<Done>) {
    let mut own = EffectSet::EMPTY;
    for witness in &summary.witnesses {
        let How::Own { what, .. } = &witness.how else { continue };
        if own.has(witness.rule) {
            continue;
        }
        own = own.union(EffectSet::one(witness.rule));
        done.push(Done { kinds: EffectSet::one(witness.rule), leaf: None, what: what.clone() });
    }
    let rest = summary.kinds.without(explained).without(own);
    for rule in rest.iter() {
        done.push(Done { kinds: EffectSet::one(rule), leaf: None, what: rule.said().into() });
    }
}

/// The kinds each site's row adds at the call, by the site's index, with what the row says:
/// read off the witnesses that say a site's row (`How::Site` with `said`).
fn site_rows(summary: &Summary) -> HashMap<usize, (EffectSet, &str), Global> {
    let mut rows: HashMap<usize, (EffectSet, &str), Global> = HashMap::new_in(Global);
    for witness in &summary.witnesses {
        let How::Site { site, said: Some(said) } = &witness.how else { continue };
        let entry = rows.entry(*site as usize).or_insert((EffectSet::EMPTY, &**said));
        entry.0 = entry.0.union(EffectSet::one(witness.rule));
    }
    rows
}

/// The function a toolchain call calls, as a refusal names it.
fn leaf_path(db: &Db, instance: &Instance) -> Option<Box<str>> {
    match instance.def {
        Def::Body(Body::Fn(func)) | Def::Leaf(func, _) => Some(fn_item_path(db, func).into()),
        Def::Body(Body::Closure(_) | Body::Coroutine(_) | Body::CoroutineByMove(_)) => None,
    }
}

/// Whether `instance` is a model's leaf or the toolchain's code.
fn toolchain(db: &Db, instance: &Instance) -> bool {
    matches!(instance.def, Def::Leaf(..)) || toolchain_crate(db, def_crate(db, instance.def))
}

/// What one call into the toolchain does below it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ToolchainCall {
    /// Every kind the toolchain's own instances under it do themselves: not what the code
    /// outside it that it calls back does, which is that code's.
    kinds: EffectSet,
    /// The instances outside the toolchain it calls back, in the order met, each once.
    escapes: Box<[Instance]>,
    /// Whether [`TOOLCHAIN_DESCENT`] stopped the reading.
    cut: bool,
}

/// What the toolchain's `instance` does below it: one pass over the summaries of the
/// toolchain's instances under it, stopping at code outside the toolchain.
#[blaze_query::tracked(returns(ref))]
#[expect(
    clippy::needless_pass_by_value,
    reason = "a tracked query's arguments are its key, which `blaze_query::tracked` hands the body by value"
)]
pub(crate) fn toolchain_call(
    db: &Db,
    env: StoredParamEnvAndCrate,
    instance: Instance,
) -> ToolchainCall {
    let mut queue: Vec<Instance> = Vec::new_in(Global);
    queue.push(instance.clone()); // clone: the query's key, reference counts
    let mut seen: HashSet<Instance, Global> = HashSet::new_in(Global);
    seen.insert(instance.clone()); // clone: as above
    let mut escapes: Vec<Instance> = Vec::new_in(Global);
    let mut kinds = EffectSet::EMPTY;
    let mut cut = false;
    let mut next = 0;
    while let Some(at) = queue.get(next) {
        next += 1;
        if next > TOOLCHAIN_DESCENT {
            cut = true;
            break;
        }
        // clone: the query's key, reference counts
        let summary = summary_at(db, Proofs::None, env.clone(), at.clone());
        let mut explained = EffectSet::EMPTY;
        for (_, (row, _)) in site_rows(&summary).iter() {
            explained = explained.union(*row);
        }
        kinds = kinds.union(explained);
        let callees = summary.sites.iter().map(|it| &it.instance).chain(summary.callees.iter());
        let mut met: Vec<Instance> = Vec::new_in(Global);
        for callee in callees {
            if !seen.insert(callee.clone()) {
                continue;
            }
            match toolchain(db, callee) {
                true => met.push(callee.clone()), // clone: the queue's, reference counts
                false => escapes.push(callee.clone()), // clone: the value's, reference counts
            }
        }
        for callee in summary.sites.iter().map(|it| &it.instance).chain(summary.callees.iter()) {
            // clone: the query's key, reference counts
            let below = summary_at(db, Proofs::None, env.clone(), callee.clone()).kinds;
            explained = explained.union(below);
        }
        let mut own = Vec::new_in(Global);
        own_done(&summary, explained, &mut own);
        for it in own {
            kinds = kinds.union(it.kinds);
        }
        queue.extend(met);
    }
    ToolchainCall { kinds, escapes: escapes.into_boxed_slice(), cut }
}
