//! Units written here, over files in a `blaze_query::MemTree`: a workspace of a lib with a
//! build script, a proc macro and a bin depending on the lib by another name, on the standard
//! library's `core`, lowered and checked crate by crate.

#![forbid(unsafe_code)]

use std::path::PathBuf;

use base_db::{Crate, CrateType, SourceRoot, SourceRootId, TargetKind, VfsPath};
use blaze_ids::ContentId;
use blaze_query::MemTree;
use blaze_units::{
    query::{BuildConfig, Command, Invocation, Mount, Origin, PackagePlan, TestSandbox, UnitPlan, Units},
    schema::{Mode, Profile, Strip, Stripped, Target, Unit, UnitDep, UnitGraph},
};
use cfg::CfgAtom;
use intern::Symbol;
use span::Edition;
use triomphe::Arc;

use super::{Appended, Db, Lower, LowerError, Lowered, Ran, crates, files, lower};

const HOST: &str = "x86_64-unknown-linux-gnu";
const LIBRARY: &str = "/rustc/abc/library";
const OUT_DIR: &str = "/w/target/debug/build/a-1/out";

const CORE: usize = 0;
const BUILD_SCRIPT: usize = 1;
const RUN: usize = 2;
const LIB: usize = 3;
const MACRO: usize = 4;
const BIN: usize = 5;
const LIB_TEST: usize = 6;

const WORKSPACE: &str = "[workspace]\nmembers = [\"a\", \"b\", \"m\"]\n\n[workspace.metadata.blaze]\nconst-eval = { steps = 5000 }\n";

/// The view's files.
const FILES: &[(&str, &[u8])] = &[
    ("Cargo.toml", WORKSPACE.as_bytes()),
    ("a/Cargo.toml", b"[package]\nname = \"a\"\n"),
    ("a/build.rs", b"fn main() {}\n"),
    ("a/src/lib.rs", b"include!(concat!(env!(\"OUT_DIR\"), \"/gen.rs\"));\n"),
    ("a/src/more.rs", b"pub fn more() {}\n"),
    ("b/Cargo.toml", b"[package]\nname = \"b\"\n"),
    ("b/src/main.rs", b"fn main() { alpha::gen(); }\n"),
    ("m/Cargo.toml", b"[package]\nname = \"m\"\n"),
    ("m/src/lib.rs", b"extern crate proc_macro;\n"),
    ("target/debug/stale.rs", b"not a source\n"),
];

fn view(tree: &MemTree) -> ContentId {
    tree.root(FILES)
}

/// [`FILES`] with each of `changes` made: a path's new bytes, or none for a file gone.
fn changed(tree: &MemTree, changes: &[(&'static str, Option<&'static [u8]>)]) -> ContentId {
    let mut files: Vec<(&str, &[u8])> = FILES.to_vec();
    for &(path, bytes) in changes {
        files.retain(|it| it.0 != path);
        if let Some(bytes) = bytes {
            files.push((path, bytes));
        }
    }
    tree.root(&files)
}

fn library(tree: &MemTree) -> ContentId {
    tree.root(&[
        ("core/Cargo.toml", b"[package]\nname = \"core\"\n"),
        ("core/src/lib.rs", b"#[path = \"../../stdarch/crates/core_arch/src/mod.rs\"] mod core_arch;\n"),
        ("stdarch/crates/core_arch/src/mod.rs", b"pub mod arch {}\n"),
    ])
}

fn profile() -> Profile {
    Profile { incremental: true, strip: Strip::Resolved(Stripped::None), ..Profile::dev() }
}

fn target(kind: &str, name: &str, src_path: &str, edition: &str) -> Target {
    Target {
        kind: vec![kind.to_owned()],
        crate_types: vec![if kind == "custom-build" { "bin" } else { kind }.to_owned()],
        name: name.to_owned(),
        src_path: PathBuf::from(src_path),
        edition: edition.to_owned(),
        required_features: None,
        doc: true,
        doctest: false,
        test: true,
    }
}

fn dep(index: usize, name: &str, noprelude: bool) -> UnitDep {
    UnitDep {
        index,
        extern_crate_name: name.to_owned(),
        public: None,
        noprelude: noprelude.then_some(true),
        nounused: None,
    }
}

fn unit(pkg: &str, target: Target, mode: Mode, is_std: bool, dependencies: Vec<UnitDep>) -> Unit {
    Unit {
        pkg_id: pkg.to_owned(),
        target,
        profile: profile(),
        platform: None,
        mode,
        checked_as_test: false,
        features: Vec::new(),
        is_std,
        dependencies,
    }
}

fn plan(cfgs: &[&str], env: &[(&str, &str)], out_dir: Option<&str>) -> UnitPlan {
    UnitPlan {
        checked_as_test: false,
        metadata: String::new(),
        extra_filename: String::new(),
        unit_id: "0123456789abcdef".to_owned(),
        cfgs: cfgs.iter().map(|it| (*it).to_owned()).collect(),
        check_cfg: Vec::new(),
        rustflags: Vec::new(),
        lints: Vec::new(),
        env: env.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect(),
        outputs: Vec::new(),
        out_dir: out_dir.map(str::to_owned),
        links: None,
        unsandboxed: false,
        runner: Vec::new(),
        harness: true,
        clippy: None,
    }
}

fn package(id: &str, name: &str, manifest_path: &str, source: Option<&str>, is_member: bool) -> PackagePlan {
    PackagePlan {
        id: id.to_owned(),
        name: name.to_owned(),
        version: "0.1.0".to_owned(),
        manifest_path: manifest_path.to_owned(),
        source: source.map(str::to_owned),
        is_member,
        default_run: None,
        checksum: None,
        tests: TestSandbox::default(),
        const_macros: None,
    }
}

fn units() -> Units {
    let core = "path+file:///rustc/abc/library/core#0.0.0";
    let a = "path+file:///w/a#0.1.0";
    let b = "path+file:///w/b#0.1.0";
    let m = "path+file:///w/m#0.1.0";
    let units = vec![
        unit(core, target("lib", "core", "/rustc/abc/library/core/src/lib.rs", "2024"), Mode::Build, true, vec![]),
        unit(a, target("custom-build", "build-script-build", "/w/a/build.rs", "2021"), Mode::Build, false, vec![]),
        unit(a, target("custom-build", "build-script-build", "/w/a/build.rs", "2021"), Mode::RunCustomBuild, false, vec![
            dep(BUILD_SCRIPT, "build_script_build", false),
        ]),
        unit(a, target("lib", "a", "/w/a/src/lib.rs", "2021"), Mode::Build, false, vec![
            dep(CORE, "core", true),
            dep(RUN, "build_script_build", false),
        ]),
        unit(m, target("proc-macro", "m", "/w/m/src/lib.rs", "2021"), Mode::Build, false, vec![dep(CORE, "core", true)]),
        unit(b, target("bin", "b", "/w/b/src/main.rs", "2024"), Mode::Build, false, vec![
            dep(LIB, "alpha", false),
            dep(MACRO, "m", false),
            dep(CORE, "core", true),
        ]),
        unit(a, target("lib", "a", "/w/a/src/lib.rs", "2021"), Mode::Test, false, vec![
            dep(CORE, "core", true),
            dep(RUN, "build_script_build", false),
        ]),
    ];
    let plans = vec![
        plan(&[], &[], None),
        plan(&[], &[], None),
        plan(&[], &[], Some(OUT_DIR)),
        plan(&["feature=\"fast\""], &[("CARGO_PKG_NAME", "a"), ("OUT_DIR", OUT_DIR)], None),
        plan(&[], &[("CARGO_PKG_NAME", "m")], None),
        plan(&[], &[("CARGO_PKG_NAME", "b")], None),
        plan(&["feature=\"fast\""], &[("CARGO_PKG_NAME", "a"), ("OUT_DIR", OUT_DIR)], None),
    ];
    let mut packages = vec![
        package(core, "core", "/rustc/abc/library/core/Cargo.toml", Some("(/rustc/abc/library/core)"), false),
        package(a, "a", "/w/a/Cargo.toml", Some("(/w/a)"), true),
        package(b, "b", "/w/b/Cargo.toml", Some("(/w/b)"), true),
        package(m, "m", "/w/m/Cargo.toml", Some("(/w/m)"), true),
    ];
    packages.sort_by(|x, y| x.id.cmp(&y.id));
    Units {
        graph: UnitGraph { version: 1, units, roots: vec![BIN, LIB_TEST] },
        plans,
        packages,
        lock: None,
        warnings: Vec::new(),
        target_dir: "/w/target".to_owned(),
        build_dir: "/w/target".to_owned(),
        finished: "`dev` profile [unoptimized + debuginfo]".to_owned(),
        link_dirs: Vec::new(),
        sandboxed_tests: false,
    }
}

