//! What each root and instance of a base refers to, by item, and the names its code is written
//! with: an edit that changes items asks again about those that refer to one that changed, or
//! call something by a name bound to another thing now, rather than about all of each crate
//! that can see the edited files (#344).

use blaze_items::{Index, ItemId, Live, Pushed, Refs};
use hir_def::{DefWithBodyId, FunctionId, ModuleId};
use hir_ty::db::HirDatabase;
use intern::Symbol;
use rustc_hash::{FxHashMap, FxHashSet};

use std::sync::OnceLock;

use super::{
    Fan, Instance,
    home::live_bodies,
    patch::{BaseInstance, FxIndexMap, probe},
};

#[derive(Clone)]
pub(super) struct ItemPush {
    pub(super) index: Index,
    /// By the index of each in the base; `Err` says why one's code can't be told.
    roots: Vec<Told>,
    instances: Vec<Told>,
    /// Of each item and name, the roots and instances whose code has it, by [`At`]: no fewer
    /// than have it, as what an edit settles is only added, and each is asked of its own
    /// [`Used`] when it is found here.
    by_item: FxHashMap<ItemId, Vec<At>>,
    by_name: FxHashMap<Symbol, Vec<At>>,
}

/// A root by its index in the base, and an instance by its index after all of the roots.
type At = u32;

type Told = Result<Used, &'static str>;

#[derive(Clone)]
struct Used {
    items: Box<[ItemId]>,
    /// In the order of their text.
    names: Box<[Symbol]>,
    /// The modules the code is written in, and those they are in.
    modules: Box<[ModuleId]>,
}

impl Used {
    fn touched(&self, pushed: &Pushed) -> bool {
        self.items.iter().any(|it| pushed.changed.contains(it))
            || pushed.rebinds(&self.modules, &self.names)
    }
}

/// What [`ItemPush::asked`] found to ask about, by the index of each in the base.
#[derive(Clone)]
pub(super) struct Asked {
    pub(super) roots: Vec<u32>,
    pub(super) instances: Vec<u32>,
}

