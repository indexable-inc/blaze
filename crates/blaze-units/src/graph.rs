//! The graph itself: cargo's root units (`unit_generator.rs`), their dependencies
//! (`unit_dependencies.rs`), the second pass that shares the host's units and settles the
//! deferred profile settings (`rebuild_unit_graph_shared`), and the JSON (`unit_graph.rs`).
//!
//! Units are rows of a `u32`-indexed column, and a unit's identity is a small `Copy` [`Key`]:
//! its package and target by index, its features as an interned set, its profile as an
//! interned class of equal profiles, and its kind and mode as bytes. Interning is one hash of
//! the key. Edges are built per unit into one column while the graph is walked in cargo's order,
//! then laid out as CSR once the build script runs are connected. What cargo computes per edge
//! from the manifests (a package's library, its profile for a use, its features, the name a
//! dependent calls it by) is computed once per package, or per package and use, and kept.
//! Sorting compares integer ranks: packages, targets, profiles and feature sets are ranked
//! once per sort with [`order`], and units sort by a `u128` of their ranks. Strings are made
//! only for the JSON and for errors.

use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::hash_map::Entry;
use std::hash::{Hash, Hasher};
use std::mem::size_of;

use anyhow::{Context as _, bail};
use blaze_hash::{FixedHasher, HashMap, HashSet};
use blaze_manifest::config::CompileKind as ProfileKind;
use blaze_manifest::profiles::{
    Lto, PanicStrategy, Profile, ProfileRoot, Strip, StripInner, UnitFor,
};
use blaze_manifest::targets::{CrateType, Target, TargetKind};
use blaze_resolve::resolver::FeaturesFor;
use blaze_resolve::{DepKind, Dependency, InternedString, PackageId, Resolve};
use cargo_util_schemas::manifest::TomlDebugInfo;

use crate::{Filter, Intent, LibRule, Package, Platformed, Request, Rule, order, schema};

/// No index: a unit not yet walked, a package not yet ranked, a dependency not renamed.
const NONE: u32 = u32::MAX;

/// What is done with a unit's target (cargo's `CompileMode`), in cargo's order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
enum Mode {
    Test,
    Build,
    Check,
    /// `check` with `--test`.
    CheckTest,
    Doctest,
    RunCustomBuild,
}

impl Mode {
    fn check(test: bool) -> Mode {
        if test { Mode::CheckTest } else { Mode::Check }
    }

    fn is_any_test(self) -> bool {
        matches!(self, Mode::Test | Mode::CheckTest | Mode::Doctest)
    }

    fn is_check(self) -> bool {
        matches!(self, Mode::Check | Mode::CheckTest)
    }

    fn schema(self) -> schema::Mode {
        match self {
            Mode::Test => schema::Mode::Test,
            Mode::Build => schema::Mode::Build,
            Mode::Check | Mode::CheckTest => schema::Mode::Check,
            Mode::Doctest => schema::Mode::Doctest,
            Mode::RunCustomBuild => schema::Mode::RunCustomBuild,
        }
    }
}

/// What a unit is compiled for (cargo's `CompileKind`): `Host` for what runs while building,
/// or `Target`, the one triple a build names (`--target`'s, or the host's own when no
/// `--target` is given, until the graph is shared).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
enum Kind {
    Host,
    Target,
}

impl Kind {
    /// The kind of a unit of a target that is `for_host` (a build script or proc macro).
    fn for_target(self, for_host: bool) -> Kind {
        if for_host { Kind::Host } else { self }
    }
}

/// What makes two units one (cargo's `UnitInner` as its interner compares it), less the
/// dependencies, which only the shared graph's units are told apart by. `class` is the
/// profile's class of equal profiles ([`Profiles`]), as cargo compares profiles.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Key {
    pkg: u32,
    /// The target, by its index among its package's.
    target: u32,
    features: u32,
    class: u32,
    kind: Kind,
    mode: Mode,
}

/// A unit (cargo's `UnitInner`), less what is the same for every unit of a build blaze makes
/// (flags, `links` overrides, artifacts, the standard library). `profile` is the first profile
/// interned with the unit's key, which cargo's interner keeps.
#[derive(Clone, Copy, Debug)]
struct Row {
    key: Key,
    profile: u32,
}

/// An edge (cargo's `UnitDep`), names by their index in [`Names`]. `dev` is whether only a
/// dev-dependency makes it (cargo's `dev_dependency_edges`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Dep {
    unit: u32,
    extern_crate_name: u32,
    /// [`NONE`] when the dependency is not renamed.
    dep_name: u32,
    unit_for: UnitFor,
    public: bool,
    dev: bool,
}

const _: () = assert!(size_of::<Key>() == 20);
const _: () = assert!(size_of::<Row>() == 24);
const _: () = assert!(size_of::<Dep>() == 20);

fn is_example(t: &Target) -> bool {
    matches!(t.kind, TargetKind::ExampleBin | TargetKind::ExampleLib(_))
}

fn is_test(t: &Target) -> bool {
    t.kind == TargetKind::Test
}

fn is_bench(t: &Target) -> bool {
    t.kind == TargetKind::Bench
}

fn is_linkable(t: &Target) -> bool {
    match &t.kind {
        TargetKind::Lib(kinds) => kinds.iter().any(|k| {
            matches!(k, CrateType::Lib | CrateType::Rlib | CrateType::Dylib | CrateType::ProcMacro)
        }),
        _ => false,
    }
}

fn crate_name(t: &Target) -> Cow<'_, str> {
    if t.name.contains('-') { Cow::Owned(t.name.replace('-', "_")) } else { Cow::Borrowed(&t.name) }
}

fn features_for(unit_for: UnitFor) -> FeaturesFor {
    FeaturesFor::from_for_host(unit_for.is_for_host_features())
}

/// cargo's `check_or_build_mode`: what a dependency of a unit in `mode` is built as, the
/// dependency's target being `for_host` or not.
fn check_or_build_mode(mode: Mode, for_host: bool) -> Mode {
    match mode {
        Mode::Check | Mode::CheckTest if !for_host => Mode::Check,
        _ => Mode::Build,
    }
}

/// Runs `f`, a call into another crate whose allocations are its own (the feature resolver's
/// lists, a profile's strings, the resolve's iterators): the allocation-budget test counts them
/// apart from the graph's.
#[cfg(not(test))]
#[inline(always)]
fn external<R>(f: impl FnOnce() -> R) -> R {
    f()
}

/// Runs `f`, which makes part of the JSON the graph returns: the allocation-budget test counts
/// it as the output.
#[cfg(not(test))]
#[inline(always)]
fn output<R>(f: impl FnOnce() -> R) -> R {
    f()
}

#[cfg(test)]
use tests::{external, output};

/// A hash table whose entries live in the caller's columns: each hash heads a chain of the
/// entries with it, by their IDs, so an entry is hashed once and compared only with the entries
/// that share its hash.
#[derive(Default)]
struct Chains {
    heads: HashMap<u64, u32>,
    /// The entry before each in its chain.
    next: Vec<u32>,
}

impl Chains {
    /// The entry hashed to `hash` that `same` holds equal; else none, `new` (the next entry's
    /// ID, the number of entries so far) being recorded under `hash`.
    fn find_or_insert(
        &mut self,
        hash: u64,
        new: u32,
        mut same: impl FnMut(u32) -> bool,
    ) -> Option<u32> {
        debug_assert_eq!(new as usize, self.next.len());
        match self.heads.entry(hash) {
            Entry::Occupied(mut head) => {
                let mut at = *head.get();
                while at != NONE {
                    if same(at) {
                        return Some(at);
                    }
                    at = self.next[at as usize];
                }
                self.next.push(*head.get());
                head.insert(new);
            }
            Entry::Vacant(head) => {
                head.insert(new);
                self.next.push(NONE);
            }
        }
        None
    }
}

/// The names edges carry, each once. An [`InternedString`] is equal only to itself, so its
/// address is its identity.
#[derive(Default)]
struct Names {
    all: Vec<InternedString>,
    ids: HashMap<usize, u32>,
}

impl Names {
    fn intern(&mut self, name: InternedString) -> u32 {
        let next = self.all.len() as u32;
        match self.ids.entry(name.as_str().as_ptr() as usize) {
            Entry::Occupied(it) => *it.get(),
            Entry::Vacant(it) => {
                it.insert(next);
                self.all.push(name);
                next
            }
        }
    }

    fn get(&self, id: u32) -> InternedString {
        self.all[id as usize]
    }

    fn cmp(&self, a: u32, b: u32) -> Ordering {
        if a == b { Ordering::Equal } else { self.get(a).cmp(&self.get(b)) }
    }

