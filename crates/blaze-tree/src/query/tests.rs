use std::sync::Arc;

use blaze_query::{Config, Database, MemTree, Query, QueryKey};
use blaze_units::query::{BuildConfig, Command, Fetch, FetchKind, Invocation, Mount, Origin};
use blaze_vfs::{View, ViewId};

use super::*;

const WS: &str = "/home/u/ws";
const A: &str = "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\nb = { path = \"b\" }\n\n[dev-dependencies]\nc = { path = \"c\" }\n";
const B: &str = "[package]\nname = \"b\"\nversion = \"0.1.0\"\nedition = \"2024\"\n";
const C: &str = "[package]\nname = \"c\"\nversion = \"0.2.0\"\nedition = \"2024\"\n";

fn asked() -> TreeAsk {
    TreeAsk {
        resolve: ResolveAsk::default(),
        targets: Vec::new(),
        edges: vec![Edge::Normal, Edge::Build, Edge::Dev],
        invert: Vec::new(),
        prune: Vec::new(),
        prefix: TreePrefix::Indent,
        no_dedupe: false,
        duplicates: false,
        format: "{p}".to_owned(),
        depth: TreeDepth::Max(u32::MAX),
        no_proc_macro: false,
        public: false,
        unicode: true,
        color: false,
    }
}

fn database(tree: &Arc<MemTree>) -> Database {
    let mut db = Database::new(Config { tree: Some(tree.clone()), dir: None, tags: 0 });
    blaze_units::query::register(&mut db);
    register(&mut db);
    db
}

fn open(db: &Database, tree: &MemTree, files: &[(&str, &str)]) -> ViewId {
    let files: Vec<(&str, &[u8])> = files.iter().map(|(path, text)| (*path, text.as_bytes())).collect();
    let config = BuildConfig {
        invocation: Invocation {
            command: Command::Build,
            args: Vec::new(),
            root: WS.to_owned(),
            cwd: WS.to_owned(),
            env: vec![("HOME".to_owned(), "/home/u".to_owned())],
            files: Vec::new(),
        },
        mounts: vec![Mount { at: WS.to_owned(), origin: Origin::View, under: "ws".to_owned() }],
        host: "x86_64-unknown-linux-gnu".to_owned(),
        library: "/rustc/0/library".to_owned(),
        std: "/s/std".to_owned(),
    };
    db.open_with(View { root: tree.root(&files) }, &config.encode())
}

fn workspace() -> [(&'static str, &'static str); 6] {
    [
        ("ws/Cargo.toml", A),
        ("ws/src/lib.rs", ""),
        ("ws/b/Cargo.toml", B),
        ("ws/b/src/lib.rs", ""),
        ("ws/c/Cargo.toml", C),
        ("ws/c/src/lib.rs", ""),
    ]
}

fn printed(db: &Database, view: ViewId, ask: &TreeAsk) -> String {
    match db.pin().get::<Tree>(view, ask).expect("printed").get() {
        Printed::Ready { out, warnings } => {
            assert!(warnings.is_empty(), "{warnings:?}");
            out
        }
        other => panic!("not printed: {other:?}"),
    }
}

#[test]
fn the_tree_is_cargos() {
    let tree = Arc::new(MemTree::default());
    let db = database(&tree);
    let view = open(&db, &tree, &workspace());
    assert_eq!(
        printed(&db, view, &asked()),
        "a v0.1.0 (/home/u/ws)\n└── b v0.1.0 (/home/u/ws/b)\n[dev-dependencies]\n└── c v0.2.0 (/home/u/ws/c)\n"
    );
    let normal = TreeAsk { edges: vec![Edge::Normal], ..asked() };
    assert_eq!(printed(&db, view, &normal), "a v0.1.0 (/home/u/ws)\n└── b v0.1.0 (/home/u/ws/b)\n");
    let inverted = TreeAsk { invert: vec!["b".to_owned()], ..asked() };
    assert_eq!(printed(&db, view, &inverted), "b v0.1.0 (/home/u/ws/b)\n└── a v0.1.0 (/home/u/ws)\n");
    let ascii = TreeAsk { unicode: false, prefix: TreePrefix::Depth, ..normal };
    assert_eq!(printed(&db, view, &ascii), "0a v0.1.0 (/home/u/ws)\n1b v0.1.0 (/home/u/ws/b)\n");
}

