//! The import graph and its partition.
//!
//! - Pearce's components equal a recursive Tarjan's on random graphs, in reverse topological
//!   order, the same for any order or repetition of the edges, and with no recursion on a
//!   200 000-node chain.
//! - Hand-written shapes, one per kind of path: the exact modules each module reads.
//! - Soundness on random shapes: every module scope a brute-force resolver reads (written here
//!   from `path_resolution.rs`'s rules: every binding a name can ever have, so a superset of
//!   any state the real resolver passes through) is one `may_read` allows.
//! - `partition` makes the same, bounded number of allocations however many edges there are.

use std::collections::{BTreeSet, HashMap, HashSet};

use base_db::Crate;
use blaze_mem::{Frame, Tag, frame};
use hir_expand::{
    mod_path::{ModPath, PathKind},
    name::Name,
};
use hegel::generators as gs;
use hegel::{Hegel, Settings, TestCase};
use intern::Symbol;
use span::Edition;
use hir_expand::fixture;

use crate::{
    nameres::{
        facts::graph::{
            ImportGraph, ImportShape, Partition, ShapeImport, ShapeModule, components,
            import_graph, partition,
        },
        path_resolution::ResolveMode,
    },
    test_db::TestDB,
};

fn name(text: &str) -> Name {
    Name::new_symbol_root(Symbol::intern(text))
}

fn path(kind: PathKind, segments: &[&str]) -> ModPath {
    ModPath::from_segments(kind, segments.iter().map(|it| name(it)))
}

/// The crate the shapes are of, and a dependency for `$crate` paths into another crate.
fn crates() -> (TestDB, Crate, Crate) {
    let db = fixture::db("//- /main.rs crate:main deps:dep\n//- /dep.rs crate:dep\n").into_db();
    let all = base_db::all_crates(&db);
    assert_eq!(all.len(), 2, "the fixture has two crates");
    let (this, other) = (all[0], all[1]);
    (db, this, other)
}

/// Runs `property` over `cases` cases, or `HEGEL_TEST_CASES` of them where that is set (a long
/// run).
fn run_cases(cases: u64, property: impl FnMut(TestCase)) {
    let cases = std::env::var("HEGEL_TEST_CASES").ok().and_then(|it| it.parse().ok()).unwrap_or(cases);
    Hegel::new(property).settings(Settings::new().test_cases(cases).database(None)).run();
}

#[derive(Default)]
struct ModuleBuilder {
    parent: Option<u32>,
    local_names: Vec<(Name, u32)>,
    imports: Vec<ShapeImport>,
    other_paths: Vec<ModPath>,
}

struct Builder {
    krate: Crate,
    edition: Edition,
    modules: Vec<ModuleBuilder>,
    extern_prelude_self: Vec<Name>,
}

impl Builder {
    const ROOT: u32 = 0;

    fn new(krate: Crate, edition: Edition) -> Builder {
        Builder {
            krate,
            edition,
            modules: vec![ModuleBuilder::default()],
            extern_prelude_self: Vec::new(),
        }
    }

    fn child(&mut self, parent: u32, child_name: Name) -> u32 {
        let id = self.modules.len() as u32;
        self.modules.push(ModuleBuilder { parent: Some(parent), ..ModuleBuilder::default() });
        self.modules[parent as usize].local_names.push((child_name, id));
        id
    }

    fn module(&mut self, parent: u32, child_name: &str) -> u32 {
        self.child(parent, name(child_name))
    }

    fn raw_import(&mut self, module: u32, import: ShapeImport) {
        self.modules[module as usize].imports.push(import);
    }

    /// `use path;`, binding its last segment.
    fn import(&mut self, module: u32, kind: PathKind, segments: &[&str]) {
        let path = path(kind, segments);
        let binds = path.segments().last().copied();
        self.raw_import(module, ShapeImport { path, binds, is_glob: false, is_prelude: false });
    }

    fn import_as(&mut self, module: u32, kind: PathKind, segments: &[&str], alias: &str) {
        let import =
            ShapeImport { path: path(kind, segments), binds: Some(name(alias)), is_glob: false, is_prelude: false };
        self.raw_import(module, import);
    }

    /// `use path::*;`.
    fn glob(&mut self, module: u32, kind: PathKind, segments: &[&str]) {
        let import = ShapeImport { path: path(kind, segments), binds: None, is_glob: true, is_prelude: false };
        self.raw_import(module, import);
    }

    /// `#[prelude_import] use path::*;`.
    fn prelude_import(&mut self, module: u32, kind: PathKind, segments: &[&str]) {
        let import = ShapeImport { path: path(kind, segments), binds: None, is_glob: true, is_prelude: true };
        self.raw_import(module, import);
    }

