//! The job's unit graph as flat columns, built once when the job is lowered, so no unit's work
//! walks the graph through strings or sets: each unit's dependencies as a CSR list sorted as
//! cargo's `build_map` visits them, its package as a `u32`, its role as a flag byte, the build
//! script runs it is linked with (cargo's `to_link`) as a second CSR list, and the units built
//! for build scripts and proc macros as a bitset.
//!
//! Cost: 8 bytes per edge (each way), 4 per link edge, 13 per unit, `N/8` bytes per bitset. Every walk
//! (`closure`, `linked_in`) is one pass with a bitset of `N/64` words and no other allocation.
//! Building it is `O(N + E)` plus cargo's `to_link` once per unit with one shared memo.

#![forbid(unsafe_code)]

use std::hash::{Hash, Hasher};

use blaze_build_script::apply::{GraphUnit, to_link_in};
use blaze_hash::{FixedHasher, HashMap};
use blaze_ids::{bits::BitSet, radix::Csr};
use blaze_units::{
    query::Units,
    schema::{Mode, Unit},
};

use super::error::UnitError;

/// A unit's role, one bit each.
pub(crate) mod flag {
    /// A build script's compile or a proc macro: built for the host (cargo's `for_host`).
    pub(crate) const FOR_HOST: u8 = 1;
    /// A library linked into what depends on it (cargo's `is_linkable`).
    pub(crate) const LINKABLE: u8 = 2;
    /// A build script's run.
    pub(crate) const RUN: u8 = 4;
    /// A crate of the standard library.
    pub(crate) const STD: u8 = 8;
    /// A test (`--test`).
    pub(crate) const TEST: u8 = 16;
}

/// For each unit, how many build script runs of its closure are not applied yet: once none
/// is, what its crate is made from is final. Kept by the writer, which alone applies runs.
///
/// Built with one walk up the dependents per run, `O(R · (N + E))`; each run applied costs
/// the units it reaches, with nothing allocated.
pub(crate) struct Waits {
    /// By unit.
    left: Vec<u32>,
    /// The units whose closure holds run `r`: `waiting[start[r]..start[r + 1]]`, empty for a
    /// unit that is no run.
    start: Vec<u32>,
    waiting: Vec<u32>,
    applied: BitSet<u32>,
}

impl Waits {
    pub(crate) fn new(graph: &Graph) -> Waits {
        let count = graph.len();
        let mut left = vec![0u32; count];
        let mut start = Vec::with_capacity(count + 1);
        let mut waiting = Vec::new();
        let mut seen = BitSet::<u32>::new(count);
        let mut pending = Vec::new();
        start.push(0);
        for run in 0..count as u32 {
            if graph.has(run, flag::RUN) {
                seen.clear();
                seen.insert(run);
                pending.push(run);
                while let Some(at) = pending.pop() {
                    for &up in graph.dependents(at) {
                        if seen.insert(up) {
                            pending.push(up);
                            waiting.push(up);
                            left[up as usize] += 1;
                        }
                    }
                }
            }
            start.push(waiting.len() as u32);
        }
        Waits { left, start, waiting, applied: BitSet::<u32>::new(count) }
    }

    /// Whether `unit` waits on no run.
    pub(crate) fn ready(&self, unit: u32) -> bool {
        self.left.get(unit as usize) == Some(&0)
    }

    /// Run `run` applied: the units it was the last run left for go into `ready`. A run applied
    /// again changes nothing.
    pub(crate) fn applied(&mut self, run: u32, ready: &mut Vec<u32>) {
        if run as usize >= self.left.len() || !self.applied.insert(run) {
            return;
        }
        let range = self.start[run as usize] as usize..self.start[run as usize + 1] as usize;
        for &unit in &self.waiting[range] {
            let left = &mut self.left[unit as usize];
            *left -= 1;
            if *left == 0 {
                ready.push(unit);
            }
        }
    }
}

pub(crate) struct Graph {
    /// Unit `i`'s dependencies are `deps[dep_start[i]..dep_start[i + 1]]`, sorted by their
    /// packages' ids, as cargo's `build_map` visits them.
    dep_start: Vec<u32>,
    deps: Vec<u32>,
    /// The units depending on unit `i`: `up[up_start[i]..up_start[i + 1]]`, in unit order.
    up_start: Vec<u32>,
    up: Vec<u32>,
    /// The runs of build scripts unit `i` is linked with, in cargo's order:
    /// `links[link_start[i]..link_start[i + 1]]`.
    link_start: Vec<u32>,
    links: Vec<u32>,
    /// Each unit's package, by index into `Units::packages`.
    package: Vec<u32>,
    flags: Vec<u8>,
    /// The units compiled for build scripts and proc macros: all a script's compile or a proc
    /// macro depends on, and themselves.
    build_time: BitSet<u32>,
    /// The packages with a library target, by index into `Units::packages`.
    lib_packages: BitSet<u32>,
    /// Units by [`target_hash`] of what they build, each hash's units in order: how an artifact
    /// finds its unit with no string built.
    by_target: HashMap<u64, Vec<u32>>,
}