#[test]
fn a_source_edit_prints_from_the_tree_it_had() {
    let tree = Arc::new(MemTree::default());
    let db = database(&tree);
    let first = printed(&db, open(&db, &tree, &workspace()), &asked());
    let mut edited = workspace();
    edited[1] = ("ws/src/lib.rs", "pub fn f() {}\n");
    let view = open(&db, &tree, &edited);
    assert_eq!(printed(&db, view, &asked()), first);
    assert!(db.why(view, &QueryKey { kind: Tree::KIND, input: Tree::input(&asked()) }).is_none());
}

#[test]
fn a_registry_dependency_is_fetched_first() {
    let tree = Arc::new(MemTree::default());
    let db = database(&tree);
    let manifest = "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\nserde = \"1\"\n";
    let view = open(&db, &tree, &[("ws/Cargo.toml", manifest), ("ws/src/lib.rs", "")]);
    assert_eq!(
        db.pin().get::<Tree>(view, &asked()).expect("printed").get(),
        Printed::Needs(vec![Fetch {
            what: "crates.io index".to_owned(),
            url: "https://index.crates.io/se/rd/serde".to_owned(),
            kind: FetchKind::IndexFile,
            at: "/home/u/.cargo/registry/index/index.crates.io-1949cf8c6b5b557f/.cache/se/rd/serde".to_owned(),
        }])
    );
}

#[test]
fn features_and_duplicates_are_cargos_error() {
    let tree = Arc::new(MemTree::default());
    let db = database(&tree);
    let view = open(&db, &tree, &workspace());
    let both = TreeAsk { edges: vec![Edge::Features], duplicates: true, ..asked() };
    assert_eq!(
        db.pin().get::<Tree>(view, &both).expect("printed").get(),
        Printed::Failed(LoadError {
            what: "the `-e features` flag does not support `--duplicates`".to_owned(),
            at: None
        })
    );
}

fn manifest(name: &str, version: &str, deps: &str) -> String {
    format!("[package]\nname = \"{name}\"\nversion = \"{version}\"\nedition = \"2024\"\n{deps}")
}

fn view_of(db: &Database, tree: &MemTree, files: &[(String, String)]) -> ViewId {
    let files: Vec<(&str, &str)> = files.iter().map(|(path, text)| (path.as_str(), text.as_str())).collect();
    open(db, tree, &files)
}

#[test]
fn a_cycle_through_a_dev_dependency_is_cut_where_it_closes() {
    let tree = Arc::new(MemTree::default());
    let db = database(&tree);
    let files = [
        ("ws/Cargo.toml".to_owned(), manifest("a", "0.1.0", "\n[dev-dependencies]\nc = { path = \"c\" }\n")),
        ("ws/src/lib.rs".to_owned(), String::new()),
        ("ws/c/Cargo.toml".to_owned(), manifest("c", "0.2.0", "\n[dependencies]\na = { path = \"..\" }\n")),
        ("ws/c/src/lib.rs".to_owned(), String::new()),
    ];
    let view = view_of(&db, &tree, &files);
    let cut = "a v0.1.0 (/home/u/ws)\n[dev-dependencies]\n└── c v0.2.0 (/home/u/ws/c)\n    └── a v0.1.0 (/home/u/ws) (*)\n";
    assert_eq!(printed(&db, view, &asked()), cut);
    // With every repeat printed in full, the cycle is still cut at the node already above.
    assert_eq!(printed(&db, view, &TreeAsk { no_dedupe: true, ..asked() }), cut);
}

