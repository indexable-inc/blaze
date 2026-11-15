//! The real front end on real text: a library and a binary that calls it, written into a
//! temporary directory, keyed with blaze-unitkey and checked with blaze-front's two tasks in
//! the command's snapshots. No scripted front.
//!
//! The invariant: a surface record is a function of its unit's text and its dependencies'
//! surfaces (built twice, the same bytes); a body record is found again, by answering its
//! reads on the new universe, after an edit that no read of it saw, and is not after an edit
//! to a signature it read; and a dependency is never lowered in the process that checks its
//! dependent (no `crate_resolution` runs for it).
//!
//! The planted bug: `snapshot::LOWER_DEPS`, the snapshot that does not give its dependencies,
//! which the oracle [`lowered`] names by [`LOWERED`].

use core::alloc::Allocator;
use std::alloc::Global;

use blaze_ids::ContentId;
use blaze_units::schema::EdgeMarks;
use blaze_keyed::{
    driver::Bodies,
    front::{Front, Table, Universe},
    kind::{Body, Key, Namespace, Origin},
    record::{self, Fact, Owner},
};

use crate::{
    front::{Command, Source, SourceError, cfg_of, host, workspace_of},
    graph::{Dep, Graph, Unit, UnitKind},
    snapshot::{self, LOWER_DEPS, RESOLVED},
};

/// The oracle's name for a dependency lowered in the process that checks its dependent.
const LOWERED: &str = "a dependency was resolved from source in a snapshot that should have given it";

/// The disk, for the tests' files.
struct Disk;

impl Source for Disk {
    fn read<B: core::alloc::Allocator>(&mut self, path: &str, into: &mut Vec<u8, B>) -> Result<bool, SourceError> {
        into.clear();
        match std::fs::read(path) {
            Ok(bytes) => {
                into.extend_from_slice(&bytes);
                Ok(true)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(_) => Err("a test's file does not read"),
        }
    }

    fn exists(&mut self, path: &str) -> Result<bool, SourceError> {
        Ok(std::path::Path::new(path).is_file())
    }
}

const LIB: &str = "#![no_core]\npub fn make(x: u32) -> u32 { x }\npub fn other() -> u32 { 1 }\n";
const LIB_OTHER_BODY: &str = "#![no_core]\npub fn make(x: u32) -> u32 { x }\npub fn other() -> u32 { 2 }\n";
const LIB_SIGNATURE: &str = "#![no_core]\npub fn make(x: u64) -> u64 { x }\npub fn other() -> u32 { 1 }\n";
// No block in `main` holds an item or a `use`: such a block's def map is refused by
// `blaze_front::facts::BLOCK_SCOPE`.
const BIN: &str = "#![no_core]\nfn main() { let _ = lib::make(1); }\n";

/// The two crates in a fresh directory of their own, `lib` its library's text.
fn fixture<A: core::alloc::Allocator + Clone>(name: &str, lib: &str, alloc: A) -> Box<str, A> {
    let dir = std::env::temp_dir().join(format!("blaze-command-front-{}-{name}", std::process::id()));
    let dir = dir.to_str().expect("the temporary directory's path is UTF-8");
    std::fs::create_dir_all(format!("{dir}/lib")).expect("the fixture's directory");
    std::fs::create_dir_all(format!("{dir}/bin")).expect("the fixture's directory");
    std::fs::write(format!("{dir}/lib/lib.rs"), lib).expect("the library's root");
    std::fs::write(format!("{dir}/bin/main.rs"), BIN).expect("the binary's root");
    crate::boxed(dir, alloc)
}

/// The graph of the fixture in `dir`: the library (unit 0) and the binary (unit 1), each
/// owner the prefix of its crate key.
fn graph<A: Allocator + Clone>(dir: &str, alloc: A) -> Graph<A> {
    let text = |it: &str| crate::boxed(it, alloc.clone());
    let unit = |name: &str, root: &str, kind: UnitKind, deps: Vec<Dep<A>, A>| Unit {
        name: text(name),
        package: text(&format!("{name} 0.1.0")),
        edition: span::Edition::Edition2021,
        root: text(root),
        kind,
        cfg: Vec::new_in(alloc.clone()),
        deps,
        env: Vec::new_in(alloc.clone()),
        member: true,
        // Set below from the front end's crate keys, as `graph::load` sets them.
        owner: Owner([0; 8]),
        namespace: Namespace::LOCAL,
        key: ContentId([0; 32]),
        origin: Origin::Private,
        bodies: Bodies::Asked,
        blaze_edition: None,
        settings: ContentId::of("blaze-command test unit settings", b"1"),
        blocked: None,
    };
    let mut units = Vec::new_in(alloc.clone());
    units.push(unit("lib", &format!("{dir}/lib/lib.rs"), UnitKind::Lib, Vec::new_in(alloc.clone())));
    let mut deps = Vec::new_in(alloc.clone());
    deps.push(Dep { name: text("lib"), unit: 0, marks: EdgeMarks { prelude: true, sysroot: false } });
    units.push(unit("bin", &format!("{dir}/bin/main.rs"), UnitKind::Bin, deps));
    let triple = host().expect("the test runs on a host blaze has a target for");
    let mut graph = Graph {
        units,
        tool: ContentId::of("blaze-command test tool", b"1"),
        settings: ContentId::of("blaze-command test settings", b"1"),
        target: ContentId::of("blaze-command test target", triple.as_bytes()),
        triple: text(triple),
        checkout: text(dir),
        identity: ContentId([0; 32]),
        load: crate::graph::LoadTimes::default(),
        programs: Vec::new_in(alloc.clone()),
        library: crate::graph::LibraryNames { dir: text("/library"), printed: text("/rustc/0/library") },
    };
    key_units(&mut graph, alloc);
    graph
}

/// Each unit's crate key and owner from its cfg, as `graph::load` sets them.
fn key_units<A: Allocator + Clone>(graph: &mut Graph<A>, alloc: A) {
    let triple = host().expect("the test runs on a host blaze has a target for");
    let workspace = triomphe::Arc::new(workspace_of(triple).expect("the host's target"));
    let mut cfgs = Vec::new_in(alloc.clone());
    cfgs.extend(graph.units.iter().map(|unit| cfg_of(&unit.cfg)));
    let keys = snapshot::crate_keys(&*graph, &workspace, &cfgs, alloc).expect("the fixture's crate keys");
    for (unit, key) in graph.units.iter_mut().zip(keys.iter()) {
        unit.key = *key;
        unit.owner = Owner::of_subject(key);
    }
}

fn command<A: Allocator + Clone + Send + Sync>(graph: &Graph<A>, alloc: A) -> Command<'_, A> {
    let mut command = Command::new(graph, alloc).expect("the fixture's command");
    command.gather(0, &mut Disk).expect("the library's text");
    command.gather(1, &mut Disk).expect("the binary's text");
    command
}