    /// As `Option<InternedString>` orders: none first.
    fn cmp_opt(&self, a: u32, b: u32) -> Ordering {
        match (a == NONE, b == NONE) {
            (true, true) => Ordering::Equal,
            (true, false) => Ordering::Less,
            (false, true) => Ordering::Greater,
            (false, false) => self.cmp(a, b),
        }
    }
}

/// The feature lists of units, each list once, in one column.
#[derive(Default)]
struct FeatureSets {
    names: Vec<InternedString>,
    sets: Vec<(u32, u32)>,
    chains: Chains,
}

impl FeatureSets {
    fn get(&self, id: u32) -> &[InternedString] {
        let (start, len) = self.sets[id as usize];
        &self.names[start as usize..(start + len) as usize]
    }

    fn intern(&mut self, features: &[InternedString]) -> u32 {
        let mut hasher = FixedHasher::default();
        hasher.write_usize(features.len());
        for it in features {
            hasher.write_usize(it.as_str().as_ptr() as usize);
        }
        let next = self.sets.len() as u32;
        let (names, sets) = (&self.names, &self.sets);
        let same = |id: u32| {
            let (start, len) = sets[id as usize];
            names[start as usize..(start + len) as usize] == *features
        };
        if let Some(id) = self.chains.find_or_insert(hasher.finish(), next, same) {
            return id;
        }
        let start = self.names.len() as u32;
        self.names.extend_from_slice(features);
        self.sets.push((start, features.len() as u32));
        next
    }
}

/// The profiles of units, each once, and their classes: profiles cargo holds equal (all but
/// the name, the root, `hint-mostly-unused` and whether `debuginfo` and `strip` are deferred)
/// are one class, which is what a unit's key holds. The first profile of a class stays its
/// unit's, as cargo's interner keeps the first unit made.
#[derive(Default)]
struct Profiles {
    exact: Vec<Profile>,
    class_of: Vec<u32>,
    /// Each class's first profile.
    classes: Vec<u32>,
    by_exact: Chains,
    by_class: Chains,
}

impl Profiles {
    fn intern(&mut self, profile: Profile) -> u32 {
        let next = self.exact.len() as u32;
        let exact = &self.exact;
        let hash = exact_hash(&profile);
        if let Some(id) =
            self.by_exact.find_or_insert(hash, next, |id| same_exact(&exact[id as usize], &profile))
        {
            return id;
        }
        let classes = &self.classes;
        let class = self.by_class.find_or_insert(
            class_hash(&profile),
            classes.len() as u32,
            |class| exact[classes[class as usize] as usize] == profile,
        );
        let class = match class {
            Some(class) => class,
            None => {
                self.classes.push(next);
                self.classes.len() as u32 - 1
            }
        };
        self.exact.push(profile);
        self.class_of.push(class);
        next
    }

    fn get(&self, id: u32) -> &Profile {
        &self.exact[id as usize]
    }
}

/// Hashes what `Profile`'s `PartialEq` compares.
fn hash_class(p: &Profile, h: &mut FixedHasher) {
    p.opt_level.hash(h);
    match &p.lto {
        Lto::Off => h.write_u8(0),
        Lto::Bool(on) => {
            h.write_u8(1);
            on.hash(h);
        }
        Lto::Named(name) => {
            h.write_u8(2);
            name.hash(h);
        }
    }
    p.codegen_backend.hash(h);
    p.codegen_units.hash(h);
    p.debuginfo.into_inner().hash(h);
    p.split_debuginfo.hash(h);
    (p.debug_assertions, p.overflow_checks, p.rpath, p.incremental).hash(h);
    h.write_u8(match p.panic {
        PanicStrategy::Unwind => 0,
        PanicStrategy::Abort => 1,
        PanicStrategy::ImmediateAbort => 2,
    });
    match p.strip.into_inner() {
        StripInner::None => h.write_u8(0),
        StripInner::Named(name) => {
            h.write_u8(1);
            name.hash(h);
        }
    }
    p.rustflags.hash(h);
    p.trim_paths.hash(h);
}

fn class_hash(p: &Profile) -> u64 {
    let mut h = FixedHasher::default();
    hash_class(p, &mut h);
    h.finish()
}

fn exact_hash(p: &Profile) -> u64 {
    let mut h = FixedHasher::default();
    hash_class(p, &mut h);
    p.name.hash(&mut h);
    (p.root == ProfileRoot::Release).hash(&mut h);
    p.hint_mostly_unused.hash(&mut h);
    (p.debuginfo.is_deferred(), p.strip.is_deferred()).hash(&mut h);
    h.finish()
}

/// Whether two profiles are the same in every field, deferred or not.
fn same_exact(a: &Profile, b: &Profile) -> bool {
    a == b
        && a.name == b.name
        && a.root == b.root
        && a.hint_mostly_unused == b.hint_mostly_unused
        && a.debuginfo.is_deferred() == b.debuginfo.is_deferred()
        && a.strip.is_deferred() == b.strip.is_deferred()
}

/// A package's library, or its linkable one, as an edge to it needs it.
#[derive(Clone, Copy)]
struct Lib {
    at: u32,
    for_host: bool,
    proc_macro: bool,
}

impl Lib {
    fn of(targets: &[Target], at: usize) -> Lib {
        let t = &targets[at];
        Lib { at: at as u32, for_host: t.for_host, proc_macro: t.proc_macro }
    }
}

/// A package of the request, by its index: packages are indexed in cargo's order of package
/// ids, so the index is the package's rank.
struct Pkg<'a> {
    id: PackageId,
    package: &'a Package<'a>,
    /// The first library target (`Target::is_lib`).
    lib: Option<Lib>,
    /// The first linkable library target.
    linkable: Option<Lib>,
    /// The build scripts and binaries, as ranges of `State::target_lists`.
    scripts: (u32, u32),
    bins: (u32, u32),
    /// The dependencies, as a range of `State::pkg_deps`; [`NONE`] until first asked for.
    deps: (u32, u32),
    any_proc_macro: bool,
    links: bool,
    is_local: bool,
    /// Whether the package depends on itself publicly (cargo's `public_dependencies`).
    self_public: bool,
}

/// A dependency of a package (cargo's `Resolve::deps`): the package depended on, by index
/// ([`NONE`] when the request has no manifest of it), and the dependencies that make it.
#[derive(Clone, Copy)]
struct PkgDep<'a> {
    pkg: u32,
    id: PackageId,
    /// Whether the dependent depends on it publicly (cargo's `public_dependencies`).
    public: bool,
    deps: &'a HashSet<Dependency>,
}

/// What decides which of a package's dependencies a unit builds with (cargo's `State::deps`).
struct DepFilter {
    pkg: PackageId,
    kind: Kind,
    custom_build: bool,
    /// Whether a dependency that is not transitive (a dev-dependency) is built with.
    dev: bool,
    features_for: FeaturesFor,
}

/// The ranks units sort by: a package's index, its targets', its profiles' and its feature
/// sets', computed with [`order`] once per sort.
#[derive(Default)]
struct Ranks {
    /// Each package's first target rank in `targets`; [`NONE`] until ranked.
    target_start: Vec<u32>,
    targets: Vec<u32>,
    profiles: Vec<u32>,
    features: Vec<u32>,
    /// What [`Ranks::sort_rows`] sorted: each row's key and index.
    sorted: Vec<(u128, u32)>,
    scratch: Vec<(u128, u32)>,
    ids: Vec<u32>,
}

impl Ranks {
    fn new(packages: usize) -> Ranks {
        // alloc: a column per build.
        Ranks { target_start: vec![NONE; packages], ..Ranks::default() }
    }