/// What a unit builds, hashed: its package, target name and kinds, and mode.
pub(crate) fn target_hash(pkg_id: &str, name: &str, kinds: &[impl AsRef<str>], mode: Mode) -> u64 {
    let mut hasher = FixedHasher::default();
    pkg_id.hash(&mut hasher);
    name.hash(&mut hasher);
    kinds.len().hash(&mut hasher);
    for kind in kinds {
        kind.as_ref().hash(&mut hasher);
    }
    mode.hash(&mut hasher);
    hasher.finish()
}

fn flags_of(unit: &Unit) -> u8 {
    let target = &unit.target;
    let mut flags = 0;
    if target.is_custom_build() || target.is_proc_macro() {
        flags |= flag::FOR_HOST;
    }
    let linkable_type = target.crate_types.iter().any(|it| matches!(it.as_str(), "lib" | "rlib" | "dylib" | "proc-macro"));
    if target.is_lib() && linkable_type {
        flags |= flag::LINKABLE;
    }
    if unit.mode == Mode::RunCustomBuild {
        flags |= flag::RUN;
    }
    if unit.is_std {
        flags |= flag::STD;
    }
    if unit.mode == Mode::Test {
        flags |= flag::TEST;
    }
    flags
}

impl Graph {
    /// The graph of `units`; a unit whose package the build doesn't list, or that names a
    /// dependency past its units, is an error naming it.
    pub(crate) fn new(units: &Units) -> Result<Graph, UnitError> {
        let all = &units.graph.units;
        let count = all.len();
        let packages: HashMap<&str, u32> =
            units.packages.iter().enumerate().map(|(at, it)| (it.id.as_str(), at as u32)).collect();
        let package: Vec<u32> = all
            .iter()
            .enumerate()
            .map(|(at, it)| {
                packages
                    .get(it.pkg_id.as_str())
                    .copied()
                    .ok_or_else(|| UnitError::Package { unit: at, pkg_id: it.pkg_id.clone() }) // clone: error path
            })
            .collect::<Result<_, _>>()?;
        let flags: Vec<u8> = all.iter().map(flags_of).collect();
        let edges = all.iter().map(|it| it.dependencies.len()).sum();
        let mut dep_start = Vec::with_capacity(count + 1);
        let mut deps = Vec::with_capacity(edges);
        dep_start.push(0);
        for (at, unit) in all.iter().enumerate() {
            let from = deps.len();
            for dep in &unit.dependencies {
                if dep.index >= count {
                    return Err(UnitError::Dependency { unit: at, dep: dep.index, count });
                }
                deps.push(dep.index as u32);
            }
            deps[from..].sort_by(|a, b| all[*a as usize].pkg_id.cmp(&all[*b as usize].pkg_id));
            dep_start.push(deps.len() as u32);
        }
        // The dependents, by a counting sort of the edges on their dependency.
        let (up_start, up) = Csr::build(
            count,
            (0..count).flat_map(|from| {
                deps[dep_start[from] as usize..dep_start[from + 1] as usize].iter().map(move |&dep| (dep, from as u32))
            }),
        )
        .into_parts();
        let mut graph = Graph {
            dep_start,
            deps,
            up_start,
            up,
            link_start: Vec::with_capacity(count + 1),
            links: Vec::new(),
            package,
            flags,
            build_time: BitSet::<u32>::new(count),
            lib_packages: BitSet::<u32>::new(units.packages.len()),
            by_target: HashMap::default(),
        };
        for (at, unit) in all.iter().enumerate() {
            if unit.target.is_lib() {
                graph.lib_packages.insert(graph.package[at]);
            }
        }
        graph.by_target.reserve(count);
        for (at, unit) in all.iter().enumerate() {
            let hash = target_hash(&unit.pkg_id, &unit.target.name, &unit.target.kind, unit.mode);
            graph.by_target.entry(hash).or_default().push(at as u32);
        }

        let mut memo = HashMap::default();
        memo.reserve(count);
        let node = |at: &u32| graph.graph_unit(all, *at);
        let mut link_start = Vec::with_capacity(count + 1);
        let mut links = Vec::new();
        link_start.push(0);
        for at in 0..count as u32 {
            links.extend_from_slice(&to_link_in(at, &node, &mut memo).to_link);
            link_start.push(links.len() as u32);
        }
        graph.link_start = link_start;
        graph.links = links;

        let mut build_time = BitSet::<u32>::new(count);
        let mut pending: Vec<u32> = (0..count as u32)
            .filter(|at| graph.flags[*at as usize] & flag::FOR_HOST != 0 && graph.flags[*at as usize] & flag::RUN == 0)
            .collect();
        for at in &pending {
            build_time.insert(*at);
        }
        while let Some(at) = pending.pop() {
            for &dep in graph.deps(at) {
                if build_time.insert(dep) {
                    pending.push(dep);
                }
            }
        }
        graph.build_time = build_time;
        Ok(graph)
    }

