//! What the engine says of the build it does, beside what any process of the daemon says
//! ([`blaze_cargo::events`]): the crates of the build and how far each is, what was compiled
//! again and why, and what the store had.

use std::{
    path::PathBuf,
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use blaze_cargo::events::{COUNTED, node_ids, say};
pub(crate) use blaze_cargo::events::{Counting, at, heard, idle, said_beside, within};
pub(crate) use blaze_protocol::events::{Cache, CrateState, Step};
use blaze_protocol::events::{Cause, Event, Fns, NodeKind, SLOW_NS, Slow};
use ide_db::base_db::Crate;
use rustc_hash::{FxHashMap, FxHashSet};

/// The name each crate of the build has in its [`Event::Node`], which its [`Event::Crate`]s
/// use too: the standard library's crates are one node, `std`.
static NAMES: Mutex<Option<FxHashMap<Crate, String>>> = Mutex::new(None);

const STD: &str = "std";

/// Says the crates of the build, `crates`, each a node of the build's graph after the nodes
/// of what it waits for: [`NodeKind::Names`], or [`NodeKind::Std`] for the standard library's.
pub(crate) fn graph(db: &ide_db::RootDatabase, crates: &[Crate]) {
    if !heard() {
        return;
    }
    let mut names: FxHashMap<Crate, String> = FxHashMap::default();
    let mut taken: FxHashSet<String> = FxHashSet::default();
    let mut order: Vec<String> = Vec::new();
    for &krate in crates {
        let name = if krate.data(db).origin.is_lang() {
            STD.to_owned()
        } else {
            let extra = krate.extra_data(db);
            let shown =
                extra.display_name.as_ref().map_or_else(|| "?".to_owned(), |it| it.to_string());
            // Two versions of one crate, or a library and its program of one name.
            match taken.contains(&shown) {
                false => shown,
                true => match &extra.version {
                    Some(version) if !taken.contains(&format!("{shown} v{version}")) => {
                        format!("{shown} v{version}")
                    }
                    _ => (2..)
                        .map(|n| format!("{shown} ({n})"))
                        .find(|it| !taken.contains(it))
                        .unwrap_or(shown),
                },
            }
        };
        if taken.insert(name.clone()) {
            order.push(name.clone());
        }
        names.insert(krate, name);
    }
    let mut deps: FxHashMap<&str, FxHashSet<&str>> = FxHashMap::default();
    for &krate in crates {
        let of = names[&krate].as_str();
        let waits = deps.entry(of).or_default();
        for dep in &krate.data(db).dependencies {
            match names.get(&dep.crate_id) {
                Some(dep) if dep != of => _ = waits.insert(dep.as_str()),
                _ => {}
            }
        }
    }
    // Each node after what it waits for, in the order the crates came where that allows.
    let first = node_ids(order.len());
    let mut index: FxHashMap<&str, u32> = FxHashMap::default();
    let mut nodes: Vec<Event> = Vec::with_capacity(order.len());
    let node = |name: &str, id: u32, deps: Vec<u32>| {
        let kind = if name == STD { NodeKind::Std } else { NodeKind::Names };
        Event::Node { id, kind, name: name.to_owned(), deps }
    };
    while index.len() < order.len() {
        let before = index.len();
        for name in &order {
            if index.contains_key(name.as_str()) {
                continue;
            }
            let waits = deps.get(name.as_str());
            let ready = waits.is_none_or(|it| it.iter().all(|dep| index.contains_key(dep)));
            if ready {
                let mut on: Vec<u32> = waits.into_iter().flatten().map(|dep| index[dep]).collect();
                on.sort_unstable();
                let id = first + index.len() as u32;
                index.insert(name, id);
                nodes.push(node(name, id, on));
            }
        }
        // A cycle, which crates do not have: the rest are said waiting for nothing.
        if index.len() == before {
            for name in &order {
                if !index.contains_key(name.as_str()) {
                    let id = first + index.len() as u32;
                    index.insert(name, id);
                    nodes.push(node(name, id, Vec::new()));
                }
            }
        }
    }
    for event in nodes {
        say(event);
    }
    *crate::lock(&NAMES) = Some(names);
}

/// The name of `krate` in the graph said, if one was.
fn name_of(krate: Crate) -> Option<String> {
    crate::lock(&NAMES).as_ref()?.get(&krate).cloned()
}

/// Says `krate` is in `state` now: the node of the standard library is in the state of its
/// crate said last.
pub(crate) fn crate_is(krate: Crate, state: CrateState) {
    if let Some(name) = name_of(krate) {
        say(Event::Crate { name, state, fns: Fns::default() });
    }
}

/// Why the store has nothing for this blaze, when it can say so before a build: there is
/// none, nothing was added to it, or another blaze added to it last, whose keys are not this
/// one's.
fn cold_ahead(store: Option<&blaze_store::Store>, this: &str) -> Option<String> {
    let Some(store) = store else {
        return Some(format!("there is no store ({})", blaze_cargo::cargo::daemon::no_store()));
    };
    match store.last_writer() {
        Some(other) if other != this => {
            let mut parts = other.split('-');
            let release = parts.next().unwrap_or_default();
            Some(match parts.next() {
                Some(commit) => format!("cache keys changed since blaze {release} ({commit})"),
                None => format!("cache keys changed since blaze {release}"),
            })
        }
        Some(_) => None,
        None => Some("the store is empty".to_owned()),
    }
}

/// Says, as the workspace starts loading, why the build will find nothing in the store when
/// the store says so already, so that the view says it is cold from the start. Nothing before
/// the toolchain is known: a store opened then is none for good.
pub(crate) fn cache_ahead() {
    if !heard() || blaze_store::toolchain().is_none() {
        return;
    }
    let cold = cold_ahead(blaze_store::Store::global(), &crate::version());
    if let Some(cold) = cold {
        say(Event::Cache { cache: Cache { found: 0, fetched: 0, compiled: 0, cold: Some(cold) } });
    }
}

/// Says what the store had of the compile `stats` counted, and of one that found nothing in
/// it, why; then has the store say this blaze added to it last.
pub(crate) fn cache(stats: &hir_ty_codegen::BuildStats) {
    let store = blaze_store::Store::global();
    let this = crate::version();
    let (found, compiled) = (stats.reused as u64, stats.compiled as u64);
    let cold = (compiled > 0 && found == 0).then(|| {
        cold_ahead(store, &this).unwrap_or_else(|| "the store has none of this build".to_owned())
    });
    if let Some(store) = store.filter(|_| compiled > 0) {
        let _ = store.note_writer(&this);
    }
    say(Event::Cache { cache: Cache { found, fetched: 0, compiled, cold } });
}

/// The compile of the functions of `krate`, from now until this is dropped: a
/// [`Step::Codegen`], and every [`COUNTED`] the functions compiled so far.
pub(crate) struct Compiling {
    counting: Option<(std::sync::Arc<AtomicBool>, std::thread::JoinHandle<()>)>,
}

/// A build begins: the functions it compiles are kept track of when anyone hears it, to say
/// the slow ones ([`Event::Compiling`], [`Event::Slowest`]).
pub(crate) fn flights() {
    hir_ty_codegen::flight::begin(heard());
}

/// The most functions compiling said of each crate, and of all.
const FLYING_OF: usize = 3;
const FLYING: usize = 24;

/// The name of `krate`'s node, or `shown` when the build said no graph.
fn node_name(names: Option<&FxHashMap<Crate, String>>, krate: Crate, shown: &str) -> String {
    names.and_then(|it| it.get(&krate)).map_or_else(|| shown.to_owned(), String::clone)
}

/// The functions compiling for [`SLOW_NS`] or more, longest first, [`FLYING_OF`] of a crate.
fn flying(shown: &str) -> Vec<Slow> {
    let under_way = hir_ty_codegen::flight::under_way(std::time::Duration::from_nanos(SLOW_NS));
    if under_way.is_empty() {
        return Vec::new();
    }
    let names = crate::lock(&NAMES);
    let mut of: FxHashMap<String, usize> = FxHashMap::default();
    let mut flying = Vec::new();
    for it in under_way {
        let krate = node_name(names.as_ref(), it.krate, shown);
        let count = of.entry(krate.clone()).or_default();
        if *count < FLYING_OF {
            *count += 1;
            let ns = u64::try_from(it.took.as_nanos()).unwrap_or(u64::MAX);
            flying.push(Slow { krate, name: it.name, ns });
        }
        if flying.len() == FLYING {
            break;
        }
    }
    flying
}

pub(crate) fn compiling(krate: &str) -> Compiling {
    at(Step::Codegen, Some(krate));
    if !heard() {
        return Compiling { counting: None };
    }
    let done = std::sync::Arc::new(AtomicBool::new(false));
    *crate::lock(&COMPILED_FROM) = hir_ty_codegen::compiled_by_crate().into_iter().collect();
    let counting = {
        let (done, name) = (done.clone(), krate.to_owned());
        let from = hir_ty_codegen::compiled_so_far();
        std::thread::spawn(move || {
            let (mut said, mut said_of) = (0, FxHashMap::default());
            // Which functions were said compiling: how long each has is the view's to count.
            let mut flown: Vec<(String, String)> = Vec::new();
            let mut fly = |functions: Vec<Slow>| {
                let now: Vec<(String, String)> =
                    functions.iter().map(|it| (it.krate.clone(), it.name.clone())).collect();
                if now != flown {
                    flown = now;
                    say(Event::Compiling { functions });
                }
            };
            while !done.load(Ordering::Relaxed) {
                std::thread::park_timeout(COUNTED);
                fly(flying(&name));
                // Of each crate of the graph, once one was said; of the program alone else.
                if crate::lock(&NAMES).is_some() {
                    for (name, compiled) in compiled_by_name() {
                        if said_of.insert(name.clone(), compiled) != Some(compiled) {
                            let fns = Fns { total: 0, done: compiled, kept: 0 };
                            say(Event::Crate { name, state: CrateState::Running, fns });
                        }
                    }
                    continue;
                }
                let compiled = (hir_ty_codegen::compiled_so_far() - from) as u64;
                if compiled != said {
                    said = compiled;
                    let fns = Fns { total: 0, done: compiled, kept: 0 };
                    say(Event::Crate { name: name.clone(), state: CrateState::Running, fns });
                }
            }
            fly(Vec::new());
        })
    };
    Compiling { counting: Some((done, counting)) }
}

/// [`hir_ty_codegen::compiled_by_crate`] when the compile began.
static COMPILED_FROM: std::sync::LazyLock<Mutex<FxHashMap<Crate, usize>>> =
    std::sync::LazyLock::new(Default::default);

/// How many functions and instances the compile under way compiled of each crate of the
/// graph that it compiled any of, by its name.
fn compiled_by_name() -> Vec<(String, u64)> {
    let from = crate::lock(&COMPILED_FROM).clone();
    let names = crate::lock(&NAMES);
    let Some(names) = names.as_ref() else { return Vec::new() };
    let mut by_name: FxHashMap<String, u64> = FxHashMap::default();
    for (krate, count) in hir_ty_codegen::compiled_by_crate() {
        let compiled = count - from.get(&krate).copied().unwrap_or(0).min(count);
        if let Some(name) = names.get(&krate).filter(|_| compiled > 0) {
            *by_name.entry(name.clone()).or_default() += compiled as u64;
        }
    }
    let mut by_name: Vec<_> = by_name.into_iter().collect();
    by_name.sort_unstable();
    by_name
}

impl Drop for Compiling {
    fn drop(&mut self) {
        if let Some((done, counting)) = self.counting.take() {
            done.store(true, Ordering::Relaxed);
            counting.thread().unpark();
            let _ = counting.join();
        }
    }
}

/// Says what the build of `krate` came to, by the codegen's count, and of a build of an
/// edit, of the files `edited`, which functions it compiled again and why.
pub(crate) fn built(
    db: &dyn hir::db::HirDatabase,
    krate: &str,
    stats: &hir_ty_codegen::BuildStats,
    edited: Option<&FxHashMap<vfs::FileId, PathBuf>>,
) {
    let (recompiled, all) = hir_ty_codegen::take_recompiled(db);
    let slowest: Vec<Slow> = {
        let names = crate::lock(&NAMES);
        let slowest = hir_ty_codegen::flight::slowest().into_iter();
        let slowest = slowest.map(|it| Slow {
            krate: node_name(names.as_ref(), it.krate, krate),
            ns: u64::try_from(it.took.as_nanos()).unwrap_or(u64::MAX),
            name: it.name,
        });
        slowest.collect()
    };
    if !slowest.is_empty() {
        say(Event::Slowest { functions: slowest });
    }
    if let Some(edited) = edited {
        let count = (all - recompiled.len()) as u64;
        for it in recompiled {
            let cause = match edited.get(&it.file) {
                Some(file) => Cause::Edited { file: file.clone() },
                None => Cause::Uses,
            };
            say(Event::Rebuilt { name: it.name, krate: it.krate, cause });
        }
        if count > 0 {
            say(Event::Unlisted { count });
        }
    }
    let (bodies, reused) = (stats.bodies as u64, stats.reused as u64);
    let names = crate::lock(&NAMES).clone();
    let Some(names) = names else {
        let fns = Fns { total: bodies, done: bodies, kept: reused };
        let state = if stats.compiled == 0 { CrateState::UpToDate } else { CrateState::Built };
        say(Event::Crate { name: krate.to_owned(), state, fns });
        return;
    };
    // Each crate with what was compiled of it, the program with what the store had too; of a
    // build from cold, every other crate of the graph is up to date.
    let compiled = compiled_by_name();
    for (name, done) in &compiled {
        let kept = if name == krate { reused } else { 0 };
        let fns = Fns { total: done + kept, done: done + kept, kept };
        say(Event::Crate { name: name.clone(), state: CrateState::Built, fns });
    }
    let built: FxHashSet<&str> = compiled.iter().map(|it| it.0.as_str()).collect();
    let mut rest: Vec<&String> = names.values().filter(|it| !built.contains(it.as_str())).collect();
    rest.sort_unstable();
    rest.dedup();
    for name in rest {
        if name == krate || edited.is_none() {
            let kept = if name == krate { reused } else { 0 };
            let fns = Fns { total: kept, done: kept, kept };
            say(Event::Crate { name: name.clone(), state: CrateState::UpToDate, fns });
        }
    }
}