    /// Ranks what `rows` use: every profile and feature set so far, and the targets of each
    /// package of a row not ranked before.
    fn update(
        &mut self,
        pkgs: &[Pkg<'_>],
        profiles: &Profiles,
        features: &FeatureSets,
        rows: &[Row],
    ) -> anyhow::Result<()> {
        let ids = &mut self.ids;
        ids.clear();
        ids.extend(0..profiles.exact.len() as u32);
        ids.sort_unstable_by(|&a, &b| order::profile(profiles.get(a), profiles.get(b)));
        dense(ids, &mut self.profiles, |a, b| order::profile(profiles.get(a), profiles.get(b)));
        if self.profiles.iter().any(|&rank| rank > u32::from(u16::MAX)) {
            bail!("the build has more than {} distinct profiles", u16::MAX);
        }

        ids.clear();
        ids.extend(0..features.sets.len() as u32);
        let first = |id: u32| features.get(id).first().map_or(0, |it| order::prefix(it.as_str()));
        ids.sort_unstable_by(|&a, &b| {
            first(a).cmp(&first(b)).then_with(|| features.get(a).cmp(features.get(b)))
        });
        dense(ids, &mut self.features, |a, b| features.get(a).cmp(features.get(b)));

        for row in rows {
            let pkg = row.key.pkg as usize;
            if self.target_start[pkg] != NONE {
                continue;
            }
            let targets = &pkgs[pkg].package.manifest.targets;
            ids.clear();
            ids.extend(0..targets.len() as u32);
            let cmp = |a: u32, b: u32| order::target(&targets[a as usize], &targets[b as usize]);
            ids.sort_unstable_by(|&a, &b| cmp(a, b));
            let start = self.targets.len();
            self.targets.resize(start + targets.len(), 0);
            let mut rank = 0;
            for (i, &id) in ids.iter().enumerate() {
                if i > 0 && cmp(ids[i - 1], id) != Ordering::Equal {
                    rank += 1;
                }
                self.targets[start + id as usize] = rank;
            }
            self.target_start[pkg] = start as u32;
        }
        Ok(())
    }

    /// The order of cargo's `Ord` of units, up to the dependencies' hash: package, target,
    /// profile, kind, mode, features.
    fn key(&self, row: Row) -> u128 {
        let key = row.key;
        let target = self.targets[(self.target_start[key.pkg as usize] + key.target) as usize];
        (u128::from(key.pkg) << 96)
            | (u128::from(target) << 64)
            | (u128::from(self.profiles[row.profile as usize]) << 48)
            | (u128::from(key.kind as u8) << 44)
            | (u128::from(key.mode as u8) << 40)
            | (u128::from(self.features[key.features as usize]) << 8)
    }

    /// Sorts `ids`, rows of `rows`, by their keys, then by index.
    fn sort_ids(&mut self, rows: &[Row], ids: &mut [u32]) {
        ids.sort_unstable();
        self.sorted.clear();
        for &id in ids.iter() {
            let key = self.key(rows[id as usize]);
            self.sorted.push((key, id));
        }
        blaze_radix::sort_copy_by_key_in(&mut self.sorted, |it| it.0, &mut self.scratch);
        for (at, &(_, id)) in self.sorted.iter().enumerate() {
            ids[at] = id;
        }
    }

    /// Sorts every row into `sorted` by its key, then its index.
    fn sort_rows(&mut self, rows: &[Row]) {
        self.sorted.clear();
        for (id, &row) in rows.iter().enumerate() {
            let key = self.key(row);
            self.sorted.push((key, id as u32));
        }
        blaze_radix::sort_copy_by_key_in(&mut self.sorted, |it| it.0, &mut self.scratch);
    }
}

/// Each of the sorted `ids`' dense rank, by index into `out`: equal under `cmp`, equal rank.
fn dense(ids: &[u32], out: &mut Vec<u32>, cmp: impl Fn(u32, u32) -> Ordering) {
    out.clear();
    out.resize(ids.len(), 0);
    let mut rank = 0;
    for (i, &id) in ids.iter().enumerate() {
        if i > 0 && cmp(ids[i - 1], id) != Ordering::Equal {
            rank += 1;
        }
        out[id as usize] = rank;
    }
}

/// The unit graph's edges once known: unit `u`'s are `edges[offsets[u]..offsets[u + 1]]`.
struct Csr {
    offsets: Vec<u32>,
    edges: Vec<Dep>,
}

impl Csr {
    fn of(&self, unit: u32) -> &[Dep] {
        let at = unit as usize;
        &self.edges[self.offsets[at] as usize..self.offsets[at + 1] as usize]
    }
}

/// The shared graph (cargo's `rebuild_unit_graph_shared`), its units interned by their key and
/// their dependencies, its edges in CSR as each unit is made after its dependencies.
struct Shared {
    units: Vec<Row>,
    offsets: Vec<u32>,
    edges: Vec<Dep>,
    chains: Chains,
    /// Each unit of the first graph's unit here ([`NONE`] until shared).
    memo: Vec<u32>,
    /// The dependencies of the units being shared, a stack of each one's.
    scratch: Vec<Dep>,
    /// A profile with `strip` lowered and its debuginfo settled, by the profile and how.
    adjusted: HashMap<(u32, u8), u32>,
    /// Whether the target's units become the host's (no `--target`).
    to_host: bool,
}

impl Shared {
    fn of(&self, unit: u32) -> &[Dep] {
        let at = unit as usize;
        &self.edges[self.offsets[at] as usize..self.offsets[at + 1] as usize]
    }
}

/// How a shared unit's deferred debuginfo is settled.
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum Debuginfo {
    Keep = 0,
    Finalize = 1,
    Weaken = 2,
}

struct Proposal {
    pkg: u32,
    target: u32,
    requires_features: bool,
    mode: Mode,
}

struct State<'r, 'a> {
    request: &'r Request<'a>,
    /// The platform of each kind, whose `cfg` decides a `[target.'cfg(..)'.dependencies]`.
    platforms: [&'a Platformed; 2],
    /// Each kind as a profile reads it.
    profile_kinds: [ProfileKind; 2],
    pkgs: Vec<Pkg<'a>>,
    pkg_index: HashMap<PackageId, u32>,
    pkg_deps: Vec<PkgDep<'a>>,
    target_lists: Vec<u32>,
    public: Vec<PackageId>,

    units: Vec<Row>,
    by_key: HashMap<Key, u32>,
    /// Each unit's edges in `built`, in the order computed; [`NONE`] until it is walked.
    ranges: Vec<(u32, u32)>,
    built: Vec<Dep>,
    stack: Vec<(u32, UnitFor)>,

    profiles: Profiles,
    /// The profile of a package's unit for a use and kind (cargo's `get_profile`).
    profile_of: HashMap<(u32, UnitFor, Kind), u32>,
    /// The profile of the run of a build script, by the profile of the unit it runs for.
    run_profile_of: Vec<u32>,
    features: FeatureSets,
    /// Each package's activated features, by package and whether for the host.
    features_of: Vec<u32>,
    /// cargo's `resolve_all_features` less the package's own: `(dep, feature)` for each
    /// feature on in each dependency, by package, whether dev-dependencies count, and kind.
    dep_features: HashMap<(u32, bool, Kind), (u32, u32)>,
    dep_feature_pairs: Vec<(InternedString, InternedString)>,
    names: Names,
    /// The names a package calls another's target by (cargo's `extern_crate_name_and_dep_name`).
    extern_names: HashMap<(u32, u32, u32), (u32, u32)>,
    ranks: Ranks,
}

pub(crate) fn build(request: &Request<'_>) -> anyhow::Result<schema::UnitGraph> {
    #[cfg(test)]
    return tests::measured(request, build_graph);
    #[cfg(not(test))]
    build_graph(request)
}

fn build_graph(request: &Request<'_>) -> anyhow::Result<schema::UnitGraph> {
    for (id, package) in request.packages {
        if package.manifest.metabuild.is_some() {
            bail!("`{}` has a `metabuild`, which blaze does not build", id.name());
        }
        let unstable = &package.manifest.normalized_toml;
        if let Some(pkg) = unstable.package.as_ref()
            && (pkg.forced_target.is_some() || pkg.default_target.is_some())
        {
            bail!(
                "`{}` names a `forced-target` or `default-target` (`-Zper-package-target`), which \
                 blaze's unit graph does not read",
                id.name()
            );
        }
    }
    let mut state = State::new(request);
    let roots = state.generate_root_units()?;
    if roots.is_empty() {
        // alloc: none, the vectors being empty.
        return Ok(schema::UnitGraph { version: schema::VERSION, units: Vec::new(), roots: Vec::new() });
    }
    for &root in &roots {
        let row = state.units[root as usize];
        let target = state.target(row.key.pkg, row.key.target);
        let unit_for = if row.key.mode.is_any_test() {
            if target.proc_macro { UnitFor::new_host_test() } else { UnitFor::new_test() }
        } else if target.is_custom_build() {
            UnitFor::new_host(false)
        } else if target.proc_macro {
            UnitFor::new_host(true)
        } else if target.for_host {
            UnitFor::new_compiler()
        } else {
            UnitFor::new_normal()
        };
        state.deps_of(root, unit_for)?;
    }
    let graph = state.connect_and_sort()?;
    let shared = state.share(&graph, &roots);
    state.emit(&shared, &roots)
}

impl<'r, 'a> State<'r, 'a> {
    fn new(request: &'r Request<'a>) -> State<'r, 'a> {
        let explicit = request.target.unwrap_or(request.host);
        let n = request.packages.len();
        let mut sorted: Vec<(u64, PackageId, &'a Package<'a>)> = Vec::with_capacity(n);
        for (&id, package) in request.packages {
            sorted.push((order::prefix(id.name().as_str()), id, package));
        }
        sorted.sort_unstable_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        let targets: usize = sorted.iter().map(|it| it.2.manifest.targets.len()).sum();
        let mut pkgs = Vec::with_capacity(n);
        let mut pkg_index = HashMap::with_capacity_and_hasher(n, Default::default());
        let mut target_lists = Vec::with_capacity(targets);
        for (_, id, package) in sorted {
            pkg_index.insert(id, pkgs.len() as u32);
            let targets = &package.manifest.targets;
            let scripts = target_lists.len() as u32;
            target_lists.extend(
                targets.iter().enumerate().filter(|(_, t)| t.is_custom_build()).map(|(at, _)| at as u32),
            );
            let bins = target_lists.len() as u32;
            target_lists
                .extend(targets.iter().enumerate().filter(|(_, t)| t.is_bin()).map(|(at, _)| at as u32));
            let end = target_lists.len() as u32;
            pkgs.push(Pkg {
                id,
                package,
                lib: targets.iter().position(Target::is_lib).map(|at| Lib::of(targets, at)),
                linkable: targets.iter().position(is_linkable).map(|at| Lib::of(targets, at)),
                scripts: (scripts, bins - scripts),
                bins: (bins, end - bins),
                deps: (NONE, 0),
                any_proc_macro: targets.iter().any(|t| t.proc_macro),
                links: package.manifest.metadata.links.is_some(),
                is_local: id.source_id().is_path(),
                self_public: false,
            });
        }
        let units = n * 4;
        State {
            request,
            platforms: [request.host, explicit],
            // alloc: the triple, once per build.
            profile_kinds: [ProfileKind::Host, ProfileKind::Target(explicit.triple.clone())],
            pkgs,
            pkg_index,
            pkg_deps: Vec::with_capacity(n * 4),
            target_lists,
            public: Vec::with_capacity(4),
            units: Vec::with_capacity(units),
            by_key: HashMap::with_capacity_and_hasher(units, Default::default()),
            ranges: Vec::with_capacity(units),
            built: Vec::with_capacity(units * 4),
            stack: Vec::with_capacity(64),
            profiles: Profiles::default(),
            profile_of: HashMap::with_capacity_and_hasher(n * 2, Default::default()),
            run_profile_of: Vec::with_capacity(8),
            features: FeatureSets::default(),
            // alloc: a column per build.
            features_of: vec![NONE; n * 2],
            dep_features: HashMap::default(),
            dep_feature_pairs: Vec::with_capacity(16),
            names: Names::default(),
            extern_names: HashMap::with_capacity_and_hasher(n * 4, Default::default()),
            ranks: Ranks::new(n),
        }
    }

