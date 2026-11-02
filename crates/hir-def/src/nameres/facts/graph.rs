//! The import graph of one expansion round and its strongly connected components.
//!
//! [`partition`] over-approximates, for every import and `pub(in path)` path of each module, the
//! modules whose scope resolving it can read (every case is listed in
//! `docs/v2/plan/lanes/frontend/nameres-graph.md`), then splits the round's modules into the
//! components of that graph with Pearce's iterative algorithm. Components come in reverse
//! topological order: a component reads only itself and components before it.
//!
//! What a path can read is a least fixed point, as imports read each other. `cands(X, n)`, the
//! local modules name `n` can denote in `X`'s type namespace, is solved once per shape by a
//! worklist over `(module, name)` pairs interned to `u32`: each fact is derived once and sent
//! only to the consumers it is new for (difference propagation), with no recursion.

use std::{iter, sync::LazyLock};

use base_db::Crate;
use blaze_hash::FixedState;
use blaze_ids::radix::Csr;
use blaze_mem::Frame;
use hashbrown::{HashMap, HashSet, hash_map::Entry};
use hir_expand::{
    mod_path::{ModPath, PathKind},
    name::Name,
};
use intern::Symbol;
use span::Edition;

use crate::nameres::{
    facts::{FrameMap, FrameSet, FrameVec},
    path_resolution::ResolveMode,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ImportShape {
    pub(crate) krate: Crate,
    /// The crate's edition (`DefMap::edition`).
    pub(crate) edition: Edition,
    /// The round's modules, root first; a module is its index here, in the round's module order.
    pub(crate) modules: Box<[ShapeModule]>,
    /// Extern prelude names that denote this crate's root (`extern crate self as n`).
    pub(crate) extern_prelude_self: Box<[Name]>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ShapeModule {
    pub(crate) parent: Option<u32>,
    /// Names that denote a local module in this module's type namespace by declaration:
    /// `mod` children (expansions' too) and `extern crate self as n`, with the module.
    pub(crate) local_names: Box<[(Name, u32)]>,
    pub(crate) imports: Box<[ShapeImport]>,
    /// Other paths this module's import resolution reads: the `pub(in path)` visibilities of
    /// its `use` items.
    pub(crate) other_paths: Box<[ModPath]>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ShapeImport {
    pub(crate) path: ModPath,
    /// The name a plain import binds; `None` for a glob or `as _`.
    pub(crate) binds: Option<Name>,
    pub(crate) is_glob: bool,
    /// The crate's `#[prelude_import]`: every module reads its targets.
    pub(crate) is_prelude: bool,
}

/// The strongly connected components of a round's import graph, in reverse topological
/// order: every component a component reads comes before it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Partition {
    /// The component of each module.
    of: Box<[u32]>,
    /// Component `c`'s members are `members[starts[c]..starts[c + 1]]`, ascending.
    starts: Box<[u32]>,
    members: Box<[u32]>,
}

impl Partition {
    /// The number of components.
    pub(crate) fn len(&self) -> u32 {
        (self.starts.len() - 1) as u32
    }

    /// The component's modules, ascending.
    pub(crate) fn members(&self, component: u32) -> &[u32] {
        let c = component as usize;
        &self.members[self.starts[c] as usize..self.starts[c + 1] as usize]
    }

    /// The component of a module.
    pub(crate) fn of(&self, module: u32) -> u32 {
        self.of[module as usize]
    }

    /// The smallest module index of the component: what keys its queries.
    pub(crate) fn anchor(&self, component: u32) -> u32 {
        self.members[self.starts[component as usize] as usize]
    }

    /// Whether `upstream` may be read by `component` (it comes first or is it).
    pub(crate) fn may_read(&self, component: u32, upstream: u32) -> bool {
        debug_assert!(component < self.len() && upstream < self.len());
        upstream <= component
    }
}

/// The round's components: [`components`] of [`import_graph`], the graph in `frame`.
pub(crate) fn partition(frame: &Frame, shape: &ImportShape) -> Partition {
    components(frame, &import_graph(frame, shape))
}

/// A round's import graph in CSR form: module `m` reads `targets[offsets[m]..offsets[m + 1]]`,
/// ascending and without repeats. A module reading itself is left out: it is in its own
/// component whatever the edges.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct ImportGraph<'f> {
    offsets: FrameVec<'f, u32>,
    targets: FrameVec<'f, u32>,
}

impl<'f> ImportGraph<'f> {
    /// The graph of `edges` (`(reader, read)`) over `nodes` modules, by `Csr`'s two counting
    /// sorts (by target, then stably by source), so it depends on the set of edges only, not
    /// their order or repeats. Columns and scratch are in `frame`.
    pub(super) fn from_edges(frame: &'f Frame, nodes: u32, edges: &[(u32, u32)]) -> ImportGraph<'f> {
        let kept = edges.iter().copied().filter(move |&(src, dst)| {
            assert!(src < nodes && dst < nodes, "import graph edge {src} -> {dst} outside {nodes} modules");
            src != dst
        });
        let mut graph = Csr::new_in(frame);
        graph.rebuild_graph(nodes as usize, kept, &mut Csr::new_in(frame), &mut Vec::new_in(frame));
        // Each source's targets are ascending, so a repeat is next to its first.
        graph.dedup_rows();
        let (offsets, targets) = graph.into_parts();
        ImportGraph { offsets, targets }
    }

    pub(super) fn nodes(&self) -> u32 {
        (self.offsets.len() - 1) as u32
    }

    /// The modules `module` reads, ascending, itself left out.
    #[cfg(test)]
    pub(super) fn reads(&self, module: u32) -> &[u32] {
        let m = module as usize;
        &self.targets[self.offsets[m] as usize..self.offsets[m + 1] as usize]
    }

    #[cfg(test)]
    pub(super) fn edge_count(&self) -> usize {
        self.targets.len()
    }
}

/// One DFS frame of [`components`]: the node, the position of its next edge, and whether no
/// edge has yet reached a node visited before it (Pearce's `root`).
struct DfsFrame {
    node: u32,
    edge: u32,
    root: bool,
}

const _: () = assert!(size_of::<DfsFrame>() == 12);

/// The strongly connected components of `graph`: Pearce's PEA_FIND_SCC2 ("A space-efficient
/// algorithm for finding strongly connected components", 2016) with its recursion turned into
/// an explicit stack of frames. One `u32` per node (`rindex`) holds the DFS index, then the
/// low link, then the component, which is numbered from `n - 1` down as components complete;
/// a completed node's number is always at least any live index, so the one comparison
/// `rindex[w] < rindex[v]` serves both. A component completes after every component it reaches,
/// so numbering in completion order is reverse topological. Its work is in `frame`; the
/// partition is owned.
pub(super) fn components(frame: &Frame, graph: &ImportGraph<'_>) -> Partition {
    let n = graph.nodes();
    let (offsets, targets) = (&graph.offsets, &graph.targets);
    let mut rindex: FrameVec<'_, u32> = Vec::with_capacity_in(n as usize, frame);
    rindex.resize(n as usize, 0);
    let mut frames: FrameVec<'_, DfsFrame> = Vec::with_capacity_in(n as usize, frame);
    let mut stack: FrameVec<'_, u32> = Vec::with_capacity_in(n as usize, frame);
    let mut index = 1u32;
    let mut found = 0u32;

    for start in 0..n {
        if rindex[start as usize] != 0 {
            continue;
        }
        rindex[start as usize] = index;
        index += 1;
        frames.push(DfsFrame { node: start, edge: offsets[start as usize], root: true });
        while let Some(top) = frames.last_mut() {
            let v = top.node as usize;
            let end = offsets[v + 1];
            let mut child = NONE;
            while top.edge < end {
                let w = targets[top.edge as usize];
                if rindex[w as usize] == 0 {
                    child = w;
                    break;
                }
                if rindex[w as usize] < rindex[v] {
                    rindex[v] = rindex[w as usize];
                    top.root = false;
                }
                top.edge += 1;
            }
            if child != NONE {
                rindex[child as usize] = index;
                index += 1;
                frames.push(DfsFrame { node: child, edge: offsets[child as usize], root: true });
                continue;
            }
            let root = top.root;
            frames.pop();
            if root {
                let component = n - 1 - found;
                index -= 1;
                while let Some(&w) = stack.last() {
                    if rindex[v] > rindex[w as usize] {
                        break;
                    }
                    stack.pop();
                    rindex[w as usize] = component;
                    index -= 1;
                }
                rindex[v] = component;
                found += 1;
            } else {
                stack.push(v as u32);
            }
            // Back in the parent: the edge that led here is done.
            if let Some(parent) = frames.last_mut() {
                let u = parent.node as usize;
                if rindex[v] < rindex[u] {
                    rindex[u] = rindex[v];
                    parent.root = false;
                }
                parent.edge += 1;
            }
        }
    }

    // Completion order: the first component completed is 0.
    for at in &mut rindex {
        *at = n - 1 - *at;
    }
    // Members by component, a counting sort over the modules in order: each list ascending.
    let mut members = Csr::new_in(frame);
    members.rebuild(
        found as usize,
        rindex.iter().enumerate().map(|(module, &component)| (component, module as u32)),
        &mut stack,
    );
    Partition {
        // alloc: the query's value, three exact columns, once per round
        of: Box::from(&rindex[..]),
        // alloc: as `of`
        starts: Box::from(members.starts()),
        // alloc: as `of`
        members: Box::from(members.items()),
    }
}

/// The edges of a round: every module a module's imports and `pub(in path)` paths can read.
pub(super) fn import_graph<'f>(frame: &'f Frame, shape: &ImportShape) -> ImportGraph<'f> {
    let mut solver = Solver::new(frame, shape);
    solver.seed();
    solver.run();
    ImportGraph::from_edges(frame, shape.modules.len() as u32, &solver.edges)
}