/// Unit `unit`'s surface record from `tables` (sorted by owner).
fn surface<A: Allocator + Clone + Send + Sync>(
    command: &Command<'_, A>,
    unit: u32,
    tables: &[Table<'_>],
) -> blaze_format::Aligned {
    let keyed = command.key(unit, Global).expect("the unit's keys");
    let universe = Universe { asker: unit, tables, outside: command.late(unit) };
    command.surface(unit, keyed.surface, universe).unwrap_or_else(|error| failed("the unit's surface", error))
}

/// A task's refusal, with the whole `Unreadable` behind it when there was one (the crate, the
/// part and why).
fn failed(what: &str, error: blaze_keyed::driver::TaskFailed) -> ! {
    let unreadable = crate::front::LAST_UNREADABLE.with(|it| it.borrow_mut().take());
    match unreadable {
        Some(unreadable) => panic!("{what}: {error:?}: {unreadable}"),
        None => panic!("{what}: {error:?}"),
    }
}

/// What one checked fixture made.
struct Checked {
    lib: blaze_format::Aligned,
    bin: blaze_format::Aligned,
    main: Key<Body>,
    body: blaze_format::Aligned,
}

/// The library's surface, the binary's, and the binary's `main` body, in one process.
fn check<A: Allocator + Clone + Send + Sync>(graph: &Graph<A>, alloc: A) -> Checked {
    let command = command(graph, alloc);
    let (lib_owner, bin_owner) = (graph.units[0].owner, graph.units[1].owner);
    let lib = surface(&command, 0, &[]);
    let lib_table = Table::of(0, lib_owner, &lib, &[]).expect("the library's table");
    let bin = surface(&command, 1, &[lib_table]);
    let bin_table = Table::of(1, bin_owner, &bin, &[]).expect("the binary's table");
    let mut tables = [lib_table, bin_table];
    tables.sort_unstable_by_key(|table| table.owner);
    let keyed = command.key(1, Global).expect("the binary's keys");
    assert_eq!(keyed.items.len(), 1, "the binary has one body, `main`");
    let (item, main) = keyed.items[0];
    let universe = Universe { asker: 1, tables: &tables, outside: command.late(1) };
    let body = command.body(1, item, main, universe).unwrap_or_else(|error| failed("`main`'s body", error));
    let checked = Checked { lib: copy(&lib), bin: copy(&bin), main, body };
    command.exit();
    checked
}

fn copy(bytes: &[u8]) -> blaze_format::Aligned {
    blaze_format::Aligned::copy_from(bytes)
}

/// The address `record` (filed under `key`) is found at when its reads are answered on the
/// universe of `checked`: the read-addressed lookup a host makes.
fn found_at<A: Allocator + Clone>(record: &[u8], key: Key<Body>, checked: &Checked, graph: &Graph<A>, alloc: A) -> Key<Body> {
    let (_, traced) = record::check_read::<Body>(record, key).expect("a body record");
    let reads = traced.reads.follow(record).expect("its reads");
    let lib = Table::of(0, graph.units[0].owner, &checked.lib, &[]).expect("the library's table");
    let bin = Table::of(1, graph.units[1].owner, &checked.bin, &[]).expect("the binary's table");
    let mut tables = [lib, bin];
    tables.sort_unstable_by_key(|table| table.owner);
    let universe = Universe { asker: 1, tables: &tables, outside: &[] };
    let mut answered: Vec<Fact, A> = Vec::with_capacity_in(reads.len(), alloc);
    for read in reads {
        let digest = universe.answer(&read.id).expect("every read answers on the new universe");
        answered.push(Fact { id: read.id, digest });
    }
    record::address(key, &answered)
}

/// The oracle: crates resolved from source in the binary's surface snapshot (`surface`) and in
/// its body snapshot (`body`). Its own crate is resolved for its surface, once; nothing for a
/// body (its own crate is given too).
fn lowered(surface: usize, body: usize) -> Result<(), &'static str> {
    match (surface, body) {
        (1, 0) => Ok(()),
        _ => Err(LOWERED),
    }
}

/// The crates resolved from source by the binary's surface task and by its body task.
fn resolutions<A: Allocator + Clone + Send + Sync>(graph: &Graph<A>, alloc: A) -> (usize, usize) {
    let command = command(graph, alloc);
    let (lib_owner, bin_owner) = (graph.units[0].owner, graph.units[1].owner);
    // The library's own surface resolves the library: not counted.
    let lib = surface(&command, 0, &[]);
    let lib_table = Table::of(0, lib_owner, &lib, &[]).expect("the library's table");
    RESOLVED.with(|it| it.set(0));
    let bin = surface(&command, 1, &[lib_table]);
    let at_surface = RESOLVED.with(|it| it.replace(0));
    let bin_table = Table::of(1, bin_owner, &bin, &[]).expect("the binary's table");
    let mut tables = [lib_table, bin_table];
    tables.sort_unstable_by_key(|table| table.owner);
    let keyed = command.key(1, Global).expect("the binary's keys");
    let (item, main) = keyed.items[0];
    let universe = Universe { asker: 1, tables: &tables, outside: command.late(1) };
    // A planted snapshot may fail the body; the count is the oracle's, not the result.
    _ = command.body(1, item, main, universe);
    let at_body = RESOLVED.with(|it| it.replace(0));
    command.exit();
    (at_surface, at_body)
}