    fn targets(&self, pkg: u32) -> &'a [Target] {
        let package: &'a Package<'a> = self.pkgs[pkg as usize].package;
        &package.manifest.targets
    }

    fn target(&self, pkg: u32, at: u32) -> &'a Target {
        &self.targets(pkg)[at as usize]
    }

    fn platform_activated(&self, dep: &Dependency, kind: Kind) -> bool {
        dep.platform().is_none_or(|platform| self.platforms[kind as usize].activates(platform))
    }

    /// The package's dependencies, replacements applied (cargo's `Resolve::deps`), each with
    /// whether it is public, computed when first asked for.
    fn package_deps(&mut self, pkg: u32) -> (u32, u32) {
        let at = pkg as usize;
        if self.pkgs[at].deps.0 != NONE {
            return self.pkgs[at].deps;
        }
        let resolve: &'a Resolve = self.request.resolve;
        let id = self.pkgs[at].id;
        let start = self.pkg_deps.len() as u32;
        let (public, pkg_deps, index) = (&mut self.public, &mut self.pkg_deps, &self.pkg_index);
        external(|| {
            public.clear();
            for (dep_id, deps) in resolve.deps_not_replaced(id) {
                if deps.iter().any(|d| d.kind() == DepKind::Normal && d.is_public()) {
                    public.push(dep_id);
                }
            }
            for (dep_id, deps) in resolve.deps(id) {
                let pkg = index.get(&dep_id).copied().unwrap_or(NONE);
                pkg_deps.push(PkgDep { pkg, id: dep_id, public: public.contains(&dep_id), deps });
            }
        });
        let range = (start, self.pkg_deps.len() as u32 - start);
        self.pkgs[at].deps = range;
        self.pkgs[at].self_public = self.public.contains(&id);
        range
    }

    /// The package `dep` names, which the request must have the manifest of.
    fn known(&self, parent: u32, dep: PkgDep<'_>) -> anyhow::Result<u32> {
        if dep.pkg == NONE {
            bail!(
                "`{}` depends on `{}`, which the build has no manifest of",
                self.pkgs[parent as usize].id,
                dep.id
            );
        }
        Ok(dep.pkg)
    }

    fn profile(&mut self, pkg: u32, unit_for: UnitFor, kind: Kind) -> u32 {
        match self.profile_of.entry((pkg, unit_for, kind)) {
            Entry::Occupied(it) => *it.get(),
            Entry::Vacant(it) => {
                let p = &self.pkgs[pkg as usize];
                let profiles = self.request.profiles;
                let profile_kind = &self.profile_kinds[kind as usize];
                let profile = external(|| {
                    profiles.get_profile(
                        &p.package.manifest.package_id,
                        p.package.is_member,
                        p.is_local,
                        unit_for,
                        profile_kind,
                    )
                });
                *it.insert(self.profiles.intern(profile))
            }
        }
    }

    fn run_profile(&mut self, of: u32) -> u32 {
        let at = of as usize;
        if at >= self.run_profile_of.len() {
            self.run_profile_of.resize(self.profiles.exact.len(), NONE);
        }
        let known = self.run_profile_of[at];
        if known != NONE {
            return known;
        }
        let (profiles, unit_profile) = (self.request.profiles, self.profiles.get(of));
        let profile = external(|| profiles.get_profile_run_custom_build(unit_profile));
        let id = self.profiles.intern(profile);
        self.run_profile_of[at] = id;
        id
    }

    fn features_of(&mut self, pkg: u32, features_for: FeaturesFor) -> u32 {
        let slot = pkg as usize * 2 + usize::from(features_for == FeaturesFor::HostDep);
        let known = self.features_of[slot];
        if known != NONE {
            return known;
        }
        let (features, id) = (self.request.features, self.pkgs[pkg as usize].id);
        let on = external(|| features.activated_features(id, features_for));
        let set = self.features.intern(&on);
        self.features_of[slot] = set;
        set
    }

    fn extern_names(&mut self, parent: u32, pkg: u32, target: u32) -> anyhow::Result<(u32, u32)> {
        match self.extern_names.entry((parent, pkg, target)) {
            Entry::Occupied(it) => Ok(*it.get()),
            Entry::Vacant(it) => {
                let (from, to) = (self.pkgs[parent as usize].id, self.pkgs[pkg as usize].id);
                let package: &'a Package<'a> = self.pkgs[pkg as usize].package;
                let t = &package.manifest.targets[target as usize];
                let resolve = self.request.resolve;
                let (name, dep_name) =
                    external(|| resolve.extern_crate_name_and_dep_name(from, to, &crate_name(t)))?;
                let names = &mut self.names;
                let ids = (names.intern(name), dep_name.map_or(NONE, |it| names.intern(it)));
                Ok(*it.insert(ids))
            }
        }
    }

    fn intern(&mut self, key: Key, profile: u32) -> u32 {
        let next = self.units.len() as u32;
        match self.by_key.entry(key) {
            Entry::Occupied(it) => *it.get(),
            Entry::Vacant(it) => {
                it.insert(next);
                self.units.push(Row { key, profile });
                self.ranges.push((NONE, 0));
                next
            }
        }
    }

    /// Whether `pkg` has every `required` feature (a target's `required-features`), as cargo's
    /// `resolve_all_features` lists them: the package's own, and `dep/feature` for each feature
    /// on in each of its dependencies.
    fn has_features(
        &mut self,
        pkg: u32,
        has_dev_units: bool,
        kind: Kind,
        required: &[String],
    ) -> anyhow::Result<bool> {
        let own = self.features_of(pkg, FeaturesFor::NormalOrDev);
        let (start, len) = self.dep_features(pkg, has_dev_units, kind)?;
        let own = self.features.get(own);
        let pairs = &self.dep_feature_pairs[start as usize..(start + len) as usize];
        Ok(required.iter().all(|f| {
            own.iter().any(|it| it.as_str() == f)
                || f.split_once('/').is_some_and(|(dep, feature)| {
                    pairs.iter().any(|(d, x)| d.as_str() == dep && x.as_str() == feature)
                })
        }))
    }