/// Blaze's test harness module (`blaze-engine`'s `test_harness::MODULE`). A path of the harness
/// reads the children of the harness's parent (`DefMap::is_test_harness`).
static TEST_HARNESS: LazyLock<Symbol> = LazyLock::new(|| Symbol::intern("__blaze_test_harness"));

const ROOT: u32 = 0;
const NONE: u32 = u32::MAX;
/// Tags a consumer as a path level; untagged, it is a pair that glob-imports the source pair.
const LEVEL: u32 = 1 << 31;
/// Modules, pairs and levels stay below this: a seen-key packs two of them and a 2-bit kind.
const ID_LIMIT: usize = 1 << 31;

#[derive(Clone, Copy)]
enum Fact {
    /// The binding of `pairs[pair].name` in module `pairs[pair].module` can be `module`.
    Cand { pair: u32, module: u32 },
    /// The path of `level` looks the segment at that level up in `module`; past its last
    /// segment, the path can denote `module`.
    Reach { level: u32, module: u32 },
    /// `importer` glob-imports `target`.
    Glob { importer: u32, target: u32 },
    /// The module of `pair` glob-imports `target`, so the pair has every binding its name has
    /// in `target`.
    GlobPair { pair: u32, target: u32 },
    /// `module` is a target of a `#[prelude_import]`.
    Prelude { module: u32 },
}