#[test]
fn a_pruned_package_is_left_out_but_its_section_is_not() {
    let tree = Arc::new(MemTree::default());
    let db = database(&tree);
    let view = open(&db, &tree, &workspace());
    let prune_b = TreeAsk { prune: vec!["b".to_owned()], ..asked() };
    assert_eq!(printed(&db, view, &prune_b), "a v0.1.0 (/home/u/ws)\n[dev-dependencies]\n└── c v0.2.0 (/home/u/ws/c)\n");
    let prune_c = TreeAsk { prune: vec!["c".to_owned()], ..asked() };
    assert_eq!(printed(&db, view, &prune_c), "a v0.1.0 (/home/u/ws)\n└── b v0.1.0 (/home/u/ws/b)\n[dev-dependencies]\n");
    let unknown = TreeAsk { prune: vec!["zzz".to_owned()], ..asked() };
    assert!(matches!(db.pin().get::<Tree>(view, &unknown).expect("printed").get(), Printed::Failed(_)));
}

#[test]
fn an_inverted_tree_keeps_each_edge_in_its_section() {
    let tree = Arc::new(MemTree::default());
    let db = database(&tree);
    let view = open(&db, &tree, &workspace());
    let inverted = TreeAsk { invert: vec!["c".to_owned()], ..asked() };
    assert_eq!(printed(&db, view, &inverted), "c v0.2.0 (/home/u/ws/c)\n[dev-dependencies]\n└── a v0.1.0 (/home/u/ws)\n");
}

#[test]
fn duplicates_are_each_version_of_a_name_inverted() {
    let tree = Arc::new(MemTree::default());
    let db = database(&tree);
    let files = [
        (
            "ws/Cargo.toml".to_owned(),
            manifest("a", "0.1.0", "\n[dependencies]\nb = { path = \"b\" }\nb2 = { path = \"b2\", package = \"b\" }\n"),
        ),
        ("ws/src/lib.rs".to_owned(), String::new()),
        ("ws/b/Cargo.toml".to_owned(), manifest("b", "0.1.0", "")),
        ("ws/b/src/lib.rs".to_owned(), String::new()),
        ("ws/b2/Cargo.toml".to_owned(), manifest("b", "0.2.0", "")),
        ("ws/b2/src/lib.rs".to_owned(), String::new()),
    ];
    let view = view_of(&db, &tree, &files);
    let duplicates = TreeAsk { duplicates: true, ..asked() };
    // `a` the second time has nothing above it in the inverted tree, so no `(*)`.
    assert_eq!(
        printed(&db, view, &duplicates),
        "b v0.1.0 (/home/u/ws/b)\n└── a v0.1.0 (/home/u/ws)\n\nb v0.2.0 (/home/u/ws/b2)\n└── a v0.1.0 (/home/u/ws)\n"
    );
}

#[test]
fn no_duplicates_is_nothing_to_print() {
    let tree = Arc::new(MemTree::default());
    let db = database(&tree);
    let view = open(&db, &tree, &workspace());
    let duplicates = TreeAsk { duplicates: true, ..asked() };
    match db.pin().get::<Tree>(view, &duplicates).expect("printed").get() {
        Printed::Ready { out, warnings } => {
            assert_eq!(out, "");
            assert_eq!(warnings.len(), 1);
            assert!(warnings[0].starts_with("nothing to print."), "{warnings:?}");
        }
        other => panic!("not printed: {other:?}"),
    }
}

fn features_workspace() -> [(String, String); 4] {
    [
        ("ws/Cargo.toml".to_owned(), manifest("a", "0.1.0", "\n[dependencies]\nb = { path = \"b\", features = [\"x\"] }\n")),
        ("ws/src/lib.rs".to_owned(), String::new()),
        ("ws/b/Cargo.toml".to_owned(), manifest("b", "0.1.0", "\n[features]\nx = []\n")),
        ("ws/b/src/lib.rs".to_owned(), String::new()),
    ]
}