    /// cargo's `resolve_all_features` over `PackageSet::filter_deps`, less the package's own
    /// features.
    fn dep_features(
        &mut self,
        pkg: u32,
        has_dev_units: bool,
        kind: Kind,
    ) -> anyhow::Result<(u32, u32)> {
        let (start, len) = self.package_deps(pkg);
        match self.dep_features.entry((pkg, has_dev_units, kind)) {
            Entry::Occupied(it) => Ok(*it.get()),
            Entry::Vacant(it) => {
                let (host, platform) = (self.platforms[Kind::Host as usize], self.platforms[kind as usize]);
                let used = |d: &Dependency| {
                    (has_dev_units || d.kind() != DepKind::Development)
                        && d.platform().is_none_or(|p| platform.activates(p) || host.activates(p))
                };
                let features = self.request.features;
                let pairs = &mut self.dep_feature_pairs;
                let first = pairs.len() as u32;
                for dep in &self.pkg_deps[start as usize..(start + len) as usize] {
                    let mut proc_macro = None;
                    for d in dep.deps.iter().filter(|d| used(d)) {
                        let proc_macro = match proc_macro {
                            Some(it) => it,
                            None => {
                                if dep.pkg == NONE {
                                    bail!(
                                        "`{}` depends on `{}`, which the build has no manifest of",
                                        self.pkgs[pkg as usize].id,
                                        dep.id
                                    );
                                }
                                let it = self.pkgs[dep.pkg as usize].any_proc_macro;
                                proc_macro = Some(it);
                                it
                            }
                        };
                        let features_for = FeaturesFor::from_for_host(proc_macro || d.is_build());
                        let on =
                            external(|| features.activated_features_unverified(dep.id, features_for));
                        for feature in on.into_iter().flatten() {
                            pairs.push((d.name_in_toml(), feature));
                        }
                    }
                }
                Ok(*it.insert((first, pairs.len() as u32 - first)))
            }
        }
    }

    // ---- root units (`unit_generator.rs`) ----

    fn to_compile_mode(&self) -> Mode {
        match self.request.intent {
            Intent::Test | Intent::Bench => Mode::Test,
            Intent::Build => Mode::Build,
            Intent::Check { test } => Mode::check(test),
            Intent::Doc { .. } => Mode::Check,
        }
    }

    fn new_unit(&mut self, pkg: u32, at: u32, initial: Mode) -> u32 {
        let target = self.target(pkg, at);
        let mode = match initial {
            Mode::Test
                if is_example(target) && !self.request.filter.is_specific() && !target.tested =>
            {
                Mode::Build
            }
            mode => mode,
        };
        let features = self.features_of(pkg, FeaturesFor::from_for_host(target.proc_macro));
        let unit_for = if initial.is_any_test() {
            UnitFor::new_test()
        } else if target.for_host {
            UnitFor::new_compiler()
        } else {
            UnitFor::new_normal()
        };
        let profile = self.profile(pkg, unit_for, Kind::Target);
        let kind = Kind::Target.for_target(target.for_host);
        let class = self.profiles.class_of[profile as usize];
        self.intern(Key { pkg, target: at, features, class, kind, mode }, profile)
    }

    /// Proposes each of `packages`' targets `predicate` picks; how many.
    fn filter_targets(
        &self,
        packages: &[u32],
        predicate: impl Fn(&Target) -> bool,
        requires_features: bool,
        mode: Mode,
        out: &mut Vec<Proposal>,
    ) -> usize {
        let before = out.len();
        for &pkg in packages {
            for (at, target) in self.targets(pkg).iter().enumerate() {
                if predicate(target) {
                    out.push(Proposal { pkg, target: at as u32, requires_features, mode });
                }
            }
        }
        out.len() - before
    }

    fn list_rule_targets(
        &self,
        packages: &[u32],
        rule: &Rule,
        description: &str,
        is_expected_kind: fn(&Target) -> bool,
        mode: Mode,
        out: &mut Vec<Proposal>,
    ) -> anyhow::Result<()> {
        match rule {
            Rule::All => {
                self.filter_targets(packages, is_expected_kind, false, mode, out);
            }
            Rule::Just(names) => {
                for name in names {
                    if name.contains(['*', '?', '[']) {
                        bail!(
                            "the target pattern `{name}`: blaze does not match glob patterns yet"
                        );
                    }
                    let named = |t: &Target| is_expected_kind(t) && t.name == *name;
                    if self.filter_targets(packages, named, true, mode, out) == 0 {
                        bail!("no {description} target named `{name}`");
                    }
                }
            }
        }
        Ok(())
    }

    fn create_proposals(&self, packages: &[u32], out: &mut Vec<Proposal>) -> anyhow::Result<()> {
        match self.request.filter {
            Filter::Default { required_features_filterable } => {
                for &pkg in packages {
                    let targets = self.targets(pkg);
                    for (at, t) in targets.iter().enumerate() {
                        let picked = match self.request.intent {
                            Intent::Bench => t.benched,
                            Intent::Test => t.tested || is_example(t),
                            Intent::Build | Intent::Check { .. } => t.is_bin() || t.is_lib(),
                            // cargo's: a binary named as its package's library is left out.
                            Intent::Doc { .. } => {
                                t.doc && (!t.is_bin() || !targets.iter().any(|l| l.is_lib() && l.name == t.name))
                            }
                        };
                        if picked {
                            out.push(Proposal {
                                pkg,
                                target: at as u32,
                                requires_features: !required_features_filterable,
                                mode: self.to_compile_mode(),
                            });
                        }
                    }
                    if self.request.intent == Intent::Test {
                        let doctested =
                            targets.iter().position(|t| t.is_lib() && t.doctest && t.doctestable());
                        if let Some(at) = doctested {
                            out.push(Proposal {
                                pkg,
                                target: at as u32,
                                requires_features: false,
                                mode: Mode::Doctest,
                            });
                        }
                    }
                }
            }
            Filter::Only { all_targets, lib, bins, examples, tests, benches } => {
                if *lib != LibRule::False {
                    let mode = self.to_compile_mode();
                    let libs = self.filter_targets(packages, Target::is_lib, false, mode, out);
                    if !all_targets && libs == 0 && *lib == LibRule::True {
                        let names: Vec<String> =
                            // alloc: the error's text.
                            packages.iter().map(|&it| self.pkgs[it as usize].id.name().to_string()).collect();
                        if names.len() == 1 {
                            bail!("no library targets found in package `{}`", names[0]);
                        }
                        bail!("no library targets found in packages: {}", names.join(", "));
                    }
                }
                let default_mode = self.to_compile_mode();
                let test_filter: fn(&Target) -> bool = match tests {
                    Rule::All => |t| t.tested,
                    Rule::Just(_) => is_test,
                };
                let test_mode = match self.request.intent {
                    Intent::Build => Mode::Test,
                    Intent::Check { .. } => Mode::CheckTest,
                    _ => default_mode,
                };
                let bench_filter: fn(&Target) -> bool = match benches {
                    Rule::All => |t| t.benched,
                    Rule::Just(_) => is_bench,
                };
                self.list_rule_targets(packages, bins, "bin", Target::is_bin, default_mode, out)?;
                self.list_rule_targets(packages, examples, "example", is_example, default_mode, out)?;
                self.list_rule_targets(packages, tests, "test", test_filter, test_mode, out)?;
                self.list_rule_targets(packages, benches, "bench", bench_filter, test_mode, out)?;
            }
        }
        Ok(())
    }

    fn generate_root_units(&mut self) -> anyhow::Result<Vec<u32>> {
        let mut packages = Vec::with_capacity(self.request.roots.len());
        for id in self.request.roots {
            match self.pkg_index.get(id) {
                Some(&at) => packages.push(at),
                None => bail!("the package `{id}` asked for is not one of the build's"),
            }
        }
        packages.sort_unstable();
        packages.dedup();
        let mut proposals = Vec::with_capacity(packages.len() * 4);
        self.create_proposals(&packages, &mut proposals)?;
        let kind = if self.request.target.is_some() { Kind::Target } else { Kind::Host };
        let mut units = Vec::with_capacity(proposals.len());
        for Proposal { pkg, target, requires_features, mode } in proposals {
            let t = self.target(pkg, target);
            let available = match &t.required_features {
                Some(required) => self.has_features(pkg, self.request.has_dev_units, kind, required)?,
                None => true,
            };
            if t.is_lib() || available {
                units.push(self.new_unit(pkg, target, mode));
            } else if requires_features {
                let required = t.required_features.as_deref().unwrap_or_default();
                // alloc: the error's text.
                let quoted: Vec<String> = required.iter().map(|it| format!("`{it}`")).collect();
                bail!(
                    "target `{}` in package `{}` requires the features: {}\nConsider enabling \
                     them by passing, e.g., `--features=\"{}\"`",
                    t.name,
                    self.pkgs[pkg as usize].id.name(),
                    quoted.join(", "),
                    required.join(" ")
                );
            }
        }
        self.ranks.update(&self.pkgs, &self.profiles, &self.features, &self.units)?;
        self.ranks.sort_ids(&self.units, &mut units);
        units.dedup();
        Ok(units)
    }