    /// A `pub(in path)` of one of the module's `use` items.
    fn visibility(&mut self, module: u32, kind: PathKind, segments: &[&str]) {
        self.modules[module as usize].other_paths.push(path(kind, segments));
    }

    /// `extern crate self as alias;` in `module`; at the root it joins the extern prelude.
    fn extern_crate_self(&mut self, module: u32, alias: Name) {
        self.modules[module as usize].local_names.push((alias, Self::ROOT));
        if module == Self::ROOT {
            self.extern_prelude_self.push(alias);
        }
    }

    fn build(&self) -> ImportShape {
        ImportShape {
            krate: self.krate,
            edition: self.edition,
            modules: self
                .modules
                .iter()
                .map(|it| ShapeModule {
                    parent: it.parent,
                    local_names: it.local_names.iter().copied().collect(),
                    imports: it.imports.iter().cloned().collect(),
                    other_paths: it.other_paths.iter().copied().collect(),
                })
                .collect(),
            extern_prelude_self: self.extern_prelude_self.iter().copied().collect(),
        }
    }
}

/// Each test runs the graph code in a task of its own, as the `partition` query does.
fn task<R>(f: impl FnOnce(&Frame) -> R) -> R {
    frame::task(Tag::Nameres, f)
}

fn reads(shape: &ImportShape, module: u32) -> Vec<u32> {
    task(|frame| import_graph(frame, shape).reads(module).to_vec())
}

fn partition_of(shape: &ImportShape) -> Partition {
    task(|frame| partition(frame, shape))
}

/// Every module in exactly one component, members ascending, the anchor the smallest.
fn check_well_formed(partition: &Partition, nodes: u32) {
    let mut seen = vec![false; nodes as usize];
    for component in 0..partition.len() {
        let members = partition.members(component);
        assert!(!members.is_empty(), "component {component} is empty");
        assert!(members.windows(2).all(|it| it[0] < it[1]), "component {component}: {members:?}");
        assert_eq!(partition.anchor(component), members[0]);
        for &module in members {
            assert!(!seen[module as usize], "module {module} is in two components");
            seen[module as usize] = true;
            assert_eq!(partition.of(module), component);
        }
    }
    assert!(seen.iter().all(|&it| it), "a module is in no component");
}

fn component_sets(partition: &Partition) -> BTreeSet<Vec<u32>> {
    (0..partition.len()).map(|c| partition.members(c).to_vec()).collect()
}

/// Tarjan's algorithm, recursive, as the textbook has it.
fn tarjan(nodes: u32, edges: &[(u32, u32)]) -> BTreeSet<Vec<u32>> {
    struct State {
        adjacent: Vec<Vec<u32>>,
        index: Vec<Option<u32>>,
        low: Vec<u32>,
        on_stack: Vec<bool>,
        stack: Vec<u32>,
        next: u32,
        out: BTreeSet<Vec<u32>>,
    }

    fn visit(s: &mut State, v: u32) {
        let vi = v as usize;
        s.index[vi] = Some(s.next);
        s.low[vi] = s.next;
        s.next += 1;
        s.stack.push(v);
        s.on_stack[vi] = true;
        for at in 0..s.adjacent[vi].len() {
            let w = s.adjacent[vi][at];
            let wi = w as usize;
            match s.index[wi] {
                None => {
                    visit(s, w);
                    s.low[vi] = s.low[vi].min(s.low[wi]);
                }
                Some(index) if s.on_stack[wi] => s.low[vi] = s.low[vi].min(index),
                Some(_) => {}
            }
        }
        if Some(s.low[vi]) == s.index[vi] {
            let mut component = Vec::new();
            loop {
                let w = s.stack.pop().unwrap();
                s.on_stack[w as usize] = false;
                component.push(w);
                if w == v {
                    break;
                }
            }
            component.sort_unstable();
            s.out.insert(component);
        }
    }

    let n = nodes as usize;
    let mut adjacent = vec![Vec::new(); n];
    for &(src, dst) in edges {
        adjacent[src as usize].push(dst);
    }
    let mut state = State {
        adjacent,
        index: vec![None; n],
        low: vec![0; n],
        on_stack: vec![false; n],
        stack: Vec::new(),
        next: 0,
        out: BTreeSet::new(),
    };
    for v in 0..nodes {
        if state.index[v as usize].is_none() {
            visit(&mut state, v);
        }
    }
    state.out
}

