//! The machine: the feature fixpoint over (package, side) nodes, then the units walked from the
//! roots. Both are explicit worklists (no recursion); every set grows only, so the fixpoint is
//! reached whatever order the work is taken in (the property tests take it in drawn orders).

use blaze_manifest::layout::{DepKind, ExtraKind};
use blaze_manifest::lock::{Lock, LockId, LockSource};
use blaze_own::{AVec, Alloc, BTreeMap, String, Vec};

use crate::input::{Ask, Bin, CfgAtom, DepSource, FeatureValue, Lib, Package, Resolver, TargetDesc};
use crate::extra::{Extra, has_test_cfg};
use crate::{LeftOut, LeftOutKind, Pass, PartialGraph, Plant, Text, Unit, UnitDep, UnitError, UnitGraph, UnitId, UnitKind};

/// An answer to a [`Pass::Need`].
pub(crate) enum Answer<'a> {
    Pending,
    /// No manifest on disk for it: never guessed; a walk that reaches it ends
    /// [`Pass::Partial`], naming it.
    Unavailable,
    Read(Package<'a>),
}

/// Which feature namespace (and platform) a node is: `Target`, or `Host` (build scripts,
/// proc macros and everything they depend on, under resolver 2 and 3).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
#[repr(u8)]
pub(crate) enum Side {
    Target = 0,
    Host = 1,
}

/// A feature value, borrowed from the facts.
#[derive(Clone, Copy)]
enum V<'s> {
    Feature(&'s str),
    Dep(&'s str),
    DepFeature(&'s str, &'s str, bool),
}

impl<'s> V<'s> {
    fn of(value: &'s FeatureValue<'_>) -> V<'s> {
        match value {
            FeatureValue::Feature(f) => V::Feature(f),
            FeatureValue::Dep(d) => V::Dep(d),
            FeatureValue::DepFeature { dep, feature, weak } => V::DepFeature(dep, feature, *weak),
        }
    }

    /// A dependency's `features = [..]` entry or a command line's: the same spelling.
    fn parse(text: &'s str) -> V<'s> {
        if let Some(dep) = text.strip_prefix("dep:") {
            return V::Dep(dep);
        }
        match text.split_once('/') {
            Some((dep, feature)) => match dep.strip_suffix('?') {
                Some(dep) => V::DepFeature(dep, feature, true),
                None => V::DepFeature(dep, feature, false),
            },
            None => V::Feature(text),
        }
    }
}

#[derive(Clone, Copy)]
enum Work<'s> {
    /// Walk the node's always-on dependencies.
    Visit(u32),
    /// Apply a value on the node; `by` is the package that asked (for errors).
    Value(u32, V<'s>, &'s str),
    /// Turn on the dependency entry `dep` of the node: its default and listed features, and
    /// walk it.
    Edge(u32, u32),
    /// Feature `feature` of the package the node's entry `dep` names.
    DepFeat(u32, u32, &'s str),
}

/// The fixpoint's state.
struct State<'s, 'a> {
    /// Per node (`id * 2 + side`): its features, a bit per index into its package's features.
    feats: AVec<'a, AVec<'a, u64>>,
    visited: AVec<'a, bool>,
    /// Per node: its optional dependency entries turned on, a bit per entry.
    active: AVec<'a, AVec<'a, u64>>,
    /// Weak `dep?/feature` values waiting for `dep` to be turned on: (node, dep, feature).
    deferred: AVec<'a, (u32, &'s str, &'s str)>,
    needs: AVec<'a, LockId>,
    work: AVec<'a, Work<'s>>,
}

fn set(bits: &mut AVec<'_, u64>, at: usize) -> bool {
    let (word, bit) = (at / 64, 1u64 << (at % 64));
    if bits.len() <= word {
        bits.resize(word + 1, 0);
    }
    let new = bits[word] & bit == 0;
    bits[word] |= bit;
    new
}

fn get(bits: &[u64], at: usize) -> bool {
    bits.get(at / 64).is_some_and(|w| w & (1u64 << (at % 64)) != 0)
}

fn node(id: LockId, side: Side) -> u32 {
    id.0 * 2 + side as u32
}

fn node_id(node: u32) -> LockId {
    LockId(node / 2)
}

fn node_side(node: u32) -> Side {
    match node % 2 {
        0 => Side::Target,
        _ => Side::Host,
    }
}

fn own<'a>(s: &str, alloc: Alloc<'a>) -> Text<'a> {
    String::from_str_in(s, alloc)
}

/// The sans-IO request: the lock, the platforms and the ask; packages answered as asked.
pub struct Resolution<'l, 'a> {
    alloc: Alloc<'a>,
    lock: &'l Lock<'a>,
    target: &'l TargetDesc<'a>,
    host: &'l TargetDesc<'a>,
    ask: &'l Ask<'a>,
    resolver: Resolver,
    members: AVec<'a, bool>,
    answers: AVec<'a, Answer<'a>>,
    /// Per package, per dependency entry: whether its platform holds on (target, host).
    holds: AVec<'a, AVec<'a, (bool, bool)>>,
    plant: Plant,
}