const _: () = assert!(size_of::<Fact>() == 12);

/// An entry of one of the singly linked lists in [`Solver::links`].
#[derive(Clone, Copy)]
struct Link {
    value: u32,
    next: u32,
}

/// One interned `(module, name)`.
struct PairRow {
    name: Name,
    module: u32,
    /// The first candidate pushed, kept here instead of in `seen` (see [`first_or_seen`]).
    first: u32,
    /// The modules the binding can be: a list in `links`.
    cands: u32,
    /// Who gets each new candidate: path levels (tagged [`LEVEL`]) and pairs that glob this one.
    consumers: u32,
    /// The module's previous pair, so a glob found later reaches every pair of its importer.
    next_in_module: u32,
}

const _: () = assert!(size_of::<PairRow>() == 24);

/// One import or `pub(in path)` path. Its levels are `first_level..=first_level + len`: level
/// `i < len` looks segment `i` up, level `len` is what the path denotes.
#[derive(Clone, Copy)]
struct PathRow<'a> {
    module: u32,
    first_level: u32,
    segments: &'a [Name],
    /// The name a plain import binds to what the path denotes.
    binds: Option<Name>,
    /// A glob import (not the prelude's): its module reads and takes every name of its targets.
    glob: bool,
    /// A `#[prelude_import]`: every module reads its targets.
    prelude: bool,
}