#[test]
fn pearce_equals_tarjan() {
    run_cases(1024, |tc| {
        let nodes: u32 = tc.draw(gs::integers().min_value(1).max_value(47));
        let edges: Vec<(u32, u32)> = tc.draw(
            gs::vecs(gs::tuples!(gs::integers().max_value(nodes - 1), gs::integers().max_value(nodes - 1)))
                .max_size(159),
        );
        task(|frame| {
            let partition = components(frame, &ImportGraph::from_edges(frame, nodes, &edges));
            check_well_formed(&partition, nodes);
            assert_eq!(component_sets(&partition), tarjan(nodes, &edges));
            for &(src, dst) in &edges {
                assert!(
                    partition.of(dst) <= partition.of(src),
                    "edge {} -> {} goes to a later component",
                    src,
                    dst
                );
                assert!(partition.may_read(partition.of(src), partition.of(dst)));
            }
            // The order of the edges and their repeats change nothing.
            let mut shuffled: Vec<(u32, u32)> = edges.iter().rev().copied().collect();
            shuffled.extend_from_slice(&edges);
            assert_eq!(
                ImportGraph::from_edges(frame, nodes, &shuffled),
                ImportGraph::from_edges(frame, nodes, &edges)
            );
            assert_eq!(components(frame, &ImportGraph::from_edges(frame, nodes, &shuffled)), partition);
        })
    });
}

#[test]
fn deep_chain_needs_no_recursion() {
    task(|frame| {
        let nodes = 200_000u32;
        let edges: Vec<(u32, u32)> = (0..nodes - 1).map(|it| (it, it + 1)).collect();
        let partition = components(frame, &ImportGraph::from_edges(frame, nodes, &edges));
        assert_eq!(partition.len(), nodes);
        for module in 0..nodes {
            assert_eq!(partition.of(module), nodes - 1 - module);
        }
    });
}

#[test]
fn empty_and_self_loops() {
    task(|frame| {
        let none = components(frame, &ImportGraph::from_edges(frame, 0, &[]));
        assert_eq!(none.len(), 0);
        let graph = ImportGraph::from_edges(frame, 2, &[(0, 0), (1, 1), (1, 1)]);
        assert_eq!(graph.edge_count(), 0);
        let partition = components(frame, &graph);
        assert_eq!(partition.len(), 2);
        check_well_formed(&partition, 2);
    });
}

#[test]
fn glob_cycle_is_one_component() {
    let (_db, this, _) = crates();
    let mut b = Builder::new(this, Edition::Edition2021);
    let a = b.module(Builder::ROOT, "a");
    let c = b.module(Builder::ROOT, "b");
    b.glob(a, PathKind::Crate, &["b"]);
    b.glob(c, PathKind::Crate, &["a"]);
    let shape = b.build();
    assert_eq!(reads(&shape, a), [Builder::ROOT, c]);
    assert_eq!(reads(&shape, c), [Builder::ROOT, a]);
    let p = partition_of(&shape);
    check_well_formed(&p, 3);
    assert_eq!(p.len(), 2);
    assert_eq!(p.of(a), p.of(c));
    assert_eq!(p.members(p.of(a)), [a, c]);
    assert_eq!(p.anchor(p.of(a)), a);
    assert!(p.of(Builder::ROOT) < p.of(a));
    assert!(p.may_read(p.of(a), p.of(Builder::ROOT)));
    assert!(!p.may_read(p.of(Builder::ROOT), p.of(a)));
}

#[test]
fn chain_is_three_components_in_order() {
    let (_db, this, _) = crates();
    let mut b = Builder::new(this, Edition::Edition2021);
    let a = Builder::ROOT;
    let middle = b.module(a, "b");
    let last = b.module(middle, "c");
    b.import(a, PathKind::SELF, &["b", "X"]);
    b.import(middle, PathKind::SELF, &["c", "Y"]);
    let shape = b.build();
    assert_eq!(reads(&shape, a), [middle]);
    assert_eq!(reads(&shape, middle), [last]);
    assert_eq!(reads(&shape, last), [] as [u32; 0]);
    let p = partition_of(&shape);
    assert_eq!(p.len(), 3);
    assert_eq!((p.of(last), p.of(middle), p.of(a)), (0, 1, 2));
}

#[test]
fn super_reads_the_ancestor_it_names() {
    let (_db, this, _) = crates();
    let mut b = Builder::new(this, Edition::Edition2021);
    let a = b.module(Builder::ROOT, "a");
    let inner = b.module(a, "b");
    let c = b.module(Builder::ROOT, "c");
    b.import(inner, PathKind::Super(2), &["c", "Y"]);
    b.glob(a, PathKind::Super(1), &[]);
    // Above the root: nothing.
    b.import(Builder::ROOT, PathKind::Super(1), &["c", "Y"]);
    let shape = b.build();
    assert_eq!(reads(&shape, inner), [Builder::ROOT, c]);
    assert_eq!(reads(&shape, a), [Builder::ROOT]);
    assert_eq!(reads(&shape, Builder::ROOT), [] as [u32; 0]);
}

