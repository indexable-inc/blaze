//! The graph's interning, sharing and order rules, and the allocation budget of
//! [`super::build`]: what it allocates beyond its output and the calls it makes into the
//! manifest and resolve crates.

use std::cell::{Cell, RefCell};
use std::cmp::Ordering;

use blaze_manifest::profiles::{DebugInfo, Profile};
use blaze_resolve::InternedString;
use cargo_util_schemas::manifest::TomlDebugInfo;

use super::{Chains, FeatureSets, NONE, Names, Profiles, dense};
use crate::{Request, order, schema};

/// What one [`super::build`] allocated on its thread: in all, inside [`external`] calls, and
/// inside [`output`] (the JSON it returns).
#[derive(Clone, Copy, Debug, Default)]
#[cfg_attr(not(feature = "query"), allow(dead_code))]
pub(super) struct Budget {
    total: u64,
    external: u64,
    output: u64,
}

impl Budget {
    /// What the graph allocated itself: its columns, maps and buffers.
    #[cfg_attr(not(feature = "query"), allow(dead_code))]
    fn own(self) -> u64 {
        self.total - self.external - self.output
    }
}

thread_local! {
    static ARMED: Cell<bool> = const { Cell::new(false) };
    static EXTERNAL: Cell<u64> = const { Cell::new(0) };
    static OUTPUT: Cell<u64> = const { Cell::new(0) };
    static BUILT: RefCell<Vec<(Budget, schema::UnitGraph)>> = const { RefCell::new(Vec::new()) };
}

pub(super) fn external<R>(f: impl FnOnce() -> R) -> R {
    let (result, used) = blaze_alloc_count::measure(f);
    EXTERNAL.set(EXTERNAL.get() + used.allocations);
    result
}

pub(super) fn output<R>(f: impl FnOnce() -> R) -> R {
    let (result, used) = blaze_alloc_count::measure(f);
    OUTPUT.set(OUTPUT.get() + used.allocations);
    result
}