struct Solver<'a, 'f> {
    shape: &'a ImportShape,
    paths: FrameVec<'f, PathRow<'a>>,
    /// The path of each level.
    level_path: FrameVec<'f, u32>,
    /// The first module each level reached, kept here instead of in `seen`.
    level_first: FrameVec<'f, u32>,
    pair_ids: FrameMap<'f, (u32, Name), u32>,
    pairs: FrameVec<'f, PairRow>,
    /// Head of each module's list of pairs (through `PairRow::next_in_module`).
    module_pairs: FrameVec<'f, u32>,
    /// Head of each module's list of glob targets (in `links`).
    module_globs: FrameVec<'f, u32>,
    links: FrameVec<'f, Link>,
    /// Every fact pushed but a set's first (inline in its row) and `GlobPair`s, which are made
    /// once each by construction.
    seen: FrameSet<'f, u64>,
    work: FrameVec<'f, Fact>,
    /// The first level of every path that looks its first segment up in the std prelude too.
    prelude_levels: FrameVec<'f, u32>,
    edges: FrameVec<'f, (u32, u32)>,
}

#[inline]
fn seen_key(kind: u64, a: u32, b: u32) -> u64 {
    (kind << 62) | (u64::from(a) << 31) | u64::from(b)
}

/// Whether `value` is new to a set whose first element is kept inline in `first`: most sets
/// here (the modules a level reaches, the modules a name denotes) have one, so most pushes cost
/// a load from a dense column, not a hash probe.
#[inline]
fn first_or_seen(first: &mut u32, seen: &mut FrameSet<'_, u64>, key: u64, value: u32) -> bool {
    if *first == NONE {
        *first = value;
        true
    } else {
        *first != value && seen.insert(key)
    }
}

#[inline]
fn link(links: &mut FrameVec<'_, Link>, head: &mut u32, value: u32) {
    links.push(Link { value, next: *head });
    *head = (links.len() - 1) as u32;
}

impl<'a, 'f> Solver<'a, 'f> {
    /// Every table sized from the shape once, in `frame`: exactly for the paths and levels, by
    /// estimate (a few facts per path level) for the fixed point's.
    fn new(frame: &'f Frame, shape: &'a ImportShape) -> Self {
        let modules = shape.modules.len();
        let (mut paths, mut levels, mut locals, mut preludes) = (0usize, 0usize, 0usize, 0usize);
        for module in &shape.modules {
            locals += module.local_names.len();
            paths += module.imports.len() + module.other_paths.len();
            for import in &module.imports {
                levels += import.path.segments().len() + 1;
                preludes += usize::from(import.is_prelude);
            }
            for path in &module.other_paths {
                levels += path.segments().len() + 1;
            }
        }
        assert!(
            modules < ID_LIMIT && levels < ID_LIMIT,
            "an import shape of {modules} modules and {levels} path levels: the graph's IDs are 31 bits"
        );
        let estimate = levels + locals + paths;
        let mut module_pairs = Vec::with_capacity_in(modules, frame);
        module_pairs.resize(modules, NONE);
        let mut module_globs = Vec::with_capacity_in(modules, frame);
        module_globs.resize(modules, NONE);
        let mut level_first = Vec::with_capacity_in(levels, frame);
        level_first.resize(levels, NONE);
        Solver {
            shape,
            paths: Vec::with_capacity_in(paths, frame),
            level_path: Vec::with_capacity_in(levels, frame),
            level_first,
            pair_ids: HashMap::with_capacity_and_hasher_in(estimate, FixedState, frame),
            pairs: Vec::with_capacity_in(estimate, frame),
            module_pairs,
            module_globs,
            links: Vec::with_capacity_in(4 * estimate, frame),
            seen: HashSet::with_capacity_and_hasher_in(estimate, FixedState, frame),
            work: Vec::with_capacity_in(estimate, frame),
            prelude_levels: Vec::with_capacity_in(paths, frame),
            edges: Vec::with_capacity_in(2 * levels + modules * (1 + preludes), frame),
        }
    }