#[test]
fn features_are_nodes_between_a_package_and_its_dependency() {
    let tree = Arc::new(MemTree::default());
    let db = database(&tree);
    let view = view_of(&db, &tree, &features_workspace());
    let features = TreeAsk { edges: vec![Edge::Normal, Edge::Build, Edge::Dev, Edge::Features], ..asked() };
    assert_eq!(
        printed(&db, view, &features),
        "a v0.1.0 (/home/u/ws)\n├── b feature \"default\"\n│   └── b v0.1.0 (/home/u/ws/b)\n└── b feature \"x\"\n    └── b v0.1.0 (/home/u/ws/b)\n"
    );
    // Inverted, the features lead up to the member and the command line's feature of it.
    let inverted = TreeAsk { invert: vec!["b".to_owned()], ..features };
    assert_eq!(
        printed(&db, view, &inverted),
        "b v0.1.0 (/home/u/ws/b)\n├── b feature \"default\"\n│   └── a v0.1.0 (/home/u/ws)\n│       └── a feature \"default\" (command-line)\n└── b feature \"x\"\n    └── a v0.1.0 (/home/u/ws) (*)\n"
    );
}

#[test]
fn a_cycle_of_features_is_cut_at_the_feature_already_printed() {
    let tree = Arc::new(MemTree::default());
    let db = database(&tree);
    let mut files = features_workspace();
    files[2].1 = manifest("b", "0.1.0", "\n[features]\nx = [\"y\"]\ny = [\"x\"]\n");
    let view = view_of(&db, &tree, &files);
    let features = TreeAsk { edges: vec![Edge::Normal, Edge::Build, Edge::Dev, Edge::Features], ..asked() };
    let cut = "a v0.1.0 (/home/u/ws)\n├── b feature \"default\"\n│   └── b v0.1.0 (/home/u/ws/b)\n└── b feature \"x\"\n    ├── b v0.1.0 (/home/u/ws/b)\n    └── b feature \"y\"\n        ├── b v0.1.0 (/home/u/ws/b)\n        └── b feature \"x\" (*)\n";
    assert_eq!(printed(&db, view, &features), cut);
    assert_eq!(printed(&db, view, &TreeAsk { no_dedupe: true, ..features }), cut);
}

#[test]
fn a_repeat_is_a_memo_hit_that_allocates_nothing() {
    let tree = Arc::new(MemTree::default());
    let db = database(&tree);
    let view = open(&db, &tree, &workspace());
    let ask = asked();
    let first = printed(&db, view, &ask);
    let pinned = db.pin();
    assert!(pinned.get::<Tree>(view, &ask).is_ok());
    let hit = blaze_alloc_count::assert_at_most(0, "a repeated cargo tree", || pinned.get::<Tree>(view, &ask).is_ok());
    assert!(hit);
    assert_eq!(printed(&db, view, &ask), first);
}

/// What printing the trees allocated, and what it printed.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
struct Measured {
    allocations: u64,
    out: String,
}

impl Versioned for Measured {
    const SCHEMA: SchemaHash = blaze_format::schema("blaze-tree test measured 1");
}

/// [`Tree`], counting what it allocates past the feature resolve: the graph and the walk.
struct Measure;

impl Query for Measure {
    const KIND: QueryKind = QueryKind(0x61f0);
    const NAME: &'static str = "cargo tree, measured";
    const PRODUCER: ContentId = producer("blaze-tree test measure 1");
    type Key = TreeAsk;
    type Value = Cbor<Measured>;

    fn input(key: &TreeAsk) -> ContentId {
        key_id("blaze-tree test measure key 1", key)
    }