#[test]
fn crate_and_self() {
    let (_db, this, _) = crates();
    let mut b = Builder::new(this, Edition::Edition2021);
    let a = b.module(Builder::ROOT, "a");
    let c = b.module(Builder::ROOT, "b");
    let m = b.module(c, "m");
    b.import(c, PathKind::Crate, &["a", "X"]);
    b.import(c, PathKind::SELF, &["m", "Y"]);
    let shape = b.build();
    assert_eq!(reads(&shape, c), [Builder::ROOT, a, m]);
}

#[test]
fn dollar_crate_of_this_crate_and_another() {
    let (_db, this, other) = crates();
    let mut b = Builder::new(this, Edition::Edition2021);
    let a = b.module(Builder::ROOT, "a");
    let c = b.module(Builder::ROOT, "b");
    b.import(a, PathKind::DollarCrate(this), &["b", "X"]);
    b.import(c, PathKind::DollarCrate(other), &["a", "X"]);
    let shape = b.build();
    assert_eq!(reads(&shape, a), [Builder::ROOT, c]);
    assert_eq!(reads(&shape, c), [] as [u32; 0]);
}

#[test]
fn plain_paths_by_edition() {
    let (_db, this, _) = crates();
    let shape = |edition| {
        let mut b = Builder::new(this, edition);
        let a = b.module(Builder::ROOT, "a");
        let c = b.module(Builder::ROOT, "b");
        let child = b.module(c, "c");
        b.import(c, PathKind::Plain, &["a", "X"]);
        b.import(c, PathKind::Plain, &["c", "Y"]);
        let v = b.module(Builder::ROOT, "v");
        let v_child = b.module(v, "c");
        b.visibility(v, PathKind::Plain, &["c", "Z"]);
        (b.build(), a, c, child, v, v_child)
    };
    // 2015: an import's plain path starts at the crate root; a visibility's at its module.
    let (s2015, a, c, _, v, v_child) = shape(Edition::Edition2015);
    assert_eq!(reads(&s2015, c), [Builder::ROOT, a]);
    assert_eq!(reads(&s2015, v), [v_child]);
    // 2018 on: both at the module.
    for edition in [Edition::Edition2018, Edition::Edition2021, Edition::Edition2024] {
        let (shape, _, c, child, v, v_child) = shape(edition);
        assert_eq!(reads(&shape, c), [child]);
        assert_eq!(reads(&shape, v), [v_child]);
    }
    // `::a` in 2015 is the crate root's `a`; from 2018 on, the extern prelude's only.
    let absolute = |edition| {
        let mut b = Builder::new(this, edition);
        let a = b.module(Builder::ROOT, "a");
        let c = b.module(Builder::ROOT, "b");
        b.import(c, PathKind::Abs, &["a", "X"]);
        (b.build(), a, c)
    };
    let (s2015, a, c) = absolute(Edition::Edition2015);
    assert_eq!(reads(&s2015, c), [Builder::ROOT, a]);
    let (s2018, _, c) = absolute(Edition::Edition2018);
    assert_eq!(reads(&s2018, c), [] as [u32; 0]);
}

#[test]
fn extern_crate_self_names_the_root() {
    let (_db, this, _) = crates();
    let shape = |edition| {
        let mut b = Builder::new(this, edition);
        let a = b.module(Builder::ROOT, "a");
        let c = b.module(Builder::ROOT, "b");
        let d = b.module(Builder::ROOT, "d");
        let e = b.module(Builder::ROOT, "e");
        b.extern_crate_self(Builder::ROOT, name("n"));
        b.extern_crate_self(d, name("me"));
        b.import(c, PathKind::Plain, &["n", "a", "X"]);
        b.import(c, PathKind::Abs, &["n", "a", "Y"]);
        b.import(d, PathKind::Plain, &["me", "a", "X"]);
        // Not declared in `e`, and not in the extern prelude.
        b.import(e, PathKind::Plain, &["me", "a", "X"]);
        (b.build(), a, c, d, e)
    };
    for edition in [Edition::Edition2015, Edition::Edition2018, Edition::Edition2021] {
        let (shape, a, c, d, e) = shape(edition);
        assert_eq!(reads(&shape, c), [Builder::ROOT, a], "{edition:?}");
        if edition != Edition::Edition2015 {
            assert_eq!(reads(&shape, d), [Builder::ROOT, a], "{edition:?}");
            assert_eq!(reads(&shape, e), [] as [u32; 0], "{edition:?}");
        }
    }
}