fn config(view: ContentId, library: ContentId) -> BuildConfig {
    BuildConfig {
        invocation: Invocation {
            command: Command::Build,
            args: Vec::new(),
            root: "/w".to_owned(),
            cwd: "/w".to_owned(),
            env: Vec::new(),
            files: Vec::new(),
        },
        mounts: vec![
            Mount { at: LIBRARY.to_owned(), origin: Origin::Root(library), under: String::new() },
            Mount { at: "/w".to_owned(), origin: Origin::View, under: String::new() },
        ],
        host: HOST.to_owned(),
        library: LIBRARY.to_owned(),
        std: "/s/std".to_owned(),
    }
}

struct Built {
    lowered: Lowered,
}

/// `units` lowered over the view and library of [`view`] and [`library`].
fn lowered(units: &Units) -> Result<Built, LowerError> {
    let tree = MemTree::default();
    let root = view(&tree);
    let config = config(root, library(&tree));
    let lowered = lower(Lower { units, config: &config, files: &tree, root, host_cpu: None })?;
    Ok(Built { lowered })
}

fn krate(built: &Built, unit: usize) -> Crate {
    built.lowered.krate(unit).unwrap_or_else(|| panic!("unit {unit} has no crate"))
}

/// The source root `krate`'s root file is in: what the crate reads files from.
fn root_of(db: &Db, krate: Crate) -> Arc<SourceRoot> {
    let file = krate.data(db).root_file_id;
    let id = db.file_source_root(file).source_root_id(db);
    db.source_root(id).source_root(db)
}

fn root_id_of(db: &Db, krate: Crate) -> SourceRootId {
    db.file_source_root(krate.data(db).root_file_id).source_root_id(db)
}

fn path(text: &str) -> VfsPath {
    VfsPath::from(base_db::AbsPathBuf::try_from(text).unwrap())
}

fn flag(name: &str) -> CfgAtom {
    CfgAtom::Flag(Symbol::intern(name))
}

fn key_value(key: &str, value: &str) -> CfgAtom {
    CfgAtom::KeyValue { key: Symbol::intern(key), value: Symbol::intern(value) }
}

#[test]
fn every_unit_that_compiles_is_a_crate() {
    let built = lowered(&units()).unwrap();
    let db = built.lowered.db();
    assert!(built.lowered.krate(RUN).is_none());
    for unit in [CORE, BUILD_SCRIPT, LIB, MACRO, BIN, LIB_TEST] {
        assert!(built.lowered.krate(unit).is_some(), "unit {unit}");
    }
    assert_eq!(base_db::all_crates(db).len(), 6);
    // What every task shares answers as the writer does.
    fn shared<T: Send + Sync>(_: &T) {}
    let crates = built.lowered.crates();
    shared(&*crates);
    for unit in [CORE, RUN, LIB, MACRO, BIN] {
        assert_eq!(crates.krate(unit), built.lowered.krate(unit));
    }
    assert!(crates.is_proc_macro(MACRO) && !crates.is_proc_macro(LIB));
    // The lib and its test are two crates of one root file: the lib's is the file, the test's
    // an alias of it, which its harness is appended to.
    assert_ne!(krate(&built, LIB), krate(&built, LIB_TEST));
    let (lib_root, test_root) = (krate(&built, LIB).data(db).root_file_id, krate(&built, LIB_TEST).data(db).root_file_id);
    assert_eq!(db.files().alias_of(test_root), Some(lib_root));
    // A crate comes after every crate it depends on.
    let all = base_db::all_crates(db);
    let at = |unit| all.iter().position(|it| *it == krate(&built, unit)).unwrap();
    assert!(at(CORE) < at(LIB) && at(LIB) < at(BIN) && at(MACRO) < at(BIN));
}

#[test]
fn units_apart_only_in_what_the_front_end_does_not_read_are_one_crate() {
    let mut units = units();
    // The build script again at another opt-level, as cargo keeps a host unit whose debuginfo
    // it weakened apart from one it did not: one crate.
    let mut optimized = units.graph.units[BUILD_SCRIPT].clone();
    optimized.profile.opt_level = "3".to_owned();
    units.graph.units.push(optimized);
    units.plans.push(units.plans[BUILD_SCRIPT].clone());
    // Again with a feature: a cfg the front end reads, so another crate.
    units.graph.units.push(units.graph.units[BUILD_SCRIPT].clone());
    units.plans.push(plan(&["feature=\"extra\""], &[], None));
    let (optimized, featured) = (units.graph.units.len() - 2, units.graph.units.len() - 1);
    let built = lowered(&units).unwrap();
    assert_eq!(krate(&built, optimized), krate(&built, BUILD_SCRIPT));
    assert_ne!(krate(&built, featured), krate(&built, BUILD_SCRIPT));
    assert_eq!(base_db::all_crates(built.lowered.db()).len(), 7);
}

#[test]
fn deps_are_by_extern_name_in_the_units_order() {
    let built = lowered(&units()).unwrap();
    let db = built.lowered.db();
    let deps = &krate(&built, BIN).data(db).dependencies;
    let names: Vec<&str> = deps.iter().map(|it| it.name.as_str()).collect();
    assert_eq!(names, ["alpha", "m", "core"]);
    assert_eq!(deps[0].crate_id, krate(&built, LIB));
    assert_eq!(deps[1].crate_id, krate(&built, MACRO));
    assert_eq!(deps[2].crate_id, krate(&built, CORE));
    assert!(deps[0].is_prelude() && !deps[0].is_sysroot());
    assert!(!deps[2].is_prelude() && deps[2].is_sysroot());
    // A build script's run is no crate's dependency.
    let lib_deps: Vec<&str> = krate(&built, LIB).data(db).dependencies.iter().map(|it| it.name.as_str()).collect();
    assert_eq!(lib_deps, ["core"]);
}

#[test]
fn editions_kinds_and_origins_are_cargos() {
    let built = lowered(&units()).unwrap();
    let db = built.lowered.db();
    let data = |unit| krate(&built, unit).data(db).clone();
    assert_eq!(data(LIB).edition, Edition::Edition2021);
    assert_eq!(data(BIN).edition, Edition::Edition2024);
    assert_eq!(data(LIB).target_kind, TargetKind::Lib { is_proc_macro: false });
    assert_eq!(data(LIB_TEST).target_kind, TargetKind::Test);
    assert_eq!(data(BIN).target_kind, TargetKind::Bin);
    assert_eq!(data(BUILD_SCRIPT).target_kind, TargetKind::BuildScript);
    assert!(data(MACRO).target_kind.is_proc_macro());
    let types = |unit| data(unit).crate_types.iter().collect::<Vec<_>>();
    assert_eq!(types(LIB), [CrateType::Lib]);
    assert_eq!(types(MACRO), [CrateType::ProcMacro]);
    assert_eq!(types(BIN), [CrateType::Bin]);
    assert_eq!(types(BUILD_SCRIPT), [CrateType::Bin]);
    // rustc builds a `--test` crate as an executable, whatever its target's types.
    assert_eq!(types(LIB_TEST), [CrateType::Bin]);
    assert!(data(CORE).origin.is_lang());
    assert!(data(LIB).origin.is_local());
    // A member's proc macros run in the workspace's root.
    assert_eq!(data(MACRO).proc_macro_cwd.as_str(), "/w");
    let extra = krate(&built, BIN).extra_data(db);
    assert_eq!(extra.version.as_deref(), Some("0.1.0"));
    assert_eq!(extra.display_name.as_ref().map(|it| it.canonical_name().as_str().to_owned()).as_deref(), Some("b"));
    assert_eq!(krate(&built, LIB).workspace_data(db).const_eval.steps, 5000);
    // The workspace's budget is its own crates': std's are evaluated as in every workspace.
    assert_eq!(krate(&built, CORE).workspace_data(db).const_eval, base_db::ConstEvalBudget::default());
}