impl ItemPush {
    /// Made on the threads `fan` has, each taking a part of the items, the roots and the
    /// instances in turn.
    pub(super) fn new(
        db: &dyn HirDatabase,
        fan: &Fan<'_>,
        compiled: &[(FunctionId, String)],
        instances: &[Instance],
    ) -> ItemPush {
        const PART: usize = 64;
        let start = std::time::Instant::now();
        let crates: Vec<base_db::Crate> = base_db::all_crates(db)
            .iter()
            .copied()
            .filter(|it| it.data(db).origin.is_local())
            .collect();
        // A base of code from the store resolved the names of few crates: each thread takes
        // one, and waits on the others its crate depends on.
        let resolve = |db: &dyn HirDatabase, at: usize| {
            _ = hir_def::nameres::crate_def_map(db, crates[at]);
        };
        fan(crates.len(), &resolve);
        let modules = blaze_items::modules_in(db, &crates);
        let declared = start.elapsed();
        let parts = |n: usize| n.div_ceil(PART);
        const MODULES: usize = 4;
        let (of_modules, of_roots) = (modules.len().div_ceil(MODULES), parts(compiled.len()));
        fn lock<T>(n: usize) -> Vec<OnceLock<T>> {
            (0..n).map(|_| OnceLock::new()).collect()
        }
        let entries: Vec<OnceLock<blaze_items::Entries>> = lock(of_modules);
        let write = |db: &dyn HirDatabase, at: usize| {
            let of = at * MODULES..((at + 1) * MODULES).min(modules.len());
            entries[at].get_or_init(|| blaze_items::entries_in(db, &modules[of]));
        };
        fan(of_modules, &write);
        (0..of_modules).for_each(|at| write(db, at));
        let index =
            Index::of_entries(db, &crates, entries.into_iter().filter_map(OnceLock::into_inner));
        let written = start.elapsed();
        // A root or instance of the base can be of code an edit since took away: only what
        // the index has is asked of.
        let live = index.live();
        let roots: Vec<OnceLock<Vec<Told>>> = lock(of_roots);
        let of_instances: Vec<OnceLock<Vec<Told>>> = lock(parts(instances.len()));
        let part = |all: usize, at: usize| at * PART..((at + 1) * PART).min(all);
        let work = |db: &dyn HirDatabase, at: usize| {
            if let Some(at) = at.checked_sub(of_roots) {
                of_instances[at].get_or_init(|| {
                    let refs = Refs::new(db);
                    let mut bodies: FxHashMap<DefWithBodyId, Told> = FxHashMap::default();
                    let mut of_body = |body: DefWithBodyId| -> Told {
                        bodies.entry(body).or_insert_with(|| used(db, &refs, body, &live)).clone()
                    };
                    part(instances.len(), at)
                        .map(|i| used_by_instance(db, &refs, &instances[i], &live, &mut of_body))
                        .collect::<Vec<_>>()
                });
            } else {
                roots[at].get_or_init(|| {
                    let refs = Refs::new(db);
                    part(compiled.len(), at)
                        .map(|i| used(db, &refs, compiled[i].0.into(), &live))
                        .collect::<Vec<_>>()
                });
            }
        };
        let count = of_roots + of_instances.len();
        fan(count, &work);
        (0..count).for_each(|at| work(db, at));
        drop(live);
        let made = start.elapsed();
        let flat = |parts: Vec<OnceLock<Vec<Told>>>| {
            parts.into_iter().filter_map(OnceLock::into_inner).flatten().collect::<Vec<_>>()
        };
        let (roots, instances) = (flat(roots), flat(of_instances));
        if std::env::var_os("RA_JIT_STATS").is_some() {
            let mut why: FxHashMap<&str, usize> = FxHashMap::default();
            for it in roots.iter().chain(&instances) {
                if let Err(it) = it {
                    *why.entry(it).or_default() += 1;
                }
            }
            let mut why: Vec<_> = why.into_iter().collect();
            why.sort_by(|a, b| b.1.cmp(&a.1));
            let unknown = |it: &[Told]| it.iter().filter(|it| it.is_err()).count();
            eprintln!(
                "items-made: modules {} us, indexed {} us, told {} us; {} of {} roots and {} of {} instances unknown: {:?}",
                declared.as_micros(),
                (written - declared).as_micros(),
                (made - written).as_micros(),
                unknown(&roots),
                roots.len(),
                unknown(&instances),
                instances.len(),
                &why[..why.len().min(6)]
            );
        }
        let mut items = ItemPush {
            index,
            roots,
            instances,
            by_item: FxHashMap::default(),
            by_name: FxHashMap::default(),
        };
        for at in 0..(items.roots.len() + items.instances.len()) as At {
            items.link(at, None);
        }
        items
    }