#[test]
fn prelude_import_is_read_by_every_module() {
    let (_db, this, _) = crates();
    let shape = |edition| {
        let mut b = Builder::new(this, edition);
        let prelude = b.module(Builder::ROOT, "prelude");
        let m = b.module(prelude, "m");
        let x = b.module(Builder::ROOT, "x");
        b.prelude_import(Builder::ROOT, PathKind::Crate, &["prelude"]);
        b.import(x, PathKind::Plain, &["m", "Y"]);
        (b.build(), prelude, m, x)
    };
    let (s2021, prelude, m, x) = shape(Edition::Edition2021);
    // The prelude, the module whose import says which module it is, and `m` through it.
    assert_eq!(reads(&s2021, x), [Builder::ROOT, prelude, m]);
    assert_eq!(reads(&s2021, m), [Builder::ROOT, prelude]);
    assert_eq!(reads(&s2021, prelude), [Builder::ROOT]);
    assert_eq!(reads(&s2021, Builder::ROOT), [prelude]);
    let p = partition_of(&s2021);
    assert_eq!(p.of(Builder::ROOT), p.of(prelude));
    assert!(p.of(prelude) < p.of(m) && p.of(prelude) < p.of(x));
    // A 2015 import's plain path starts at the root, not the std prelude.
    let (s2015, prelude, _, x) = shape(Edition::Edition2015);
    assert_eq!(reads(&s2015, x), [Builder::ROOT, prelude]);
}

#[test]
fn pub_in_path_reads_what_it_names() {
    let (_db, this, _) = crates();
    let mut b = Builder::new(this, Edition::Edition2021);
    let a = b.module(Builder::ROOT, "a");
    let _inner = b.module(a, "inner");
    let c = b.module(Builder::ROOT, "b");
    let d = b.module(Builder::ROOT, "c");
    b.visibility(c, PathKind::Crate, &["a"]);
    b.visibility(d, PathKind::Crate, &["a", "inner"]);
    let shape = b.build();
    assert_eq!(reads(&shape, c), [Builder::ROOT]);
    assert_eq!(reads(&shape, d), [Builder::ROOT, a]);
}

#[test]
fn test_harness_reads_its_parent() {
    let (_db, this, _) = crates();
    let shape = |module_name| {
        let mut b = Builder::new(this, Edition::Edition2021);
        let t = b.module(Builder::ROOT, "t");
        let h = b.module(t, module_name);
        let a = b.module(Builder::ROOT, "a");
        b.import(h, PathKind::Crate, &["a", "X"]);
        (b.build(), t, h, a)
    };
    let (harness, t, h, a) = shape("__blaze_test_harness");
    assert_eq!(reads(&harness, h), [Builder::ROOT, t, a]);
    let (plain, _, h, a) = shape("h");
    assert_eq!(reads(&plain, h), [Builder::ROOT, a]);
}

#[test]
fn names_reach_through_globs_and_imports() {
    let (_db, this, _) = crates();
    let mut b = Builder::new(this, Edition::Edition2021);
    let a = b.module(Builder::ROOT, "a");
    let inner = b.module(a, "inner");
    let c = b.module(Builder::ROOT, "b");
    let d = b.module(Builder::ROOT, "c");
    let e = b.module(Builder::ROOT, "d");
    let f = b.module(Builder::ROOT, "e");
    b.glob(c, PathKind::Crate, &["a"]);
    b.import(d, PathKind::Crate, &["b", "inner", "X"]);
    b.import_as(e, PathKind::Crate, &["a", "inner"], "renamed");
    b.import(f, PathKind::Crate, &["d", "renamed", "X"]);
    let shape = b.build();
    // `inner` of `b` is `a`'s, through the glob; `a` itself is not read by `c`.
    assert_eq!(reads(&shape, c), [Builder::ROOT, a]);
    assert_eq!(reads(&shape, d), [Builder::ROOT, inner, c]);
    assert_eq!(reads(&shape, f), [Builder::ROOT, inner, e]);
}

/// `c::E::V` reads the scope of the module that declares `E` (`declared_enum_visibility`); it
/// is reachable through the import that brought `E` to `b`, so the order allows the read.
#[test]
fn enum_declaration_is_upstream() {
    let (_db, this, _) = crates();
    let mut b = Builder::new(this, Edition::Edition2021);
    let a = b.module(Builder::ROOT, "a");
    let c = b.module(Builder::ROOT, "b");
    let d = b.module(Builder::ROOT, "c");
    b.import(c, PathKind::Crate, &["a", "E"]);
    b.import(d, PathKind::Crate, &["b", "E", "V"]);
    let shape = b.build();
    assert_eq!(reads(&shape, d), [Builder::ROOT, c]);
    let p = partition_of(&shape);
    assert!(p.may_read(p.of(d), p.of(a)));
}