impl<'l, 'a> Resolution<'l, 'a> {
    pub fn new(lock: &'l Lock<'a>, target: &'l TargetDesc<'a>, host: &'l TargetDesc<'a>, ask: &'l Ask<'a>, resolver: Resolver, alloc: Alloc<'a>) -> Resolution<'l, 'a> {
        let n = lock.packages.len();
        let mut members = Vec::with_capacity_in(n, alloc);
        members.resize(n, false);
        let mut answers = Vec::with_capacity_in(n, alloc);
        let mut holds = Vec::with_capacity_in(n, alloc);
        for _ in 0..n {
            answers.push(Answer::Pending);
            holds.push(Vec::new_in(alloc));
        }
        Resolution { alloc, lock, target, host, ask, resolver, members, answers, holds, plant: Plant::None }
    }

    #[cfg(test)]
    pub(crate) fn plant(&mut self, plant: Plant) {
        self.plant = plant;
    }

    /// A workspace member's facts (its dev-dependencies count; it may be a root).
    pub fn member(&mut self, id: LockId, package: Package<'a>) -> Result<(), UnitError<'a>> {
        self.members[id.index()] = true;
        self.package(id, Some(package))
    }

    /// The answer to [`crate::Need`] `id`: its facts, or `None` when its manifest is not
    /// available.
    pub fn package(&mut self, id: LockId, package: Option<Package<'a>>) -> Result<(), UnitError<'a>> {
        let Some(package) = package else {
            self.answers[id.index()] = Answer::Unavailable;
            return Ok(());
        };
        let mut holds = Vec::with_capacity_in(package.deps.len(), self.alloc);
        for dep in package.deps.iter() {
            let pair = match &dep.platform {
                None => (true, true),
                Some(platform) => {
                    let on = |desc: &TargetDesc<'_>| {
                        desc.holds(platform, self.alloc).map_err(|error| UnitError::Platform {
                            package: package.name.clone(),
                            platform: match platform {
                                crate::input::Platform::Triple(t) | crate::input::Platform::Cfg(t) => t.clone(),
                            },
                            error,
                        })
                    };
                    (on(self.target)?, on(self.host)?)
                }
            };
            holds.push(pair);
        }
        self.holds[id.index()] = holds;
        self.answers[id.index()] = Answer::Read(package);
        Ok(())
    }

    /// The graph, or the packages whose facts it lacks.
    pub fn pass(&self) -> Result<Pass<'a>, UnitError<'a>> {
        self.run(&mut |n| n - 1)
    }

    /// [`Resolution::pass`] taking the work in the order `pick` chooses (it is handed the
    /// worklist's length and returns an index).
    pub(crate) fn run(&self, pick: &mut dyn FnMut(usize) -> usize) -> Result<Pass<'a>, UnitError<'a>> {
        let state = self.fixpoint(pick)?;
        if !state.needs.is_empty() {
            let mut needs = state.needs;
            needs.sort_unstable();
            needs.dedup();
            return Ok(Pass::Need(needs));
        }
        self.units(&state)
    }

    fn decouple(&self) -> bool {
        self.resolver.decouples_host()
    }

    /// The side a node of `side` keeps its features under: one namespace under resolver 1.
    fn fside(&self, side: Side) -> Side {
        match self.decouple() {
            true => side,
            false => Side::Target,
        }
    }

    /// The facts of `id`; `None` (and a need when not asked yet) when there are none.
    fn facts<'s>(&'s self, id: LockId, needs: &mut AVec<'a, LockId>) -> Option<&'s Package<'a>> {
        match &self.answers[id.index()] {
            Answer::Read(package) => Some(package),
            Answer::Unavailable => None,
            Answer::Pending => {
                needs.push(id);
                None
            }
        }
    }

    /// Whether dependency entry `e` of `id` takes part from a node of `side`.
    fn passes(&self, id: LockId, package: &Package<'_>, side: Side, e: usize) -> bool {
        let dep = &package.deps[e];
        if dep.kind == DepKind::Dev {
            // Dev-dependencies are a workspace member's only (a published package's are not
            // in the lock), and take part when tests are built; resolver 1 always unifies them.
            let wanted = (self.ask.tests || self.resolver == Resolver::V1 || self.plant == Plant::DevLeaks) && self.plant != Plant::DevIgnored;
            if !self.members[id.index()] || !wanted {
                return false;
            }
        }
        if !self.decouple() {
            // Resolver 1: a target-specific dependency's features count on every target.
            return true;
        }
        let (on_target, on_host) = self.holds[id.index()][e];
        match dep.kind == DepKind::Build || side == Side::Host {
            true => on_host,
            false => on_target,
        }
    }