#[test]
fn a_crate_type_rustc_does_not_name_is_an_error_naming_the_unit() {
    let mut units = units();
    units.graph.units[LIB].target.crate_types = vec!["rlib".to_owned(), "shared".to_owned()];
    let Err(error) = lowered(&units) else { panic!("a crate type no rustc names was lowered") };
    assert!(error.what.contains("the crate type `shared`"), "{}", error.what);
    assert!(error.what.contains("`a` (lib"), "{}", error.what);
}

#[test]
fn cfgs_are_the_targets_with_the_units() {
    let built = lowered(&units()).unwrap();
    let db = built.lowered.db();
    let lib = krate(&built, LIB).cfg_options(db).clone();
    assert!(lib.check_atom(&key_value("feature", "fast")));
    assert!(lib.check_atom(&key_value("target_os", "linux")));
    assert!(lib.check_atom(&flag("debug_assertions")));
    assert!(!lib.check_atom(&flag("test")));
    assert!(!lib.check_atom(&flag("proc_macro")));
    let test = krate(&built, LIB_TEST).cfg_options(db).clone();
    assert!(test.check_atom(&flag("test")));
    assert!(krate(&built, MACRO).cfg_options(db).check_atom(&flag("proc_macro")));
}

#[test]
fn env_is_the_units() {
    let built = lowered(&units()).unwrap();
    let db = built.lowered.db();
    assert_eq!(krate(&built, LIB).env(db).get("CARGO_PKG_NAME").as_deref(), Some("a"));
    assert_eq!(krate(&built, BIN).env(db).get("CARGO_PKG_NAME").as_deref(), Some("b"));
    assert_eq!(krate(&built, BIN).env(db).get("OUT_DIR"), None);
}

#[test]
fn files_are_at_cargos_paths_in_their_roots() {
    let built = lowered(&units()).unwrap();
    let db = built.lowered.db();
    let paths = built.lowered.paths();
    let lib_root = krate(&built, LIB).data(db).root_file_id;
    assert_eq!(paths.get(&lib_root).map(String::as_str), Some("/w/a/src/lib.rs"));
    assert!(db.file_text(lib_root).text(db).starts_with("include!"));
    // Each package's root holds its files and no other's.
    let a = root_of(db, krate(&built, LIB));
    assert!(a.file_for_path(&path("/w/a/src/more.rs")).is_some());
    assert!(a.file_for_path(&path("/w/b/src/main.rs")).is_none());
    assert!(!a.is_library);
    // What the build writes is read by nobody.
    assert!(!paths.values().any(|it| it.starts_with("/w/target/")));
    // The standard library is one library root: core reads stdarch by `#[path]`.
    let core = root_of(db, krate(&built, CORE));
    assert!(core.is_library);
    assert!(core.file_for_path(&path("/rustc/abc/library/stdarch/crates/core_arch/src/mod.rs")).is_some());
    assert_ne!(root_id_of(db, krate(&built, CORE)), root_id_of(db, krate(&built, LIB)));
}

#[test]
fn a_path_under_no_mount_is_refused_naming_it() {
    let mut units = units();
    let elsewhere = "path+file:///elsewhere/e#0.1.0";
    units.graph.units.push(unit(elsewhere, target("lib", "e", "/elsewhere/e/src/lib.rs", "2021"), Mode::Build, false, vec![]));
    units.plans.push(plan(&[], &[], None));
    units.packages.push(package(elsewhere, "e", "/elsewhere/e/Cargo.toml", Some("(/elsewhere/e)"), false));
    units.packages.sort_by(|x, y| x.id.cmp(&y.id));
    let Err(error) = lowered(&units) else { panic!("a package under no mount was lowered") };
    assert_eq!(error.at.as_deref(), Some("/elsewhere/e"));
    assert!(error.what.contains("under no mount"), "{}", error.what);
}

#[test]
fn a_run_reaches_the_units_depending_on_it_and_no_other() {
    let mut built = lowered(&units()).unwrap();
    let ran = Ran {
        cfgs: vec!["has_gen".to_owned(), "gen=\"two\"".to_owned()],
        env: vec![("GEN".to_owned(), "1".to_owned())],
        out_dir: OUT_DIR.to_owned(),
        files: vec![("gen.rs".to_owned(), b"pub fn gen() {}\n".to_vec())],
    };
    built.lowered.ran(RUN, &ran).unwrap();
    let db = built.lowered.db();
    for unit in [LIB, LIB_TEST] {
        let krate = krate(&built, unit);
        assert!(krate.cfg_options(db).check_atom(&flag("has_gen")), "unit {unit}");
        assert!(krate.cfg_options(db).check_atom(&key_value("gen", "two")), "unit {unit}");
        assert_eq!(krate.env(db).get("GEN").as_deref(), Some("1"));
        assert_eq!(krate.env(db).get("OUT_DIR").as_deref(), Some(OUT_DIR));
    }
    let bin = krate(&built, BIN);
    assert!(!bin.cfg_options(db).check_atom(&flag("has_gen")));
    assert_eq!(bin.env(db).get("GEN"), None);

    let generated = path(&format!("{OUT_DIR}/gen.rs"));
    let file = root_of(db, krate(&built, LIB)).file_for_path(&generated).copied().unwrap();
    assert_eq!(&**db.file_text(file).text(db), "pub fn gen() {}\n");
    assert!(root_of(db, bin).file_for_path(&generated).is_none());
    assert!(root_of(db, krate(&built, MACRO)).file_for_path(&generated).is_none());
    assert_eq!(built.lowered.paths().get(&file).map(String::as_str), Some(&*format!("{OUT_DIR}/gen.rs")));

    // Applied again without the file and the cfg, the run leaves neither.
    let again = Ran { cfgs: Vec::new(), files: Vec::new(), ..ran };
    built.lowered.ran(RUN, &again).unwrap();
    let db = built.lowered.db();
    assert!(!krate(&built, LIB).cfg_options(db).check_atom(&flag("has_gen")));
    assert!(root_of(db, krate(&built, LIB)).file_for_path(&generated).is_none());
    assert!(!built.lowered.paths().contains_key(&file));
}

#[test]
fn a_run_is_refused_for_what_is_no_run() {
    let mut built = lowered(&units()).unwrap();
    let ran = Ran { out_dir: OUT_DIR.to_owned(), ..Ran::default() };
    assert!(built.lowered.ran(LIB, &ran).is_err());
    let elsewhere = Ran { out_dir: "/w/target/other".to_owned(), ..Ran::default() };
    assert!(built.lowered.ran(RUN, &elsewhere).is_err());
    assert!(built.lowered.macros(LIB, Ok(Vec::new())).is_err());
    assert!(built.lowered.macros(MACRO, Ok(Vec::new())).is_ok());
}