    fn graph_unit(&self, all: &[Unit], at: u32) -> GraphUnit<u32, u32> {
        let deps = self.deps(at).to_vec();
        let custom_build = all[at as usize].target.is_custom_build();
        let own_runs = match custom_build {
            true => Vec::new(),
            false => deps
                .iter()
                .copied()
                .filter(|dep| self.has(*dep, flag::RUN) && self.package[*dep as usize] == self.package[at as usize])
                .collect(),
        };
        GraphUnit { deps, own_runs, for_host: self.has(at, flag::FOR_HOST), linkable: self.has(at, flag::LINKABLE) }
    }

    pub(crate) fn len(&self) -> usize {
        self.flags.len()
    }

    #[inline]
    pub(crate) fn deps(&self, at: u32) -> &[u32] {
        let at = at as usize;
        &self.deps[self.dep_start[at] as usize..self.dep_start[at + 1] as usize]
    }

    /// The units depending on `at` directly.
    #[inline]
    pub(crate) fn dependents(&self, at: u32) -> &[u32] {
        let at = at as usize;
        &self.up[self.up_start[at] as usize..self.up_start[at + 1] as usize]
    }

    /// The build script runs `at` is linked with, in cargo's order.
    #[inline]
    pub(crate) fn links(&self, at: u32) -> &[u32] {
        let at = at as usize;
        &self.links[self.link_start[at] as usize..self.link_start[at + 1] as usize]
    }

    #[inline]
    pub(crate) fn has(&self, at: u32, flag: u8) -> bool {
        self.flags.get(at as usize).is_some_and(|it| it & flag != 0)
    }

    #[inline]
    pub(crate) fn package(&self, at: u32) -> u32 {
        self.package[at as usize]
    }

    /// Whether unit `at`'s package has a library target.
    #[inline]
    pub(crate) fn package_has_lib(&self, at: u32) -> bool {
        self.lib_packages.contains(self.package(at))
    }

    #[inline]
    pub(crate) fn build_time(&self, at: u32) -> bool {
        self.build_time.contains(at)
    }

    /// The unit that builds `kinds` target `name` of `pkg_id` in `mode`.
    pub(crate) fn find(&self, all: &[Unit], pkg_id: &str, name: &str, kinds: &[impl AsRef<str>], mode: Mode) -> Option<u32> {
        let found = self.by_target.get(&target_hash(pkg_id, name, kinds, mode))?;
        found.iter().copied().find(|at| {
            let unit = &all[*at as usize];
            unit.pkg_id == pkg_id
                && unit.target.name == name
                && unit.target.kind.iter().map(String::as_str).eq(kinds.iter().map(AsRef::as_ref))
                && unit.mode == mode
        })
    }

    /// Every unit `unit` depends on, itself first, each once, each before its dependencies
    /// (what depends on a unit comes before it).
    pub(crate) fn closure(&self, unit: u32) -> Vec<u32> {
        let mut seen = BitSet::<u32>::new(self.len());
        let mut post = Vec::with_capacity(self.len().min(256));
        let mut stack = vec![(unit, false)];
        while let Some((at, expanded)) = stack.pop() {
            if expanded {
                post.push(at);
                continue;
            }
            if !seen.insert(at) {
                continue;
            }
            stack.push((at, true));
            for &dep in self.deps(at).iter().rev() {
                if !seen.contains(dep) {
                    stack.push((dep, false));
                }
            }
        }
        post.reverse();
        post
    }

    /// The crates linked into `unit`'s image, in [`Graph::closure`]'s order: its own, and each
    /// library reached through libraries, not through a build script or proc macro.
    pub(crate) fn linked_in(&self, unit: u32) -> Vec<u32> {
        let mut inside = BitSet::<u32>::new(self.len());
        inside.insert(unit);
        let mut pending = vec![unit];
        while let Some(at) = pending.pop() {
            for &dep in self.deps(at) {
                if self.has(dep, flag::LINKABLE) && !self.has(dep, flag::FOR_HOST) && inside.insert(dep) {
                    pending.push(dep);
                }
            }
        }
        let mut order = self.closure(unit);
        order.retain(|at| inside.contains(*at) && !self.has(*at, flag::RUN));
        order
    }
}
