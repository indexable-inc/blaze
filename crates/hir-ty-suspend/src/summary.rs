//! may-suspend of an instance, a query over its monomorphized runtime body, bottom up: each
//! instance summarized once, however many callers reach it.

use base_db::Db;
use blaze_hash::HashSet;
use blaze_query::db::FromId;
use hir_def::{DefWithBodyId, FunctionId, signatures::FunctionSignature};
use hir_ty::{
    InferBodyId,
    db::{InternedClosureId, InternedCoroutineId},
    traits::StoredParamEnvAndCrate,
};

use crate::dispatch::{Cx, Def, Instance, LONGEST, Marked, Target, Unseen, body_of, sites};

/// One step toward the root.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Step {
    Walk(Instance),
    Root(FunctionId),
    Marked(Marked),
}

/// The first step of the shortest path to the root, and how many calls the path has.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Hop {
    pub(crate) via: Step,
    pub(crate) calls: u32,
}

/// What one instance is to the check.
#[derive(Debug, PartialEq, Eq, Default)]
pub(crate) struct Summary {
    /// How it may park its fiber; `None` when it never does.
    pub(crate) suspends: Option<Hop>,
    /// Every instance it calls, puts in a vtable or makes a pointer of: the walk's edges, each once.
    pub(crate) callees: Box<[Instance]>,
    pub(crate) unseen: Option<Unseen>,
    /// It calls a root itself: it is the fiber driver's own code.
    pub(crate) driver: bool,
}

/// What `instance` is to the check, in `env`. A cycle of instances starts from "never
/// suspends" and rises to its fixpoint: may-suspend only grows, and a path only shortens.
#[blaze_query::tracked(returns(ref), cycle_initial = |_, _, _, _| Summary::default())]
pub(crate) fn summary(db: &Db, env: StoredParamEnvAndCrate, instance: Instance) -> Summary {
    let cx = Cx::new(db, env);
    let body = match body_of(db, &cx.stored, &instance) {
        Ok(body) => body,
        Err(_) => return Summary { unseen: Some(Unseen::NoMir), ..Summary::default() },
    };
    let sites = sites(&cx, body);
    let mut best: Option<Hop> = None;
    let mut driver = false;
    let mut seen = HashSet::default();
    let mut callees = Vec::new(); // alloc: the summary's edges, once per instance
    let mut edge = |instance: &Instance, callees: &mut Vec<Instance>| {
        if seen.insert(*instance) {
            callees.push(*instance);
        }
    };
    for (_, targets) in &sites.calls {
        for target in targets {
            let hop = match target {
                Target::Root(root) => {
                    driver = true;
                    Some(Hop { via: Step::Root(*root), calls: 1 })
                }
                Target::Marked(marked) => Some(Hop { via: Step::Marked(*marked), calls: 1 }),
                Target::Walk(callee) => {
                    edge(callee, &mut callees);
                    summary(db, cx.stored, *callee).suspends.as_ref().map(|it| Hop {
                        via: Step::Walk(*callee),
                        calls: it.calls.saturating_add(1),
                    })
                }
            };
            if let Some(hop) = hop
                && best.as_ref().is_none_or(|it| hop.calls < it.calls)
            {
                best = Some(hop);
            }
        }
    }
    let made = sites.slots.iter().flat_map(|it| &it.targets).chain(sites.reified.iter().flat_map(|it| &it.targets));
    for target in made {
        if let Target::Walk(callee) = target {
            edge(callee, &mut callees);
        }
    }
    Summary { suspends: best, callees: callees.into_boxed_slice(), unseen: sites.unseen, driver }
}

/// Whether what `target` reaches may park the fiber.
pub(crate) fn suspends(db: &Db, env: &StoredParamEnvAndCrate, target: &Target) -> bool {
    match target {
        Target::Root(_) | Target::Marked(_) => true,
        Target::Walk(instance) => summary(db, *env, *instance).suspends.is_some(),
    }
}

/// The calls from `target` to the root, each named: `` `variance` `` → `` `fetch` `` → … →
/// `` `suspend` ``. Built only for an error.
pub(crate) fn path(db: &Db, env: &StoredParamEnvAndCrate, target: &Target) -> Box<[String]> {
    let mut at = match target {
        Target::Walk(instance) => Step::Walk(*instance),
        Target::Root(root) => Step::Root(*root),
        Target::Marked(marked) => Step::Marked(*marked),
    };
    let mut names = Vec::new(); // alloc: an error's path, once per error
    loop {
        names.push(step_name(db, &at));
        let Step::Walk(instance) = &at else { break };
        let Some(hop) = &summary(db, *env, *instance).suspends else { break };
        if names.len() > LONGEST {
            break;
        }
        at = hop.via;
    }
    names.into_boxed_slice()
}

fn step_name(db: &Db, step: &Step) -> String {
    match step {
        Step::Walk(instance) => instance_name(db, instance.def),
        Step::Root(root) => fn_name(db, *root),
        // alloc: an error's path
        Step::Marked(Marked::Method(method)) => format!("{} (`#[blaze::may_suspend]`, through `dyn`)", fn_name(db, *method)),
        // alloc: an error's path
        Step::Marked(Marked::Field(_)) => "a function pointer from a `#[blaze::may_suspend]` field".to_owned(),
    }
}

/// An instance as an error names it.
pub(crate) fn instance_name(db: &Db, def: Def) -> String {
    let parent = |owner: InferBodyId<'_>| match owner {
        InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(func)) => fn_name(db, func),
        InferBodyId::DefWithBodyId(_) | InferBodyId::AnonConstId(_) => "a constant".to_owned(), // alloc: an error's path
    };
    match def {
        Def::Fn(func) => fn_name(db, func),
        // alloc: an error's path
        Def::Closure(id) => format!("a closure in {}", parent(InternedClosureId::from_id(id).loc(db).owner)),
        Def::Coroutine(id) | Def::CoroutineByMove(id) => {
            // alloc: an error's path
            format!("an `async` block in {}", parent(InternedCoroutineId::from_id(id).loc(db).owner))
        }
    }
}

fn fn_name(db: &Db, func: FunctionId) -> String {
    format!("`{}`", FunctionSignature::of(db, func).name.as_str()) // alloc: an error's path
}

/// The item whose body `def` is written in.
pub(crate) fn owner_of(db: &Db, def: Def) -> Option<DefWithBodyId> {
    let of = |owner: InferBodyId<'_>| match owner {
        InferBodyId::DefWithBodyId(it) => Some(it),
        InferBodyId::AnonConstId(_) => None,
    };
    match def {
        Def::Fn(func) => Some(func.into()),
        Def::Closure(id) => of(InternedClosureId::from_id(id).loc(db).owner),
        Def::Coroutine(id) | Def::CoroutineByMove(id) => of(InternedCoroutineId::from_id(id).loc(db).owner),
    }
}