    /// Why the instance at `at` of the base can't be told, if it can't.
    pub(super) fn instance_unknown(&self, at: u32) -> Option<&'static str> {
        self.instances.get(at as usize)?.as_ref().err().copied()
    }

    fn told(&self, at: At) -> &Told {
        match (at as usize).checked_sub(self.roots.len()) {
            None => &self.roots[at as usize],
            Some(i) => &self.instances[i],
        }
    }

    /// Notes what `at` has in [`Self::by_item`] and [`Self::by_name`], past what it had in `was`.
    fn link(&mut self, at: At, was: Option<&Used>) {
        let Ok(used) = (match (at as usize).checked_sub(self.roots.len()) {
            None => &self.roots[at as usize],
            Some(i) => &self.instances[i],
        }) else {
            return;
        };
        for item in &used.items {
            if was.is_none_or(|it| !it.items.contains(item)) {
                self.by_item.entry(*item).or_default().push(at);
            }
        }
        for name in &used.names {
            if was.is_none_or(|it| {
                it.names.binary_search_by(|n| n.as_str().cmp(name.as_str())).is_err()
            }) {
                self.by_name.entry(name.clone()).or_default().push(at);
            }
        }
    }

    /// The roots and instances that refer to what the edit changed, or call something by a
    /// name bound to something else now, and those that can't be told.
    pub(super) fn asked(&self, pushed: &Pushed) -> Asked {
        let unknown = (self.roots.iter().chain(&self.instances).enumerate())
            .filter(|(_, it)| it.is_err())
            .map(|(at, _)| at as At);
        let users = pushed.changed.iter().filter_map(|it| self.by_item.get(it));
        let named = pushed.rebound().filter_map(|it| self.by_name.get(it));
        let mut found: Vec<At> = users.chain(named).flatten().copied().chain(unknown).collect();
        found.sort_unstable();
        found.dedup();
        let ask = |at: &At| self.told(*at).as_ref().ok().is_none_or(|it| it.touched(pushed));
        let (mut roots, mut instances) = (Vec::new(), Vec::new());
        for at in found.into_iter().filter(ask) {
            match (at as usize).checked_sub(self.roots.len()) {
                None => roots.push(at),
                Some(i) => instances.push(i as u32),
            }
        }
        Asked { roots, instances }
    }

    /// Notes what the roots and instances asked about refer to now, of the revision after the
    /// edit: what called a thing by a name bound to another calls that one now.
    pub(super) fn settle(
        &mut self,
        db: &dyn HirDatabase,
        fan: &Fan<'_>,
        asked: &Asked,
        compiled: &[(FunctionId, String)],
        instances: &FxIndexMap<Instance, BaseInstance>,
    ) {
        let (roots, instances) = Settled::asked(asked, compiled, instances);
        let live = self.index.live();
        self.take_settled(Settled::new(db, fan, &roots, &instances, &live));
    }

    /// Takes what `asked` refers to for unknown, as it could not be settled.
    pub(super) fn forget(&mut self, asked: &Asked) {
        for &at in &asked.roots {
            if let Some(it) = self.roots.get_mut(at as usize) {
                *it = Err(PANICKED);
            }
        }
        for &at in &asked.instances {
            if let Some(it) = self.instances.get_mut(at as usize) {
                *it = Err(PANICKED);
            }
        }
    }

    pub(super) fn take_settled(&mut self, settled: Settled) {
        let roots = self.roots.len() as At;
        let told = (settled.roots.into_iter())
            .chain(settled.instances.into_iter().map(|(i, used)| (roots + i, used)));
        for (at, used) in told {
            let slot = match (at as usize).checked_sub(self.roots.len()) {
                None => &mut self.roots[at as usize],
                Some(i) => &mut self.instances[i],
            };
            let was = std::mem::replace(slot, used);
            self.link(at, was.as_ref().ok());
        }
    }
}

/// What roots and instances refer to after an edit, by the index of each in the base.
pub(super) struct Settled {
    roots: Vec<(u32, Told)>,
    instances: Vec<(u32, Told)>,
}

impl Settled {
    /// The roots and instances of `asked`, by the index of each.
    pub(super) fn asked(
        asked: &Asked,
        compiled: &[(FunctionId, String)],
        instances: &FxIndexMap<Instance, BaseInstance>,
    ) -> (Vec<(u32, FunctionId)>, Vec<(u32, Instance)>) {
        let roots = asked.roots.iter().map(|&i| (i, compiled[i as usize].0)).collect();
        let instances = asked
            .instances
            .iter()
            .filter_map(|&i| Some((i, instances.get_index(i as usize)?.0.clone())));
        (roots, instances.collect())
    }