/// A module that globs its own child and is globbed back, through `self` and `super`.
#[test]
fn self_and_super_globs() {
    let (_db, this, _) = crates();
    let mut b = Builder::new(this, Edition::Edition2021);
    let a = b.module(Builder::ROOT, "a");
    let child = b.module(a, "child");
    b.glob(a, PathKind::SELF, &["child"]);
    b.glob(child, PathKind::Super(1), &[]);
    b.glob(a, PathKind::SELF, &[]);
    let shape = b.build();
    assert_eq!(reads(&shape, a), [child]);
    assert_eq!(reads(&shape, child), [a]);
    let p = partition_of(&shape);
    assert_eq!(p.members(p.of(a)), [a, child]);
}

/// Reads bytes as small numbers, 0 past the end, so shrinking the bytes shrinks the shape.
struct Bytes<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Bytes<'_> {
    fn next(&mut self, bound: u32) -> u32 {
        let byte = self.bytes.get(self.at).copied().unwrap_or(0);
        self.at += 1;
        if bound <= 1 { 0 } else { u32::from(byte) % bound }
    }
}

const POOL: [&str; 5] = ["a", "b", "c", "d", "e"];
const HARNESS: &str = "__blaze_test_harness";

fn pick(bytes: &mut Bytes<'_>) -> Name {
    name(POOL[bytes.next(POOL.len() as u32) as usize])
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Def {
    Module(u32),
    Enum { id: u32, container: u32, imported: bool },
    /// Any other item, a variant too: a path stops at it.
    Item,
}

impl Def {
    fn imported(self) -> Def {
        match self {
            Def::Enum { id, container, .. } => Def::Enum { id, container, imported: true },
            other => other,
        }
    }
}

/// A random crate: its shape, and what the shape leaves out (module names, the items each
/// module declares, each enum's variants), which the brute-force resolver uses.
#[derive(Debug)]
struct World {
    shape: ImportShape,
    names: Vec<Option<Name>>,
    decls: Vec<Vec<(Name, Def)>>,
    variants: Vec<Vec<Name>>,
}

fn world(bytes: &[u8], this: Crate, other: Crate) -> World {
    let mut r = Bytes { bytes, at: 0 };
    let editions = [Edition::Edition2015, Edition::Edition2018, Edition::Edition2021];
    let edition = editions[r.next(3) as usize];
    let count = 1 + r.next(7);
    let mut b = Builder::new(this, edition);
    let mut names = vec![None];
    for module in 1..count {
        let parent = r.next(module);
        let child_name = if r.next(8) == 0 { name(HARNESS) } else { pick(&mut r) };
        b.child(parent, child_name);
        names.push(Some(child_name));
    }
    if r.next(3) == 0 {
        let alias = pick(&mut r);
        b.extern_crate_self(Builder::ROOT, alias);
    }
    if r.next(4) == 0 {
        let (module, alias) = (r.next(count), pick(&mut r));
        b.extern_crate_self(module, alias);
    }
    let mut decls = vec![Vec::new(); count as usize];
    let mut variants = Vec::new();
    for module in 0..count {
        for _ in 0..r.next(3) {
            let item = pick(&mut r);
            if r.next(2) == 0 {
                let id = variants.len() as u32;
                let len = 1 + r.next(2);
                variants.push((0..len).map(|_| pick(&mut r)).collect());
                decls[module as usize].push((item, Def::Enum { id, container: module, imported: false }));
            } else {
                decls[module as usize].push((item, Def::Item));
            }
        }
        for _ in 0..r.next(4) {
            let kind = match r.next(7) {
                0 | 1 => PathKind::Plain,
                2 => PathKind::Crate,
                3 => PathKind::Super(r.next(3) as u8),
                4 => PathKind::Abs,
                5 => PathKind::DollarCrate(this),
                _ => PathKind::DollarCrate(other),
            };
            let len = r.next(4);
            let segments: Vec<Name> = (0..len).map(|_| pick(&mut r)).collect();
            let is_glob = r.next(3) == 0;
            let is_prelude = is_glob && r.next(6) == 0;
            let binds = match (is_glob, r.next(6)) {
                (true, _) => None,
                (false, 0) => Some(pick(&mut r)),
                (false, 1) => None,
                (false, _) => segments.last().copied(),
            };
            let path = ModPath::from_segments(kind, segments);
            b.raw_import(module, ShapeImport { path, binds, is_glob, is_prelude });
        }
        if r.next(3) == 0 {
            let kind = match r.next(4) {
                0 => PathKind::Plain,
                1 => PathKind::Crate,
                2 => PathKind::Super(r.next(3) as u8),
                _ => PathKind::Abs,
            };
            let len = r.next(4);
            let segments: Vec<Name> = (0..len).map(|_| pick(&mut r)).collect();
            b.modules[module as usize].other_paths.push(ModPath::from_segments(kind, segments));
        }
    }
    World { shape: b.build(), names, decls, variants }
}

/// Name resolution over a [`World`] by `path_resolution.rs`'s rules, with every binding a name
/// can ever get at once (no shadowing, no visibility), to a fixed point, recording every
/// module whose scope a lookup reads.
struct Model<'w> {
    world: &'w World,
    scopes: Vec<HashMap<Name, HashSet<Def>>>,
    preludes: BTreeSet<u32>,
    reads: HashSet<(u32, u32)>,
}