    /// The facts the shape states: declared module names, the first step of every path, the
    /// prelude's and the test harness's reads.
    fn seed(&mut self) {
        let shape = self.shape;
        let modules = shape.modules.len() as u32;
        for (x, module) in shape.modules.iter().enumerate() {
            let x = x as u32;
            for &(name, child) in &module.local_names {
                let pair = self.pair(x, name);
                self.push(Fact::Cand { pair, module: child });
            }
            if let Some(parent) = module.parent
                && shape.modules[parent as usize]
                    .local_names
                    .iter()
                    .any(|&(name, child)| child == x && name == *TEST_HARNESS)
            {
                self.edge(x, parent);
            }
            for import in &module.imports {
                let binds = if import.is_glob { None } else { import.binds };
                let glob = import.is_glob && !import.is_prelude;
                self.add_path(x, &import.path, ResolveMode::Import, binds, glob, import.is_prelude);
                // Which module the prelude is, every module learns from this import's
                // resolution, so each comes after the module that has it.
                if import.is_prelude {
                    for reader in 0..modules {
                        self.edge(reader, x);
                    }
                }
            }
            for path in &module.other_paths {
                self.add_path(x, path, ResolveMode::Other, None, false, false);
            }
        }
    }

    /// Adds a path of `module` and its first step, by kind as `DefMap::resolve_single` takes
    /// it: where the first segment is looked up, and what the extern prelude adds.
    fn add_path(
        &mut self,
        module: u32,
        path: &'a ModPath,
        mode: ResolveMode,
        binds: Option<Name>,
        glob: bool,
        prelude: bool,
    ) {
        let shape = self.shape;
        let segments = path.segments();
        let first_level = self.level_path.len() as u32;
        let id = self.paths.len() as u32;
        self.level_path.extend(iter::repeat_n(id, segments.len() + 1));
        self.paths.push(PathRow { module, first_level, segments, binds, glob, prelude });
        let extern_prelude_root =
            segments.first().is_some_and(|first| shape.extern_prelude_self.contains(first));
        match path.kind {
            PathKind::DollarCrate(krate) => {
                if krate == shape.krate {
                    self.push(Fact::Reach { level: first_level, module: ROOT });
                }
            }
            PathKind::Crate => self.push(Fact::Reach { level: first_level, module: ROOT }),
            PathKind::Super(levels) => {
                let mut at = Some(module);
                for _ in 0..levels {
                    at = at.and_then(|m| shape.modules[m as usize].parent);
                }
                if let Some(ancestor) = at {
                    self.push(Fact::Reach { level: first_level, module: ancestor });
                }
            }
            // The crate root, then the extern prelude (`::*` alone names the root).
            PathKind::Plain | PathKind::Abs
                if shape.edition == Edition::Edition2015
                    && (path.kind == PathKind::Abs || mode == ResolveMode::Import) =>
            {
                self.push(Fact::Reach { level: first_level, module: ROOT });
                if extern_prelude_root {
                    self.push(Fact::Reach { level: first_level + 1, module: ROOT });
                }
            }
            // The module itself, the extern prelude, then the std prelude.
            PathKind::Plain => {
                if !segments.is_empty() {
                    self.push(Fact::Reach { level: first_level, module });
                    self.prelude_levels.push(first_level);
                    if extern_prelude_root {
                        self.push(Fact::Reach { level: first_level + 1, module: ROOT });
                    }
                }
            }
            // The extern prelude only.
            PathKind::Abs => {
                if extern_prelude_root {
                    self.push(Fact::Reach { level: first_level + 1, module: ROOT });
                }
            }
        }
    }

    /// The interned pair `(module, name)`. A new one takes, through the module's globs, the
    /// same name of each glob target.
    fn pair(&mut self, module: u32, name: Name) -> u32 {
        let id = self.pairs.len() as u32;
        match self.pair_ids.entry((module, name)) {
            Entry::Occupied(entry) => *entry.get(),
            Entry::Vacant(entry) => {
                assert!(self.pairs.len() < ID_LIMIT, "an import shape with 2^31 (module, name) pairs");
                entry.insert(id);
                self.pairs.push(PairRow {
                    name,
                    module,
                    first: NONE,
                    cands: NONE,
                    consumers: NONE,
                    next_in_module: self.module_pairs[module as usize],
                });
                self.module_pairs[module as usize] = id;
                let mut at = self.module_globs[module as usize];
                while at != NONE {
                    let Link { value: target, next } = self.links[at as usize];
                    self.work.push(Fact::GlobPair { pair: id, target });
                    at = next;
                }
                id
            }
        }
    }

