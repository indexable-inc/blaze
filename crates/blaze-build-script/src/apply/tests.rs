//! The cases of cargo's tests of who receives what a script printed
//! (`tests/testsuite/build_script.rs`, `build_script_extra_link_arg.rs` and
//! `build_script_env.rs` in rust-lang/cargo at 5f94df478, MIT or Apache-2.0). Each names the
//! test it comes from.

use super::*;

fn target(name: &str, kind: &str, crate_type: &str) -> Target {
    Target { name: name.into(), kind: vec![kind.into()], crate_types: vec![crate_type.into()] }
}

fn package(targets: Vec<Target>) -> Package {
    Package { name: "foo".into(), version: "0.0.1".into(), targets, ..Package::default() }
}

fn searching(paths: &[&str], out: &str) -> Output {
    let library_paths = paths
        .iter()
        .map(|it| match it.starts_with(out) {
            true => LibraryPath::CargoArtifact(it.into()),
            false => LibraryPath::External(it.into()),
        })
        .collect();
    Output { library_paths, ..Output::default() }
}

/// `linker_search_path_preference`: directories in a script's `OUT_DIR` first, each kind in
/// the order of the scripts and of their lines.
#[test]
fn search_paths_in_out_dirs_first() {
    let foo = searching(&["/usr/lib", "/f/out/libs2", "/lib", "/f/out/libs1"], "/f/out");
    let a = searching(&["/usr/lib3", "/a/out/libsA.2", "/lib3", "/a/out/libsA.1"], "/a/out");
    let b = searching(&["/usr/lib2", "/b/out/libsB.1", "/lib2", "/b/out/libsB.2"], "/b/out");
    let linked = [
        Linked { own: true, output: &foo },
        Linked { own: false, output: &a },
        Linked { own: false, output: &b },
    ];
    let lib = target("foo", "lib", "lib");
    let compile = Compile::of(&package(vec![lib.clone()]), &lib, false, &linked, &[&foo]);
    let order: Vec<_> = compile.search.iter().map(|it| it.to_str().unwrap()).collect();
    assert_eq!(
        order,
        [
            "/f/out/libs2",
            "/f/out/libs1",
            "/a/out/libsA.2",
            "/a/out/libsA.1",
            "/b/out/libsB.1",
            "/b/out/libsB.2",
            "/usr/lib",
            "/lib",
            "/usr/lib3",
            "/lib3",
            "/usr/lib2",
            "/lib2"
        ]
    );
}

/// `custom_build_script_rustc_flags`, `propagation_of_l_flags` and `flags_go_into_tests`:
/// `-l` for the package's library alone (or its programs when it has none), `-L` for
/// everything linked, cfgs and variables for every target of the package.
#[test]
fn libs_search_and_cfgs() {
    let own = Output {
        library_paths: vec![LibraryPath::External("/p/dummy".into())],
        library_links: vec!["nonexistinglib".into()],
        cfgs: vec!["foo".into()],
        check_cfgs: vec!["cfg(foo)".into()],
        env: vec![("FOO".into(), "1".into())],
        ..Output::default()
    };
    let dep = Output {
        library_paths: vec![LibraryPath::External("/dep".into())],
        library_links: vec!["dep".into()],
        cfgs: vec!["dep".into()],
        ..Output::default()
    };
    let linked = [Linked { own: true, output: &own }, Linked { own: false, output: &dep }];
    let lib = target("foo", "lib", "lib");
    let bin = target("bar", "bin", "bin");
    let with_lib = package(vec![lib.clone(), bin.clone()]);
    let compile = Compile::of(&with_lib, &lib, false, &linked, &[&own]);
    assert_eq!(
        compile.args(),
        [
            "-L",
            "/p/dummy",
            "-L",
            "/dep",
            "-l",
            "nonexistinglib",
            "--cfg",
            "foo",
            "--check-cfg",
            "cfg(foo)"
        ]
    );
    assert_eq!(compile.env, [("FOO".to_owned(), "1".to_owned())]);
    let compile = Compile::of(&with_lib, &bin, false, &linked, &[&own]);
    assert!(compile.libs.is_empty(), "the library has them");
    assert_eq!(compile.cfgs, ["foo"]);
    let compile = Compile::of(&package(vec![bin.clone()]), &bin, false, &linked, &[&own]);
    assert_eq!(compile.libs, ["nonexistinglib"]);
}