impl<'w> Model<'w> {
    fn new(world: &'w World) -> Model<'w> {
        let mut scopes: Vec<HashMap<Name, HashSet<Def>>> =
            vec![HashMap::new(); world.shape.modules.len()];
        for (module, data) in world.shape.modules.iter().enumerate() {
            for &(local, child) in data.local_names.iter() {
                scopes[module].entry(local).or_default().insert(Def::Module(child));
            }
            for &(item, def) in &world.decls[module] {
                scopes[module].entry(item).or_default().insert(def);
            }
        }
        Model { world, scopes, preludes: BTreeSet::new(), reads: HashSet::new() }
    }

    fn add(&mut self, module: u32, binding: Name, def: Def) -> bool {
        self.scopes[module as usize].entry(binding).or_default().insert(def)
    }

    fn get(&self, module: u32, binding: &Name) -> Vec<Def> {
        self.scopes[module as usize].get(binding).map_or_else(Vec::new, |it| it.iter().copied().collect())
    }

    fn read(&mut self, reader: u32, read: u32) {
        self.reads.insert((reader, read));
    }

    fn resolve(&mut self, module: u32, path: &ModPath, mode: ResolveMode) -> Vec<Def> {
        let world = self.world;
        let shape = &world.shape;
        let segments = path.segments();
        let in_extern_prelude = |it: &Name| shape.extern_prelude_self.contains(it);
        let (mut current, resolved) = match path.kind {
            PathKind::DollarCrate(krate) if krate == shape.krate => (vec![Def::Module(0)], 0),
            PathKind::DollarCrate(_) => return Vec::new(),
            PathKind::Crate => (vec![Def::Module(0)], 0),
            PathKind::Super(levels) => {
                let mut at = Some(module);
                for _ in 0..levels {
                    at = at.and_then(|it| shape.modules[it as usize].parent);
                }
                match at {
                    Some(ancestor) => (vec![Def::Module(ancestor)], 0),
                    None => return Vec::new(),
                }
            }
            PathKind::Plain | PathKind::Abs
                if shape.edition == Edition::Edition2015
                    && (path.kind == PathKind::Abs || mode == ResolveMode::Import) =>
            {
                let Some(first) = segments.first() else { return vec![Def::Module(0)] };
                self.read(module, 0);
                let mut defs = self.get(0, first);
                if in_extern_prelude(first) {
                    defs.push(Def::Module(0));
                }
                (defs, 1)
            }
            PathKind::Plain => {
                let Some(first) = segments.first() else { return Vec::new() };
                self.read(module, module);
                let mut defs = self.get(module, first);
                if in_extern_prelude(first) {
                    defs.push(Def::Module(0));
                }
                for prelude in self.preludes.clone() {
                    self.read(module, prelude);
                    defs.extend(self.get(prelude, first));
                }
                (defs, 1)
            }
            PathKind::Abs => {
                let Some(first) = segments.first() else { return Vec::new() };
                (if in_extern_prelude(first) { vec![Def::Module(0)] } else { Vec::new() }, 1)
            }
        };
        let parent = shape.modules[module as usize].parent;
        let is_harness = parent.is_some() && world.names[module as usize] == Some(name(HARNESS));
        for segment in &segments[resolved..] {
            let mut next = Vec::new();
            for def in current {
                match def {
                    Def::Module(scope) => {
                        self.read(module, scope);
                        if let (true, Some(parent)) = (is_harness, parent) {
                            self.read(module, parent);
                        }
                        next.extend(self.get(scope, segment));
                    }
                    Def::Enum { id, container, imported } => {
                        if imported {
                            self.read(module, container);
                        }
                        if world.variants[id as usize].contains(segment) {
                            next.push(Def::Item);
                        }
                    }
                    Def::Item => {}
                }
            }
            next.sort_unstable();
            next.dedup();
            current = next;
        }
        current
    }

    fn run(&mut self) {
        let world = self.world;
        loop {
            let mut changed = false;
            for (module, data) in world.shape.modules.iter().enumerate() {
                let module = module as u32;
                for import in data.imports.iter() {
                    let denoted = self.resolve(module, &import.path, ResolveMode::Import);
                    if import.is_glob && import.is_prelude {
                        for def in denoted {
                            if let Def::Module(target) = def {
                                changed |= self.preludes.insert(target);
                            }
                        }
                    } else if import.is_glob {
                        for def in denoted {
                            match def {
                                Def::Module(target) => {
                                    self.read(module, target);
                                    let all: Vec<(Name, Def)> = self.scopes[target as usize]
                                        .iter()
                                        .flat_map(|(n, defs)| defs.iter().map(move |&d| (*n, d)))
                                        .collect();
                                    for (binding, def) in all {
                                        changed |= self.add(module, binding, def.imported());
                                    }
                                }
                                Def::Enum { id, .. } => {
                                    for &variant in &world.variants[id as usize] {
                                        changed |= self.add(module, variant, Def::Item);
                                    }
                                }
                                Def::Item => {}
                            }
                        }
                    } else if let Some(binding) = import.binds {
                        for def in denoted {
                            changed |= self.add(module, binding, def.imported());
                        }
                    }
                }
                for other in data.other_paths.iter() {
                    self.resolve(module, other, ResolveMode::Other);
                }
            }
            if !changed {
                break;
            }
        }
    }
}

#[test]
fn partition_allows_every_read_of_a_brute_force_resolver() {
    let (_db, this, other) = crates();
    run_cases(2048, |tc| {
        let bytes: Vec<u8> = tc.draw(gs::binary().max_size(191));
        let world = world(&bytes, this, other);
        let mut model = Model::new(&world);
        model.run();
        let p = partition_of(&world.shape);
        check_well_formed(&p, world.shape.modules.len() as u32);
        for &(reader, read) in &model.reads {
            assert!(
                p.may_read(p.of(reader), p.of(read)),
                "module {} reads module {}, which comes after it: {:?}",
                reader,
                read,
                world
            );
        }
    });
}

/// Module `m{i}` globs `m{i + 1}` and imports `super::m{i + 1}::x`: edges grow with the chain.
fn chain(krate: Crate, len: u32) -> ImportShape {
    let mut b = Builder::new(krate, Edition::Edition2021);
    let modules: Vec<u32> = (0..len).map(|i| b.module(Builder::ROOT, &format!("m{i}"))).collect();
    for i in 0..len as usize - 1 {
        let next = format!("m{}", i + 1);
        b.glob(modules[i], PathKind::Super(1), &[&next]);
        b.import(modules[i], PathKind::Super(1), &[&next, "x"]);
    }
    b.build()
}

/// `partition`'s global allocations: the value's three columns (`of`, `starts`, `members`),
/// each at exact size. Everything else is in the task's frame, which reserves its memory from
/// the system and not the global heap: the solver's 12 tables (paths, the levels' paths and
/// first modules, the pair map and rows, the per-module pair and glob heads, the links, the
/// seen set, the worklist, the prelude levels, the edge list), the graph's 5 (the starts and
/// sources by target, the cursors, offsets, targets) and the components' 5 (`rindex`, frames,
/// stack, component starts, members), so none of them, nor any growth past the fixed point's
/// estimate, is counted here.
const PARTITION_ALLOCATIONS: u64 = 3;

#[test]
fn partition_allocations_do_not_grow_with_edges() {
    let (_db, this, _) = crates();
    let small = chain(this, 16);
    let large = chain(this, 4096);
    task(|frame| {
        assert!(import_graph(frame, &large).edge_count() > 100 * import_graph(frame, &small).edge_count());
    });
    // The first call interns the test harness's name, and reserves the task's frame.
    partition_of(&small);
    let (small_partition, small_used) = blaze_alloc_count::measure(|| partition_of(&small));
    let (large_partition, large_used) = blaze_alloc_count::measure(|| partition_of(&large));
    assert_eq!(small_partition.len(), 17);
    assert_eq!(large_partition.len(), 4097);
    assert_eq!(small_used.allocations, large_used.allocations, "{small_used:?} vs {large_used:?}");
    assert!(
        large_used.allocations <= PARTITION_ALLOCATIONS,
        "{} allocations, budget {PARTITION_ALLOCATIONS}",
        large_used.allocations
    );
}