#[test]
fn lowering_twice_gives_equal_crates() {
    let units = units();
    let first = lowered(&units).unwrap();
    let second = lowered(&units).unwrap();
    let (one, two) = (first.lowered.db(), second.lowered.db());
    for unit in [CORE, BUILD_SCRIPT, LIB, MACRO, BIN, LIB_TEST] {
        assert_eq!(krate(&first, unit).content(one), krate(&second, unit).content(two), "unit {unit}");
    }
    assert_eq!(base_db::all_crates_content(one), base_db::all_crates_content(two));
    // The lib and its test differ by `cfg(test)`.
    assert_ne!(krate(&first, LIB).content(one), krate(&first, LIB_TEST).content(one));
}

#[test]
fn cfgs_parse_as_rustc_takes_them() {
    assert_eq!(crates::atom("foo").unwrap(), flag("foo"));
    assert_eq!(crates::atom("feature=\"a-b\"").unwrap(), key_value("feature", "a-b"));
    assert_eq!(crates::atom(r#"k="a\"b""#).unwrap(), key_value("k", "a\"b"));
    assert!(crates::atom("k=unquoted").is_err());
    assert!(crates::atom("1x").is_err());
}

/// Ambiguous for a type that is `Clone`, so naming its item there does not compile.
trait AmbiguousIfClone<A> {
    fn not_clone() {}
}
impl<T: ?Sized> AmbiguousIfClone<()> for T {}
#[allow(dead_code)]
struct IsClone;
impl<T: ?Sized + Clone> AmbiguousIfClone<IsClone> for T {}

/// The database is one value shared by reference: it is `Sync` and `Send`, and has no `Clone`
/// to make a handle with.
const _: fn() = || {
    fn shared<T: Send + Sync>() {}
    shared::<Db>();
    shared::<Lowered>();
    <Db as AmbiguousIfClone<_>>::not_clone();
};

#[test]
fn many_threads_read_the_one_database_by_reference() {
    let mut built = lowered(&units()).unwrap();
    let lowered = &built.lowered;
    let alone: Vec<(usize, bool, Option<String>)> = [LIB, LIB_TEST, BIN, MACRO]
        .into_iter()
        .map(|unit| {
            let krate = lowered.krate(unit).unwrap();
            (unit, krate.cfg_options(lowered.db()).check_atom(&flag("test")), krate.env(lowered.db()).get("CARGO_PKG_NAME"))
        })
        .collect();
    let read: Vec<Vec<(usize, bool, Option<String>)>> = std::thread::scope(|scope| {
        let threads: Vec<_> = (0..8)
            .map(|at| {
                scope.spawn(move || {
                    let db: &Db = lowered.db();
                    let mut units = [LIB, LIB_TEST, BIN, MACRO];
                    units.rotate_left(at % 4);
                    let mut seen: Vec<_> = units
                        .into_iter()
                        .map(|unit| {
                            let krate = lowered.crates().krate(unit).unwrap();
                            (unit, krate.cfg_options(db).check_atom(&flag("test")), krate.env(db).get("CARGO_PKG_NAME"))
                        })
                        .collect();
                    seen.sort();
                    seen
                })
            })
            .collect();
        threads.into_iter().map(|it| it.join().unwrap()).collect()
    });
    let mut alone = alone;
    alone.sort();
    assert!(read.iter().all(|it| *it == alone), "{read:?} against {alone:?}");
    // Once no thread holds it, the one writer writes it.
    let ran = Ran { cfgs: vec!["late".to_owned()], out_dir: OUT_DIR.to_owned(), ..Ran::default() };
    built.lowered.ran(RUN, &ran).unwrap();
    assert!(krate(&built, LIB).cfg_options(built.lowered.db()).check_atom(&flag("late")));
}

#[test]
fn a_run_is_checked_as_it_would_be_applied() {
    let built = lowered(&units()).unwrap();
    let good = Ran { cfgs: vec!["x".to_owned()], out_dir: OUT_DIR.to_owned(), ..Ran::default() };
    assert!(built.lowered.check_ran(RUN, &good).is_ok());
    assert!(built.lowered.check_ran(LIB, &good).is_err());
    let elsewhere = Ran { out_dir: "/w/target/other".to_owned(), ..Ran::default() };
    assert!(built.lowered.check_ran(RUN, &elsewhere).is_err());
    let bad = Ran { cfgs: vec!["k=unquoted".to_owned()], out_dir: OUT_DIR.to_owned(), ..Ran::default() };
    let error = built.lowered.check_ran(RUN, &bad).unwrap_err();
    assert_eq!(error.at.as_deref(), Some(&*built.lowered.crates().units[RUN].what));
    assert!(built.lowered.check_macros(MACRO).is_ok() && built.lowered.check_macros(LIB).is_err());
}

#[test]
fn a_units_identity_covers_what_it_is_made_from_deeply() {
    let units = units();
    let mut first = lowered(&units).unwrap();
    let mut second = lowered(&units).unwrap();
    let all = [CORE, BUILD_SCRIPT, LIB, MACRO, BIN, LIB_TEST];
    let before: Vec<_> = all.iter().map(|&unit| first.lowered.identity(unit)).collect();
    for (at, &unit) in all.iter().enumerate() {
        assert!(before[at].is_some(), "unit {unit}");
        assert_eq!(before[at], second.lowered.identity(unit), "unit {unit}: lowered twice");
    }
    assert_eq!(first.lowered.identity(RUN), None);
    assert_ne!(first.lowered.identity(LIB), first.lowered.identity(LIB_TEST));

    // A file the run leaves, and nothing else: the lib's own cfgs and env are as before, so
    // only the file's content tells its identity apart, and the bin's through the lib's.
    let ran = |text: &str| Ran {
        out_dir: OUT_DIR.to_owned(),
        files: vec![("gen.rs".to_owned(), text.as_bytes().to_vec())],
        ..Ran::default()
    };
    first.lowered.ran(RUN, &ran("pub fn gen() {}\n")).unwrap();
    let after: Vec<_> = all.iter().map(|&unit| first.lowered.identity(unit)).collect();
    let changed = |unit| {
        let at = all.iter().position(|it| *it == unit).unwrap();
        before[at] != after[at]
    };
    assert!(changed(LIB) && changed(LIB_TEST) && changed(BIN));
    assert!(!changed(CORE) && !changed(MACRO));
    first.lowered.ran(RUN, &ran("pub fn gen() { loop {} }\n")).unwrap();
    assert_ne!(first.lowered.identity(LIB), after[2]);
    first.lowered.ran(RUN, &ran("pub fn gen() {}\n")).unwrap();
    assert_eq!(first.lowered.identity(LIB), after[2], "the same inputs, the same identity");
}

#[test]
fn a_refresh_reads_the_view_again_and_no_fixed_mount() {
    let units = units();
    let mut built = lowered(&units).unwrap();
    let core = built.lowered.identity(CORE);
    let lib = built.lowered.identity(LIB);
    let bin_root = krate(&built, BIN).data(built.lowered.db()).root_file_id;
    // The library's root is not in this tree: a refresh that read it would fail.
    let tree = MemTree::default();
    let (more, new): (&'static [u8], &'static [u8]) = (b"pub fn more() -> u8 { 1 }\n", b"pub fn new() {}\n");
    let root = changed(&tree, &[("a/src/more.rs", Some(more)), ("a/src/new.rs", Some(new))]);
    let config = config(root, library(&MemTree::default()));
    let refreshed = built.lowered.refresh(Lower { units: &units, config: &config, files: &tree, root, host_cpu: None }, |_| Ok(()));
    assert_eq!(refreshed, Ok(true));
    let db = built.lowered.db();
    let a = root_of(db, krate(&built, LIB));
    let more = a.file_for_path(&path("/w/a/src/more.rs")).copied().unwrap();
    assert!(db.file_text(more).text(db).starts_with("pub fn more() -> u8"));
    let new = a.file_for_path(&path("/w/a/src/new.rs")).copied().unwrap();
    assert_eq!(built.lowered.paths().get(&new).map(String::as_str), Some("/w/a/src/new.rs"));
    assert_eq!(krate(&built, BIN).data(db).root_file_id, bin_root, "a file that did not change keeps its id");
    assert_eq!(built.lowered.identity(CORE), core, "nothing of the library changed");
    assert_ne!(built.lowered.identity(LIB), lib);
}

#[test]
fn a_refresh_seeds_once_in_its_write_and_a_refused_seed_is_its_error() {
    let units = units();
    let tree = MemTree::default();
    let more: &'static [u8] = b"pub fn more() -> u8 { 1 }\n";
    let root = changed(&tree, &[("a/src/more.rs", Some(more))]);
    let config = config(root, library(&MemTree::default()));
    let lower = || Lower { units: &units, config: &config, files: &tree, root, host_cpu: None };
    let mut built = lowered(&units).unwrap();
    let mut seeded = 0;
    let refreshed = built.lowered.refresh(lower(), |_| {
        seeded += 1;
        Ok(())
    });
    assert_eq!((refreshed, seeded), (Ok(true), 1));
    let again = built.lowered.refresh(lower(), |_| panic!("seeded with no text written"));
    assert_eq!(again, Ok(true), "the view did not move");

    let mut built = lowered(&units).unwrap();
    let refused = crate::LowerError { what: "the edit's parse was not taken".to_owned(), at: Some("/w/a/src/more.rs".to_owned()) };
    let error = built.lowered.refresh(lower(), |_| Err(refused.clone()));
    assert_eq!(error, Err(refused));
}

#[test]
fn a_refresh_refuses_a_root_file_gone_and_leaves_a_new_workspace_root_to_lower() {
    let units = units();
    let tree = MemTree::default();
    let library = library(&tree);
    let refresh = |changes: &[(&'static str, Option<&'static [u8]>)]| {
        let mut built = lowered(&units).unwrap();
        let root = changed(&tree, changes);
        let config = config(root, library);
        built.lowered.refresh(Lower { units: &units, config: &config, files: &tree, root, host_cpu: None }, |_| Ok(()))
    };
    let Err(error) = refresh(&[("b/src/main.rs", None)]) else { panic!("a crate's root file went and nothing said so") };
    assert!(error.what.contains("root file"), "{error}");
    let manifest: &'static [u8] = b"[workspace]\nmembers = [\"a\", \"b\", \"m\"]\n";
    assert_eq!(refresh(&[("Cargo.toml", Some(manifest))]), Ok(false), "the workspace's const-eval is every crate's");
}

#[test]
fn a_walk_leaves_out_the_directories_skipped_and_only_those() {
    let tree = MemTree::default();
    let view = view(&tree);
    let mounts = [Mount { at: "/w".to_owned(), origin: Origin::View, under: String::new() }];
    let mounts = files::Mounts { mounts: &mounts, view, tree: &tree, read_view: std::cell::Cell::new(false) };
    let walked = |skipped: &[String]| {
        let mut out = Vec::new();
        mounts.walk("/w", skipped, &mut out).unwrap();
        out.into_iter().map(|it| it.path).collect::<Vec<_>>()
    };
    let every = walked(&[]);
    assert_eq!(every.len(), FILES.len());
    assert!(every.iter().any(|it| it == "/w/target/debug/stale.rs"));
    assert!(mounts.read_view.get(), "the files were the view's");
    let skipped = walked(&["/w/target".to_owned(), "/w/a/src/lib.rs".to_owned()]);
    assert!(!skipped.iter().any(|it| it.starts_with("/w/target/") || it == "/w/a/src/lib.rs"), "{skipped:?}");
    assert_eq!(skipped.len(), FILES.len() - 2);
    assert_eq!(walked(&["/w/targe".to_owned()]), every, "a skipped path is a whole name, not a prefix");
}

/// The lib's test and the lib share `/w/a/src/lib.rs`, but the test's root is its own, an alias
/// of the file: a harness appended for the test is its root's text and appended module and in
/// its identity alone, written once; the file on disk is never appended to.
#[test]
fn an_appended_root_is_its_crates_text_until_the_next_job() {
    let units = units();
    let mut built = lowered(&units).unwrap();
    let (lib, test) = (built.lowered.identity(LIB), built.lowered.identity(LIB_TEST));
    let db = built.lowered.db();
    let (disk_file, root) = (krate(&built, LIB).data(db).root_file_id, krate(&built, LIB_TEST).data(db).root_file_id);
    assert_ne!(root, disk_file, "the test's root is its own");
    assert_eq!(db.files().alias_of(root), Some(disk_file));
    let disk = text_of(&built.lowered, disk_file);
    let harness = |body: &str| format!("\n#[cfg(test)]\npub mod __harness {{ {body} }}\n");
    let append = |lowered: &mut Lowered, unit: usize, after: Option<String>| {
        lowered.append_roots([unit as u32], |_, asked, _| {
            assert_eq!(asked, unit);
            Ok(after.clone().map(|after| Appended { after, module: "__harness" }))
        })
    };

    let recorded = harness("pub fn main() {}");
    assert_eq!(append(&mut built.lowered, LIB_TEST, Some(recorded.clone())), Ok(1));
    let db = built.lowered.db();
    let input = db.file_text(root);
    assert_eq!(**input.text(db), *format!("{disk}{recorded}"));
    assert_eq!(input.appended_root_module(db), Some("__harness"));
    assert_eq!(text_of(&built.lowered, disk_file), disk, "the file on disk is left as it is");
    let with = built.lowered.identity(LIB_TEST);
    assert_ne!(with, test, "the harness is in the test's identity");
    assert_eq!(built.lowered.identity(LIB), lib, "and not in the lib's, whose root is the file");

    assert_eq!(append(&mut built.lowered, LIB_TEST, Some(recorded.clone())), Ok(0), "already so");
    assert_eq!(append(&mut built.lowered, LIB_TEST, None), Ok(0), "no harness: left as it is");
    assert_eq!(built.lowered.identity(LIB_TEST), with);
    let Err(error) = append(&mut built.lowered, LIB, Some(harness("pub fn other() {}"))) else {
        panic!("the file on disk was appended to")
    };
    assert!(error.what.contains("can't have a module appended"), "{error}");
    assert_eq!(text_of(&built.lowered, disk_file), disk, "nothing written");

    // Given again (its proc macros loaded), a unit makes its root again.
    let expanded = harness("pub fn main() {} pub fn expanded() {}");
    assert_eq!(append(&mut built.lowered, LIB_TEST, Some(expanded.clone())), Ok(1));
    assert_eq!(text_of(&built.lowered, root), format!("{disk}{expanded}"));
    let with = built.lowered.identity(LIB_TEST);

    // The next job, of the same view: a harness of another mode is another text, and the
    // unit's identity moves with it.
    let tree = MemTree::default();
    let view = view(&tree);
    let config = config(view, library(&tree));
    let next = Lower { units: &units, config: &config, files: &tree, root: view, host_cpu: None };
    assert_eq!(built.lowered.refresh(next, |_| Ok(())), Ok(true));
    let unrecorded = harness("pub fn main() { let _ = 1; }");
    assert_eq!(append(&mut built.lowered, LIB_TEST, Some(unrecorded.clone())), Ok(1));
    assert_eq!(text_of(&built.lowered, root), format!("{disk}{unrecorded}"));
    let other = built.lowered.identity(LIB_TEST);
    assert_ne!(other, with);
    assert_ne!(other, test);
    assert_eq!(built.lowered.identity(LIB), lib);
}

fn text_of(lowered: &Lowered, file: base_db::FileId) -> String {
    lowered.db().file_text(file).text(lowered.db()).to_string()
}

const MACRO_TEST: usize = 7;

/// [`units`] with the lib test of the proc macro `m`, as `blaze test` builds it beside `m`.
fn units_with_macro_test() -> Units {
    let mut units = units();
    let m = "path+file:///w/m#0.1.0";
    let test = unit(m, target("proc-macro", "m", "/w/m/src/lib.rs", "2021"), Mode::Test, false, vec![dep(CORE, "core", true)]);
    units.graph.units.push(test);
    units.plans.push(plan(&[], &[], None));
    units
}

/// The module blaze appends to a test's root: its harness.
fn test_harness() -> Appended {
    Appended { after: "\n#[cfg(test)]\npub mod __harness { pub fn main() {} }\n".to_owned(), module: "__harness" }
}

/// Appends the harness to the proc macro's test's root.
fn append_macro_test_root(lowered: &mut Lowered) {
    let written = lowered.append_roots([MACRO_TEST as u32], |_, unit, _| {
        assert_eq!(unit, MACRO_TEST);
        Ok(Some(test_harness()))
    });
    assert_eq!(written, Ok(1));
}

/// `blaze build` builds the proc macro `m`; `blaze test` builds it and its lib test, whose
/// harness goes in a root of the test's own: the proc macro's root is its file on disk either
/// way, so it is one unit, of one identity, under both, and nothing on it is built again
/// between them.
#[test]
fn the_lib_of_a_proc_macro_is_one_unit_under_build_and_test() {
    let mut build = lowered(&units()).unwrap();
    let mut test = lowered(&units_with_macro_test()).unwrap();
    append_macro_test_root(&mut test.lowered);
    assert_eq!(build.lowered.identity(MACRO), test.lowered.identity(MACRO));
    assert_eq!(build.lowered.identity(BIN), test.lowered.identity(BIN), "nor is the bin on it");

    let db = test.lowered.db();
    let (lib_root, test_root) = (krate(&test, MACRO).data(db).root_file_id, krate(&test, MACRO_TEST).data(db).root_file_id);
    assert_ne!(lib_root, test_root, "the proc macro and its test have roots of their own");
    assert_eq!(db.files().alias_of(lib_root), None, "the proc macro's root is its file on disk");
    assert_eq!(db.files().alias_of(test_root), Some(lib_root));
    // A path still names the file on disk alone; the alias has its path.
    let source_root = db.source_root(db.file_source_root(lib_root).source_root_id(db)).source_root(db);
    assert_eq!(source_root.file_for_path(&path("/w/m/src/lib.rs")), Some(&lib_root));
    let paths = test.lowered.paths();
    for file in [lib_root, test_root] {
        assert_eq!(paths.get(&file).map(String::as_str), Some("/w/m/src/lib.rs"));
    }
    assert!(text_of(&test.lowered, test_root).ends_with(&test_harness().after));
    assert!(!text_of(&test.lowered, lib_root).contains("__harness"), "the proc macro reads no harness");
}

/// An edit of a root file reaches every root of it in the same change: the file on disk, which
/// is the proc macro's root, and its test's root with its harness after it.
#[test]
fn an_edit_of_a_root_file_reaches_every_root_of_it() {
    let units = units_with_macro_test();
    let mut built = lowered(&units).unwrap();
    append_macro_test_root(&mut built.lowered);
    let db = built.lowered.db();
    let (disk, test_root) = (krate(&built, MACRO).data(db).root_file_id, krate(&built, MACRO_TEST).data(db).root_file_id);

    const EDITED: &[u8] = b"extern crate proc_macro;\npub fn edited() {}\n";
    let tree = MemTree::default();
    let root = changed(&tree, &[("m/src/lib.rs", Some(EDITED))]);
    let config = config(root, library(&tree));
    let next = Lower { units: &units, config: &config, files: &tree, root, host_cpu: None };
    assert_eq!(built.lowered.refresh(next, |_| Ok(())), Ok(true));
    let edited = std::str::from_utf8(EDITED).unwrap();
    assert_eq!(text_of(&built.lowered, disk), edited);
    assert_eq!(text_of(&built.lowered, test_root), format!("{edited}{}", test_harness().after));
    // Made again by the next job, it is as it is already.
    assert_eq!(built.lowered.append_roots([MACRO_TEST as u32], |_, _, _| Ok(Some(test_harness()))), Ok(0));
}

/// The bin reaches the proc macro `m`, whose expanders are not built when lowered; the lib's
/// test reaches none.
#[test]
fn a_crates_macros_are_loaded_once_every_proc_macro_it_reaches_is() {
    let mut built = lowered(&units()).unwrap();
    let db = built.lowered.db();
    assert!(super::macros_loaded(db, krate(&built, LIB_TEST)));
    assert!(super::macros_loaded(db, krate(&built, CORE)));
    assert!(!super::macros_loaded(db, krate(&built, BIN)), "`m` is not built");
    built.lowered.macros(MACRO, Ok(Vec::new())).unwrap();
    assert!(super::macros_loaded(built.lowered.db(), krate(&built, BIN)));
    built.lowered.macros(MACRO, Err(base_db::ProcMacroLoadingError::FailedToBuild)).unwrap();
    assert!(!super::macros_loaded(built.lowered.db(), krate(&built, BIN)), "a macro that failed never loads");
}

/// The bin's root file (`fn main() { alpha::gen(); }`) as a proc macro's span names it: the
/// file at the bin's edition, the range under the file's root item, the root context.
fn bin_span(built: &Built) -> (u32, u32, u32, u32) {
    let file = krate(built, BIN).data(built.lowered.db()).root_file_id;
    let editioned = span::EditionedFileId::new(file, Edition::Edition2024);
    let ctx = span::SyntaxContext::root(Edition::Edition2024).into_u32();
    (file.index(), editioned.as_u32(), span::ROOT_ERASED_FILE_AST_ID.into_raw(), ctx)
}

#[test]
fn what_a_proc_macro_asks_of_its_input_is_answered_from_the_file() {
    use super::{SubRequest, SubResponse, answer};

    let built = lowered(&units()).unwrap();
    let db = built.lowered.db();
    let (raw, file_id, ast_id, _) = bin_span(&built);
    let name = "/w/b/src/main.rs".to_owned();
    assert_eq!(answer(db, SubRequest::FilePath { file_id: raw }), SubResponse::FilePathResult { name: name.clone() });
    assert_eq!(
        answer(db, SubRequest::LocalFilePath { file_id: raw }),
        SubResponse::LocalFilePathResult { name: Some(name) }
    );
    assert_eq!(
        answer(db, SubRequest::SourceText { file_id, ast_id, start: 3, end: 7 }),
        SubResponse::SourceTextResult { text: Some("main".to_owned()) }
    );
    assert_eq!(
        answer(db, SubRequest::SourceText { file_id, ast_id, start: 3, end: 700 }),
        SubResponse::SourceTextResult { text: None },
        "a range past the file's end has no text"
    );
    assert_eq!(
        answer(db, SubRequest::LineColumn { file_id, ast_id, offset: 12 }),
        SubResponse::LineColumnResult { line: 1, column: 13 },
        "1-based, as proc_macro's"
    );
    assert_eq!(
        answer(db, SubRequest::ByteRange { file_id, ast_id, start: 3, end: 7 }),
        SubResponse::ByteRangeResult { range: 3..7 }
    );
}

#[test]
fn a_span_written_in_a_file_is_its_own_source_and_has_no_parent() {
    use super::{SubRequest, SubResponse, answer};

    let built = lowered(&units()).unwrap();
    let db = built.lowered.db();
    let (_, file_id, ast_id, ctx) = bin_span(&built);
    assert_eq!(
        answer(db, SubRequest::SpanSource { file_id, ast_id, start: 12, end: 17, ctx }),
        SubResponse::SpanSourceResult { file_id, ast_id, start: 12, end: 17, ctx }
    );
    assert_eq!(
        answer(db, SubRequest::SpanParent { file_id, ast_id, start: 12, end: 17, ctx }),
        SubResponse::SpanParentResult { parent_span: None }
    );
}

/// `fn` and `alpha` of the bin's root joined: from where the first starts to where the second
/// ends, as rustc's `Span::join`; of two contexts, the first.
#[test]
fn two_spans_of_one_file_join_to_the_span_covering_both() {
    use proc_macro_api::bridge::questions::SpanJoin;

    use super::{SubRequest, SubResponse, answer};

    let built = lowered(&units()).unwrap();
    let db = built.lowered.db();
    let (_, file_id, ast_id, ctx) = bin_span(&built);
    let join = |ctx_second: u32| SubRequest::SpanJoin {
        file_id,
        ast_id_first: ast_id,
        start_first: 0,
        end_first: 2,
        ctx_first: ctx,
        ast_id_second: ast_id,
        start_second: 12,
        end_second: 17,
        ctx_second,
    };
    assert_eq!(
        answer(db, join(ctx)),
        SubResponse::SpanJoinResult { span: Some(SpanJoin { ast_id, start: 0, end: 17, ctx }) }
    );
    let other = span::SyntaxContext::root(Edition::Edition2021).into_u32();
    assert_ne!(other, ctx);
    assert_eq!(
        answer(db, join(other)),
        SubResponse::SpanJoinResult { span: Some(SpanJoin { ast_id, start: 0, end: 2, ctx }) }
    );
}

/// A server's numbers that name nothing of the build are a refusal naming the field, never a
/// panic: the server is user code.
#[test]
fn a_question_naming_nothing_of_the_build_is_refused() {
    use super::{SubRequest, SubResponse, answer};

    let built = lowered(&units()).unwrap();
    let db = built.lowered.db();
    let (raw, file_id, ast_id, ctx) = bin_span(&built);
    let refused = |req: SubRequest, field: &str| match answer(db, req) {
        SubResponse::Cancel { reason } => assert!(reason.contains(field), "{field}: {reason}"),
        other => panic!("{field}: answered {other:?}"),
    };
    refused(SubRequest::FilePath { file_id: u32::MAX }, "file_id");
    refused(SubRequest::LocalFilePath { file_id: raw + 1000 }, "file_id");
    refused(SubRequest::SourceText { file_id: file_id | 0x8000_0000, ast_id, start: 0, end: 1 }, "reserved");
    refused(SubRequest::SourceText { file_id: file_id | 0x7F80_0000, ast_id, start: 0, end: 1 }, "edition");
    refused(SubRequest::ByteRange { file_id, ast_id, start: 7, end: 3 }, "range");
    refused(SubRequest::ByteRange { file_id, ast_id: 1_000_000, start: 0, end: 1 }, "ast_id");
    refused(SubRequest::LineColumn { file_id, ast_id, offset: 1_000_000 }, "offset");
    refused(SubRequest::SpanSource { file_id, ast_id, start: 0, end: 1, ctx: u32::MAX }, "ctx");
    let unowned = blaze_query::db::Id::MAX_U32 - 2;
    refused(SubRequest::SpanParent { file_id, ast_id, start: 0, end: 1, ctx: unowned }, "ctx");
    refused(
        SubRequest::SpanJoin {
            file_id,
            ast_id_first: ast_id,
            start_first: 0,
            end_first: 2,
            ctx_first: ctx,
            ast_id_second: ast_id,
            start_second: 17,
            end_second: 12,
            ctx_second: ctx,
        },
        "range",
    );
}

#[test]
fn a_cycle_of_units_is_refused() {
    let mut units = units();
    units.graph.units[CORE].dependencies.push(dep(BIN, "b", false));
    let Err(error) = lowered(&units) else { panic!("a cycle was lowered") };
    assert!(error.what.contains("cycle"), "{}", error.what);
}

const PROC_MACRO: usize = 7;

const CONST_MACROS: &[u8] = b"#[proc_macro]\npub fn double(input: TokenStream) -> TokenStream { input }\n";

/// [`units`] with the standard library's `proc_macro`, and `a`'s const macros in
/// `a/src/macros.rs`.
fn const_macro_units() -> Units {
    let mut units = units();
    let proc_macro = "path+file:///rustc/abc/library/proc_macro#0.0.0";
    let src = "/rustc/abc/library/proc_macro/src/lib.rs";
    let crate_of = unit(proc_macro, target("lib", "proc_macro", src, "2024"), Mode::Build, true, vec![dep(CORE, "core", true)]);
    units.graph.units.push(crate_of);
    units.plans.push(plan(&[], &[], None));
    let manifest = "/rustc/abc/library/proc_macro/Cargo.toml";
    units.packages.push(package(proc_macro, "proc_macro", manifest, Some("(/rustc/abc/library/proc_macro)"), false));
    units.packages.sort_by(|x, y| x.id.cmp(&y.id));
    let a = units.packages.iter_mut().find(|it| it.name == "a").unwrap();
    a.const_macros = Some("/w/a/src/macros.rs".to_owned());
    units
}

/// `units` lowered over [`FILES`] with `changes` made, and a library with `proc_macro`.
fn lowered_with(units: &Units, tree: &MemTree, changes: &[(&'static str, Option<&'static [u8]>)]) -> Result<Built, LowerError> {
    let root = changed(tree, changes);
    let config = config(root, library_with_proc_macro(tree));
    let lowered = lower(Lower { units, config: &config, files: tree, root, host_cpu: None })?;
    Ok(Built { lowered })
}

fn library_with_proc_macro(tree: &MemTree) -> ContentId {
    tree.root(&[
        ("core/Cargo.toml", b"[package]\nname = \"core\"\n"),
        ("core/src/lib.rs", b"#[path = \"../../stdarch/crates/core_arch/src/mod.rs\"] mod core_arch;\n"),
        ("stdarch/crates/core_arch/src/mod.rs", b"pub mod arch {}\n"),
        ("proc_macro/Cargo.toml", b"[package]\nname = \"proc_macro\"\n"),
        ("proc_macro/src/lib.rs", b"pub struct TokenStream;\n"),
    ])
}

/// The const macros of `krate`'s macro crate, by name.
fn const_macros_of(db: &Db, krate: Crate) -> Vec<String> {
    let all = hir_expand::proc_macro::ProcMacros::get(db);
    let macros = all.by_crate(db).get(&krate).cloned().expect("the macro crate has macros");
    macros.macros().expect("listed").iter().map(|it| it.name.as_str().to_owned()).collect()
}

/// The crate `krate` depends on by `name`.
fn dependency_of(db: &Db, krate: Crate, name: &str) -> Option<Crate> {
    krate.data(db).dependencies.iter().find(|it| it.name.as_str() == name).map(|it| it.crate_id)
}

#[test]
fn a_packages_const_macros_are_one_crate_its_crates_depend_on_by_the_files_stem() {
    let units = const_macro_units();
    let tree = MemTree::default();
    let built = lowered_with(&units, &tree, &[("a/src/macros.rs", Some(CONST_MACROS))]).unwrap();
    let db = built.lowered.db();
    let macros = dependency_of(db, krate(&built, LIB), "macros").expect("the lib depends on its const macros");
    assert_eq!(dependency_of(db, krate(&built, LIB_TEST), "macros"), Some(macros), "one crate a package");
    assert_eq!(dependency_of(db, krate(&built, BIN), "macros"), None, "`b` has no const macros");
    assert_eq!(dependency_of(db, krate(&built, BUILD_SCRIPT), "macros"), None, "a build script has none");

    // Every crate depending on them has the cfg rustc never sets; no other crate does.
    let has_cfg = |unit| krate(&built, unit).cfg_options(db).check_atom(&flag("blaze_const_macros"));
    assert!(has_cfg(LIB) && has_cfg(LIB_TEST));
    assert!(!has_cfg(BUILD_SCRIPT) && !has_cfg(BIN));

    // A proc macro's crate of the lib's dependencies, cfgs and edition, which no unit compiles.
    let data = macros.data(db);
    assert_eq!(data.target_kind, TargetKind::Lib { is_proc_macro: true });
    assert_eq!(data.edition, Edition::Edition2021);
    assert_eq!(built.lowered.paths().get(&data.root_file_id).map(String::as_str), Some("/w/a/src/macros.rs"));
    let names: Vec<&str> = data.dependencies.iter().map(|it| it.name.as_str()).collect();
    assert_eq!(names, ["core", "proc_macro", "const_ast"]);
    let cfg = macros.cfg_options(db);
    assert!(cfg.check_atom(&flag("proc_macro")) && cfg.check_atom(&flag("blaze_const_macros")));
    assert!(cfg.check_atom(&key_value("feature", "fast")) && !cfg.check_atom(&flag("test")));
    assert!((0..units.graph.units.len()).all(|unit| built.lowered.krate(unit) != Some(macros)));

    // blaze's `const-ast`, beside the standard library, on `core` alone.
    let const_ast = dependency_of(db, macros, "const_ast").unwrap();
    let root = const_ast.data(db).root_file_id;
    assert_eq!(built.lowered.paths().get(&root).map(String::as_str), Some("/rustc/abc/blaze/const-ast/lib.rs"));
    let on: Vec<Crate> = const_ast.data(db).dependencies.iter().map(|it| it.crate_id).collect();
    assert_eq!(on, [krate(&built, CORE)]);

    // Its macros are listed from its file, and every crate comes after what it depends on.
    assert_eq!(const_macros_of(db, macros), ["double"]);
    let all = base_db::all_crates(db);
    assert_eq!(all.len(), 9);
    let at = |krate: Crate| all.iter().position(|it| *it == krate).unwrap();
    assert!(at(krate(&built, PROC_MACRO)) < at(macros) && at(const_ast) < at(macros));
    assert!(at(macros) < at(krate(&built, LIB)) && at(macros) < at(krate(&built, LIB_TEST)));
}

#[test]
fn const_macros_named_by_a_file_that_does_not_exist_are_an_error_naming_it() {
    let tree = MemTree::default();
    let Err(error) = lowered_with(&const_macro_units(), &tree, &[]) else { panic!("a missing file was lowered") };
    assert!(error.what.contains("does not exist"), "{}", error.what);
    assert_eq!(error.at.as_deref(), Some("/w/a/src/macros.rs"));
}

#[test]
fn a_refresh_lists_the_const_macros_again_once_their_file_changes() {
    let units = const_macro_units();
    let tree = MemTree::default();
    let mut built = lowered_with(&units, &tree, &[("a/src/macros.rs", Some(CONST_MACROS))]).unwrap();
    let macros = dependency_of(built.lowered.db(), krate(&built, LIB), "macros").unwrap();
    let more: &'static [u8] = b"#[proc_macro]\npub fn double(input: TokenStream) -> TokenStream { input }\n\
        #[proc_macro_attribute]\npub fn twice(attr: TokenStream, item: TokenStream) -> TokenStream { item }\n";
    let root = changed(&tree, &[("a/src/macros.rs", Some(more))]);
    let with_more = config(root, library_with_proc_macro(&tree));
    let refreshed =
        built.lowered.refresh(Lower { units: &units, config: &with_more, files: &tree, root, host_cpu: None }, |_| Ok(()));
    assert_eq!(refreshed, Ok(true));
    assert_eq!(const_macros_of(built.lowered.db(), macros), ["double", "twice"]);

    let root = changed(&tree, &[]);
    let without = config(root, library_with_proc_macro(&tree));
    let Err(error) =
        built.lowered.refresh(Lower { units: &units, config: &without, files: &tree, root, host_cpu: None }, |_| Ok(()))
    else {
        panic!("the const macros' file went and nothing said so")
    };
    assert!(error.what.contains("const macros"), "{}", error.what);
}

/// rustc compiles what a builtin derive expands to, so a build's database expands them: a
/// derived `Clone` is an impl of the crate whose `clone` has a body, not one only recorded.
#[test]
fn a_derived_method_has_a_body() {
    let tree = MemTree::default();
    let root = view(&tree);
    let library = tree.root(&[
        ("core/Cargo.toml", b"[package]\nname = \"core\"\n"),
        (
            "core/src/lib.rs",
            b"#![feature(no_core, lang_items, rustc_attrs, decl_macro)]\n#![no_core]\n\
              #[lang = \"pointee_sized\"] pub trait PointeeSized {}\n\
              #[lang = \"meta_sized\"] pub trait MetaSized: PointeeSized {}\n\
              #[lang = \"sized\"] pub trait Sized: MetaSized {}\n\
              #[lang = \"clone\"] pub trait Clone: Sized { fn clone(&self) -> Self; }\n\
              #[rustc_builtin_macro] pub macro derive($item:item) {}\n\
              #[rustc_builtin_macro] pub macro Clone($item:item) {}\n\
              #[derive(Clone)] pub struct Point;\n\
              #[path = \"../../stdarch/crates/core_arch/src/mod.rs\"] mod core_arch;\n",
        ),
        ("stdarch/crates/core_arch/src/mod.rs", b"pub mod arch {}\n"),
    ]);
    let config = config(root, library);
    let units = units();
    let lowered = lower(Lower { units: &units, config: &config, files: &tree, root, host_cpu: None }).unwrap();
    let db = lowered.db();
    let core = lowered.krate(CORE).expect("core is a crate");
    let def_map = hir_def::nameres::crate_def_map(db, core);
    let scope = def_map.module(db, def_map.crate_root(db)).scope(db);
    assert_eq!(scope.builtin_derive_impls().len(), 0, "a derive only recorded");
    let bodies: Vec<&str> = scope
        .impls()
        .flat_map(|impl_| impl_.impl_items(db).items.iter())
        .filter_map(|&(_, item)| match item {
            hir_def::AssocItemId::FunctionId(func) => {
                let signature = hir_def::signatures::FunctionSignature::of(db, func);
                signature.has_body().then(|| signature.name.as_str())
            }
            _ => None,
        })
        .collect();
    assert_eq!(bodies, ["clone"]);
}

/// cargo passes a proc-macro target `--extern proc_macro` beside build-std's `noprelude` one:
/// `proc_macro` is in a proc macro's extern prelude, so its `use proc_macro::TokenStream`
/// resolves without an `extern crate`, and in no other crate's.
#[test]
fn a_proc_macros_proc_macro_is_in_its_extern_prelude_alone() {
    let mut units = units();
    let proc_macro = "path+file:///rustc/abc/library/proc_macro#0.0.0";
    let src = "/rustc/abc/library/proc_macro/src/lib.rs";
    let crate_of = unit(proc_macro, target("lib", "proc_macro", src, "2024"), Mode::Build, true, vec![dep(CORE, "core", true)]);
    units.graph.units.push(crate_of);
    units.plans.push(plan(&[], &[], None));
    let manifest = "/rustc/abc/library/proc_macro/Cargo.toml";
    units.packages.push(package(proc_macro, "proc_macro", manifest, Some("(/rustc/abc/library/proc_macro)"), false));
    units.packages.sort_by(|x, y| x.id.cmp(&y.id));
    for at in [MACRO, BIN] {
        units.graph.units[at].dependencies.push(dep(PROC_MACRO, "proc_macro", true));
    }
    let tree = MemTree::default();
    let built = lowered_with(&units, &tree, &[("m/src/lib.rs", Some(b"use proc_macro::TokenStream;\n"))]).unwrap();
    let db = built.lowered.db();
    let in_prelude = |unit| {
        let data = krate(&built, unit).data(db);
        data.dependencies.iter().find(|it| it.name.as_str() == "proc_macro").map(|it| it.is_prelude())
    };
    assert_eq!(in_prelude(MACRO), Some(true));
    assert_eq!(in_prelude(BIN), Some(false), "only a proc macro is passed `--extern proc_macro`");
}