/// `link_arg_transitive_not_allowed`, `cdylib_link_arg_transitive`,
/// `cdylib_extra_link_args_should_not_apply_to_unit_tests` and the per-target forms.
#[test]
fn link_args_reach_their_targets() {
    let own = Output {
        linker_args: vec![
            (LinkArgTarget::All, "all".into()),
            (LinkArgTarget::Bin, "bins".into()),
            (LinkArgTarget::SingleBin("one".into()), "one".into()),
            (LinkArgTarget::Test, "tests".into()),
            (LinkArgTarget::Bench, "benches".into()),
            (LinkArgTarget::Example, "examples".into()),
            (LinkArgTarget::Cdylib, "cdylib".into()),
        ],
        ..Output::default()
    };
    let dep = Output {
        linker_args: vec![
            (LinkArgTarget::All, "dep-all".into()),
            (LinkArgTarget::Cdylib, "dep-cdylib".into()),
        ],
        ..Output::default()
    };
    let linked = [Linked { own: true, output: &own }, Linked { own: false, output: &dep }];
    let pkg = package(vec![]);
    let args = |target: &Target, test| Compile::of(&pkg, target, test, &linked, &[]).link_args;
    assert_eq!(args(&target("one", "bin", "bin"), false), ["all", "bins", "one"]);
    assert_eq!(args(&target("two", "bin", "bin"), false), ["all", "bins"]);
    assert_eq!(args(&target("t", "test", "bin"), true), ["all", "tests"]);
    assert_eq!(args(&target("b", "bench", "bin"), true), ["all", "benches"]);
    assert_eq!(args(&target("e", "example", "bin"), false), ["all", "examples"]);
    assert_eq!(args(&target("e", "example", "cdylib"), false), ["all"]);
    assert_eq!(args(&target("c", "cdylib", "cdylib"), false), ["all", "cdylib", "dep-cdylib"]);
    assert_eq!(args(&target("c", "cdylib", "cdylib"), true), ["all"]);
}

/// `build_map`: a unit is linked with its package's script and those of the libraries it
/// links, each once and in order; a build dependency's go to the dynamic library path.
#[test]
fn which_scripts_a_unit_is_linked_with() {
    // app -> a (script) -> b (script); app -> c (script); app's script depends on d (script).
    let graph = |unit: &&str| -> GraphUnit<&str, &str> {
        let (deps, own_runs, for_host, linkable): (Vec<&str>, Vec<&str>, bool, bool) = match *unit {
            "app" => (vec!["a", "c", "app-script"], vec!["run-app"], false, false),
            "a" => (vec!["b"], vec!["run-a"], false, true),
            "b" => (vec![], vec!["run-b"], false, true),
            "c" => (vec!["b"], vec!["run-c"], false, true),
            "app-script" => (vec!["d"], vec![], true, false),
            "d" => (vec![], vec!["run-d"], false, true),
            _ => unreachable!(),
        };
        GraphUnit { deps, own_runs, for_host, linkable }
    };
    let mut memo = HashMap::default();
    let scripts = to_link("app", &graph, &mut memo);
    assert_eq!(scripts.to_link, ["run-app", "run-a", "run-b", "run-c"]);
    assert_eq!(to_link("app-script", &graph, &mut memo).to_link, ["run-d"]);
    assert!(scripts.plugins.contains("run-d"), "{scripts:?}");
}