    // ---- dependencies (`unit_dependencies.rs`) ----

    /// Walks the graph from `root` depth first, each unit's dependencies in the order computed,
    /// as cargo's recursive `deps_of` does: a unit reached again is not walked again, whatever
    /// it is reached for.
    fn deps_of(&mut self, root: u32, unit_for: UnitFor) -> anyhow::Result<()> {
        self.stack.push((root, unit_for));
        while let Some((unit, unit_for)) = self.stack.pop() {
            if self.ranges[unit as usize].0 != NONE {
                continue;
            }
            self.compute_deps(unit, unit_for)?;
            let (start, len) = self.ranges[unit as usize];
            for at in (start..start + len).rev() {
                let dep = self.built[at as usize];
                self.stack.push((dep.unit, dep.unit_for));
            }
        }
        Ok(())
    }

    fn compute_deps(&mut self, id: u32, unit_for: UnitFor) -> anyhow::Result<()> {
        let row = self.units[id as usize];
        let pkg = row.key.pkg;
        let (deps_start, deps_len) = self.package_deps(pkg);
        let own_public = self.pkgs[pkg as usize].self_public;
        let start = self.built.len() as u32;
        if row.key.mode == Mode::RunCustomBuild {
            // cargo's `compute_deps_custom_build`. `links` overrides in the configuration are
            // read where the request is made: the request has none.
            let script_for = unit_for.for_custom_build();
            let target = row.key.target;
            let dep =
                self.new_unit_dep(id, pkg, target, script_for, Kind::Host, Mode::Build, None, own_public)?;
            self.built.push(dep);
            self.ranges[id as usize] = (start, 1);
            return Ok(());
        }
        let target = self.target(pkg, row.key.target);
        let custom_build = target.is_custom_build();
        let filter = DepFilter {
            pkg: self.pkgs[pkg as usize].id,
            kind: row.key.kind,
            custom_build,
            dev: is_test(target) || is_example(target) || row.key.mode.is_any_test(),
            features_for: features_for(unit_for),
        };
        for at in deps_start..deps_start + deps_len {
            let dep = self.pkg_deps[at as usize];
            let (mut used, mut transitive) = (false, false);
            for d in dep.deps {
                if self.dep_used(d, &filter) {
                    used = true;
                    transitive |= d.is_transitive();
                }
            }
            if !used {
                continue;
            }
            let dep_pkg = self.known(pkg, dep)?;
            let Some(lib) = self.pkgs[dep_pkg as usize].lib else { continue };
            let mode = check_or_build_mode(row.key.mode, lib.for_host);
            let dep_for = unit_for.with_dependency(custom_build, lib.for_host, lib.proc_macro);
            let kind = row.key.kind.for_target(lib.for_host);
            let mut edge =
                self.new_unit_dep(id, dep_pkg, lib.at, dep_for, kind, mode, None, dep.public)?;
            edge.dev = !transitive;
            self.built.push(edge);
        }

        if !custom_build {
            // cargo's `dep_build_script`.
            let (scripts, count) = self.pkgs[pkg as usize].scripts;
            for at in scripts..scripts + count {
                let script = self.target_lists[at as usize];
                let profile = self.run_profile(row.profile);
                let script_for = unit_for.for_custom_build();
                let dep = self.new_unit_dep(
                    id,
                    pkg,
                    script,
                    script_for,
                    row.key.kind,
                    Mode::RunCustomBuild,
                    Some(profile),
                    own_public,
                )?;
                self.built.push(dep);
            }

            if !target.is_lib() || row.key.mode == Mode::Doctest {
                // cargo's `maybe_lib`.
                if let Some(lib) = self.pkgs[pkg as usize].linkable {
                    let mode = check_or_build_mode(row.key.mode, lib.for_host);
                    let dep_for = unit_for.with_dependency(custom_build, lib.for_host, lib.proc_macro);
                    let kind = row.key.kind.for_target(lib.for_host);
                    let dep = self.new_unit_dep(id, pkg, lib.at, dep_for, kind, mode, None, own_public)?;
                    self.built.push(dep);
                }

                if !row.key.mode.is_check()
                    && row.key.mode.is_any_test()
                    && (is_test(target) || is_bench(target))
                {
                    let (bins, count) = self.pkgs[pkg as usize].bins;
                    for at in bins..bins + count {
                        let bin_at = self.target_lists[at as usize];
                        let bin = self.target(pkg, bin_at);
                        if let Some(required) = &bin.required_features
                            && !self.has_features(pkg, false, row.key.kind, required)?
                        {
                            continue;
                        }
                        let kind = row.key.kind.for_target(bin.for_host);
                        let dep = self.new_unit_dep(
                            id,
                            pkg,
                            bin_at,
                            UnitFor::new_normal(),
                            kind,
                            Mode::Build,
                            None,
                            own_public,
                        )?;
                        self.built.push(dep);
                    }
                }
            }
        }
        self.ranges[id as usize] = (start, self.built.len() as u32 - start);
        Ok(())
    }

    /// cargo's `State::deps`: whether the unit builds with the dependency.
    fn dep_used(&self, dep: &Dependency, filter: &DepFilter) -> bool {
        if filter.custom_build != dep.is_build() {
            return false;
        }
        if !dep.is_transitive() && !filter.dev {
            return false;
        }
        if !self.platform_activated(dep, filter.kind) {
            return false;
        }
        !dep.is_optional()
            || self.request.features.is_dep_activated(
                filter.pkg,
                filter.features_for,
                dep.name_in_toml(),
            )
    }

    /// cargo's `new_unit_dep` and `new_unit_dep_with_profile`: `profile` none for the one
    /// the package's profile settings give.
    #[allow(clippy::too_many_arguments)]
    fn new_unit_dep(
        &mut self,
        parent: u32,
        pkg: u32,
        target: u32,
        unit_for: UnitFor,
        kind: Kind,
        mode: Mode,
        profile: Option<u32>,
        public: bool,
    ) -> anyhow::Result<Dep> {
        let profile = match profile {
            Some(profile) => profile,
            None => self.profile(pkg, unit_for, kind),
        };
        let parent_pkg = self.units[parent as usize].key.pkg;
        let (extern_crate_name, dep_name) = self.extern_names(parent_pkg, pkg, target)?;
        let features = self.features_of(pkg, features_for(unit_for));
        let class = self.profiles.class_of[profile as usize];
        let unit = self.intern(Key { pkg, target, features, class, kind, mode }, profile);
        Ok(Dep { unit, extern_crate_name, dep_name, unit_for, public, dev: false })
    }