    fn execute(cx: &mut Cx<'_>, ask: &TreeAsk) -> Result<Cbor<Measured>, Cancelled> {
        let opts = ask.options();
        let resolve = ResolveAsk { dev_units: ask.edges.contains(&Edge::Dev), ..ask.resolve.clone() };
        let measured = resolve_view(cx, &resolve, |view| {
            let (input, kinds) = view_input(view, &opts)?;
            let trees = crate::resolve_trees(&input, &opts, &kinds).map_err(|error| load_error(&error, None))?;
            let mut out = String::with_capacity(1 << 16);
            let (printed, used) = blaze_alloc_count::measure(|| {
                crate::print_trees(&input, &opts, &kinds, trees.into_iter().map(Ok), &mut out, &mut |it: &str| {
                    assert!(it.is_empty(), "warned: {it}")
                })
            });
            printed.map_err(|error| load_error(&error, None))?;
            Ok(Measured { allocations: used.allocations, out })
        })?;
        match measured {
            Resolving::Done(measured) => Ok(Cbor(measured)),
            Resolving::Needs(_) | Resolving::Failed(_) => panic!("the workspace did not resolve"),
        }
    }
}

/// The member `a` and eight path packages under it: `a -> p1 -> .. -> p8` as a chain, or with
/// each depending on every later one (36 edges rather than 8, and many more lines).
fn packages(dense: bool) -> Vec<(String, String)> {
    const N: usize = 8;
    let deps = |from: usize, prefix: &str| -> String {
        let to: Vec<usize> = match dense {
            true => (from + 1..=N).collect(),
            false => (from + 1..=N.min(from + 1)).collect(),
        };
        let lines: String = to.iter().map(|j| format!("p{j} = {{ path = \"{prefix}p{j}\" }}\n")).collect();
        format!("\n[dependencies]\n{lines}")
    };
    let mut files = vec![
        ("ws/Cargo.toml".to_owned(), manifest("a", "0.1.0", &deps(0, ""))),
        ("ws/src/lib.rs".to_owned(), String::new()),
    ];
    for i in 1..=N {
        files.push((format!("ws/p{i}/Cargo.toml"), manifest(&format!("p{i}"), "0.1.0", &deps(i, "../"))));
        files.push((format!("ws/p{i}/src/lib.rs"), String::new()));
    }
    files
}

/// The graph's and the walk's buffers, a fixed number, beside the output (which the caller
/// sizes): a few per package (the dependencies' iterator of blaze-resolve's resolve graph,
/// each path source's text, once), none per edge and none per line.
const FIXED: u64 = 48;
const PER_PACKAGE: u64 = 6;

#[test]
fn the_graph_and_walk_allocate_a_fixed_number_of_buffers() {
    let measure = |dense: bool| -> (Measured, String) {
        let tree = Arc::new(MemTree::default());
        let mut db = database(&tree);
        db.register::<Measure>();
        let view = view_of(&db, &tree, &packages(dense));
        let measured = db.pin().get::<Measure>(view, &asked()).expect("measured").get();
        (measured, printed(&db, view, &asked()))
    };
    let (chain, chain_out) = measure(false);
    let (dense, dense_out) = measure(true);
    assert_eq!(chain.out, chain_out);
    assert_eq!(dense.out, dense_out);
    assert!(chain_out.starts_with("a v0.1.0 (/home/u/ws)\n└── p1 v0.1.0 (/home/u/ws/p1)\n    └── p2 v0.1.0"), "{chain_out}");
    assert!(dense_out.lines().count() > 2 * chain_out.lines().count(), "{dense_out}");
    let budget = FIXED + PER_PACKAGE * 9;
    assert!(chain.allocations <= budget, "a chain of 9 packages: {} allocations, budget {budget}", chain.allocations);
    assert!(dense.allocations <= budget, "9 packages, 36 edges: {} allocations, budget {budget}", dense.allocations);
    // More edges and lines, the same packages: the same buffers, a few more for growth at most.
    assert!(
        dense.allocations <= chain.allocations + 4,
        "edges cost allocations: {} for the chain, {} dense",
        chain.allocations,
        dense.allocations
    );
}