/// What a unit is linked with, met again, is read from the memo: nothing is allocated and the
/// graph is not asked.
#[test]
fn a_unit_met_again_is_free() {
    let graph = |unit: &u32| -> GraphUnit<u32, u32> {
        let deps = (*unit + 1..(*unit + 3).min(8)).collect();
        GraphUnit { deps, own_runs: vec![100 + *unit], for_host: false, linkable: true }
    };
    let mut memo = HashMap::default();
    let first = to_link_in(0, &graph, &mut memo).to_link.clone();
    let again = blaze_alloc_count::assert_at_most(0, "a memo hit", || {
        to_link_in(0, &graph, &mut memo).to_link.len()
    });
    assert_eq!(again, first.len());
}

/// `rebuild_only_on_explicit_paths`, `rerun_if_directory`, `rerun_if_env_changes` and
/// `rerun_if_env_exists_in_config`.
#[test]
fn when_a_script_runs_again() {
    let dir = std::env::temp_dir().join(format!("blaze-build-script-rerun-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("watched/deep")).unwrap();
    std::fs::write(dir.join("foo"), "").unwrap();
    std::fs::write(dir.join("watched/deep/file"), "").unwrap();
    assert_eq!(Output::default().rerun(&dir), Rerun::Package);
    let output = Output {
        rerun_if_changed: vec!["foo".into(), dir.join("watched")],
        rerun_if_env_changed: vec!["FOO".into()],
        ..Output::default()
    };
    let rerun = output.rerun(&dir);
    assert_eq!(
        rerun,
        Rerun::Watched { files: vec!["foo".into(), "watched".into()], env: vec!["FOO".into()] }
    );
    let foo = |_: &str| Some("foo");
    let ran = SystemTime::now() + std::time::Duration::from_secs(60);
    assert_eq!(rerun.stale(&dir, ran, &foo, &foo), Some(None));
    let bar = |_: &str| Some("bar");
    assert!(matches!(rerun.stale(&dir, ran, &foo, &bar), Some(Some(Stale::Env { .. }))));
    let before = SystemTime::UNIX_EPOCH;
    let newer = Some(Some(Stale::Newer(dir.join("foo"))));
    assert_eq!(rerun.stale(&dir, before, &foo, &foo), newer);
    // A file deep in a watched directory counts.
    let file = std::fs::File::options().write(true).open(dir.join("foo")).unwrap();
    file.set_modified(before).unwrap();
    let watched = dir.join("watched");
    assert_eq!(rerun.stale(&dir, before, &foo, &foo), Some(Some(Stale::Newer(watched))));
    std::fs::remove_file(dir.join("foo")).unwrap();
    let missing = Some(Some(Stale::Missing(dir.join("foo"))));
    assert_eq!(rerun.stale(&dir, ran, &foo, &foo), missing);
    assert_eq!(Rerun::Package.stale(&dir, ran, &foo, &foo), None);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A check of a kept run's files and variables that finds nothing changed allocates nothing
/// once the thread's path buffer has grown: each file is one `stat`, each variable compared
/// borrowed.
#[test]
fn a_fresh_run_is_checked_without_allocating() {
    let dir = std::env::temp_dir().join(format!("blaze-build-script-fresh-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    for file in ["build.rs", "src/a.rs", "src/b.rs"] {
        std::fs::write(dir.join(file), "").unwrap();
    }
    let rerun = Rerun::Watched {
        files: vec!["build.rs".into(), "src/a.rs".into(), "src/b.rs".into()],
        env: vec!["CC".into(), "CFLAGS".into(), "UNSET".into()],
    };
    let vars = |name: &str| match name {
        "CC" => Some("cc"),
        "CFLAGS" => Some("-O2"),
        _ => None,
    };
    let ran = SystemTime::now() + std::time::Duration::from_secs(60);
    assert_eq!(rerun.stale(&dir, ran, &vars, &vars), Some(None));
    let fresh = blaze_alloc_count::assert_at_most(0, "checking a kept run", || {
        rerun.stale(&dir, ran, &vars, &vars)
    });
    assert_eq!(fresh, Some(None));
    let _ = std::fs::remove_dir_all(&dir);
}