    /// cargo's `connect_run_custom_build_deps` (the run of a build script waits for the runs of
    /// the build scripts of the `links` packages its package depends on), then the edges in CSR,
    /// each unit's sorted as cargo sorts `UnitDep`s.
    fn connect_and_sort(&mut self) -> anyhow::Result<Csr> {
        let n = self.units.len();
        let is_run = |units: &[Row], unit: u32| units[unit as usize].key.mode == Mode::RunCustomBuild;

        // The units that depend on each build script's run, as CSR.
        // alloc: three columns per build.
        let mut first = vec![0u32; n + 1];
        for dep in &self.built {
            if is_run(&self.units, dep.unit) {
                first[dep.unit as usize + 1] += 1;
            }
        }
        for at in 0..n {
            first[at + 1] += first[at];
        }
        let mut cursor = Vec::with_capacity(n);
        cursor.extend_from_slice(&first[..n]);
        // alloc: a column per build.
        let mut parents = vec![0u32; first[n] as usize];
        for (unit, &(start, len)) in self.ranges.iter().enumerate() {
            if start == NONE {
                continue;
            }
            for dep in &self.built[start as usize..(start + len) as usize] {
                if is_run(&self.units, dep.unit) {
                    let slot = &mut cursor[dep.unit as usize];
                    parents[*slot as usize] = unit as u32;
                    *slot += 1;
                }
            }
        }

        // alloc: grows only with the runs of `links` packages' build scripts.
        let mut extras: Vec<(u32, Dep)> = Vec::with_capacity(4);
        for run in 0..n {
            let row = self.units[run];
            if row.key.mode != Mode::RunCustomBuild {
                continue;
            }
            let begin = extras.len();
            for &parent in &parents[first[run] as usize..first[run + 1] as usize] {
                let (start, len) = self.ranges[parent as usize];
                for at in start..start + len {
                    let other = self.built[at as usize];
                    let other_row = self.units[other.unit as usize];
                    if other_row.key.pkg == row.key.pkg {
                        continue;
                    }
                    let other_pkg = &self.pkgs[other_row.key.pkg as usize];
                    let linkable = other_pkg.linkable.is_some_and(|lib| lib.at == other_row.key.target);
                    if !(linkable && other_pkg.links) || other.dev {
                        continue;
                    }
                    let (other_start, other_len) = self.ranges[other.unit as usize];
                    let other_deps = &self.built[other_start as usize..(other_start + other_len) as usize];
                    let Some(&run_dep) = other_deps.iter().find(|it| is_run(&self.units, it.unit)) else {
                        continue;
                    };
                    let dep_name = if other.dep_name != NONE {
                        other.dep_name
                    } else {
                        self.names.intern(other_pkg.id.name())
                    };
                    let dep = Dep { dep_name, dev: false, ..run_dep };
                    if !extras[begin..].iter().any(|(_, it)| *it == dep) {
                        extras.push((run as u32, dep));
                    }
                }
            }
        }

        let mut offsets = Vec::with_capacity(n + 1);
        let mut edges = Vec::with_capacity(self.built.len() + extras.len());
        let mut extra = 0;
        for (unit, &(start, len)) in self.ranges.iter().enumerate() {
            offsets.push(edges.len() as u32);
            if start != NONE {
                edges.extend_from_slice(&self.built[start as usize..(start + len) as usize]);
            }
            while extra < extras.len() && extras[extra].0 == unit as u32 {
                edges.push(extras[extra].1);
                extra += 1;
            }
        }
        offsets.push(edges.len() as u32);

        // Every unit's rank in cargo's order of units, then each unit's edges by it.
        self.ranks.update(&self.pkgs, &self.profiles, &self.features, &self.units)?;
        self.ranks.sort_rows(&self.units);
        let mut rank = first;
        rank.truncate(n);
        let mut previous = None;
        let mut current = 0;
        for &(key, unit) in &self.ranks.sorted {
            if previous.is_some_and(|it| it != key) {
                current += 1;
            }
            previous = Some(key);
            rank[unit as usize] = current;
        }
        let names = &self.names;
        for unit in 0..n {
            let edges = &mut edges[offsets[unit] as usize..offsets[unit + 1] as usize];
            edges.sort_unstable_by(|a, b| {
                rank[a.unit as usize]
                    .cmp(&rank[b.unit as usize])
                    .then_with(|| a.unit_for.cmp(&b.unit_for))
                    .then_with(|| names.cmp(a.extern_crate_name, b.extern_crate_name))
                    .then_with(|| names.cmp_opt(a.dep_name, b.dep_name))
                    .then_with(|| a.public.cmp(&b.public))
                    .then_with(|| a.unit.cmp(&b.unit))
            });
        }
        Ok(Csr { offsets, edges })
    }

    // ---- sharing (`rebuild_unit_graph_shared`) and the JSON (`unit_graph.rs`) ----

    fn share(&mut self, graph: &Csr, roots: &[u32]) -> Shared {
        let n = self.units.len();
        let mut shared = Shared {
            units: Vec::with_capacity(n),
            offsets: Vec::with_capacity(n + 1),
            edges: Vec::with_capacity(graph.edges.len()),
            chains: Chains::default(),
            // alloc: a column per build.
            memo: vec![NONE; n],
            scratch: Vec::with_capacity(64),
            adjusted: HashMap::default(),
            to_host: self.request.target.is_none(),
        };
        shared.offsets.push(0);
        for &root in roots {
            self.traverse_and_share(&mut shared, graph, root, false);
        }
        shared
    }

    /// cargo's `traverse_and_share`: the unit as the shared graph has it, the host's own
    /// triple made `Host`, its dependencies' identity part of it, `strip` lowered to
    /// `debuginfo` where nothing has debuginfo, and a host unit's deferred debuginfo dropped
    /// unless the target builds the same unit.
    fn traverse_and_share(&mut self, sh: &mut Shared, graph: &Csr, id: u32, for_host: bool) -> u32 {
        let known = sh.memo[id as usize];
        if known != NONE {
            return known;
        }
        let base = sh.scratch.len();
        for &dep in graph.of(id) {
            let unit = self.traverse_and_share(sh, graph, dep.unit, dep.unit_for.is_for_host());
            sh.scratch.push(Dep { unit, ..dep });
        }
        let row = self.units[id as usize];
        let kind = if sh.to_host && row.key.kind == Kind::Target { Kind::Host } else { row.key.kind };
        let profile = self.profiles.get(row.profile);
        let profiles = &self.profiles;
        let strip = profile.strip.is_deferred()
            && !profile.debuginfo.is_turned_on()
            && sh.scratch[base..]
                .iter()
                .all(|d| !profiles.get(sh.units[d.unit as usize].profile).debuginfo.is_turned_on());
        let debuginfo = if for_host && sh.to_host && profile.debuginfo.is_deferred() {
            let probe = Key { kind: Kind::Target, ..row.key };
            let built = self.by_key.get(&probe).is_some_and(|&it| self.ranges[it as usize].0 != NONE);
            if built { Debuginfo::Finalize } else { Debuginfo::Weaken }
        } else {
            Debuginfo::Keep
        };
        let profile = self.adjusted(sh, row.profile, strip, debuginfo);
        let key = Key { kind, class: self.profiles.class_of[profile as usize], ..row.key };

        let mut hasher = FixedHasher::default();
        key.hash(&mut hasher);
        for dep in &sh.scratch[base..] {
            hasher.write_u32(dep.unit);
        }
        let next = sh.units.len() as u32;
        let (units, offsets, edges, deps) = (&sh.units, &sh.offsets, &sh.edges, &sh.scratch[base..]);
        let same = |at: u32| {
            let at = at as usize;
            let theirs = &edges[offsets[at] as usize..offsets[at + 1] as usize];
            units[at].key == key
                && theirs.len() == deps.len()
                && theirs.iter().zip(deps).all(|(a, b)| a.unit == b.unit)
        };
        let new = match sh.chains.find_or_insert(hasher.finish(), next, same) {
            Some(found) => found,
            None => {
                sh.units.push(Row { key, profile });
                sh.edges.extend_from_slice(&sh.scratch[base..]);
                sh.offsets.push(sh.edges.len() as u32);
                next
            }
        };
        sh.scratch.truncate(base);
        sh.memo[id as usize] = new;
        new
    }

    /// The profile `of` with `strip` lowered to `debuginfo` when `strip`, and its debuginfo
    /// settled as `debuginfo` says.
    fn adjusted(&mut self, sh: &mut Shared, of: u32, strip: bool, debuginfo: Debuginfo) -> u32 {
        let how = u8::from(strip) | ((debuginfo as u8) << 1);
        if how == 0 {
            return of;
        }
        match sh.adjusted.entry((of, how)) {
            Entry::Occupied(it) => *it.get(),
            Entry::Vacant(it) => {
                // clone: once per profile and adjustment.
                let mut profile = self.profiles.get(of).clone();
                if strip {
                    profile.strip = profile.strip.strip_debuginfo();
                }
                let canonical = profile.debuginfo.finalize();
                match debuginfo {
                    Debuginfo::Keep => {}
                    Debuginfo::Finalize => profile.debuginfo = canonical,
                    Debuginfo::Weaken => profile.debuginfo = canonical.weaken(),
                }
                *it.insert(self.profiles.intern(profile))
            }
        }
    }