/// The invariant: the library's surface record is the same bytes built twice, each time in a
/// fresh command.
#[test]
fn surface_is_the_same_bytes_twice() {
    let dir = fixture("twice", LIB, Global);
    let graph = graph(&dir, Global);
    let first = check(&graph, Global);
    let second = check(&graph, Global);
    assert_eq!(&first.lib[..], &second.lib[..], "the library's surface record built twice");
    assert_eq!(&first.bin[..], &second.bin[..], "the binary's surface record built twice");
    assert_eq!(&first.body[..], &second.body[..], "`main`'s body record built twice");
}

// --- an edge's marks (`blaze_units::schema::EdgeFacts::marks`, the one rule) -------------------

/// One crate of a marks fixture: its name (its unit's and its edge's), its root's text,
/// whether it is a toolchain crate, and its dependencies, each `(unit, noprelude)`, every one
/// before it.
struct Krate<'a> {
    name: &'a str,
    text: &'a str,
    toolchain: bool,
    deps: &'a [(u32, bool)],
}

/// The graph of `crates` in a fresh directory, each a library whose edges are marked by the
/// one rule from the facts cargo gives (`graph::load` gives them from cargo's edge the same).
fn marks_graph(fixture: &str, crates: &[Krate<'_>]) -> Graph<Global> {
    let dir = std::env::temp_dir().join(format!("blaze-command-marks-{}-{fixture}", std::process::id()));
    let dir = dir.to_str().expect("the temporary directory's path is UTF-8").to_owned();
    let text = |it: &str| crate::boxed(it, Global);
    let mut units = Vec::new_in(Global);
    for krate in crates {
        std::fs::create_dir_all(format!("{dir}/{}", krate.name)).expect("a crate's directory");
        let root = format!("{dir}/{}/lib.rs", krate.name);
        std::fs::write(&root, krate.text).expect("a crate's root");
        let mut deps = Vec::new_in(Global);
        for &(on, noprelude) in krate.deps {
            let target = &crates[on as usize];
            let marks = blaze_units::schema::EdgeFacts {
                dependent_is_std: krate.toolchain,
                dependent_is_proc_macro: false,
                target_is_std: target.toolchain,
                name: target.name,
                noprelude,
            }
            .marks();
            deps.push(Dep { name: text(target.name), unit: on, marks });
        }
        units.push(Unit {
            name: text(krate.name),
            package: text(&format!("{} 0.1.0", krate.name)),
            edition: span::Edition::Edition2021,
            root: text(&root),
            kind: UnitKind::Lib,
            cfg: Vec::new_in(Global),
            deps,
            env: Vec::new_in(Global),
            member: !krate.toolchain,
            owner: Owner([0; 8]),
            namespace: Namespace::LOCAL,
            key: ContentId([0; 32]),
            origin: if krate.toolchain { Origin::Toolchain } else { Origin::Private },
            bodies: Bodies::NotAsked,
            blaze_edition: None,
            settings: ContentId::of("blaze-command test unit settings", b"1"),
            blocked: None,
        });
    }
    let triple = host().expect("the test runs on a host blaze has a target for");
    let mut graph = Graph {
        units,
        tool: ContentId::of("blaze-command test tool", b"1"),
        settings: ContentId::of("blaze-command test settings", b"1"),
        target: ContentId::of("blaze-command test target", triple.as_bytes()),
        triple: text(triple),
        checkout: text(&dir),
        identity: ContentId([0; 32]),
        load: crate::graph::LoadTimes::default(),
        programs: Vec::new_in(Global),
        library: crate::graph::LibraryNames { dir: text("/library"), printed: text("/rustc/0/library") },
    };
    key_units(&mut graph, Global);
    graph
}

/// Every unit of `graph` (in its order) keyed and surfaced through the real front; per unit,
/// how many imports and `extern crate`s of its own did not resolve in its surface snapshot
/// (rustc's E0432 and E0463), or why it was refused.
fn unresolved(graph: &Graph<Global>) -> Vec<Result<usize, String>, Global> {
    use hir_def::nameres::diagnostics::DefDiagnosticKind;
    let mut command = Command::new(graph, Global).expect("the fixture's command");
    let mut made: Vec<Option<blaze_format::Aligned>, Global> = Vec::new_in(Global);
    made.resize_with(graph.units.len(), || None);
    let mut out = Vec::new_in(Global);
    for unit in 0..graph.units.len() as u32 {
        let record = match key_and_surface(&mut command, graph, unit, &made, false) {
            Ok(record) => record,
            Err(why) => {
                out.push(Err(why));
                continue;
            }
        };
        let counted = (|| -> Result<usize, String> {
            let tables = tables_of(graph, unit, &made)?;
            let universe = Universe { asker: unit, tables: &tables, outside: command.late(unit) };
            let opened = snapshot::open(&command, unit, &universe, snapshot::Task::Surface).map_err(|error| format!("the snapshot: {error:?}"))?;
            let map = hir_def::nameres::crate_def_map(&opened.db, opened.own);
            let count = map
                .diagnostics(&opened.db)
                .iter()
                .filter(|it| matches!(it.kind, DefDiagnosticKind::UnresolvedImport { .. } | DefDiagnosticKind::UnresolvedExternCrate { .. }))
                .count();
            Ok(count)
        })();
        made[unit as usize] = Some(record);
        out.push(counted);
    }
    command.exit();
    out
}

/// The crates of the extern-prelude test: a toolchain `alloc` given to two user crates as
/// build-std gives it (`noprelude` when `noprelude`), one naming it with no `extern crate`.
fn alloc_crates(noprelude: bool) -> Vec<Result<usize, String>, Global> {
    let graph = marks_graph(
        if noprelude { "alloc" } else { "alloc-control" },
        &[
            Krate { name: "alloc", text: "#![no_core]\npub mod vec {\n    pub struct Vec;\n}\n", toolchain: true, deps: &[] },
            Krate { name: "without", text: "#![no_core]\nuse alloc::vec::Vec;\n", toolchain: false, deps: &[(0, noprelude)] },
            Krate { name: "with", text: "#![no_core]\nextern crate alloc;\nuse alloc::vec::Vec;\n", toolchain: false, deps: &[(0, noprelude)] },
        ],
    );
    unresolved(&graph)
}

/// The invariant (rule 4): a std root build-std gives a user crate is in no extern prelude, as
/// rustc has it (`--extern noprelude:alloc`): `use alloc::vec::Vec;` with no `extern crate
/// alloc;` does not resolve, and with one it does.
///
/// The control: the same edge with no `noprelude` (every edge's prelude mark `true`, as the
/// command's snapshot gave it before) resolves the import with no `extern crate`, which the
/// pinned nightly refuses (E0432).
#[test]
fn a_std_root_is_named_only_by_its_extern_crate() {
    let got = alloc_crates(true);
    assert!(matches!(got[0], Ok(0)), "the toolchain `alloc`: {:?}", got[0]);
    assert!(matches!(got[1], Ok(n) if n > 0), "`use alloc::..` with no `extern crate alloc;` resolved: {:?}", got[1]);
    assert!(matches!(got[2], Ok(0)), "`use alloc::..` after `extern crate alloc;` did not resolve: {:?}", got[2]);

    let control = alloc_crates(false);
    assert!(matches!(control[1], Ok(0)), "the control: an edge in the extern prelude resolves the import: {:?}", control[1]);
}

/// The invariant: a user crate's own `mod test` is what `use test::f;` names, beside a
/// toolchain `test` given as build-std gives it (`noprelude`): the import resolves, and the
/// edge is in no extern prelude, so no second `test` is there for it to be ambiguous with.
/// (hir-def reports no ambiguity of its own; the edge's mark is what keeps rustc's E0659 out.)
#[test]
fn a_crate_s_own_module_is_not_shadowed_by_a_std_root() {
    let graph = marks_graph(
        "test",
        &[
            Krate { name: "test", text: "#![no_core]\npub fn g() {}\n", toolchain: true, deps: &[] },
            Krate { name: "user", text: "#![no_core]\nmod test {\n    pub fn f() {}\n}\nuse test::f;\n", toolchain: false, deps: &[(0, true)] },
        ],
    );
    assert!(!graph.units[1].deps[0].marks.prelude, "build-std's `test` edge is in the user crate's extern prelude");
    let got = unresolved(&graph);
    assert!(matches!(got[1], Ok(0)), "`use test::f;` of the crate's own `mod test`: {:?}", got[1]);
}

/// The invariant: a toolchain crate's declared dependency on `core` is no sysroot edge, so a
/// `#![no_core]` toolchain crate keeps it: the facade `rustc-std-workspace-core`'s
/// `pub use core::*;` exports core's names, which a crate depending on the facade imports.
///
/// The control (the line the coordinator changed in `snapshot.rs`, now the one rule's): a
/// `#![no_core]` user crate's `core` is a sysroot edge, which hir-def drops from its extern
/// prelude, so its `use core::make;` does not resolve.
#[test]
fn the_facade_re_exports_core_and_a_no_core_user_crate_has_none() {
    let graph = marks_graph(
        "facade",
        &[
            Krate { name: "core", text: "#![no_core]\npub fn make() {}\n", toolchain: true, deps: &[] },
            Krate { name: "facade", text: "#![no_core]\npub use core::*;\n", toolchain: true, deps: &[(0, false)] },
            Krate { name: "through", text: "#![no_core]\nuse facade::make;\n", toolchain: false, deps: &[(1, false)] },
            Krate { name: "user", text: "#![no_core]\nuse core::make;\n", toolchain: false, deps: &[(0, false)] },
        ],
    );
    let got = unresolved(&graph);
    assert!(matches!(got[1], Ok(0)), "the facade's `pub use core::*;`: {:?}", got[1]);
    assert!(matches!(got[2], Ok(0)), "`use facade::make;` through the facade: {:?}", got[2]);
    assert!(matches!(got[3], Ok(n) if n > 0), "the control: a `#![no_core]` user crate resolved `use core::make;`: {:?}", got[3]);
}

/// The invariant (rule 30): the marks the command's snapshot gives every edge of hello world's
/// graph with std are the ones the units graph's edge gives by `UnitDep::marks`, which
/// blaze-lower's `crates::dependency` calls (blaze-lower is no dependency of blaze-command, so
/// the daemon path's function is reached through the one it calls). Edge by edge, by the
/// dependent's crate in the snapshot's crate graph and the edge's name.
#[test]
fn the_snapshot_marks_every_edge_as_the_units_graph_does() {
    with_hello(|inputs| {
        let (units, _) = crate::graph::resolve(inputs).unwrap_or_else(|error| panic!("resolving hello world: {error}"));
        let graph = crate::graph::from_units(&units, inputs, Global).unwrap_or_else(|error| panic!("hello world's graph: {error}"));
        let (kept, order) = crate::graph::crates_of(&units, Global).unwrap_or_else(|error| panic!("hello world's crates: {error}"));
        let triple = host().expect("the test runs on a host blaze has a target for");
        let workspace = triomphe::Arc::new(workspace_of(triple).expect("the host's target"));
        let mut cfgs = Vec::new_in(Global);
        cfgs.extend(graph.units.iter().map(|unit| cfg_of(&unit.cfg)));
        let mut all = Vec::new_in(Global);
        all.extend(0..graph.units.len() as u32);
        let (builder, built) = snapshot::crate_graph(&graph, &all, base_db::FileId::from_raw, &workspace, &cfgs, Global)
            .unwrap_or_else(|(unit, rule)| panic!("unit {unit}'s crate: {rule}"));
        let raw = &units.graph.units;
        let (mut edges, mut wrong) = (0usize, Vec::new_in(Global));
        for (at, &old) in order.iter().enumerate() {
            let crate_ = &builder[built[at].1];
            for dep in raw[old].dependencies.iter().filter(|dep| kept[dep.index]) {
                let want = dep.marks(&raw[old], &raw[dep.index]);
                let given = crate_.basic.dependencies.iter().find(|it| it.name.symbol().as_str() == dep.extern_crate_name);
                edges += 1;
                match given {
                    Some(it) if it.is_prelude() == want.prelude && it.is_sysroot() == want.sysroot => {}
                    other => wrong.push(format!(
                        "`{}` -> `{}`: the units graph gives {want:?}, the snapshot {:?}",
                        graph.units[at].name,
                        dep.extern_crate_name,
                        other.map(|it| (it.is_prelude(), it.is_sysroot()))
                    )),
                }
            }
        }
        assert!(edges > 0, "hello world's graph has no edge");
        assert!(wrong.is_empty(), "{} of {edges} edges marked otherwise:\n{}", wrong.len(), wrong.join("\n"));
    });
}

/// The machine the goal test's commands are sized for.
const MACHINE: blaze_keyed::machine::Machine = blaze_keyed::machine::Machine {
    cores: 4,
    fast_cores: 4,
    page_bytes: 16384,
    memory_bytes: 16 << 30,
    qos: false,
    huge_pages: false,
};

/// One `check` of `graph` through the real front, in `home`: its summary.
fn checked_in(graph: &Graph<Global>, home: &std::path::Path) -> Result<crate::run::Summary<Global>, String> {
    let mut command = Command::new(graph, Global).map_err(|error| format!("the command: {error}"))?;
    let done = crate::run::check(graph, &mut command, home, MACHINE, Global).map_err(|error| format!("the check: {error}"));
    command.exit();
    done
}

/// The invariant (rules 21 and 77): a unit's manifest and surface are functions of its text,
/// its cfg, its own profile settings and its dependencies' surfaces, never of the command, so
/// in one home a `build` after a `check` finds every surface the check filed and makes none
/// (job merge32: the first build after six checks remade all 7 std surfaces, because
/// `Graph::settings`, which holds `check` or `build`, was in every unit's `Settings`). The
/// build's graph is the check's with the graph's settings and identity a link's: its
/// certificate misses, so each unit is looked up by its keys.
///
/// The control: a real input moved, a cfg atom of the library, remakes the library's surface.
#[test]
fn a_build_finds_every_surface_its_check_filed() {
    let dir = fixture("goal", LIB, Global);
    let home = std::path::PathBuf::from(format!("{dir}/home"));
    _ = std::fs::remove_dir_all(&home);
    let as_check = graph(&dir, Global);
    let cold = checked_in(&as_check, &home).unwrap_or_else(|why| panic!("the cold check: {why}"));
    assert!(cold.units.iter().all(|unit| unit.surfaces > 0), "the cold check made every surface: {:?}", cold.units);

    // The graph `build` loads differs in what only the certificate and the link keys take.
    let mut as_build = graph(&dir, Global);
    as_build.settings = ContentId::of("blaze-command test settings", b"build");
    as_build.identity = ContentId::of("blaze-command test identity", b"build");
    let build = checked_in(&as_build, &home).unwrap_or_else(|why| panic!("the build's units: {why}"));
    assert!(build.gathered, "the build's certificate missed and its units were looked up by their keys");
    let surfaces: Vec<u32, Global> = build.units.iter().map(|unit| unit.surfaces).collect();
    assert!(surfaces.iter().all(|&it| it == 0), "the build after a check ran surface tasks {surfaces:?}, not none: {:?}", build.units);

    let mut moved = graph(&dir, Global);
    moved.units[0].cfg.push(crate::graph::CfgAtom::Flag(crate::boxed("blaze_test_moved", Global)));
    key_units(&mut moved, Global);
    moved.identity = ContentId::of("blaze-command test identity", b"cfg moved");
    let control = checked_in(&moved, &home).unwrap_or_else(|why| panic!("the check with a cfg moved: {why}"));
    _ = std::fs::remove_dir_all(&dir[..]);
    assert!(control.units[0].surfaces > 0, "the control: a moved cfg atom did not remake the library's surface: {:?}", control.units);
}

/// The invariant: `main`'s body record is found again after an edit to another body of the
/// library, and is not after an edit to the signature it calls.
#[test]
fn body_found_again_after_another_body_redone_after_its_signature() {
    let dir = fixture("edits", LIB, Global);
    let before = check(&graph(&dir, Global), Global);
    let (address, _) = record::check_read::<Body>(&before.body, before.main).expect("`main`'s record");

    std::fs::write(format!("{dir}/lib/lib.rs"), LIB_OTHER_BODY).expect("the body edit");
    let graph_body = graph(&dir, Global);
    let after_body = check(&graph_body, Global);
    assert_eq!(after_body.main, before.main, "the binary's text did not move, so `main`'s key did not");
    assert_eq!(
        found_at(&before.body, before.main, &after_body, &graph_body, Global),
        address,
        "`main`'s record is found again after an edit to another body"
    );
    assert_eq!(&after_body.body[..], &before.body[..], "and made again, it is the same bytes");

    std::fs::write(format!("{dir}/lib/lib.rs"), LIB_SIGNATURE).expect("the signature edit");
    let graph_signature = graph(&dir, Global);
    let after_signature = check(&graph_signature, Global);
    assert_ne!(
        found_at(&before.body, before.main, &after_signature, &graph_signature, Global),
        address,
        "`main`'s record is not found after an edit to the signature it calls"
    );
}

// --- every unit of std through the real front (AGENTS.md rule 82) ------------------------------

/// The library `graph::load` is given, found as blaze finds it (blaze-cargo's
/// `toolchain::library`): the directory `BLAZE_LIBRARY` names, else the `library/` of this tree.
/// One that is not there fails the test naming the path: it is never skipped.
fn std_library() -> (std::path::PathBuf, String, String) {
    let dir = match std::env::var_os("BLAZE_LIBRARY").filter(|it| !it.is_empty()) {
        Some(named) => std::path::PathBuf::from(named),
        None => std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../library"),
    };
    let dir = dir.canonicalize().unwrap_or_else(|error| panic!("the standard library is not at {}: {error}", dir.display()));
    let digest = std::fs::read_to_string(dir.join("DIGEST")).unwrap_or_else(|error| panic!("{}/DIGEST: {error}", dir.display()));
    let upstream = std::fs::read_to_string(dir.join("UPSTREAM")).unwrap_or_else(|error| panic!("{}/UPSTREAM: {error}", dir.display()));
    let commit = blaze_units::stdlib::library_commit(&upstream).unwrap_or_else(|why| panic!("{}/UPSTREAM: {why}", dir.display()));
    (dir, digest.trim().to_owned(), commit)
}

/// A hello-world checkout in a fresh directory, and its graph as `check --no-daemon` loads it
/// (`graph::load`: the workspace's units and std's, std's scripts applied by their ports).
fn hello_graph() -> Graph<Global> {
    with_hello(|inputs| crate::graph::load(inputs, Global).unwrap_or_else(|error| panic!("loading hello world's graph: {error}")))
}

/// `body` given the load inputs of hello world (a fresh checkout over the real `library/`).
fn with_hello<T>(body: impl FnOnce(&crate::graph::Inputs<'_>) -> T) -> T {
    let dir = std::env::temp_dir().join(format!("blaze-command-std-front-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("src")).expect("the checkout's directory");
    std::fs::create_dir_all(dir.join("cargo-home")).expect("an empty CARGO_HOME, so no config of this machine is read");
    std::fs::write(dir.join("Cargo.toml"), "[package]\nname = \"hello\"\nversion = \"0.1.0\"\nedition = \"2024\"\n")
        .expect("the manifest");
    std::fs::write(dir.join("src/main.rs"), "fn main() {\n    println!(\"hello\");\n}\n").expect("the program");
    let dir = dir.to_str().expect("the temporary directory's path is UTF-8").to_owned();
    let (library, digest, commit) = std_library();
    let home = std::env::var("HOME").expect("HOME is set, and cargo reads it");
    let mut env = vec![("CARGO_HOME".to_owned(), format!("{dir}/cargo-home")), ("HOME".to_owned(), home)];
    env.sort();
    let tools = std::path::PathBuf::from(format!("{dir}/tools"));
    let host = host().expect("the test runs on a host blaze has a target for");
    let inputs = crate::graph::Inputs {
        invocation: blaze_units::query::Invocation {
            command: blaze_units::query::Command::Check,
            args: Vec::new(),
            root: dir.clone(),
            cwd: dir.clone(),
            env,
            files: Vec::new(),
        },
        host,
        library: crate::graph::Library { dir: &library, digest: &digest, commit: &commit },
        std: "/blaze-test/std",
        builds: "/blaze-test/builds",
        sources: "/blaze-test/sources",
        blaze: "test",
        front_end: ContentId::of("blaze-command std front test front end", b"1"),
        tools: &tools,
        org: None,
        trusted_writer: false,
    };
    body(&inputs)
}

/// Unit `unit`'s closure (itself and every unit it depends on, transitively), ascending.
fn closure_of(graph: &Graph<Global>, unit: u32) -> Vec<u32, Global> {
    let mut out = Vec::new_in(Global);
    let mut stack = Vec::new_in(Global);
    stack.push(unit);
    while let Some(at) = stack.pop() {
        if out.contains(&at) {
            continue;
        }
        out.push(at);
        stack.extend(graph.units[at as usize].deps.iter().map(|dep| dep.unit));
    }
    out.sort_unstable();
    out
}

/// The units in an order where every unit follows its dependencies (Kahn's, ties by index).
fn dependency_order(graph: &Graph<Global>) -> Vec<u32, Global> {
    let count = graph.units.len();
    let mut done = Vec::new_in(Global);
    done.resize(count, false);
    let mut order = Vec::with_capacity_in(count, Global);
    while order.len() < count {
        let next = (0..count).find(|&at| {
            !done[at] && graph.units[at].deps.iter().all(|dep| done.get(dep.unit as usize).copied().unwrap_or(false))
        });
        let at = next.unwrap_or_else(|| panic!("the graph's units depend on each other in a cycle"));
        done[at] = true;
        order.push(at as u32);
    }
    order
}

/// One unit's key and surface through `command`'s own tasks, its dependencies' surfaces in
/// `made`; its surface record, or every line its refusal said.
fn key_and_surface(
    command: &mut Command<'_, Global>,
    graph: &Graph<Global>,
    unit: u32,
    made: &[Option<blaze_format::Aligned>],
    withhold: bool,
) -> Result<blaze_format::Aligned, String> {
    let told = |command: &Command<'_, Global>, what: String| -> String {
        let mut out = what;
        for (_, line) in command.told(Global).iter().filter(|(of, _)| *of == unit) {
            out.push_str(&format!("\n      {line}"));
        }
        if let Some(unreadable) = crate::front::LAST_UNREADABLE.with(|it| it.borrow_mut().take()) {
            out.push_str(&format!("\n      {unreadable}"));
        }
        out
    };
    command.gather(unit, &mut Disk).map_err(|error| told(&*command, format!("gather: {error}")))?;
    let command = &*command;
    let keyed = command.key(unit, Global).map_err(|error| told(command, format!("key: {error:?}")))?;
    // The planted bug (`withhold`): the universe without the dependencies' tables.
    let tables = if withhold { Vec::new_in(Global) } else { tables_of(graph, unit, made)? };
    let universe = Universe { asker: unit, tables: &tables, outside: command.late(unit) };
    command.surface(unit, keyed.surface, universe).map_err(|error| told(command, format!("surface: {error:?}")))
}

/// The tables of unit `unit`'s dependencies (its closure but itself), from their surfaces in
/// `made`, sorted by owner: a surface task's universe.
fn tables_of<'m>(graph: &Graph<Global>, unit: u32, made: &'m [Option<blaze_format::Aligned>]) -> Result<Vec<Table<'m>, Global>, String> {
    let mut tables = Vec::new_in(Global);
    for member in closure_of(graph, unit) {
        if member == unit {
            continue;
        }
        let record = made[member as usize].as_ref().ok_or_else(|| format!("dependency unit {member} has no surface"))?;
        let table = Table::of(member, graph.units[member as usize].owner, record, &[])
            .map_err(|error| format!("dependency unit {member}'s table: {error:?}"))?;
        tables.push(table);
    }
    tables.sort_unstable_by_key(|table| table.owner);
    Ok(tables)
}

/// The invariant (AGENTS.md rule 82): every unit of std keys and surfaces through the real
/// front, `Command`'s own gather, key and surface tasks over the real `library/` (never a
/// scripted front), in dependency order, as `check --no-daemon` runs them for hello world. A
/// unit whose dependency failed is not run and is listed as skipped; every other unit runs
/// past any failure. The one failure lists every refusal with every line its task told.
///
/// Cost: std's whole closure keyed and surfaced in one process. In a release blaze core's
/// surface task alone took 10.9 to 13.8 s (job nd3); a debug build is several times that, so
/// this runs for minutes. It is a normal test, not ignored: the main agent runs it alone, by
/// name.
#[test]
fn every_unit_of_std_keys_and_surfaces_through_the_real_front() {
    let graph = hello_graph();
    let mut command = Command::new(&graph, Global).unwrap_or_else(|error| panic!("the command over hello world: {error}"));
    let mut made: Vec<Option<blaze_format::Aligned>, Global> = Vec::new_in(Global);
    made.resize_with(graph.units.len(), || None);
    let mut failed: Vec<bool, Global> = Vec::new_in(Global);
    failed.resize(graph.units.len(), false);
    let mut report = String::new();
    let (mut ran, mut surfaced) = (0usize, 0usize);
    let of_std = |at: u32| graph.units[at as usize].origin == Origin::Toolchain;
    let total = (0..graph.units.len() as u32).filter(|&at| of_std(at)).count();
    for unit in dependency_order(&graph) {
        let named = &graph.units[unit as usize];
        // std's units alone, core's first: each surface record is kept in `made` and handed on
        // as its table to every unit whose closure holds it.
        if !of_std(unit) {
            continue;
        }
        let shown = format!("`{}` ({})", named.name, named.package);
        if let Some(dep) = named.deps.iter().find(|dep| failed[dep.unit as usize]) {
            failed[unit as usize] = true;
            let dep = &graph.units[dep.unit as usize];
            report.push_str(&format!("  {shown}: skipped, its dependency `{}` failed\n", dep.name));
            continue;
        }
        ran += 1;
        // A panic fails its unit alone (rule 20), as the command's `named` path fails one task:
        // caught here, said under the unit with its text, and the units that do not depend on
        // it still run, so one run lists every unit's outcome.
        let outcome =
            std::panic::catch_unwind(core::panic::AssertUnwindSafe(|| key_and_surface(&mut command, &graph, unit, &made, false)));
        match outcome {
            Ok(Ok(record)) => {
                made[unit as usize] = Some(record);
                surfaced += 1;
            }
            Ok(Err(why)) => {
                failed[unit as usize] = true;
                report.push_str(&format!("  {shown}: {why}\n"));
            }
            Err(payload) => {
                failed[unit as usize] = true;
                let text = payload
                    .downcast_ref::<&str>()
                    .map(|it| (*it).to_owned())
                    .or_else(|| payload.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "a panic with no text".to_owned());
                report.push_str(&format!("  {shown}: panicked:\n      {}\n", text.replace('\n', "\n      ")));
            }
        }
    }
    // The control: a unit of std with a dependency, its universe without its dependencies'
    // tables, is refused by name (`snapshot::NO_TABLE`), and the report would hold it.
    let planted = dependency_order(&graph)
        .into_iter()
        .find(|&at| of_std(at) && !graph.units[at as usize].deps.is_empty() && made[at as usize].is_some());
    let control = planted.map(|at| key_and_surface(&mut command, &graph, at, &made, true));
    command.exit();
    assert!(ran > 0, "hello world's graph has no unit of std");
    assert!(
        report.is_empty(),
        "{surfaced} of std's {total} units keyed and surfaced ({ran} ran, the rest skipped); refused:\n{report}"
    );
    match control {
        Some(Err(why)) => assert!(why.contains(snapshot::NO_TABLE), "the control was refused by another rule: {why}"),
        Some(Ok(_)) => panic!("the control: a surface task without its dependencies' tables succeeded"),
        None => panic!("the control: no unit of std with a dependency surfaced"),
    }
}

/// The invariant (`graph::asked`): `check` asks the bodies of the checkout's own units alone,
/// never a dependency's nor std's, though std's packages are members of the library's
/// workspace as blaze-units loads it (job merge25 asked all of core's). The control: hello's
/// own unit, a member, is asked; without it the test would pass on a graph that asks nothing.
#[test]
fn a_dependency_s_bodies_are_not_asked() {
    let graph = hello_graph();
    let mut asked = Vec::new_in(Global);
    let mut std = 0usize;
    for unit in graph.units.iter() {
        let toolchain = unit.origin == Origin::Toolchain;
        std += usize::from(toolchain);
        if unit.bodies == Bodies::Asked || unit.member {
            asked.push(format!("`{}` ({}): member {}, bodies {:?}, origin {:?}", unit.name, unit.package, unit.member, unit.bodies, unit.origin));
        }
        assert!(!toolchain || (unit.bodies == Bodies::NotAsked && !unit.member), "std's `{}` ({}) is asked", unit.name, unit.package);
    }
    assert!(std > 0, "hello world's graph has no unit of std");
    assert!(
        asked.len() == 1 && asked[0].starts_with("`hello` (hello 0.1.0): member true, bodies Asked"),
        "the units whose bodies `check` asks are hello's alone; they are:\n{}",
        asked.join("\n")
    );
}

/// The invariant: no dependency is lowered in the process that checks the binary; the planted
/// snapshot that does not give its dependency is caught by name.
#[test]
fn dependency_never_lowered() {
    let dir = fixture("lowered", LIB, Global);
    let graph = graph(&dir, Global);
    let (surface, body) = resolutions(&graph, Global);
    assert_eq!(lowered(surface, body), Ok(()), "resolved: {surface} at the surface, {body} at the body");

    LOWER_DEPS.with(|it| it.set(true));
    let (surface, body) = resolutions(&graph, Global);
    LOWER_DEPS.with(|it| it.set(false));
    assert_eq!(lowered(surface, body), Err(LOWERED), "the planted bug: {surface} at the surface, {body} at the body");
}

/// A library whose macro call spells `mod deferred;` and expands to nothing: `deferred.rs` is
/// a candidate file the program does not use.
const LIB_UNUSED: &str = "#![no_core]\nmacro_rules! nothing { ($($t:tt)*) => {} }\nnothing! { mod deferred; }\npub fn make(x: u32) -> u32 { x }\n";
/// The same, its macro keeping its tokens: `deferred.rs` is a module of the def map.
const LIB_USED: &str = "#![no_core]\nmacro_rules! keep { ($($t:tt)*) => { $($t)* } }\nkeep! { mod deferred; }\npub fn make(x: u32) -> u32 { x }\n";
/// A late read the gather cannot answer: its argument is not a literal.
const DEFERRED_FILE: &str = "include!(concat!(env!(\"OUT_DIR\"), \"/x.rs\"));\n";

/// The library of a fixture whose root is `lib`, with `deferred.rs` beside it, gathered, keyed
/// and surfaced under `plant`: `Ok`, or the rule the gather or a task refused it by.
fn deferred_surface(name: &str, lib: &str, plant: crate::front::DeferPlant) -> Result<(), &'static str> {
    use crate::{
        CommandError,
        front::{DEFER_PLANT, DeferPlant},
    };
    let dir = fixture(name, lib, Global);
    std::fs::write(format!("{dir}/lib/deferred.rs"), DEFERRED_FILE).expect("the deferred file");
    let graph = graph(&dir, Global);
    let rule = |error: blaze_keyed::driver::TaskFailed| match error {
        blaze_keyed::driver::TaskFailed::Refused { what } => what,
        other => panic!("refused by {other:?}, not by a rule"),
    };
    DEFER_PLANT.with(|it| it.set(plant));
    let out = (|| {
        let mut command = Command::new(&graph, Global).expect("the fixture's command");
        match command.gather(0, &mut Disk) {
            Ok(()) => {}
            Err(CommandError::Graph { rule, .. } | CommandError::Unit { rule, .. }) => return Err(rule),
            Err(other) => panic!("the gather stopped by {other}"),
        }
        let keyed = command.key(0, Global).map_err(rule)?;
        let universe = Universe { asker: 0, tables: &[], outside: command.late(0) };
        let out = command.surface(0, keyed.surface, universe).map(|_| ()).map_err(rule);
        command.exit();
        out
    })();
    DEFER_PLANT.with(|it| it.set(DeferPlant::None));
    out
}