    /// The locked package dependency entry `e` of `id` names.
    fn locked(&self, id: LockId, package: &Package<'a>, e: usize) -> Result<LockId, UnitError<'a>> {
        let dep = &package.deps[e];
        let me = self.lock.package(id);
        let mut found: Option<LockId> = None;
        let mut count = 0usize;
        let named = || me.deps.iter().copied().filter(|d| self.lock.package(*d).name == dep.package.as_str());
        for d in named() {
            found = Some(d);
            count += 1;
        }
        if count > 1 {
            // Several of the name: by the kind of source, then by the requirement.
            let kind_fits = |d: LockId, allow_patched_path: bool| match (&dep.source, &self.lock.package(d).source) {
                (DepSource::Path, source) => core::mem::discriminant(source) == core::mem::discriminant(&me.source),
                (DepSource::Registry { .. }, LockSource::Registry { .. }) => true,
                (DepSource::Registry { .. } | DepSource::Git { .. }, LockSource::Path) => allow_patched_path,
                (DepSource::Git { url }, LockSource::Git { url: locked, .. }) => url.trim_end_matches(".git") == locked.trim_end_matches(".git"),
                _ => false,
            };
            let req = match &dep.source {
                DepSource::Registry { req: Some(req) } => match blaze_semver::VersionReq::parse_in(req, blaze_semver::Limits::CARGO, self.alloc) {
                    Ok(req) => Some(req),
                    Err(_) => return Err(UnitError::Requirement { package: package.name.clone(), dep: dep.key.clone(), req: req.clone() }),
                },
                _ => None,
            };
            found = None;
            count = 0;
            for d in named() {
                if kind_fits(d, false) && req.as_ref().is_none_or(|req| req.matches(&self.lock.package(d).version)) {
                    found = Some(d);
                    count += 1;
                }
            }
            if count == 0 {
                // A `[patch.*]` entry can replace a registry or git dependency with a checkout
                // path package (`LockSource::Path`).
                for d in named() {
                    if kind_fits(d, true) && req.as_ref().is_none_or(|req| req.matches(&self.lock.package(d).version)) {
                        found = Some(d);
                        count += 1;
                    }
                }
                if count > 1
                    && let DepSource::Git { url } = &dep.source
                {
                    let want = url.trim_end_matches(".git");
                    let mut narrowed = None;
                    let mut narrowed_count = 0usize;
                    for d in named() {
                        if kind_fits(d, true)
                            && let Some(Answer::Read(candidate)) = self.answers.get(d.index())
                            && candidate.patched_from.iter().any(|src| src.trim_end_matches(".git") == want)
                        {
                            narrowed = Some(d);
                            narrowed_count += 1;
                        }
                    }
                    if narrowed_count == 1 {
                        found = narrowed;
                        count = 1;
                    }
                }
            }
        }
        match (found, count) {
            (Some(d), 1) => Ok(d),
            (_, 0) => Err(UnitError::NotLocked { package: package.name.clone(), dep: dep.key.clone() }),
            _ => Err(UnitError::AmbiguousLock { package: package.name.clone(), dep: dep.key.clone() }),
        }
    }

    /// The side the package an entry names is on, from a node of `side`.
    fn child_side(&self, side: Side, kind: DepKind, child: &Package<'_>) -> Side {
        match self.decouple() && (kind == DepKind::Build || child.is_proc_macro()) {
            true => Side::Host,
            false => side,
        }
    }

    fn no_feature(&self, package: &Package<'a>, feature: &str, by: &str) -> UnitError<'a> {
        UnitError::NoSuchFeature { package: package.name.clone(), feature: own(feature, self.alloc), asked_by: own(by, self.alloc) }
    }

    fn fixpoint<'s>(&'s self, pick: &mut dyn FnMut(usize) -> usize) -> Result<State<'s, 'a>, UnitError<'a>> {
        let alloc = self.alloc;
        let nodes = self.lock.packages.len() * 2;
        let mut st = State {
            feats: Vec::with_capacity_in(nodes, alloc),
            visited: Vec::with_capacity_in(nodes, alloc),
            active: Vec::with_capacity_in(nodes, alloc),
            deferred: Vec::new_in(alloc),
            needs: Vec::new_in(alloc),
            work: Vec::new_in(alloc),
        };
        for _ in 0..nodes {
            st.feats.push(Vec::new_in(alloc));
            st.active.push(Vec::new_in(alloc));
        }
        st.visited.resize(nodes, false);
        self.seed(&mut st)?;
        while !st.work.is_empty() {
            let at = pick(st.work.len());
            let work = st.work.swap_remove(at);
            self.step(&mut st, work)?;
        }
        Ok(st)
    }

