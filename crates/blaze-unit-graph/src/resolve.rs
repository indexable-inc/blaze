//! The machine: the feature fixpoint over (package, side) nodes, then the units walked from the
//! roots. Both are explicit worklists (no recursion); every set grows only, so the fixpoint is
//! reached whatever order the work is taken in (the property tests take it in drawn orders).

use blaze_manifest::layout::DepKind;
use blaze_manifest::lock::{Lock, LockId, LockSource};
use blaze_own::{AVec, Alloc, BTreeMap, String, Vec};

use crate::input::{Ask, CfgAtom, DepSource, FeatureValue, Package, Resolver, TargetDesc};
use crate::extra::has_test_cfg;
use crate::{Pass, Plant, Text, Unit, UnitDep, UnitError, UnitGraph, UnitId, UnitKind};

/// An answer to a [`Pass::Need`].
pub(crate) enum Answer<'a> {
    Pending,
    /// No manifest on disk for it: listed in [`UnitGraph::unavailable`], never guessed.
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
            let wanted = self.ask.tests || self.resolver == Resolver::V1 || self.plant == Plant::DevLeaks;
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
            let kind_fits = |d: LockId| match (&dep.source, &self.lock.package(d).source) {
                (DepSource::Path, source) => core::mem::discriminant(source) == core::mem::discriminant(&me.source),
                (DepSource::Registry { .. }, LockSource::Registry { .. }) => true,
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
                if kind_fits(d) && req.as_ref().is_none_or(|req| req.matches(&self.lock.package(d).version)) {
                    found = Some(d);
                    count += 1;
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

    /// The units, walked from the roots.
    fn units<'s>(&'s self, st: &State<'s, 'a>) -> Result<Pass<'a>, UnitError<'a>> {
        let alloc = self.alloc;
        let mut needs: AVec<'a, LockId> = Vec::new_in(alloc);
        let mut unavailable: AVec<'a, LockId> = Vec::new_in(alloc);
        let mut keys: AVec<'a, Key> = Vec::new_in(alloc);
        let mut index: BTreeMap<Key, u32, Alloc<'a>> = BTreeMap::new_in(alloc);
        let mut tmp: AVec<'a, Tmp<'a>> = Vec::new_in(alloc);
        let add = |key: Key, keys: &mut AVec<'a, Key>, index: &mut BTreeMap<Key, u32, Alloc<'a>>, tmp: &mut AVec<'a, Tmp<'a>>| -> u32 {
            if let Some(&at) = index.get(&key) {
                return at;
            }
            let at = keys.len() as u32;
            keys.push(key);
            index.insert(key, at);
            tmp.push(Tmp { deps: Vec::new_in(alloc), build_script: None, incomplete: false });
            at
        };
        for &root in self.ask.roots.iter() {
            let Answer::Read(package) = &self.answers[root.index()] else { return Err(UnitError::RootUnanswered { root }) };
            if let Some(lib) = &package.lib {
                let key = match lib.proc_macro {
                    true => Key::new(root, Kind::ProcMacro, 0, Side::Host, self.fside(Side::Host)),
                    false => Key::new(root, Kind::Lib, 0, Side::Target, Side::Target),
                };
                add(key, &mut keys, &mut index, &mut tmp);
                if self.ask.tests {
                    add(Key::new(root, Kind::TestLib, 0, Side::Target, Side::Target), &mut keys, &mut index, &mut tmp);
                }
            }
            for i in 0..package.bins.len() as u32 {
                add(Key::new(root, Kind::Bin, i, Side::Target, Side::Target), &mut keys, &mut index, &mut tmp);
                if self.ask.tests {
                    add(Key::new(root, Kind::TestBin, i, Side::Target, Side::Target), &mut keys, &mut index, &mut tmp);
                }
            }
            // Integration tests, examples and benches: by the same switch as the library's
            // test unit (test.md, "The other targets").
            // Roots only: a dependency's layout lists none of these (the layout reads them only
            // with `with_test_targets`, for the workspace the user builds).
            if self.ask.tests {
                for i in 0..package.extras.len() as u32 {
                    add(Key::new(root, Kind::Extra, i, Side::Target, Side::Target), &mut keys, &mut index, &mut tmp);
                }
            }
        }
        let mut next = 0usize;
        while next < keys.len() {
            let key = keys[next];
            let at = next as u32;
            next += 1;
            let id = LockId(key.id);
            let Answer::Read(package) = &self.answers[id.index()] else { continue };
            let n = node(id, key.fside);
            let mut deps: AVec<'a, (Text<'a>, u32)> = Vec::new_in(alloc);
            let mut incomplete = false;
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
                        incomplete = true;
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
                let ckey = match lib.proc_macro {
                    true => Key::new(child_id, Kind::ProcMacro, 0, Side::Host, self.fside(Side::Host)),
                    false => Key::new(child_id, Kind::Lib, 0, platform, self.fside(platform)),
                };
                let name = match dep.renamed() {
                    true => underscored(&dep.key, alloc),
                    false => lib.crate_name.clone(),
                };
                let cu = add(ckey, &mut keys, &mut index, &mut tmp);
                deps.push((name, cu));
            }
            // A binary, and an integration test, example or bench, uses its package's plain
            // library unit (never the library's test unit).
            if matches!(key.kind, Kind::Bin | Kind::TestBin | Kind::Extra)
                && let Some(lib) = &package.lib
            {
                let own_lib = match lib.proc_macro {
                    true => Key::new(id, Kind::ProcMacro, 0, Side::Host, self.fside(Side::Host)),
                    false => Key::new(id, Kind::Lib, 0, Side::Target, Side::Target),
                };
                let cu = add(own_lib, &mut keys, &mut index, &mut tmp);
                deps.push((lib.crate_name.clone(), cu));
            }
            let build_script = match (key.kind != Kind::BuildScript, &package.build) {
                (true, Some(_)) => Some(add(Key::new(id, Kind::BuildScript, 0, Side::Host, key.fside), &mut keys, &mut index, &mut tmp)),
                _ => None,
            };
            deps.sort_by(|a, b| (a.0.as_str(), a.1).cmp(&(b.0.as_str(), b.1)));
            deps.dedup_by(|a, b| a.0 == b.0 && a.1 == b.1);
            tmp[at as usize] = Tmp { deps, build_script, incomplete };
        }
        if !needs.is_empty() {
            needs.sort_unstable();
            needs.dedup();
            return Ok(Pass::Need(needs));
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
        unavailable.sort_unstable();
        unavailable.dedup();
        Ok(Pass::Done(UnitGraph { units, unavailable, resolver: self.resolver }))
    }

    fn unit(&self, st: &State<'_, 'a>, key: Key, package: &Package<'a>, t: &Tmp<'a>, new_id: &[u32]) -> Unit<'a> {
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
        let lib_root = package.lib.as_ref().map(|lib| (&lib.crate_name, &lib.root));
        let bin = |i: u32| &package.bins[i as usize];
        let (kind, crate_name, root) = match key.kind {
            Kind::Lib => (UnitKind::Lib, lib_root.map(|l| l.0.clone()), lib_root.map(|l| l.1.clone())),
            Kind::ProcMacro => (UnitKind::ProcMacro, lib_root.map(|l| l.0.clone()), lib_root.map(|l| l.1.clone())),
            Kind::TestLib => (UnitKind::Test { bin: None }, lib_root.map(|l| l.0.clone()), lib_root.map(|l| l.1.clone())),
            Kind::Bin => (UnitKind::Bin(bin(key.idx).name.clone()), Some(underscored(&bin(key.idx).name, alloc)), Some(bin(key.idx).root.clone())),
            Kind::TestBin => (
                UnitKind::Test { bin: Some(bin(key.idx).name.clone()) },
                Some(underscored(&bin(key.idx).name, alloc)),
                Some(bin(key.idx).root.clone()),
            ),
            Kind::BuildScript => (UnitKind::BuildScript, Some(own("build_script_build", alloc)), package.build.clone()),
            Kind::Extra => {
                let extra = &package.extras[key.idx as usize];
                (extra.unit_kind(), Some(underscored(&extra.name, alloc)), Some(extra.root.clone()))
            }
        };
        // `test` by the kind: the library's and a binary's test units, and a harnessed
        // integration test or bench (the one rule, also the record's).
        let cfg = unit_cfg(desc, &features, has_test_cfg(&kind), key.kind == Kind::ProcMacro, alloc);
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
            crate_name: crate_name.unwrap_or_else(|| String::new_in(alloc)),
            root: root.unwrap_or_else(|| String::new_in(alloc)),
            features,
            cfg,
            deps,
            build_script: t.build_script.map(|b| UnitId(new_id[b as usize])),
            links: package.links.clone(),
            incomplete: t.incomplete,
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
pub(crate) fn unit_cfg<'a>(desc: &TargetDesc<'a>, features: &[Text<'a>], test: bool, proc_macro: bool, alloc: Alloc<'a>) -> AVec<'a, CfgAtom<'a>> {
    let mut cfg = Vec::with_capacity_in(desc.cfg.len() + features.len() + 3, alloc);
    cfg.extend(desc.cfg.iter().cloned());
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

struct Tmp<'a> {
    deps: AVec<'a, (Text<'a>, u32)>,
    build_script: Option<u32>,
    incomplete: bool,
}