    /// Asked on the threads `fan` has, each taking a part of the roots and the instances.
    pub(super) fn new(
        db: &dyn HirDatabase,
        fan: &Fan<'_>,
        roots: &[(u32, FunctionId)],
        instances: &[(u32, Instance)],
        live: &Live,
    ) -> Settled {
        const PART: usize = 16;
        let (of_roots, of_instances) = (roots.len().div_ceil(PART), instances.len().div_ceil(PART));
        let parts: Vec<OnceLock<Vec<(u32, Told)>>> =
            (0..of_roots + of_instances).map(|_| OnceLock::new()).collect();
        let work = |db: &dyn HirDatabase, at: usize| {
            parts[at].get_or_init(|| {
                let refs = Refs::new(db);
                let part = |all: usize, at: usize| at * PART..((at + 1) * PART).min(all);
                match at.checked_sub(of_roots) {
                    None => part(roots.len(), at)
                        .map(|i| (roots[i].0, used(db, &refs, roots[i].1.into(), live)))
                        .collect(),
                    Some(at) => {
                        let mut bodies: FxHashMap<DefWithBodyId, Told> = FxHashMap::default();
                        let mut of_body = |body: DefWithBodyId| -> Told {
                            bodies
                                .entry(body)
                                .or_insert_with(|| used(db, &refs, body, live))
                                .clone()
                        };
                        part(instances.len(), at)
                            .map(|i| {
                                let (at, instance) = &instances[i];
                                (*at, used_by_instance(db, &refs, instance, live, &mut of_body))
                            })
                            .collect()
                    }
                }
            });
        };
        fan(parts.len(), &work);
        (0..parts.len()).for_each(|at| work(db, at));
        let mut parts = parts.into_iter().filter_map(OnceLock::into_inner);
        let roots = parts.by_ref().take(of_roots).flatten().collect();
        Settled { roots, instances: parts.flatten().collect() }
    }
}

/// What an instance refers to: its arguments, and the bodies it has the code of or mentions a
/// type of.
fn used_by_instance(
    db: &dyn HirDatabase,
    refs: &Refs<'_>,
    instance: &Instance,
    live: &Live,
    of_body: &mut dyn FnMut(DefWithBodyId) -> Told,
) -> Told {
    let bodies = match probe(|| live_bodies(db, instance, live)).ok_or(PANICKED)? {
        Ok(it) => it,
        Err(true) => return Err(GONE),
        Err(false) => return Err("an instance whose bodies can't be told"),
    };
    let mut items = probe(|| refs.of_args(instance.args.as_ref())).ok_or(PANICKED)??;
    let (mut names, mut modules) = (Vec::new(), Vec::new());
    for body in bodies {
        let of = of_body(body)?;
        items.extend(of.items);
        names.extend(of.names);
        modules.extend(of.modules);
    }
    let mut unique = FxHashSet::default();
    items.retain(|it| unique.insert(*it));
    let mut unique = FxHashSet::default();
    modules.retain(|it| unique.insert(*it));
    names.sort_unstable_by(|a, b| a.as_str().cmp(b.as_str()));
    names.dedup();
    Ok(Used { items: items.into(), names: names.into(), modules: modules.into() })
}

/// Why code its file no longer has can't be told.
const GONE: &str = "code that is gone";

const PANICKED: &str = "a query that panicked";

fn used(db: &dyn HirDatabase, refs: &Refs<'_>, body: DefWithBodyId, live: &Live) -> Told {
    if !live.has_body(db, body) {
        return Err(GONE);
    }
    let id = match body {
        DefWithBodyId::FunctionId(it) => ItemId::Function(it),
        DefWithBodyId::ConstId(it) => ItemId::Const(it),
        DefWithBodyId::StaticId(it) => ItemId::Static(it),
        DefWithBodyId::VariantId(_) => return Err("a variant's discriminant"),
    };
    let names = probe(|| blaze_items::code_names(db, body)).ok_or(PANICKED)?.ok_or(GONE)?;
    let mut items = probe(|| refs.of_code(id, Some(body))).ok_or(PANICKED)??;
    items.push(id);
    let modules = probe(|| blaze_items::modules_of(db, id)).ok_or(PANICKED)?;
    Ok(Used { items: items.into(), names: names.into(), modules: modules.into() })
}