    /// The roots' features, from the command line's flags.
    fn seed<'s>(&'s self, st: &mut State<'s, 'a>) -> Result<(), UnitError<'a>> {
        let ask = self.ask;
        let mut roots: AVec<'_, (LockId, &'s Package<'a>)> = Vec::new_in(self.alloc);
        for &root in ask.roots.iter() {
            match &self.answers[root.index()] {
                Answer::Read(package) => roots.push((root, package)),
                Answer::Pending | Answer::Unavailable => return Err(UnitError::RootUnanswered { root }),
            }
        }
        // A proc-macro root is built for the host, and its other targets for the target: its
        // features are turned on in both namespaces (cargo's `FeatureResolver::do_resolve`).
        let sides = |package: &Package<'_>| match self.decouple() && package.is_proc_macro() {
            true => [Some(Side::Target), Some(Side::Host)],
            false => [Some(Side::Target), None],
        };
        for &(root, package) in roots.iter() {
            for side in sides(package).into_iter().flatten() {
                let n = node(root, self.fside(side));
                if !ask.no_default_features && package.feature("default").is_some() {
                    st.work.push(Work::Value(n, V::Feature("default"), "the command line"));
                }
                if ask.all_features {
                    for feature in package.features.iter() {
                        st.work.push(Work::Value(n, V::Feature(&feature.name), "--all-features"));
                    }
                }
                st.work.push(Work::Visit(n));
            }
        }
        for text in ask.features.iter() {
            let mut any = false;
            for &(root, package) in roots.iter() {
                let value = match text.split_once('/') {
                    Some((p, f)) if p == package.name.as_str() => Some(V::Feature(f)),
                    Some((p, _)) => package.deps.iter().any(|d| d.key.as_str() == p).then(|| V::parse(text)),
                    None => package.feature(text).map(|_| V::Feature(text)),
                };
                if let Some(value) = value {
                    any = true;
                    for side in sides(package).into_iter().flatten() {
                        st.work.push(Work::Value(node(root, self.fside(side)), value, "--features"));
                    }
                }
            }
            if !any {
                return Err(UnitError::NoRootHasFeature { feature: text.clone() });
            }
        }
        Ok(())
    }

    fn step<'s>(&'s self, st: &mut State<'s, 'a>, work: Work<'s>) -> Result<(), UnitError<'a>> {
        match work {
            Work::Visit(n) => {
                if st.visited[n as usize] {
                    return Ok(());
                }
                let id = node_id(n);
                let Some(package) = self.facts(id, &mut st.needs) else { return Ok(()) };
                st.visited[n as usize] = true;
                for (e, dep) in package.deps.iter().enumerate() {
                    if !dep.optional && self.passes(id, package, node_side(n), e) {
                        st.work.push(Work::Edge(n, e as u32));
                    }
                }
            }
            Work::Edge(n, e) => {
                let id = node_id(n);
                let Some(package) = self.facts(id, &mut st.needs) else { return Ok(()) };
                let dep = &package.deps[e as usize];
                let child_id = self.locked(id, package, e as usize)?;
                let Some(child) = self.facts(child_id, &mut st.needs) else { return Ok(()) };
                let cn = node(child_id, self.fside(self.child_side(node_side(n), dep.kind, child)));
                if dep.default_features && child.feature("default").is_some() {
                    st.work.push(Work::Value(cn, V::Feature("default"), &package.name));
                }
                for feature in dep.features.iter() {
                    st.work.push(Work::Value(cn, V::parse(feature), &package.name));
                }
                st.work.push(Work::Visit(cn));
            }
            Work::DepFeat(n, e, feature) => {
                let id = node_id(n);
                let Some(package) = self.facts(id, &mut st.needs) else { return Ok(()) };
                let dep = &package.deps[e as usize];
                let child_id = self.locked(id, package, e as usize)?;
                let Some(child) = self.facts(child_id, &mut st.needs) else { return Ok(()) };
                let cn = node(child_id, self.fside(self.child_side(node_side(n), dep.kind, child)));
                st.work.push(Work::Value(cn, V::parse(feature), &package.name));
            }
            Work::Value(n, value, by) => self.value(st, n, value, by)?,
        }
        Ok(())
    }

    fn value<'s>(&'s self, st: &mut State<'s, 'a>, n: u32, value: V<'s>, by: &'s str) -> Result<(), UnitError<'a>> {
        let id = node_id(n);
        let Some(package) = self.facts(id, &mut st.needs) else { return Ok(()) };
        let side = node_side(n);
        let no_dep = |name: &str| UnitError::NoSuchDependency { package: package.name.clone(), dep: own(name, self.alloc) };
        match value {
            V::Feature(name) => {
                let Some(f) = package.feature(name) else {
                    // The planted bug: `dep:x` hid the implicit feature `x`, and it is made anyway.
                    if self.plant == Plant::DepMakesImplicit && package.deps.iter().any(|d| d.optional && d.key.as_str() == name) {
                        st.work.push(Work::Value(n, V::Dep(name), by));
                        return Ok(());
                    }
                    return Err(self.no_feature(package, name, by));
                };
                if set(&mut st.feats[n as usize], f) {
                    for value in package.features[f].values.iter() {
                        st.work.push(Work::Value(n, V::of(value), &package.name));
                    }
                }
            }
            V::Dep(name) => {
                if !package.deps.iter().any(|d| d.key.as_str() == name) {
                    return Err(no_dep(name));
                }
                for (e, dep) in package.deps.iter().enumerate() {
                    if dep.key.as_str() != name || !self.passes(id, package, side, e) {
                        continue;
                    }
                    if dep.optional {
                        set(&mut st.active[n as usize], e);
                    }
                    st.work.push(Work::Edge(n, e as u32));
                    for &(dn, dname, feature) in st.deferred.iter() {
                        if dn == n && dname == name {
                            st.work.push(Work::DepFeat(n, e as u32, feature));
                        }
                    }
                }
            }
            V::DepFeature(name, feature, weak) => {
                if !package.deps.iter().any(|d| d.key.as_str() == name) {
                    return Err(no_dep(name));
                }
                for (e, dep) in package.deps.iter().enumerate() {
                    if dep.key.as_str() != name || !self.passes(id, package, side, e) {
                        continue;
                    }
                    if dep.optional {
                        let on = get(&st.active[n as usize], e);
                        if weak && !on && self.plant != Plant::WeakEnables {
                            st.deferred.push((n, name, feature));
                            continue;
                        }
                        st.work.push(Work::Value(n, V::Dep(name), by));
                        // `x/f` also turns on the feature named `x` when there is one (an
                        // implicit feature, or one written so); `x?/f` never does.
                        if !weak && package.feature(name).is_some() {
                            st.work.push(Work::Value(n, V::Feature(name), by));
                        }
                    }
                    st.work.push(Work::DepFeat(n, e as u32, feature));
                }
            }
        }
        Ok(())
    }

    /// The `required-features` entries of the root `id`'s target `kind` `name` that are off,
    /// sorted as `required` is, by the fixpoint's features of the root's target node (the
    /// Cargo Book, "Cargo Targets", "The `required-features` field": a feature of the package,
    /// or `dep/feature`, which holds when the dependency is on with that feature); empty when
    /// all are on. A name the package lacks is the package's error, never a skip. A dependency
    /// whose facts are not answered keeps the target (empty): the walk reports that package as
    /// needed or unavailable. An integration test, example or bench builds with the
    /// dev-dependencies (rule 8), so its `dep/feature` may name one; a binary's may not.
    fn required_off<'s>(&'s self, st: &State<'s, 'a>, id: LockId, package: &Package<'a>, kind: LeftOutKind, name: &str, required: &[Text<'a>]) -> Result<AVec<'a, Text<'a>>, UnitError<'a>> {
        let n = node(id, Side::Target) as usize;
        let mut off = Vec::new_in(self.alloc);
        let asked_by = || {
            let mut by = String::new_in(self.alloc);
            by.push_str("the required-features of ");
            by.push_str(kind.table());
            by.push(' ');
            by.push_str(name);
            by
        };
        let with_dev = kind != LeftOutKind::Bin;
        for text in required.iter() {
            let Some((dep_key, feature)) = text.as_str().split_once('/') else {
                match package.feature(text.as_str()) {
                    Some(f) if get(&st.feats[n], f) => {}
                    Some(_) => off.push(text.clone()),
                    None => return Err(UnitError::NoSuchFeature { package: package.name.clone(), feature: text.clone(), asked_by: asked_by() }),
                }
                continue;
            };
            let mut named = false;
            let mut on = false;
            for (e, dep) in package.deps.iter().enumerate() {
                let kind_counts = dep.kind == DepKind::Normal || (with_dev && dep.kind == DepKind::Dev);
                if dep.key.as_str() != dep_key || !kind_counts {
                    continue;
                }
                named = true;
                let (on_target, _) = self.holds[id.index()][e];
                if !on_target || (dep.optional && !get(&st.active[n], e)) {
                    continue;
                }
                let child_id = self.locked(id, package, e)?;
                let Answer::Read(child) = &self.answers[child_id.index()] else { return Ok(Vec::new_in(self.alloc)) };
                let Some(f) = child.feature(feature) else {
                    return Err(UnitError::NoSuchFeature { package: child.name.clone(), feature: own(feature, self.alloc), asked_by: asked_by() });
                };
                let cn = node(child_id, self.fside(self.child_side(Side::Target, dep.kind, child))) as usize;
                on |= get(&st.feats[cn], f);
            }
            if !named {
                return Err(UnitError::NoSuchDependency { package: package.name.clone(), dep: own(dep_key, self.alloc) });
            }
            if !on {
                off.push(text.clone());
            }
        }
        Ok(off)
    }

    /// The units, walked from the roots.
    fn units<'s>(&'s self, st: &State<'s, 'a>) -> Result<Pass<'a>, UnitError<'a>> {
        let alloc = self.alloc;
        let mut needs: AVec<'a, LockId> = Vec::new_in(alloc);
        let mut unavailable: AVec<'a, LockId> = Vec::new_in(alloc);
        let mut keys: AVec<'a, Key> = Vec::new_in(alloc);
        let mut index: BTreeMap<Key, u32, Alloc<'a>> = BTreeMap::new_in(alloc);
        let mut tmp: AVec<'a, Tmp<'s, 'a>> = Vec::new_in(alloc);
        let mut left_out: AVec<'a, LeftOut<'a>> = Vec::new_in(alloc);
        // Each unit is added as the target it compiles (finding 9): its key's kind is the
        // target's, and its crate name and root are made from it, never an empty text for a
        // kind its package lacks.
        let add = |id: LockId, target: Target<'s, 'a>, platform: Side, fside: Side, keys: &mut AVec<'a, Key>, index: &mut BTreeMap<Key, u32, Alloc<'a>>, tmp: &mut AVec<'a, Tmp<'s, 'a>>| -> u32 {
            let key = Key::new(id, target.kind(), target.idx(), platform, fside);
            if let Some(&at) = index.get(&key) {
                return at;
            }
            let at = keys.len() as u32;
            keys.push(key);
            index.insert(key, at);
            tmp.push(Tmp { target, deps: Vec::new_in(alloc), build_script: None });
            at
        };
        for &root in self.ask.roots.iter() {
            let Answer::Read(package) = &self.answers[root.index()] else { return Err(UnitError::RootUnanswered { root }) };
            if let Some(lib) = &package.lib {
                match lib.proc_macro {
                    true => add(root, Target::ProcMacro(lib), Side::Host, self.fside(Side::Host), &mut keys, &mut index, &mut tmp),
                    false => add(root, Target::Lib(lib), Side::Target, Side::Target, &mut keys, &mut index, &mut tmp),
                };
                if self.ask.tests {
                    // A proc-macro library's test harness is that library's target compiled as a
                    // test, so it is built for the host and resolves its dependencies' features
                    // on the host's side, as the proc macro does (cargo: a target that is for the
                    // host, a proc macro's, is compiled for the host whatever `--target` says, and
                    // its features are the host dependency's). On the target side it took the
                    // target's unified features: tempo's `tempo-precompiles-macros` harness then
                    // reached `alloy-primitives` with the node's `asm-keccak`, and the macro side
                    // was asked to run sha3-asm's native build script (check-g044908).
                    match lib.proc_macro {
                        true => add(root, Target::TestLib(lib), Side::Host, self.fside(Side::Host), &mut keys, &mut index, &mut tmp),
                        false => add(root, Target::TestLib(lib), Side::Target, Side::Target, &mut keys, &mut index, &mut tmp),
                    };
                }
            }
            // A target whose `required-features` are not all on is skipped (the Cargo Book,
            // "Cargo Targets", "The `required-features` field": "If any of the required
            // features are not enabled, the target will be skipped."), and said: a
            // [`LeftOut`] naming it and what is off (rule 3). Checked anyway, tempo's
            // `tempo-localnet` (`required-features = ["localnet"]`, `localnet = ["dep:nix"]`)
            // was three false "cannot find `nix`"; turso's bench `logical_log_serialization`
            // (`bench`) and example `sync_example` (`sync`) were checked without their
            // features (c1814).
            let mut skipped = |kind: LeftOutKind, name: &Text<'a>, off: AVec<'a, Text<'a>>| {
                left_out.push(LeftOut { package: root, package_name: package.name.clone(), kind, name: name.clone(), off });
            };
            for i in 0..package.bins.len() as u32 {
                // Its test unit goes with it.
                let bin = &package.bins[i as usize];
                let off = self.required_off(st, root, package, LeftOutKind::Bin, bin.name.as_str(), &bin.required_features)?;
                if !off.is_empty() {
                    skipped(LeftOutKind::Bin, &bin.name, off);
                    continue;
                }
                add(root, Target::Bin(i, bin), Side::Target, Side::Target, &mut keys, &mut index, &mut tmp);
                if self.ask.tests {
                    add(root, Target::TestBin(i, bin), Side::Target, Side::Target, &mut keys, &mut index, &mut tmp);
                }
            }
            // Integration tests, examples and benches: by the same switch as the library's
            // test unit (test.md, "The other targets").
            // Roots only: a dependency's layout lists none of these (the layout reads them only
            // with `with_test_targets`, for the workspace the user builds).
            if self.ask.tests {
                for (i, extra) in package.extras.iter().enumerate() {
                    let kind = match extra.kind {
                        ExtraKind::Test => LeftOutKind::Test,
                        ExtraKind::Example => LeftOutKind::Example,
                        ExtraKind::Bench => LeftOutKind::Bench,
                    };
                    let off = self.required_off(st, root, package, kind, extra.name.as_str(), &extra.required_features)?;
                    if !off.is_empty() {
                        skipped(kind, &extra.name, off);
                        continue;
                    }
                    add(root, Target::Extra(i as u32, extra), Side::Target, Side::Target, &mut keys, &mut index, &mut tmp);
                }
            }
        }
        // By (package, kind, name), each once (a root asked twice is one): the record's order.
        left_out.sort_by(|a: &LeftOut<'a>, b: &LeftOut<'a>| (a.package, a.kind, a.name.as_str()).cmp(&(b.package, b.kind, b.name.as_str())));
        left_out.dedup_by(|a, b| a.package == b.package && a.kind == b.kind && a.name == b.name);
        let mut next = 0usize;
        while next < keys.len() {
            let key = keys[next];
            let at = next as u32;
            next += 1;
            let id = LockId(key.id);
            let Answer::Read(package) = &self.answers[id.index()] else { continue };
            let n = node(id, key.fside);
            let mut deps: AVec<'a, (Text<'a>, u32)> = Vec::new_in(alloc);
            for (e, dep) in package.deps.iter().enumerate() {
                let wanted = match key.kind {
                    Kind::BuildScript => dep.kind == DepKind::Build,
                    Kind::TestLib | Kind::TestBin | Kind::Extra => dep.kind == DepKind::Normal || (dep.kind == DepKind::Dev && self.members[id.index()]),
                    Kind::Lib | Kind::ProcMacro | Kind::Bin => dep.kind == DepKind::Normal,
                };
                if !wanted {
                    continue;
                }
                let (on_target, on_host) = self.holds[id.index()][e];
                let on = match key.kind == Kind::BuildScript || key.platform == Side::Host {
                    true => on_host,
                    false => on_target,
                };
                if !on || (dep.optional && !get(&st.active[n as usize], e)) {
                    continue;
                }
                let child_id = self.locked(id, package, e)?;
                let child = match &self.answers[child_id.index()] {
                    Answer::Read(child) => child,
                    Answer::Unavailable => {
                        unavailable.push(child_id);
                        continue;
                    }
                    Answer::Pending => {
                        needs.push(child_id);
                        continue;
                    }
                };
                let Some(lib) = &child.lib else { continue };
                let platform = match key.kind == Kind::BuildScript || lib.proc_macro {
                    true => Side::Host,
                    false => key.platform,
                };
                let name = match dep.renamed() {
                    true => underscored(&dep.key, alloc),
                    false => lib.crate_name.clone(),
                };
                let cu = match lib.proc_macro {
                    true => add(child_id, Target::ProcMacro(lib), Side::Host, self.fside(Side::Host), &mut keys, &mut index, &mut tmp),
                    false => add(child_id, Target::Lib(lib), platform, self.fside(platform), &mut keys, &mut index, &mut tmp),
                };
                deps.push((name, cu));
            }
            // A binary, and an integration test, example or bench, uses its package's plain
            // library unit (never the library's test unit).
            if matches!(key.kind, Kind::Bin | Kind::TestBin | Kind::Extra)
                && let Some(lib) = &package.lib
            {
                let cu = match lib.proc_macro {
                    true => add(id, Target::ProcMacro(lib), Side::Host, self.fside(Side::Host), &mut keys, &mut index, &mut tmp),
                    false => add(id, Target::Lib(lib), Side::Target, Side::Target, &mut keys, &mut index, &mut tmp),
                };
                deps.push((lib.crate_name.clone(), cu));
            }
            let build_script = match (key.kind != Kind::BuildScript, &package.build) {
                (true, Some(script)) => Some(add(id, Target::BuildScript(script), Side::Host, key.fside, &mut keys, &mut index, &mut tmp)),
                _ => None,
            };
            deps.sort_by(|a, b| (a.0.as_str(), a.1).cmp(&(b.0.as_str(), b.1)));
            deps.dedup_by(|a, b| a.0 == b.0 && a.1 == b.1);
            tmp[at as usize].deps = deps;
            tmp[at as usize].build_script = build_script;
        }
        if !needs.is_empty() {
            needs.sort_unstable();
            needs.dedup();
            return Ok(Pass::Need(needs));
        }
        // A package reached without its manifest: no graph, the missing named (states.md 35).
        if let Some(partial) = PartialGraph::of(unavailable) {
            return Ok(Pass::Partial(partial));
        }
        // The order: by package (name, version, source), then kind, then configuration; ids
        // are positions in it, so the bytes do not depend on the lock's order or the walk's.
        let mut order: AVec<'a, u32> = Vec::with_capacity_in(keys.len(), alloc);
        for i in 0..keys.len() as u32 {
            order.push(i);
        }
        let sort_key = |k: &Key| {
            let p = self.lock.package(LockId(k.id));
            (p.name.as_str(), &p.version, p.source_text.as_str(), k.kind, k.idx, k.platform, k.fside)
        };
        order.sort_by(|a, b| sort_key(&keys[*a as usize]).cmp(&sort_key(&keys[*b as usize])));
        let mut new_id: AVec<'a, u32> = Vec::with_capacity_in(keys.len(), alloc);
        new_id.resize(keys.len(), 0);
        for (pos, &old) in order.iter().enumerate() {
            new_id[old as usize] = pos as u32;
        }
        let mut units = Vec::with_capacity_in(keys.len(), alloc);
        for &old in order.iter() {
            let key = keys[old as usize];
            let id = LockId(key.id);
            let Answer::Read(package) = &self.answers[id.index()] else { continue };
            let t = &tmp[old as usize];
            units.push(self.unit(st, key, package, t, &new_id));
        }
        // Each package of a unit with its metadata, once, by id (`UnitGraph::meta`).
        let mut ids: AVec<'a, LockId> = Vec::with_capacity_in(units.len(), alloc);
        ids.extend(units.iter().map(|unit: &Unit<'a>| unit.package));
        ids.sort_unstable();
        ids.dedup();
        let mut meta = Vec::with_capacity_in(ids.len(), alloc);
        for id in ids.iter().copied() {
            if let Answer::Read(package) = &self.answers[id.index()] {
                meta.push((id, package.meta.clone()));
            }
        }
        Ok(Pass::Done(UnitGraph { units, resolver: self.resolver, meta, left_out }))
    }

    fn unit(&self, st: &State<'_, 'a>, key: Key, package: &Package<'a>, t: &Tmp<'_, 'a>, new_id: &[u32]) -> Unit<'a> {
        let alloc = self.alloc;
        let id = LockId(key.id);
        let n = node(id, key.fside);
        let mut features = Vec::new_in(alloc);
        for (f, def) in package.features.iter().enumerate() {
            if get(&st.feats[n as usize], f) {
                features.push(def.name.clone());
            }
        }
        let desc = match key.platform {
            Side::Target => self.target,
            Side::Host => self.host,
        };
        // The unit's target, added with it (its key's kind is the target's).
        let (kind, crate_name, root) = match t.target {
            Target::Lib(lib) => (UnitKind::Lib, lib.crate_name.clone(), lib.root.clone()),
            Target::ProcMacro(lib) => (UnitKind::ProcMacro, lib.crate_name.clone(), lib.root.clone()),
            Target::TestLib(lib) => (UnitKind::Test { bin: None }, lib.crate_name.clone(), lib.root.clone()),
            Target::Bin(_, bin) => (UnitKind::Bin(bin.name.clone()), underscored(&bin.name, alloc), bin.root.clone()),
            Target::TestBin(_, bin) => (UnitKind::Test { bin: Some(bin.name.clone()) }, underscored(&bin.name, alloc), bin.root.clone()),
            Target::BuildScript(script) => (UnitKind::BuildScript, own("build_script_build", alloc), script.clone()),
            Target::Extra(_, extra) => (extra.unit_kind(), underscored(&extra.name, alloc), extra.root.clone()),
        };
        // `test` by the kind: the library's and a binary's test units, and a harnessed
        // integration test or bench (the one rule, also the record's).
        let cfg = unit_cfg(desc, &features, has_test_cfg(&kind), ignores_profile_panic(&kind), key.kind == Kind::ProcMacro, alloc);
        let mut deps = Vec::with_capacity_in(t.deps.len(), alloc);
        for (name, cu) in t.deps.iter() {
            deps.push(UnitDep { name: name.clone(), unit: UnitId(new_id[*cu as usize]) });
        }
        let locked = self.lock.package(id);
        Unit {
            package: id,
            name: package.name.clone(),
            version: {
                let mut v = String::new_in(alloc);
                let _ = core::fmt::Write::write_fmt(&mut v, format_args!("{}", locked.version));
                v
            },
            kind,
            for_host: key.platform == Side::Host,
            edition: package.edition,
            crate_name,
            root,
            features,
            cfg,
            deps,
            build_script: t.build_script.map(|b| UnitId(new_id[b as usize])),
            links: package.links.clone(),
        }
    }
}

/// `name` with `-` as `_`: a crate name.
fn underscored<'a>(name: &str, alloc: Alloc<'a>) -> Text<'a> {
    let mut out = String::with_capacity_in(name.len(), alloc);
    for ch in name.chars() {
        out.push(if ch == '-' { '_' } else { ch });
    }
    out
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
#[repr(u8)]
enum Kind {
    Lib,
    ProcMacro,
    Bin,
    BuildScript,
    TestLib,
    TestBin,
    /// An integration test, example or bench: `idx` is its place in [`Package::extras`].
    Extra,
}