    fn emit(&mut self, sh: &Shared, roots: &[u32]) -> anyhow::Result<schema::UnitGraph> {
        let m = sh.units.len();
        self.ranks.update(&self.pkgs, &self.profiles, &self.features, &sh.units)?;
        self.ranks.sort_rows(&sh.units);
        let mut sorted = std::mem::take(&mut self.ranks.sorted);
        // alloc: a column per build.
        let mut key = vec![0u128; m];
        for &(k, unit) in &sorted {
            key[unit as usize] = k;
        }
        let mut run = 0;
        while run < m {
            let mut end = run + 1;
            while end < m && sorted[end].0 == sorted[run].0 {
                end += 1;
            }
            if end - run > 1 {
                sorted[run..end].sort_unstable_by(|a, b| cmp_shared(sh, &key, &self.names, a.1, b.1));
            }
            run = end;
        }
        // alloc: a column per build.
        let mut index = vec![0u32; m];
        for (at, &(_, unit)) in sorted.iter().enumerate() {
            index[unit as usize] = at as u32;
        }

        let explicit = self.request.target.unwrap_or(self.request.host);
        let mut profiles: Vec<Option<schema::Profile>> = Vec::with_capacity(self.profiles.exact.len());
        profiles.resize_with(self.profiles.exact.len(), || None);
        let mut pkg_ids: Vec<Option<String>> = Vec::with_capacity(self.pkgs.len());
        pkg_ids.resize_with(self.pkgs.len(), || None);
        let mut out = Vec::with_capacity(m);
        for &(_, id) in &sorted {
            let row = sh.units[id as usize];
            let pkg = &self.pkgs[row.key.pkg as usize];
            let target = self.target(row.key.pkg, row.key.target);
            let slot = &mut profiles[row.profile as usize];
            if slot.is_none() {
                *slot = Some(output(|| schema_profile(self.profiles.get(row.profile)))?);
            }
            let profile = slot.as_ref().expect("converted above");
            let pkg_id = pkg_ids[row.key.pkg as usize]
                // alloc: once per package, for the output.
                .get_or_insert_with(|| output(|| pkg.id.to_spec().to_string()));
            let names = &self.names;
            let unit = output(|| -> anyhow::Result<schema::Unit> {
                Ok(schema::Unit {
                    // clone: the output owns its strings.
                    pkg_id: pkg_id.clone(),
                    target: schema_target(pkg.id, target)?,
                    // clone: the output owns its strings.
                    profile: profile.clone(),
                    platform: match row.key.kind {
                        Kind::Host => None,
                        // alloc: the output owns its strings.
                        Kind::Target => Some(explicit.triple.clone()),
                    },
                    mode: row.key.mode.schema(),
                    checked_as_test: row.key.mode == Mode::CheckTest,
                    // alloc: the output owns its strings.
                    features: self.features.get(row.key.features).iter().map(|it| it.to_string()).collect(),
                    is_std: false,
                    dependencies: sh
                        .of(id)
                        .iter()
                        .map(|dep| schema::UnitDep {
                            index: index[dep.unit as usize] as usize,
                            // alloc: the output owns its strings.
                            extern_crate_name: names.get(dep.extern_crate_name).to_string(),
                            public: Some(dep.public),
                            noprelude: Some(false),
                            nounused: Some(false),
                        })
                        // alloc: the output.
                        .collect(),
                })
            })?;
            out.push(unit);
        }
        // alloc: the output.
        let roots: Vec<usize> = output(|| roots.iter().map(|&it| index[sh.memo[it as usize] as usize] as usize).collect());
        self.ranks.sorted = sorted;
        Ok(schema::UnitGraph { version: schema::VERSION, units: out, roots })
    }
}

/// cargo's `Ord` of the shared graph's units. Two units that differ only in what they
/// depend on are ordered by cargo's `dep_hash`, which hashes where its interned
/// dependencies happen to be in memory, so cargo prints them in either order from one run
/// to the next. blaze orders them by their dependencies, compared the same way, which is
/// one of cargo's orders.
fn cmp_shared(sh: &Shared, key: &[u128], names: &Names, a: u32, b: u32) -> Ordering {
    if a == b {
        return Ordering::Equal;
    }
    key[a as usize].cmp(&key[b as usize]).then_with(|| {
        let (x, y) = (sh.of(a), sh.of(b));
        for (p, q) in x.iter().zip(y) {
            let order = cmp_shared(sh, key, names, p.unit, q.unit)
                .then_with(|| p.unit_for.cmp(&q.unit_for))
                .then_with(|| names.cmp(p.extern_crate_name, q.extern_crate_name))
                .then_with(|| names.cmp_opt(p.dep_name, q.dep_name))
                .then_with(|| p.public.cmp(&q.public));
            if order != Ordering::Equal {
                return order;
            }
        }
        x.len().cmp(&y.len())
    })
}

/// A profile as cargo prints it: what `Profile`'s `Serialize` writes, read as the schema's.
/// `trim-paths` is stable on the pinned cargo, which prints a profile that sets none with its
/// default.
fn schema_profile(p: &Profile) -> anyhow::Result<schema::Profile> {
    let stripped = match p.strip.into_inner() {
        StripInner::None => schema::Stripped::None,
        // clone: the output owns its strings.
        StripInner::Named(name) => schema::Stripped::Named(name.clone()),
    };
    // alloc: once per profile, for the output.
    let none = || serde_json::Value::from("none");
    let trim_paths = match &p.trim_paths {
        None => none(),
        Some(trim) => match serde_json::to_value(trim).context("a profile's `trim-paths`")? {
            serde_json::Value::Null => none(),
            value => value,
        },
    };
    Ok(schema::Profile {
        // clone: the output owns its strings.
        name: p.name.clone(),
        // clone: the output owns its strings.
        opt_level: p.opt_level.clone(),
        lto: match &p.lto {
            // alloc: the output owns its strings.
            Lto::Off => "off".to_owned(),
            // alloc: the output owns its strings.
            Lto::Bool(on) => on.to_string(),
            // clone: the output owns its strings.
            Lto::Named(name) => name.clone(),
        },
        // clone: the output owns its strings.
        codegen_backend: p.codegen_backend.clone(),
        codegen_units: p.codegen_units,
        debuginfo: match p.debuginfo.into_inner() {
            TomlDebugInfo::None => schema::DebugInfo::Level(0),
            TomlDebugInfo::Limited => schema::DebugInfo::Level(1),
            TomlDebugInfo::Full => schema::DebugInfo::Level(2),
            // alloc: the output owns its strings.
            TomlDebugInfo::LineDirectivesOnly => schema::DebugInfo::Named("line-directives-only".to_owned()),
            // alloc: the output owns its strings.
            TomlDebugInfo::LineTablesOnly => schema::DebugInfo::Named("line-tables-only".to_owned()),
        },
        // clone: the output owns its strings.
        split_debuginfo: p.split_debuginfo.clone(),
        debug_assertions: p.debug_assertions,
        overflow_checks: p.overflow_checks,
        rpath: p.rpath,
        incremental: p.incremental,
        panic: match p.panic {
            PanicStrategy::Unwind => "unwind",
            PanicStrategy::Abort => "abort",
            PanicStrategy::ImmediateAbort => "immediate-abort",
        }
        // alloc: the output owns its strings.
        .to_owned(),
        strip: match p.strip {
            Strip::Resolved(_) => schema::Strip::Resolved(stripped),
            Strip::Deferred(_) => schema::Strip::Deferred(stripped),
        },
        // clone: the output owns its strings.
        rustflags: p.rustflags.clone(),
        trim_paths: Some(trim_paths),
        hint_mostly_unused: p.hint_mostly_unused,
    })
}

/// A target as `cargo metadata` prints it: what `Target::serialized` writes, read as the
/// schema's.
fn schema_target(pkg: PackageId, target: &Target) -> anyhow::Result<schema::Target> {
    let src_path = target
        .src_path
        .as_ref()
        // alloc: the error's text.
        .with_context(|| format!("`{}`'s target `{}` has no source file", pkg, target.name))?;
    // alloc: the output owns its strings.
    let one = |name: &str| vec![name.to_owned()];
    // alloc: the output owns its strings.
    let types = |kinds: &[CrateType]| -> Vec<String> { kinds.iter().map(|it| it.as_str().to_owned()).collect() };
    let kind = match &target.kind {
        TargetKind::Lib(kinds) => types(kinds),
        TargetKind::Bin => one("bin"),
        TargetKind::ExampleBin | TargetKind::ExampleLib(_) => one("example"),
        TargetKind::Test => one("test"),
        TargetKind::CustomBuild => one("custom-build"),
        TargetKind::Bench => one("bench"),
    };
    let crate_types = match &target.kind {
        TargetKind::Lib(kinds) | TargetKind::ExampleLib(kinds) => types(kinds),
        _ => one("bin"),
    };
    Ok(schema::Target {
        kind,
        crate_types,
        // clone: the output owns its strings.
        name: target.name.clone(),
        // clone: the output owns its strings.
        src_path: src_path.clone(),
        // alloc: the output owns its strings.
        edition: target.edition.to_string(),
        // clone: the output owns its strings.
        required_features: target.required_features.clone(),
        doc: target.doc,
        doctest: target.doctest && target.doctestable(),
        test: target.tested,
    })
}

#[cfg(test)]
mod tests;