    fn push(&mut self, fact: Fact) {
        let new = match fact {
            Fact::Cand { pair, module } => first_or_seen(
                &mut self.pairs[pair as usize].first,
                &mut self.seen,
                seen_key(0, pair, module),
                module,
            ),
            Fact::Reach { level, module } => first_or_seen(
                &mut self.level_first[level as usize],
                &mut self.seen,
                seen_key(1, level, module),
                module,
            ),
            Fact::Glob { importer, target } => self.seen.insert(seen_key(2, importer, target)),
            Fact::Prelude { module } => self.seen.insert(seen_key(3, 0, module)),
            Fact::GlobPair { .. } => true,
        };
        if new {
            self.work.push(fact);
        }
    }

    #[inline]
    fn edge(&mut self, reader: u32, read: u32) {
        if reader != read {
            self.edges.push((reader, read));
        }
    }

    /// Sends a candidate of a pair to one of its consumers.
    #[inline]
    fn deliver(&mut self, consumer: u32, module: u32) {
        match consumer & LEVEL {
            0 => self.push(Fact::Cand { pair: consumer, module }),
            _ => self.push(Fact::Reach { level: (consumer & !LEVEL) + 1, module }),
        }
    }

    /// Makes `consumer` get every candidate of `source`: those it has now, and each later one
    /// as it is found.
    fn subscribe(&mut self, source: u32, consumer: u32) {
        link(&mut self.links, &mut self.pairs[source as usize].consumers, consumer);
        let mut at = self.pairs[source as usize].cands;
        while at != NONE {
            let Link { value, next } = self.links[at as usize];
            self.deliver(consumer, value);
            at = next;
        }
    }

    fn reach(&mut self, level: u32, module: u32) {
        let path = self.paths[self.level_path[level as usize] as usize];
        match path.segments.get((level - path.first_level) as usize) {
            Some(&segment) => {
                self.edge(path.module, module);
                let source = self.pair(module, segment);
                self.subscribe(source, level | LEVEL);
            }
            None => {
                if let Some(name) = path.binds {
                    let pair = self.pair(path.module, name);
                    self.push(Fact::Cand { pair, module });
                }
                if path.glob {
                    self.edge(path.module, module);
                    self.push(Fact::Glob { importer: path.module, target: module });
                }
                if path.prelude {
                    self.push(Fact::Prelude { module });
                }
            }
        }
    }

    /// The least fixed point. A fact joins its list when it is taken off the worklist, and a
    /// consumer subscribing replays the list, so each consumer gets each fact once.
    fn run(&mut self) {
        while let Some(fact) = self.work.pop() {
            match fact {
                Fact::Cand { pair, module } => {
                    link(&mut self.links, &mut self.pairs[pair as usize].cands, module);
                    let mut at = self.pairs[pair as usize].consumers;
                    while at != NONE {
                        let Link { value: consumer, next } = self.links[at as usize];
                        self.deliver(consumer, module);
                        at = next;
                    }
                }
                Fact::Reach { level, module } => self.reach(level, module),
                Fact::Glob { importer, target } => {
                    link(&mut self.links, &mut self.module_globs[importer as usize], target);
                    let mut pair = self.module_pairs[importer as usize];
                    while pair != NONE {
                        self.work.push(Fact::GlobPair { pair, target });
                        pair = self.pairs[pair as usize].next_in_module;
                    }
                }
                Fact::GlobPair { pair, target } => {
                    let name = self.pairs[pair as usize].name;
                    let source = self.pair(target, name);
                    self.subscribe(source, pair);
                }
                Fact::Prelude { module } => {
                    for reader in 0..self.shape.modules.len() as u32 {
                        self.edge(reader, module);
                    }
                    for at in 0..self.prelude_levels.len() {
                        let level = self.prelude_levels[at];
                        self.push(Fact::Reach { level, module });
                    }
                }
            }
        }
    }
}