/// A unit's cfg set (rule 13): its platform's atoms, `feature = "f"` for each feature on,
/// `debug_assertions` (`check` builds the dev profile, whose `debug-assertions` is on, build
/// overrides included), `test` on a test harness and `proc_macro` on a proc macro; sorted.
/// The one place it is made: the record ([`crate::record`]) holds none and remakes it here.
///
/// `unwinds` ([`ignores_profile_panic`]): the unit is a test or bench, which Cargo builds with
/// `unwind` whatever the profile says (the Cargo Book, Profiles, `panic`: "Tests, benchmarks,
/// build scripts, and proc macros ignore the `panic` setting. The `rustc` test harness currently
/// requires `unwind` behavior"): its `panic` atom is `unwind` in place of the desc's (the
/// profile's). Seen 2026-11-21: a `[profile.dev] panic = "abort"` workspace's lib-test unit cut
/// its `#[cfg(panic = "unwind")]` tests and `blaze test` ran 0 (blaze-cli test_command_runs.rs
/// `runs_both_workspace_packages`).
pub(crate) fn unit_cfg<'a>(desc: &TargetDesc<'a>, features: &[Text<'a>], test: bool, unwinds: bool, proc_macro: bool, alloc: Alloc<'a>) -> AVec<'a, CfgAtom<'a>> {
    let mut cfg = Vec::with_capacity_in(desc.cfg.len() + features.len() + 4, alloc);
    if unwinds {
        cfg.extend(desc.cfg.iter().filter(|atom| atom.name.as_str() != PANIC_KEY).cloned());
        cfg.push(CfgAtom::pair(PANIC_KEY, "unwind", alloc));
    } else {
        cfg.extend(desc.cfg.iter().cloned());
    }
    for feature in features.iter() {
        cfg.push(CfgAtom { name: own("feature", alloc), value: Some(feature.clone()) });
    }
    cfg.push(CfgAtom::flag("debug_assertions", alloc));
    if test {
        cfg.push(CfgAtom::flag("test", alloc));
    }
    if proc_macro {
        cfg.push(CfgAtom::flag("proc_macro", alloc));
    }
    cfg.sort();
    cfg.dedup();
    cfg
}