/// The invariant (`front::Deferred`): a late read the gather cannot answer, in a file only a
/// macro call's tokens reach, fails nothing while the def map does not use the file, and
/// refuses the unit by `front::DEFERRED` once it does. The controls, each failing by name: the
/// gather refusing in a candidate file as in any other refuses the unused case by
/// `front::NOT_LITERAL`; the surface never refusing what was deferred lets the used case past
/// `front::DEFERRED`.
#[test]
fn a_candidate_file_fails_the_unit_only_when_used() {
    use crate::front::{DEFERRED, DeferPlant, NOT_LITERAL};
    assert_eq!(deferred_surface("deferred-unused", LIB_UNUSED, DeferPlant::None), Ok(()), "the unused candidate file");
    assert_eq!(deferred_surface("deferred-used", LIB_USED, DeferPlant::None), Err(DEFERRED), "the used candidate file");
    assert_eq!(
        deferred_surface("deferred-gather", LIB_UNUSED, DeferPlant::RefuseAtGather),
        Err(NOT_LITERAL),
        "RefuseAtGather: the control"
    );
    let planted = deferred_surface("deferred-never", LIB_USED, DeferPlant::NeverRefuse);
    assert_ne!(planted, Err(DEFERRED), "NeverRefuse: the control was still refused by the deferral");
}