/// `build(request)`, measured and kept with what it made while [`armed`] runs.
pub(super) fn measured(
    request: &Request<'_>,
    build: fn(&Request<'_>) -> anyhow::Result<schema::UnitGraph>,
) -> anyhow::Result<schema::UnitGraph> {
    if !ARMED.get() {
        return build(request);
    }
    EXTERNAL.set(0);
    OUTPUT.set(0);
    let (graph, used) = blaze_alloc_count::measure(|| build(request));
    let budget = Budget { total: used.allocations, external: EXTERNAL.get(), output: OUTPUT.get() };
    if let Ok(graph) = &graph {
        BUILT.with(|it| it.borrow_mut().push((budget, graph.clone())));
    }
    graph
}

/// Runs `f`, and the graphs it built on this thread with their budgets.
#[cfg_attr(not(feature = "query"), allow(dead_code))]
fn armed(f: impl FnOnce()) -> Vec<(Budget, schema::UnitGraph)> {
    BUILT.with(|it| it.borrow_mut().clear());
    ARMED.set(true);
    f();
    ARMED.set(false);
    BUILT.with(|it| std::mem::take(&mut *it.borrow_mut()))
}

fn dev() -> Profile {
    Profile {
        name: "dev".to_owned(),
        debuginfo: DebugInfo::Resolved(TomlDebugInfo::Full),
        debug_assertions: true,
        overflow_checks: true,
        incremental: true,
        ..Profile::default()
    }
}

#[test]
fn profiles_cargo_holds_equal_are_one_class_and_each_is_kept_as_made() {
    let mut profiles = Profiles::default();
    let target = profiles.intern(dev());
    let host = profiles.intern(Profile { debuginfo: DebugInfo::Deferred(TomlDebugInfo::Full), ..dev() });
    let renamed = profiles.intern(Profile { name: "test".to_owned(), ..dev() });
    let optimized = profiles.intern(Profile { opt_level: "3".to_owned(), ..dev() });

    assert_eq!(profiles.intern(dev()), target, "the same profile is interned once");
    assert_ne!(host, target, "a deferred debuginfo is kept apart from a resolved one");
    assert!(profiles.get(host).debuginfo.is_deferred());
    assert!(!profiles.get(target).debuginfo.is_deferred());
    assert_ne!(renamed, target);
    let class = |id: u32| profiles.class_of[id as usize];
    assert_eq!(class(host), class(target), "cargo compares profiles without deferral");
    assert_eq!(class(renamed), class(target), "or the name");
    assert_ne!(class(optimized), class(target));
    assert_eq!(profiles.classes[class(target) as usize], target, "a class's first profile");
}

#[test]
fn feature_sets_are_interned_by_their_names_in_order() {
    let (a, b) = (InternedString::new("a"), InternedString::new("b"));
    let mut sets = FeatureSets::default();
    let ab = sets.intern(&[a, b]);
    let empty = sets.intern(&[]);
    assert_eq!(sets.intern(&[a, b]), ab);
    assert_ne!(sets.intern(&[b, a]), ab, "cargo compares the lists, not sets");
    assert_eq!(sets.intern(&[]), empty);
    assert_eq!(sets.get(ab), [a, b]);
    assert!(sets.get(empty).is_empty());
}

#[test]
fn names_are_interned_once_and_order_as_strings() {
    let mut names = Names::default();
    let b = names.intern(InternedString::new("b"));
    let a = names.intern(InternedString::new("a"));
    assert_eq!(names.intern(InternedString::new(&String::from("b"))), b);
    assert_eq!(names.cmp(a, b), Ordering::Less);
    assert_eq!(names.cmp(b, b), Ordering::Equal);
    assert_eq!(names.cmp_opt(NONE, a), Ordering::Less, "an unrenamed dependency first");
    assert_eq!(names.cmp_opt(b, NONE), Ordering::Greater);
    assert_eq!(names.cmp_opt(NONE, NONE), Ordering::Equal);
}

#[test]
fn chains_tell_apart_entries_that_share_a_hash() {
    let values = [10, 20, 10, 30];
    let mut chains = Chains::default();
    let mut ids: Vec<u32> = Vec::new();
    let mut kept: Vec<u32> = Vec::new();
    for value in values {
        let next = kept.len() as u32;
        let found = chains.find_or_insert(7, next, |id| kept[id as usize] == value);
        ids.push(found.unwrap_or_else(|| {
            kept.push(value);
            next
        }));
    }
    assert_eq!(ids, [0, 1, 0, 2]);
    assert_eq!(kept, [10, 20, 30]);
}

#[test]
fn names_compare_by_prefix_as_strings_do() {
    let names = ["", "\0", "a", "a\0", "a\0b", "abcdefgh", "abcdefgg", "abcdefghi", "abcdefgh\0", "b", "é"];
    for a in names {
        for b in names {
            assert_eq!(order::name(a, b), a.cmp(b), "{a:?} and {b:?}");
        }
    }
}

#[test]
fn dense_ranks_give_equal_items_one_rank() {
    let values = [5, 3, 5, 1];
    let mut ids: Vec<u32> = (0..values.len() as u32).collect();
    ids.sort_by_key(|&id| values[id as usize]);
    let mut ranks = Vec::new();
    dense(&ids, &mut ranks, |a, b| values[a as usize].cmp(&values[b as usize]));
    assert_eq!(ranks, [2, 1, 2, 0]);
}

/// The graph of real workspaces, built as the daemon builds it: through the load's queries.
#[cfg(feature = "query")]
mod load {
    use super::{Budget, armed};
    use crate::load::tests::{Ask, Fixture, HOST, WS, ask};
    use crate::query::Planned;
    use crate::schema::{DebugInfo, Mode, Strip, Stripped, Unit, UnitGraph};

    /// The workspace's graph (not the standard library's, which the load builds too) of the
    /// files under `ws/`, built with `args`, and its budget.
    fn graph(files: &[(&str, &str)], args: &[&str]) -> (Budget, UnitGraph) {
        let fixture = Fixture::new();
        let view = fixture.open(&Ask { args, ..ask(files) });
        let built = armed(|| match fixture.plan(view) {
            Planned::Ready(_) => {}
            Planned::Needs(fetches) => panic!("the load needs {fetches:?}"),
            Planned::Failed(error) => panic!("the load failed: {}", error.what),
        });
        let ws = format!("path+file://{WS}");
        built
            .into_iter()
            .find(|(_, graph)| graph.units.iter().any(|it| it.pkg_id.starts_with(&ws)))
            .expect("the workspace's graph was built on this thread")
    }

    /// The workspace's library units of the package called `name`.
    fn libs<'a>(graph: &'a UnitGraph, name: &str) -> Vec<(usize, &'a Unit)> {
        graph
            .units
            .iter()
            .enumerate()
            .filter(|(_, it)| !it.is_std && it.mode == Mode::Build && it.target.is_lib() && it.target.name == name)
            .collect()
    }

    fn lib<'a>(graph: &'a UnitGraph, name: &str) -> (usize, &'a Unit) {
        let libs = libs(graph, name);
        assert_eq!(libs.len(), 1, "one library unit of {name}: {libs:?}");
        libs[0]
    }

    /// The unit of the package called `name`'s build script in `mode`.
    fn script(graph: &UnitGraph, name: &str, mode: Mode) -> usize {
        let pkg_id = &lib(graph, name).1.pkg_id;
        graph
            .units
            .iter()
            .position(|it| &it.pkg_id == pkg_id && it.target.is_custom_build() && it.mode == mode)
            .unwrap_or_else(|| panic!("{name}'s build script, {mode:?}"))
    }

    const B: &str = "[package]\nname = \"b\"\nversion = \"0.1.0\"\nedition = \"2024\"\n";

    /// `a`, with a build script, depending on `b` both as a library and from its build script.
    fn build_and_normal() -> Vec<(&'static str, &'static str)> {
        let a = "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n\
                 [dependencies]\nb = { path = \"b\" }\n\n[build-dependencies]\nb = { path = \"b\" }\nc = { path = \"c\" }\n";
        let c = "[package]\nname = \"c\"\nversion = \"0.1.0\"\nedition = \"2024\"\n";
        vec![
            ("ws/Cargo.toml", a),
            ("ws/build.rs", "fn main() {}\n"),
            ("ws/src/lib.rs", ""),
            ("ws/b/Cargo.toml", B),
            ("ws/b/src/lib.rs", ""),
            ("ws/c/Cargo.toml", c),
            ("ws/c/src/lib.rs", ""),
        ]
    }

    #[test]
    fn a_build_dependency_the_target_also_builds_is_one_unit_on_the_host() {
        let (_, graph) = graph(&build_and_normal(), &[]);
        let (b_at, b) = lib(&graph, "b");
        assert_eq!(b.platform, None);
        assert_eq!(b.profile.debuginfo, DebugInfo::Level(2), "the target's debuginfo, settled");
        let (_, a) = lib(&graph, "a");
        assert!(a.dependencies.iter().any(|it| it.index == b_at && it.extern_crate_name == "b"));
        let script = &graph.units[script(&graph, "a", Mode::Build)];
        assert!(script.dependencies.iter().any(|it| it.index == b_at), "the build script shares b");
    }

    #[test]
    fn with_a_target_the_host_and_the_target_build_their_own_units() {
        let (_, graph) = graph(&build_and_normal(), &["--target", HOST]);
        let b = libs(&graph, "b");
        let mut platforms: Vec<Option<&str>> = b.iter().map(|(_, it)| it.platform.as_deref()).collect();
        platforms.sort();
        assert_eq!(platforms, [None, Some(HOST)]);
    }

    #[test]
    fn a_host_only_dependency_drops_its_deferred_debuginfo() {
        let (_, graph) = graph(&build_and_normal(), &[]);
        let (_, c) = lib(&graph, "c");
        assert_eq!(c.platform, None);
        assert_eq!(c.profile.debuginfo, DebugInfo::Level(0), "nothing for the target builds c");
    }

    #[test]
    fn a_dependency_two_packages_have_is_one_unit() {
        let a = "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n\
                 [dependencies]\nb = { path = \"b\" }\nc = { path = \"c\" }\n";
        let b = "[package]\nname = \"b\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\nc = { path = \"../c\" }\n";
        let c = "[package]\nname = \"c\"\nversion = \"0.1.0\"\nedition = \"2024\"\n";
        let files = [
            ("ws/Cargo.toml", a),
            ("ws/src/lib.rs", ""),
            ("ws/b/Cargo.toml", b),
            ("ws/b/src/lib.rs", ""),
            ("ws/c/Cargo.toml", c),
            ("ws/c/src/lib.rs", ""),
        ];
        let (_, graph) = graph(&files, &[]);
        let (c_at, _) = lib(&graph, "c");
        let (_, a) = lib(&graph, "a");
        let (_, b) = lib(&graph, "b");
        assert!(a.dependencies.iter().any(|it| it.index == c_at));
        assert!(b.dependencies.iter().any(|it| it.index == c_at));
        for unit in graph.units.iter().filter(|it| !it.is_std) {
            let mut targets: Vec<usize> = unit.dependencies.iter().map(|it| it.index).collect();
            let before = targets.len();
            targets.sort_unstable();
            targets.dedup();
            assert_eq!(targets.len(), before, "no edge twice: {unit:?}");
        }
    }

    #[test]
    fn a_release_build_strips_debuginfo_where_nothing_has_it() {
        let (_, graph) = graph(&build_and_normal(), &["--release"]);
        let (_, a) = lib(&graph, "a");
        assert_eq!(a.profile.debuginfo, DebugInfo::Level(0));
        assert_eq!(a.profile.strip, Strip::Resolved(Stripped::Named("debuginfo".to_owned())));
    }

    #[test]
    fn a_build_scripts_run_waits_for_the_runs_of_its_links_dependencies() {
        let a = "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\nl = { path = \"l\" }\n";
        let l = "[package]\nname = \"l\"\nversion = \"0.1.0\"\nedition = \"2024\"\nlinks = \"foo\"\n";
        let files = [
            ("ws/Cargo.toml", a),
            ("ws/build.rs", "fn main() {}\n"),
            ("ws/src/lib.rs", ""),
            ("ws/l/Cargo.toml", l),
            ("ws/l/build.rs", "fn main() {}\n"),
            ("ws/l/src/lib.rs", ""),
        ];
        let (_, graph) = graph(&files, &[]);
        let (a_run, l_run) = (script(&graph, "a", Mode::RunCustomBuild), script(&graph, "l", Mode::RunCustomBuild));
        let waits = graph.units[a_run].dependencies.iter().find(|it| it.index == l_run).expect("a's run waits for l's");
        assert_eq!(waits.extern_crate_name, "build_script_build");
    }

    #[test]
    fn the_graph_is_the_same_on_every_build() {
        let (_, first) = graph(&build_and_normal(), &[]);
        let (_, second) = graph(&build_and_normal(), &[]);
        assert_eq!(first, second);
    }

    /// The graph's own allocations: about sixty columns, maps and buffers, each made once and
    /// grown a few times at most. None is per unit, per edge or per package: what is per
    /// package is the other crates' (`external`) and what is per unit the JSON's (`output`).
    const OWN: u64 = 192;

    #[test]
    fn building_the_graph_allocates_a_fixed_number_of_buffers_beyond_its_output() {
        let small = build_and_normal();
        let (small_budget, small_graph) = graph(&small, &[]);

        let names: Vec<String> = (0..12).map(|it| format!("p{it}")).collect();
        let mut a = String::from("[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\n");
        for name in &names {
            a.push_str(&format!("{name} = {{ path = \"{name}\" }}\n"));
        }
        let manifests: Vec<(String, String)> = names
            .iter()
            .map(|name| {
                let manifest = format!(
                    "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\nb = {{ path = \"../b\" }}\n"
                );
                (format!("ws/{name}/Cargo.toml"), manifest)
            })
            .collect();
        let sources: Vec<String> = names.iter().map(|name| format!("ws/{name}/src/lib.rs")).collect();
        let mut large: Vec<(&str, &str)> =
            vec![("ws/Cargo.toml", a.as_str()), ("ws/src/lib.rs", ""), ("ws/b/Cargo.toml", B), ("ws/b/src/lib.rs", "")];
        for ((path, manifest), source) in manifests.iter().zip(&sources) {
            large.push((path.as_str(), manifest.as_str()));
            large.push((source.as_str(), ""));
        }
        let (large_budget, large_graph) = graph(&large, &[]);
        assert!(large_graph.units.len() > 2 * small_graph.units.len(), "{large_graph:?}");

        for (budget, what) in [(small_budget, "a small graph"), (large_budget, "a large one")] {
            assert!(budget.own() <= OWN, "{what}: {budget:?}, own budget {OWN}");
            assert!(budget.output > 0 && budget.external > 0, "{what}: {budget:?}");
        }
        assert!(
            large_budget.own() <= small_budget.own() + 48,
            "the graph's own allocations grow with it: {small_budget:?} then {large_budget:?}"
        );
    }
}