/// The cfg key of the panic strategy (the Reference, conditional compilation, `panic`;
/// blaze-world panic.rs reads the same name).
pub(crate) const PANIC_KEY: &str = "panic";

/// Whether a unit of `kind` ignores the profile's `panic` and unwinds: a test harness (the
/// library's or a binary's), an integration test or a bench, harnessed or not (Cargo's rule,
/// quoted at [`unit_cfg`]). The one rule; blaze-world panic.rs `unit_panic` asks it.
pub fn ignores_profile_panic(kind: &UnitKind<'_>) -> bool {
    matches!(kind, UnitKind::Test { .. } | UnitKind::IntegrationTest(_) | UnitKind::Bench(_))
}

/// A unit's identity: the package, the target, the platform it is built for and the feature
/// namespace its features are from.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
struct Key {
    id: u32,
    kind: Kind,
    idx: u32,
    platform: Side,
    fside: Side,
}

impl Key {
    fn new(id: LockId, kind: Kind, idx: u32, platform: Side, fside: Side) -> Key {
        Key { id: id.0, kind, idx, platform, fside }
    }
}

/// The target of the package a unit compiles, borrowed from its facts, with its place among
/// the package's binaries or extras: what a unit is added as, so its key's kind and its crate
/// name and root are always the target's (finding 9).
#[derive(Clone, Copy)]
enum Target<'s, 'a> {
    Lib(&'s Lib<'a>),
    ProcMacro(&'s Lib<'a>),
    /// The library's test harness.
    TestLib(&'s Lib<'a>),
    Bin(u32, &'s Bin<'a>),
    /// A binary's test harness.
    TestBin(u32, &'s Bin<'a>),
    /// The build script's root.
    BuildScript(&'s Text<'a>),
    /// An integration test, example or bench.
    Extra(u32, &'s Extra<'a>),
}

impl Target<'_, '_> {
    fn kind(self) -> Kind {
        match self {
            Target::Lib(_) => Kind::Lib,
            Target::ProcMacro(_) => Kind::ProcMacro,
            Target::TestLib(_) => Kind::TestLib,
            Target::Bin(..) => Kind::Bin,
            Target::TestBin(..) => Kind::TestBin,
            Target::BuildScript(_) => Kind::BuildScript,
            Target::Extra(..) => Kind::Extra,
        }
    }

    /// Its place among the package's binaries or extras; 0 for the one library and script.
    fn idx(self) -> u32 {
        match self {
            Target::Bin(i, _) | Target::TestBin(i, _) | Target::Extra(i, _) => i,
            Target::Lib(_) | Target::ProcMacro(_) | Target::TestLib(_) | Target::BuildScript(_) => 0,
        }
    }
}

struct Tmp<'s, 'a> {
    target: Target<'s, 'a>,
    deps: AVec<'a, (Text<'a>, u32)>,
    build_script: Option<u32>,
}
