#![forbid(unsafe_code)]

use std::{
    ffi::{OsStr, OsString},
    path::PathBuf,
};

#[expect(clippy::disallowed_types, reason = "a test job's slots, as the job keeps them: per job, not global")]
use std::sync::OnceLock;

use blaze_api::{Artifact, ArtifactKind, Level, UnitProfile};
use blaze_ids::ContentId;
use blaze_units::{
    query::{PackagePlan, TestSandbox, UnitPlan, Units},
    schema::{Mode, Profile, Target, Unit, UnitDep, UnitGraph},
};
use blaze_usercode::run::Value;

use super::{artifact, codegen, compile, graph, pack, run, script, test, unit_target};

const HOST: &str = "x86_64-unknown-linux-gnu";

fn target(name: &str, kind: &str, src: &str) -> Target {
    let crate_types = match kind {
        "lib" => vec!["lib".to_owned()],
        _ => vec!["bin".to_owned()],
    };
    Target {
        kind: vec![kind.to_owned()],
        crate_types,
        name: name.to_owned(),
        src_path: PathBuf::from(src),
        edition: "2024".to_owned(),
        required_features: None,
        doc: kind == "lib",
        doctest: kind == "lib",
        test: true,
    }
}

fn profile() -> Profile {
    Profile { incremental: true, ..Profile::dev() }
}

fn unit(pkg: &str, target: Target, mode: Mode, deps: &[usize]) -> Unit {
    Unit {
        pkg_id: pkg.to_owned(),
        target,
        profile: profile(),
        platform: None,
        macro_wasm: false,
        mode,
        checked_as_test: false,
        features: vec!["default".to_owned()],
        is_std: false,
        dependencies: deps
            .iter()
            .map(|it| UnitDep { index: *it, extern_crate_name: "dep".to_owned(), public: None, noprelude: None, nounused: None, dev: false })
            .collect(),
    }
}

fn plan(extra: &str, outputs: &[&str]) -> UnitPlan {
    UnitPlan {
        checked_as_test: false,
        metadata: extra.trim_start_matches('-').to_owned(),
        extra_filename: extra.to_owned(),
        unit_id: format!("{:0>16}", extra.trim_start_matches('-')),
        cfgs: Vec::new(),
        check_cfg: Vec::new(),
        rustflags: Vec::new(),
        lints: Vec::new(),
        env: Vec::new(),
        outputs: outputs.iter().map(|it| (*it).to_owned()).collect(),
        out_dir: None,
        links: None,
        unsandboxed: false,
        runner: Vec::new(),
        harness: true,
        clippy: None,
        macro_wasm: false,
    }
}

fn package(id: &str, name: &str, dir: &str) -> PackagePlan {
    PackagePlan {
        id: id.to_owned(),
        name: name.to_owned(),
        version: "0.1.0".to_owned(),
        manifest_path: format!("{dir}/Cargo.toml"),
        source: Some(format!("({dir})")),
        is_member: true,
        default_run: None,
        checksum: None,
        tests: TestSandbox::default(),
        const_macros: None,
    }
}

const P: &str = "path+file:///w/p#0.1.0";
const Z: &str = "path+file:///w/z-sys#0.1.0";

/// `z-sys` (links `z`) with a build script, and `p`, whose script reads `z`'s metadata.
fn build(dir: &std::path::Path) -> Units {
    let out = dir.join("debug/build");
    let z_out = out.join("z-sys-0001/out").to_string_lossy().into_owned();
    let p_out = out.join("p-0002/out").to_string_lossy().into_owned();
    let units = vec![
        // 0: z-sys's script compiled, 1: run.
        unit(Z, target("build-script-build", "custom-build", "/w/z-sys/build.rs"), Mode::Build, &[]),
        unit(Z, target("build-script-build", "custom-build", "/w/z-sys/build.rs"), Mode::RunCustomBuild, &[0]),
        // 2: p's script compiled, 3: run, after z-sys's run.
        unit(P, target("build-script-build", "custom-build", "/w/p/build.rs"), Mode::Build, &[]),
        unit(P, target("build-script-build", "custom-build", "/w/p/build.rs"), Mode::RunCustomBuild, &[2, 1]),
    ];
    let script_plan = |out_dir: &str, links: Option<&str>, name: &str| UnitPlan {
        out_dir: Some(out_dir.to_owned()),
        links: links.map(str::to_owned),
        env: vec![("CARGO_PKG_NAME".to_owned(), name.to_owned()), ("PROFILE".to_owned(), "debug".to_owned())],
        ..plan("", &[])
    };
    Units {
        graph: UnitGraph { version: 1, units, roots: vec![3] },
        plans: vec![
            plan("-0001", &["/w/target/debug/build/z-sys-0001/build_script_build-0001"]),
            script_plan(&z_out, Some("z"), "z-sys"),
            plan("-0002", &["/w/target/debug/build/p-0002/build_script_build-0002"]),
            script_plan(&p_out, None, "p"),
        ],
        packages: vec![package(P, "p", "/w/p"), package(Z, "z-sys", "/w/z-sys")],
        lock: None,
        warnings: Vec::new(),
        target_dir: "/w/target".to_owned(),
        build_dir: "/w/target".to_owned(),
        finished: String::new(),
        link_dirs: Vec::new(),
        sandboxed_tests: false,
    }
}

#[expect(clippy::disallowed_types, reason = "a test job's slots, as the job keeps them: per job, not global")]
fn slots(units: &Units) -> Vec<OnceLock<script::Applied>> {
    units.graph.units.iter().map(|_| OnceLock::new()).collect()
}

fn value(code: i32, stdout: &str, stderr: &str) -> Value {
    Value { code: Some(code), signal: None, stdout: stdout.as_bytes().to_vec(), stderr: stderr.as_bytes().to_vec(), files: Vec::new() }
}

#[expect(clippy::disallowed_types, reason = "a test job's slots, as the job keeps them: per job, not global")]
fn of<'a>(units: &'a Units, scripts: &'a [OnceLock<script::Applied>], tools: &'a std::path::Path) -> script::Of<'a> {
    script::Of {
        units,
        host: HOST,
        tools,
        launcher: std::path::Path::new("/opt/blaze/bin/blaze"),
        program: PathBuf::from("/w/target/debug/build/x/build-script-build"),
        scripts,
    }
}

#[test]
fn a_scripts_directives_are_applied_as_cargo_applies_them() {
    let dir = tempfile::tempdir().unwrap();
    let units = build(dir.path());
    let tools = PathBuf::from("/state/tools");
    let scripts = slots(&units);
    let z = script::script_unit(&of(&units, &scripts, &tools), 1).unwrap();
    assert_eq!(z.package.descr(), "z-sys v0.1.0 (/w/z-sys)");
    assert_eq!(z.package.links.as_deref(), Some("z"));
    let out_dir = z.out_dir.to_string_lossy().into_owned();
    let printed = format!(
        "cargo:rustc-cfg=has_z\ncargo::rustc-env=Z_VERSION=1.3\ncargo:rustc-link-lib=static=z\n\
         cargo:rustc-link-search=native={out_dir}/lib\ncargo:warning=z is old\ncargo:include={out_dir}/include\n"
    );
    let env = script::inherited(&[], &units.plans[1]);
    let judged = script::judge(&z, &env, &value(0, &printed, ""), Some(&[]), None, false);
    let output = judged.output.expect("the run succeeded");
    assert_eq!(output.cfgs, ["has_z"]);
    assert_eq!(output.env, [("Z_VERSION".to_owned(), "1.3".to_owned())]);
    assert_eq!(output.library_links, ["static=z"]);
    assert_eq!(output.library_paths.len(), 1);
    assert_eq!(output.library_paths[0].path(), std::path::Path::new(&format!("native={out_dir}/lib")));
    assert_eq!(output.metadata, [("include".to_owned(), format!("{out_dir}/include"))]);
    // cargo's warning line, of the package by name and version.
    assert_eq!(judged.diagnostics.len(), 1);
    assert_eq!(judged.diagnostics[0].level, Level::Warning);
    assert_eq!(judged.diagnostics[0].rendered, "warning: z-sys@0.1.0: z is old\n");
    let executed = script::executed(Z, &output, &out_dir);
    assert_eq!(executed.linked_libs, ["static=z"]);
    assert_eq!(executed.cfgs, ["has_z"]);
    assert_eq!(executed.env, [("Z_VERSION".to_owned(), "1.3".to_owned())]);
    assert_eq!(codegen::link_lib(&output.library_links[0]).unwrap().name, "z");

    // The dependent's script is told `DEP_Z_INCLUDE`.
    let applied = script::Applied { package_id: Z.to_owned(), links: Some("z".to_owned()), output, out_dir: out_dir.clone() };
    assert!(scripts[1].set(applied).is_ok());
    let p = script::script_unit(&of(&units, &scripts, &tools), 3).unwrap();
    assert_eq!(p.dependencies.len(), 1);
    let env = blaze_build_script::env(&p, &[]).unwrap();
    assert_eq!(env.get("DEP_Z_INCLUDE"), Some(OsStr::new(&format!("{out_dir}/include"))));
    assert_eq!(p.tools.rustc, tools.join(format!("rustc{}", std::env::consts::EXE_SUFFIX)));
}

/// cc prints `cargo:rustc-link-search=native=OUT_DIR` (blake3's `blake3_neon`, tree-sitter's
/// `tree-sitter`): the link looks in `OUT_DIR`, never in a directory named `native=OUT_DIR`,
/// whether it links a dependent's program or its build script.
#[test]
fn a_kinded_link_search_reaches_the_link_as_its_directory() {
    let search = ["native=/o/lib", "crate=/c", "dependency=/d", "/plain", "all=/a", "framework=/f"].map(PathBuf::from);
    let dirs = codegen::search_dirs(&search);
    assert_eq!(dirs, ["/o/lib", "/plain", "/a", "/f"].map(PathBuf::from));
}

#[test]
fn a_directive_the_sandbox_refuses_fails_the_build() {
    let dir = tempfile::tempdir().unwrap();
    let units = build(dir.path());
    let scripts = slots(&units);
    let tools = PathBuf::from("/state/tools");
    let z = script::script_unit(&of(&units, &scripts, &tools), 1).unwrap();
    let printed = "cargo:rustc-link-search=native=/home/me/lib\n";
    let judged = script::judge(&z, &[], &value(0, printed, ""), Some(&[]), None, false);
    assert!(judged.output.is_none() && judged.diagnostics.is_empty());
    let report = judged.failure.expect("the sandbox refused it");
    assert_eq!(report.rule.as_deref(), Some("sandbox:link-dir"));
    assert_eq!(report.subject(), Some("z-sys"));
    assert!(report.flat().contains("`cargo:rustc-link-search=native=/home/me/lib`"), "{}", report.flat());
    // Outside the sandbox nothing is checked.
    let judged = script::judge(&z, &[], &value(0, printed, ""), None, None, false);
    assert!(judged.output.is_some());
}

#[test]
fn a_failed_script_says_what_cargo_says() {
    let dir = tempfile::tempdir().unwrap();
    let units = build(dir.path());
    let scripts = slots(&units);
    let tools = PathBuf::from("/state/tools");
    let z = script::script_unit(&of(&units, &scripts, &tools), 1).unwrap();
    let judged = script::judge(&z, &[], &value(101, "cargo:warning=no zlib\n", "thread 'main' panicked\n"), Some(&[]), None, false);
    assert!(judged.output.is_none());
    // Its warning is cargo's line; its failure is a report of the script, how it ended and the
    // end of what it wrote (its directives left out).
    let [warning] = judged.diagnostics.as_slice() else { panic!("{:?}", judged.diagnostics) };
    assert_eq!(warning.rendered, "warning: z-sys@0.1.0: no zlib\n");
    let report = judged.failure.expect("the run failed");
    assert_eq!(report.rule.as_deref(), Some("build-script:failed"));
    assert_eq!(report.subject(), Some("z-sys"));
    assert_eq!(report.flat(), "the build script failed\n  it ended with exit status: 101\n  thread 'main' panicked");

    // An error it logged fails it too, said as the headline.
    let judged = script::judge(&z, &[], &value(0, "cargo::error=no `zlib`\n", ""), Some(&[]), None, true);
    assert!(judged.diagnostics.is_empty(), "{:?}", judged.diagnostics);
    let report = judged.failure.expect("it logged an error");
    assert_eq!(report.rule.as_deref(), Some("build-script:error"));
    assert_eq!(report.headline, blaze_api::Text::new().words("no ").code("zlib"));

    // A script that says its own report (`cargo:blaze-report=`) is shown by it.
    let printed = "cargo:blaze-report=headline=z-sys can't find zlib\ncargo:blaze-report=rule=llvm:pin\n\
                   cargo:blaze-report=fact.found=`ZLIB_DIR` is not set\ncargo:blaze-report=fix=nix develop\n\
                   cargo::error=no zlib: set ZLIB_DIR\n";
    let report = script::judge(&z, &[], &value(0, printed, ""), Some(&[]), None, false).failure.expect("it logged an error");
    assert_eq!(report.flat(), "z-sys can't find zlib\n  `ZLIB_DIR` is not set\n  fix: nix develop");
    assert_eq!(report.rule.as_deref(), Some("llvm:pin"));
}

#[test]
fn a_script_sees_the_jobs_environment_with_cargos_over_it_and_none_of_it_is_kept() {
    let job = vec![
        ("PATH".to_owned(), "/usr/bin".to_owned()),
        ("SECRET_TOKEN".to_owned(), "hunter2".to_owned()),
        ("CARGO_PKG_NAME".to_owned(), "wrong".to_owned()),
        ("BLAZE_SCRIPT_CACHE".to_owned(), "check".to_owned()),
    ];
    let dir = tempfile::tempdir().unwrap();
    let units = build(dir.path());
    let env = script::inherited(&job, &units.plans[1]);
    let get = |name: &str| env.iter().find(|(it, _)| it == name).map(|(_, value)| value.clone());
    assert_eq!(get("BLAZE_SCRIPT_CACHE"), None, "how blaze checks its store is the job's, never a script's");
    assert_eq!(get("CARGO_PKG_NAME"), Some(OsString::from("z-sys")));
    assert_eq!(get("SECRET_TOKEN"), Some(OsString::from("hunter2")));
    assert_eq!(get("PATH"), Some(OsString::from("/usr/bin")));

    let scripts = slots(&units);
    let tools = PathBuf::from("/state/tools");
    let z = script::script_unit(&of(&units, &scripts, &tools), 1).unwrap();
    let place = blaze_usercode::script::Place {
        code: ContentId::of("blaze v2 test 1", b"script"),
        read: vec!["/w".into()],
        network: false,
        memory: None,
        mode: blaze_usercode::run::Mode::Normal,
        roots: blaze_usercode::roots::RootMap::default(),
    };
    let one = blaze_usercode::script::prepare(&z, &env, &place).unwrap();
    let other: Vec<(String, String)> =
        job.iter().map(|(name, value)| (name.clone(), value.replace("hunter2", "swordfish"))).collect();
    let two = blaze_usercode::script::prepare(&z, &script::inherited(&other, &units.plans[1]), &place).unwrap();
    // A value passed through is in no key: what the script reads of it is traced.
    assert_eq!(one.spec.key, two.spec.key);
    assert!(!one.given.iter().any(|it| it == "SECRET_TOKEN"));
    // Nor in anything said of the run.
    let judged = script::judge(&z, &env, &value(1, "", ""), Some(&[]), None, false);
    let said = blaze_protocol::cbor::to_vec(&(&judged.diagnostics, &judged.failure)).unwrap();
    assert!(!said.windows(7).any(|it| it == b"hunter2"));
}

fn artifact_of(unit: &Unit, plan: &UnitPlan) -> Artifact {
    let package = package(&unit.pkg_id, "p", "/w/p");
    let main = artifact::main(unit, plan).unwrap();
    artifact::artifact(unit, plan, &package, &main, ContentId::of("blaze v2 test 1", b"x"), false, artifact::Runs::default()).unwrap()
}

#[test]
fn a_libs_artifact_is_cargos() {
    let lib = unit(P, target("p", "lib", "/w/p/src/lib.rs"), Mode::Build, &[]);
    let plan = plan(
        "-0123",
        &[
            "/w/target/debug/deps/libp-0123.rlib",
            "/w/target/debug/deps/libp-0123.rmeta",
            "/w/target/debug/deps/p-0123.d",
            "/w/target/debug/libp.rlib",
            "/w/target/debug/libp.d",
        ],
    );
    let it = artifact_of(&lib, &plan);
    assert_eq!(it.kind, ArtifactKind::Lib);
    assert_eq!(it.filenames, ["/w/target/debug/libp.rlib", "/w/target/debug/deps/libp-0123.rmeta"]);
    assert_eq!(it.executable, None);
    assert_eq!(it.path, "/w/target/debug/libp.rlib");
    assert_eq!(
        it.profile,
        UnitProfile { opt_level: "0".into(), debuginfo: 2, debug_assertions: true, overflow_checks: true, test: false }
    );
    assert_eq!(it.target, unit_target(&lib).unwrap());
    assert_eq!(it.features, ["default"]);
    assert_eq!(it.manifest_path, "/w/p/Cargo.toml");
}

#[test]
fn a_bins_artifact_is_cargos() {
    let bin = unit(P, target("p", "bin", "/w/p/src/main.rs"), Mode::Build, &[]);
    let plan = plan(
        "-0123",
        &["/w/target/debug/deps/p-0123", "/w/target/debug/deps/p-0123.d", "/w/target/debug/p", "/w/target/debug/p.d"],
    );
    let it = artifact_of(&bin, &plan);
    assert_eq!(it.kind, ArtifactKind::Bin);
    assert_eq!(it.filenames, ["/w/target/debug/p"]);
    assert_eq!(it.executable.as_deref(), Some("/w/target/debug/p"));
    assert!(!it.profile.test);
}

#[test]
fn a_tests_artifact_is_cargos() {
    let test = unit(P, target("p", "lib", "/w/p/src/lib.rs"), Mode::Test, &[]);
    let plan = plan("-4567", &["/w/target/debug/deps/p-4567", "/w/target/debug/deps/p-4567.d"]);
    let it = artifact_of(&test, &plan);
    assert_eq!(it.kind, ArtifactKind::Test);
    assert_eq!(it.filenames, ["/w/target/debug/deps/p-4567"]);
    assert_eq!(it.executable.as_deref(), Some("/w/target/debug/deps/p-4567"));
    assert!(it.profile.test);
}

/// A test built in its unit's own directory (cargo's new layout) is run from the profile's
/// `deps`, where its name keeps the hash: `current_exe()/../p` is the package's binary.
#[test]
fn a_test_runs_from_deps_beside_the_packages_binaries() {
    let test = unit(P, target("it", "test", "/w/p/tests/it.rs"), Mode::Test, &[]);
    let out = "/w/target/debug/build/p/4567/out";
    let plan = plan(
        "-4567",
        &[
            format!("{out}/it-4567").as_str(),
            "/w/target/debug/deps/it-4567",
            format!("{out}/it-4567.dwp").as_str(),
            format!("{out}/it-4567.d").as_str(),
        ],
    );
    let it = artifact_of(&test, &plan);
    assert_eq!(it.filenames, ["/w/target/debug/deps/it-4567"]);
    assert_eq!(it.executable.as_deref(), Some("/w/target/debug/deps/it-4567"));
    assert_eq!(artifact::made(&plan), [format!("{out}/it-4567")]);
    assert_eq!(artifact::uplifted(&format!("{out}/it-4567"), &plan), Some("/w/target/debug/deps/it-4567"));
}

#[test]
fn a_build_scripts_artifact_is_cargos() {
    let script = unit(P, target("build-script-build", "custom-build", "/w/p/build.rs"), Mode::Build, &[]);
    let plan = plan(
        "-89ab",
        &[
            "/w/target/debug/build/p-89ab/build_script_build-89ab",
            "/w/target/debug/build/p-89ab/build_script_build-89ab.d",
            "/w/target/debug/build/p-89ab/build-script-build",
        ],
    );
    let it = artifact_of(&script, &plan);
    assert_eq!(it.kind, ArtifactKind::CustomBuild);
    assert_eq!(it.filenames, ["/w/target/debug/build/p-89ab/build-script-build"]);
    assert_eq!(it.executable, None);
}

#[test]
fn a_program_runs_with_cargos_variables_over_the_jobs() {
    let bin = unit(P, target("p", "bin", "/w/p/src/main.rs"), Mode::Build, &[]);
    let mut plan = plan("-0123", &["/w/target/debug/deps/p-0123", "/w/target/debug/p"]);
    plan.env = vec![
        ("CARGO_MANIFEST_DIR".to_owned(), "/w/p".to_owned()),
        ("CARGO_PKG_NAME".to_owned(), "p".to_owned()),
        ("CARGO_PRIMARY_PACKAGE".to_owned(), "1".to_owned()),
        ("CARGO_CRATE_NAME".to_owned(), "p".to_owned()),
        ("OUT_DIR".to_owned(), "/w/target/debug/build/p-0002/out".to_owned()),
    ];
    let script = [("GIT_HASH".to_owned(), "abc".to_owned())];
    let native = ["/w/target/debug/build/z-sys-0001/out/lib".to_owned(), "/usr/lib/x".to_owned()];
    let env = artifact::run_env(&plan, "/opt/blaze/bin/blaze", &script, &native, "linux");
    let get = |name: &str| env.iter().find(|(it, _)| it == name).map(|(_, value)| value.as_str());
    assert_eq!(get("CARGO"), Some("/opt/blaze/bin/blaze"));
    assert_eq!(get("CARGO_MANIFEST_DIR"), Some("/w/p"));
    assert_eq!(get("OUT_DIR"), Some("/w/target/debug/build/p-0002/out"));
    assert_eq!(get("GIT_HASH"), Some("abc"));
    // What cargo sets for rustc only is not set for the program.
    assert_eq!(get("CARGO_PRIMARY_PACKAGE"), None);
    assert_eq!(get("CARGO_CRATE_NAME"), None);
    assert_eq!(
        get("LD_LIBRARY_PATH"),
        Some("/w/target/debug/build/z-sys-0001/out/lib:/w/target/debug/deps:/w/target/debug")
    );
    assert_eq!(artifact::dylib_var("macos"), "DYLD_FALLBACK_LIBRARY_PATH");

    let package = package(P, "p", "/w/p");
    let runs = artifact::Runs { runner: vec!["qemu-aarch64".to_owned()], env: env.clone() };
    let it = artifact::artifact(&bin, &plan, &package, "/w/target/debug/p", ContentId::of("blaze v2 test 1", b"x"), false, runs).unwrap();
    assert_eq!(it.runner, ["qemu-aarch64"]);
    assert!(it.harness);
    let job = [
        ("LD_LIBRARY_PATH".to_owned(), "/opt/lib".to_owned()),
        ("CARGO_PKG_NAME".to_owned(), "wrong".to_owned()),
        ("SECRET_TOKEN".to_owned(), "hunter2".to_owned()),
    ];
    let process = run::process_env(&job, &it, "linux");
    assert!(process.windows(2).all(|it| it[0].0 < it[1].0), "{process:?}");
    let get = |name: &str| process.iter().find(|(it, _)| it == name).map(|(_, value)| value.clone());
    assert_eq!(get("CARGO_PKG_NAME"), Some(OsString::from("p")));
    assert_eq!(get("SECRET_TOKEN"), Some(OsString::from("hunter2")));
    assert_eq!(
        get("LD_LIBRARY_PATH"),
        Some(OsString::from("/w/target/debug/build/z-sys-0001/out/lib:/w/target/debug/deps:/w/target/debug:/opt/lib"))
    );
    // A library's artifact runs nothing, and says no environment.
    let lib = unit(P, target("p", "lib", "/w/p/src/lib.rs"), Mode::Build, &[]);
    let lib_plan = self::plan("-0123", &["/w/target/debug/deps/libp-0123.rlib"]);
    let runs = artifact::Runs { runner: Vec::new(), env };
    let it = artifact::artifact(&lib, &lib_plan, &package, "/w/target/debug/deps/libp-0123.rlib", ContentId::of("blaze v2 test 1", b"x"), false, runs).unwrap();
    assert!(it.env.is_empty());
}

fn bin_artifact(name: &str) -> Artifact {
    let bin = unit(P, target(name, "bin", "/w/p/src/main.rs"), Mode::Build, &[]);
    let path = format!("/w/target/debug/{name}");
    let plan = plan("-0123", &[&format!("/w/target/debug/deps/{name}-0123"), &path]);
    artifact_of(&bin, &plan)
}

#[test]
fn run_picks_the_binary_cargo_picks() {
    let lib = {
        let lib = unit(P, target("p", "lib", "/w/p/src/lib.rs"), Mode::Build, &[]);
        artifact_of(&lib, &plan("-0123", &["/w/target/debug/deps/libp-0123.rlib"]))
    };
    let one = [lib.clone(), bin_artifact("a")];
    assert_eq!(run::choose(&one, None, None).unwrap().target.name, "a");
    let two = [lib, bin_artifact("a"), bin_artifact("b")];
    assert_eq!(run::choose(&two, Some("b"), None).unwrap().target.name, "b");
    assert_eq!(run::choose(&two, None, Some("b")).unwrap().target.name, "b");
    assert_eq!(
        run::choose(&two, None, None).unwrap_err(),
        "`cargo run` could not determine which binary to run. Use the `--bin` option to specify a binary, or the \
         `default-run` manifest key.\navailable binaries: a, b"
    );
    assert_eq!(run::choose(&two[..1], None, None).unwrap_err(), "a bin target must be available for `cargo run`");
    assert!(run::choose(&two, Some("c"), None).unwrap_err().starts_with("no bin target named `c`"));
}

#[test]
fn a_dependencys_lints_are_capped_and_rustc_closing_lines_hidden() {
    let lib = unit(P, target("p", "lib", "/w/p/src/lib.rs"), Mode::Build, &[]);
    let mut plan = plan("-0123", &[]);
    plan.lints = vec!["--warn=unused_qualifications".to_owned()];
    plan.rustflags = vec!["-D".to_owned(), "warnings".to_owned(), "-Ctarget-cpu=native".to_owned(), "-Aunused".to_owned()];
    plan.check_cfg = vec!["cfg(foo)".to_owned()];
    assert_eq!(
        compile::lint_flags(&lib, &plan, true, false),
        ["--warn=unused_qualifications", "-D", "warnings", "-Aunused", "--check-cfg", "cfg(foo)"]
    );
    assert_eq!(compile::lint_flags(&lib, &plan, false, false).last().map(String::as_str), Some("allow"));
    assert_eq!(compile::lint_flags(&lib, &plan, false, true).last().map(String::as_str), Some("warn"));
    assert_eq!(compile::lint_flags(&lib, &plan, true, true), compile::lint_flags(&lib, &plan, true, false));
    assert!(compile::local(Some("(/w/p)")));
    assert!(!compile::local(Some("(https://github.com/a/b#0123)")));
    assert!(!compile::local(None));
    assert!(compile::cargo_hides("aborting due to 2 previous errors; 1 warning emitted"));
    assert!(compile::cargo_hides("3 warnings emitted"));
    assert!(!compile::cargo_hides("For more information about this error, try `rustc --explain E0308`."));
}

#[test]
fn only_what_programs_reach_is_built_of_the_crates_cargo_caps_the_lints_of() {
    let registry = Some("(registry+https://github.com/rust-lang/crates.io-index)");
    let git = Some("(https://github.com/a/b#0123)");
    // The standard library, a registry and a git dependency: only what programs reach.
    assert!(compile::capped(true, false, Some("(/toolchain/lib/rustlib/src/rust/library/std)")));
    assert!(compile::capped(false, false, registry));
    assert!(compile::capped(false, false, git));
    assert!(compile::capped(false, false, None));
    // A workspace member and a path package outside the workspace: checked and compiled whole.
    assert!(!compile::capped(false, true, Some("(/w/p)")));
    assert!(!compile::capped(false, true, None));
    assert!(!compile::capped(false, false, Some("(/elsewhere/q)")));
}

#[test]
fn a_link_library_is_read_as_rustc_reads_l() {
    use blaze_codegen::NativeLibKind;
    let lib = codegen::link_lib("static:+whole-archive,-bundle=foo:bar").unwrap();
    assert_eq!(lib.name, "bar");
    assert_eq!(lib.kind, NativeLibKind::Static { whole: true, bundle: false });
    assert_eq!(codegen::link_lib("ssl").unwrap().kind, NativeLibKind::Dylib);
    assert_eq!(codegen::link_lib("framework=Security").unwrap().kind, NativeLibKind::Framework);
    assert!(codegen::link_lib("weird=x").is_err());
    assert_eq!(codegen::link_arg("-pthread").unwrap().name, "pthread");
    assert!(codegen::link_arg("-Wl,--wrap=malloc").is_err());
}

#[test]
fn a_crate_record_is_an_archive_of_one_member() {
    let lib = unit(P, target("p", "lib", "/w/p/src/lib.rs"), Mode::Build, &[]);
    let bytes = compile::crate_record(&lib, Some(ContentId::of("blaze v2 test 1", b"id")), &[]);
    assert!(bytes.starts_with(b"!<arch>\nlib.rmeta/"));
    assert_eq!(bytes.len() % 2, 0);
}

#[test]
fn the_graph_is_cargos_build_map_in_columns() {
    let dir = tempfile::tempdir().unwrap();
    let mut units = build(dir.path());
    // 4: z-sys's library, built with its script's run; 5: p's binary, with its run and z-sys.
    let mut lib = unit(Z, target("z_sys", "lib", "/w/z-sys/src/lib.rs"), Mode::Build, &[1]);
    lib.target.crate_types = vec!["lib".to_owned()];
    units.graph.units.push(lib);
    units.graph.units.push(unit(P, target("p", "bin", "/w/p/src/main.rs"), Mode::Build, &[4, 3]));
    units.plans.push(plan("-0004", &[]));
    units.plans.push(plan("-0005", &[]));
    let graph = graph::Graph::new(&units).unwrap();
    assert_eq!(graph.deps(5), [3, 4], "sorted by package id");
    assert_eq!(graph.closure(3), [3, 1, 0, 2]);
    assert_eq!(graph.closure(5), [5, 4, 3, 1, 0, 2]);
    // p's own run, then z-sys's through its linked library; the scripts' compiles are the host's.
    assert_eq!(graph.links(5), [3, 1]);
    assert_eq!(graph.links(3), [] as [u32; 0]);
    assert_eq!(graph.linked_in(5), [5, 4]);
    assert!(graph.build_time(0) && graph.build_time(2));
    assert!(!graph.build_time(4) && !graph.build_time(5));
    assert!(graph.package_has_lib(4) && !graph.package_has_lib(5));
    let all = &units.graph.units;
    let kinds = ["custom-build".to_owned()];
    assert_eq!(graph.find(all, Z, "build-script-build", &kinds, Mode::RunCustomBuild), Some(1));
    assert_eq!(graph.find(all, P, "build-script-build", &kinds, Mode::RunCustomBuild), Some(3));
    assert_eq!(graph.find(all, P, "build-script-build", &kinds, Mode::Test), None);
}

#[test]
fn a_relink_is_a_merge_of_sorted_columns() {
    let (mut removed, mut added) = (Vec::new(), Vec::new());
    codegen::diff(&[1, 3, 5, 7], &[2, 3, 7, 8], &mut removed, &mut added);
    assert_eq!(removed, [1, 5]);
    assert_eq!(added, [2, 8]);
    let old = vec!["a".to_owned(), "c".to_owned(), "d".to_owned()];
    let mut new = vec!["d", "b", "a", "b"];
    let (kept, gone) = codegen::renamed(old, &mut new);
    assert_eq!(kept, ["a", "b", "d"]);
    assert_eq!(gone, ["c"]);
}

#[test]
fn a_records_header_is_checked_in_place() {
    use blaze_cas::Key;
    let one = Key(ContentId::of("blaze v2 test 1", b"one"));
    let two = Key(ContentId::of("blaze v2 test 1", b"two"));
    let lib = ContentId([7; 32]);
    let mut bytes = Vec::new();
    let counts = blaze_codegen::driver::ProfileCounts { used: 5, mismatched: 1, applied: 3 };
    let profiles = [(ContentId([3; 32]), counts)];
    compile::header::write(&mut bytes, &[one, two], &[("/usr/lib/libz.so".to_owned(), lib)], &profiles);
    let body = bytes.len();
    bytes.extend_from_slice(b"cbor");
    let same = |path: &std::path::Path| (path == std::path::Path::new("/usr/lib/libz.so")).then_some([7; 32]);
    let changed = |_: &std::path::Path| Some([8; 32]);
    assert_eq!(compile::header::check(&bytes, |_| true, &mut { same }), Some(body));
    assert_eq!(compile::header::check(&bytes, |key| *key != two, &mut { same }), None, "an output gone");
    assert_eq!(compile::header::check(&bytes, |_| true, &mut { changed }), None, "a library changed");
    assert_eq!(compile::header::check(&bytes[..body - 3], |_| true, &mut { same }), None, "cut short");
    assert_eq!(compile::header::check(b"nope", |_| true, &mut { same }), None);
    assert_eq!(compile::header::profiles(&bytes).as_deref(), Some(&profiles[..]));
    assert_eq!(compile::header::profiles(&bytes[..body - 3]), None, "cut short");
}

/// The job-wide check that a profile applied to something (`codegen::judge`, what
/// `codegen::unapplied` calls): each unit's counts are this job's, else its record's, summed
/// over every unit, and a job that failed is not judged.
#[test]
fn a_profile_is_judged_over_every_unit_counted_or_recorded() {
    use blaze_codegen::driver::ProfileCounts;
    use codegen::{Accounted, Recorded};
    let profiled = ContentId([1; 32]);
    let by_blaze = blaze_lints::profile::Names { records: 10, blaze: 10, rustc: 0, foreign: 0 };
    let profiles = [codegen::Named { id: profiled, path: "a.profdata", names: Ok(by_blaze) }];
    let ran = [(profiled, ProfileCounts { used: 10, mismatched: 0, applied: 4 })];
    let never_ran = [(profiled, ProfileCounts { used: 6, mismatched: 0, applied: 0 })];
    let replayed = |counts: &[(ContentId, ProfileCounts)]| -> Result<Recorded, blaze_api::ApiError> { Ok(Recorded::Read(counts.to_vec())) };
    // `bin a` (unit 0) replayed from a record that applied, `bin b` (unit 1) built with
    // nothing applied: one applied anywhere passes.
    let units = [Accounted { unit: 0, counted: None }, Accounted { unit: 1, counted: Some(&never_ran[..]) }];
    let judged = codegen::judge(true, units, |unit| {
        assert_eq!(unit, 0, "only the unit nothing counted this job is read from its record");
        replayed(&ran)
    }, &profiles);
    assert_eq!(judged.map_err(|it| it.what), Ok(Vec::new()));
    // Both replayed, nothing applied: refused, naming the path, the ID and the cause.
    let units = [Accounted { unit: 0, counted: None }, Accounted { unit: 1, counted: None }];
    let refused = codegen::judge(true, units, |_| replayed(&never_ran), &profiles).unwrap_err().what;
    assert!(refused.contains("`-C profile-use=a.profdata`") && refused.contains(&format!("{profiled:x}")), "{refused}");
    assert!(refused.contains("none of the 12 functions") && refused.contains("key version"), "{refused}");
    // A failed job is not judged: nothing is read, nothing refused.
    let units = [Accounted { unit: 0, counted: Some(&never_ran[..]) }, Accounted { unit: 1, counted: None }];
    let judged = codegen::judge(false, units, |_| panic!("a failed job reads no record"), &profiles);
    assert_eq!(judged.map_err(|it| it.what), Ok(Vec::new()));
    // In a job that succeeded, a unit with neither counts nor a record is an error naming it.
    let units = [Accounted { unit: 1, counted: None }];
    let missing = |_| Ok(Recorded::Missing("unit 1 (`b`): the store has no record under its key 0a0b".to_owned()));
    let refused = codegen::judge(true, units, missing, &profiles).unwrap_err().what;
    assert!(refused.starts_with("unit 1 (`b`): the store has no record under its key 0a0b"), "{refused}");
}

/// A profile that applied to nothing refuses the job only when blaze made it under other keys
/// (rule 3); one rustc or clang made, or one edited, is a warning and the build goes on, as
/// under rustc (rule 4).
#[test]
fn only_a_profile_blaze_made_is_refused_for_applying_to_nothing() {
    use blaze_codegen::driver::ProfileCounts;
    use blaze_lints::profile::Names;
    use codegen::{Accounted, Named};
    let id = ContentId([1; 32]);
    let judge = |names: Result<Names, &str>, counts: ProfileCounts| {
        let profiles = [Named { id, path: "a.profdata", names }];
        let counted = [(id, counts)];
        let units = [Accounted { unit: 0, counted: Some(&counted[..]) }];
        codegen::judge(true, units, |_| panic!("every unit is counted"), &profiles).map_err(|it| it.what)
    };
    let nothing = ProfileCounts { used: 6, mismatched: 0, applied: 0 };
    let by_rustc = Names { records: 4, blaze: 0, rustc: 3, foreign: 1 };
    let warned = judge(Ok(by_rustc), nothing).unwrap();
    assert_eq!(warned.len(), 1, "{warned:?}");
    assert!(warned[0].contains("`-C profile-use=a.profdata`") && warned[0].contains("3 are symbols rustc mangled"), "{warned:?}");
    assert!(warned[0].contains("blaze can't apply profiles rustc made; make one with blaze's -Cprofile-generate"), "{warned:?}");
    let by_blaze = Names { records: 4, blaze: 4, rustc: 0, foreign: 0 };
    let refused = judge(Ok(by_blaze), nothing).unwrap_err();
    assert!(refused.contains("a.profdata") && refused.contains("key version"), "{refused}");
    // Any record found edited says the keys agree: the program changed, a warning.
    let edited = ProfileCounts { used: 6, mismatched: 1, applied: 0 };
    let warned = judge(Ok(by_blaze), edited).unwrap();
    assert!(warned.len() == 1 && warned[0].contains("edited since it was profiled"), "{warned:?}");
    // Names that don't read say nothing of who made it: a warning naming why.
    let warned = judge(Err("is not an indexed profile"), nothing).unwrap();
    assert!(warned.len() == 1 && warned[0].contains("is not an indexed profile"), "{warned:?}");
    // Applied somewhere: nothing said.
    assert_eq!(judge(Ok(by_rustc), ProfileCounts { used: 6, mismatched: 0, applied: 1 }), Ok(Vec::new()));
}

fn selection<'a>(filter: &'a [String], args: &'a [String]) -> crate::serve::work::Selection<'a> {
    crate::serve::work::Selection { filter, args, all: true, ub: None }
}

fn strings(of: &[&str]) -> Vec<String> {
    of.iter().map(|it| (*it).to_owned()).collect()
}

/// The arguments after `--` select as libtest's `parse_opts` reads them; one it doesn't take
/// is refused, never dropped.
#[test]
fn libtests_selection_is_read_as_libtest_reads_it() {
    use blaze_usercode::test::Ignored;
    let named = strings(&["parse"]);
    let args = strings(&["lex", "--exact", "--skip", "slow", "--skip=huge", "--include-ignored"]);
    let filter = test::filter(&selection(&named, &args)).expect("libtest's selection");
    assert_eq!(filter.names, ["parse", "lex"]);
    assert!(filter.exact);
    assert_eq!(filter.skip, ["slow", "huge"]);
    assert_eq!(filter.ignored, Ignored::Include);
    let only = test::filter(&selection(&[], &strings(&["--ignored"]))).expect("--ignored");
    assert_eq!(only.ignored, Ignored::Only);
    assert_eq!(test::filter(&selection(&[], &[])).expect("none").ignored, Ignored::Skip);
    let both = test::filter(&selection(&[], &strings(&["--ignored", "--include-ignored"]))).unwrap_err();
    assert_eq!(both.what, "the options --include-ignored and --ignored are mutually exclusive");
    assert_eq!(test::filter(&selection(&[], &strings(&["--skip"]))).unwrap_err().what, "argument to option 'skip' missing");
    let other = test::filter(&selection(&[], &strings(&["--nocapture"]))).unwrap_err();
    assert!(other.what.starts_with("`--nocapture` after `--`"), "{}", other.what);
}

/// A test the evaluator ran is judged as libtest judges it with its `#[should_panic]`;
/// undefined behavior and what the evaluator can't run fail whatever the test expects.
#[test]
fn an_evaluated_test_is_judged_as_libtest_judges_it() {
    use blaze_api::TestOutcome::{Failed, Passed};
    use blaze_usercode::test::ShouldPanic::{No, WithMessage, Yes};
    use test::{Ran, judged};
    let expected = WithMessage("overflow".to_owned());
    assert_eq!(judged(Ran::Returned, &No), (Passed, String::new()));
    assert_eq!(judged(Ran::Returned, &Yes), (Failed, "note: test did not panic as expected\n".to_owned()));
    assert_eq!(judged(Ran::Panicked("boom"), &No), (Failed, "boom\n".to_owned()));
    assert_eq!(judged(Ran::Panicked("boom"), &Yes), (Passed, String::new()));
    assert_eq!(judged(Ran::Panicked("attempt to add with overflow"), &expected), (Passed, String::new()));
    assert_eq!(
        judged(Ran::Panicked("boom"), &expected),
        (
            Failed,
            "note: panic did not contain expected string\n      panic message: \"boom\"\n expected substring: \"overflow\"\n"
                .to_owned()
        )
    );
    let ub = Ran::Ub { report: "error: Undefined Behavior: out-of-bounds read", seed: 7 };
    for expects in [&No, &Yes, &expected] {
        assert_eq!(judged(ub, expects), (Failed, "error: Undefined Behavior: out-of-bounds read\nseed: 7\n".to_owned()));
        let (outcome, output) = judged(Ran::Unsupported("inline assembly"), expects);
        assert_eq!(outcome, Failed);
        assert!(output.ends_with("inline assembly\n"), "{output}");
        let stopped = Ran::Stopped { message: "stack overflow", seed: 3 };
        assert_eq!(judged(stopped, expects), (Failed, "blaze's MIR evaluator stopped: stack overflow\nseed: 3\n".to_owned()));
    }
}

/// What `blaze test --ub` leaves out is the checker's default; what it gives is taken.
#[test]
fn ub_settings_are_the_checkers_where_none_is_given() {
    let default = hir_ty_ub::Config::default();
    let none = test::ub_config(blaze_api::Ub { seed: None, schedules: None });
    assert_eq!((none.seed, none.schedules, none.leaks), (default.seed, default.schedules, default.leaks));
    let given = test::ub_config(blaze_api::Ub { seed: Some(42), schedules: Some(8) });
    assert_eq!((given.seed, given.schedules), (42, 8));
}

/// A unit's identity is final once every run of its closure is applied: each run applied
/// readies the units it was the last run left for, once, and units with no run in their
/// closure are ready from the start.
#[test]
fn a_unit_waits_on_every_run_of_its_closure() {
    let dir = tempfile::tempdir().unwrap();
    let mut units = build(dir.path());
    let mut lib = unit(Z, target("z_sys", "lib", "/w/z-sys/src/lib.rs"), Mode::Build, &[1]);
    lib.target.crate_types = vec!["lib".to_owned()];
    units.graph.units.push(lib);
    units.graph.units.push(unit(P, target("p", "bin", "/w/p/src/main.rs"), Mode::Build, &[4, 3]));
    units.plans.push(plan("-0004", &[]));
    units.plans.push(plan("-0005", &[]));
    let graph = graph::Graph::new(&units).unwrap();
    assert_eq!(graph.dependents(1), [3, 4]);
    assert_eq!(graph.dependents(4), [5]);
    assert_eq!(graph.dependents(5), [] as [u32; 0]);
    let mut waits = graph::Waits::new(&graph);
    // The scripts' compiles wait on nothing; z-sys's run waits on nothing either.
    assert!(waits.ready(0) && waits.ready(1) && waits.ready(2));
    assert!(!waits.ready(3) && !waits.ready(4) && !waits.ready(5));
    let mut ready = Vec::new();
    waits.applied(1, &mut ready);
    assert_eq!(ready, [3, 4], "p's run and z-sys's library; p's binary still waits on p's run");
    waits.applied(1, &mut ready);
    assert_eq!(ready, [3, 4], "a run applied again readies nothing");
    ready.clear();
    waits.applied(3, &mut ready);
    assert_eq!(ready, [5]);
    assert!((0..6).all(|at| waits.ready(at)));
}

#[test]
fn a_pack_is_of_the_packages_binary_or_else_its_library() {
    let (app, dep) = ("path+file:///w/app#0.1.0", "path+file:///w/dep#0.1.0");
    let with = |units: Vec<Unit>| Units {
        graph: UnitGraph { version: 1, units, roots: Vec::new() },
        plans: Vec::new(),
        packages: vec![package(app, "app", "/w/app"), package(dep, "dep", "/w/dep")],
        lock: None,
        warnings: Vec::new(),
        target_dir: "/w/target".to_owned(),
        build_dir: "/w/target".to_owned(),
        finished: String::new(),
        link_dirs: Vec::new(),
        sandboxed_tests: false,
    };
    let units = with(vec![
        unit(dep, target("dep", "lib", "/w/dep/src/lib.rs"), Mode::Check, &[]),
        unit(app, target("build-script-build", "custom-build", "/w/app/build.rs"), Mode::Build, &[]),
        unit(app, target("app", "lib", "/w/app/src/lib.rs"), Mode::Check, &[0]),
        unit(app, target("app", "bin", "/w/app/src/main.rs"), Mode::Check, &[2]),
    ]);
    assert_eq!(pack::program_unit(&units, "app").unwrap(), 3);
    assert_eq!(pack::program_unit(&units, "dep").unwrap(), 0);
    let none = pack::program_unit(&units, "other").unwrap_err().what;
    assert!(none.contains("-p other"), "{none}");

    let mut tested = unit(app, target("app", "bin", "/w/app/src/main.rs"), Mode::Check, &[]);
    tested.checked_as_test = true;
    let units = with(vec![tested, unit(app, target("app", "lib", "/w/app/src/lib.rs"), Mode::Check, &[])]);
    assert_eq!(pack::program_unit(&units, "app").unwrap(), 1, "a binary checked as a test is no program");

    let units = with(vec![
        unit(app, target("one", "bin", "/w/app/src/bin/one.rs"), Mode::Check, &[]),
        unit(app, target("two", "bin", "/w/app/src/bin/two.rs"), Mode::Check, &[]),
    ]);
    let refused = pack::program_unit(&units, "app").unwrap_err().what;
    assert!(refused.contains("`one`, `two`"), "{refused}");
}

/// A unit of a package the build doesn't list, or naming a dependency past its units, stops
/// the graph: no edge is dropped and no unit is given a package it isn't of.
#[test]
fn a_graph_with_a_unit_it_cant_place_is_refused() {
    use super::error::UnitError;
    let dir = tempfile::tempdir().unwrap();
    let mut units = build(dir.path());
    units.graph.units.push(unit(P, target("p", "bin", "/w/p/src/main.rs"), Mode::Build, &[9]));
    units.plans.push(plan("-0004", &[]));
    assert_eq!(graph::Graph::new(&units).err(), Some(UnitError::Dependency { unit: 4, dep: 9, count: 5 }));

    let mut units = build(dir.path());
    units.graph.units.push(unit("path+file:///w/q#0.1.0", target("q", "lib", "/w/q/src/lib.rs"), Mode::Build, &[]));
    units.plans.push(plan("-0004", &[]));
    let refused = graph::Graph::new(&units).err();
    assert_eq!(refused, Some(UnitError::Package { unit: 4, pkg_id: "path+file:///w/q#0.1.0".to_owned() }));
}

/// What a build script's run is told is never guessed: an `OUT_DIR` under no `<profile>/build`,
/// a manifest in no directory and a compile with no output are errors naming them.
#[test]
fn a_script_run_missing_what_cargo_tells_it_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let tools = PathBuf::from("/state/tools");

    let mut units = build(dir.path());
    units.plans[1].out_dir = Some("/w/elsewhere/out".to_owned());
    let scripts = slots(&units);
    let refused = script::script_unit(&of(&units, &scripts, &tools), 1).unwrap_err();
    assert!(refused.contains("`/w/elsewhere/out`, is under no `<profile>/build`"), "{refused}");

    let mut units = build(dir.path());
    let z = units.packages.iter_mut().find(|it| it.id == Z).unwrap();
    z.manifest_path = "Cargo.toml".to_owned();
    let refused = script::script_unit(&of(&units, &scripts, &tools), 1).unwrap_err();
    assert!(refused.contains("the manifest of `z-sys`, `Cargo.toml`, is in no directory"), "{refused}");

    let mut units = build(dir.path());
    units.plans[0].outputs.clear();
    let refused = script::named_program(&units, 0).unwrap_err().what;
    assert!(refused.contains("the build script compiled by unit 0 has no output"), "{refused}");
    assert_eq!(
        script::named_program(&build(dir.path()), 0).unwrap(),
        PathBuf::from("/w/target/debug/build/z-sys-0001/build_script_build-0001")
    );
}

/// A test binary relinked against a changed C library runs every test again: the native code
/// is in no code hash a test reaches, so it is in the suite's context.
#[test]
fn a_test_binarys_context_holds_the_native_inputs_it_linked() {
    let context = ContentId([1; 32]);
    assert_eq!(compile::with_natives(context, &[]), context);
    let libz = |n: u8| ("/w/target/debug/build/z-sys-0001/out/libz.a".to_owned(), ContentId([n; 32]));
    let with = compile::with_natives(context, &[libz(2)]);
    assert_ne!(with, context);
    assert_eq!(with, compile::with_natives(context, &[libz(2)]));
    assert_ne!(with, compile::with_natives(context, &[libz(3)]), "an edit of the library");
}

/// A std unit's record key reads its dependencies by what they are: in another workspace's
/// graph, where every unit sits at another index, the key is the same, and std is built once.
#[test]
fn a_units_key_is_the_same_wherever_the_graph_puts_its_dependencies() {
    let core = unit("core", target("core", "lib", "/rustc/0/library/core/src/lib.rs"), Mode::Build, &[]);
    let mut alloc = unit("alloc", target("alloc", "lib", "/rustc/0/library/alloc/src/lib.rs"), Mode::Build, &[0]);
    alloc.is_std = true;
    let app = unit("app", target("app", "bin", "/w/src/main.rs"), Mode::Build, &[]);
    let first = [core.clone(), alloc.clone()];
    let mut moved = alloc.clone();
    moved.dependencies[0].index = 2;
    let second = [app, alloc, core];
    let bytes = |graph: &[Unit], u: &Unit| {
        let keyed = compile::KeyedUnit::among(u, |at| Ok(&graph[at])).unwrap();
        blaze_protocol::cbor::to_vec(&keyed).unwrap()
    };
    assert_eq!(bytes(&first, &first[1]), bytes(&second, &moved));
    let mut other = moved.clone();
    other.dependencies[0].index = 0;
    assert_ne!(bytes(&first, &first[1]), bytes(&second, &other), "a dependency on another unit is another key");
}

/// A build script's compile is one program under every goal, so `blaze check` and `blaze
/// build` key its record alike and the second finds what the first stored; a library checked
/// is not the library built, and keys the goal.
#[test]
fn a_build_scripts_record_is_keyed_alike_under_every_goal() {
    use crate::serve::work::Goal;
    let goals = [Goal::Check, Goal::Build, Goal::Test, Goal::Run];
    let script = unit("p", target("build-script-build", "custom-build", "/w/p/build.rs"), Mode::Build, &[]);
    let keyed: Vec<u8> = goals.iter().map(|goal| compile::keyed_goal(*goal, &script)).collect();
    assert!(keyed.iter().all(|it| *it == keyed[0]), "{keyed:?}");
    let lib = unit("p", target("p", "lib", "/w/p/src/lib.rs"), Mode::Build, &[]);
    assert_ne!(compile::keyed_goal(Goal::Check, &lib), compile::keyed_goal(Goal::Build, &lib));
}

const MACROS: &[u8] = b"extern crate proc_macro;\nuse proc_macro::TokenStream;\n\n\
#[proc_macro]\npub fn shout(input: TokenStream) -> TokenStream { input }\n\n\
#[proc_macro_attribute]\npub fn tag(_: TokenStream, item: TokenStream) -> TokenStream { item }\n";

/// The standard library's `proc_macro`, as much of it as the macros' signatures name.
const PROC_MACRO_LIB: &[u8] = b"pub struct TokenStream;\n";

/// A proc macro `m` on the standard library's `core` and `proc_macro`, lowered over files in
/// memory: the proc macro is unit 2. It is built for the host, as a plan with
/// `Spec::macro_wasm` off has it; every build plans its proc macros as wasm modules, whose
/// roots have the bridge's glue appended (`blaze_lower::harness::wasm_macro`).
fn lowered_proc_macro() -> blaze_lower::Lowered {
    use blaze_units::{
        query::{BuildConfig, Command, Invocation, Mount, Origin},
        schema::{Strip, Stripped},
    };
    let tree = blaze_query::MemTree::default();
    let library = tree.root(&[
        ("core/Cargo.toml", b"[package]\nname = \"core\"\n"),
        ("core/src/lib.rs", b"pub mod marker {}\n"),
        ("proc_macro/Cargo.toml", b"[package]\nname = \"proc_macro\"\n"),
        ("proc_macro/src/lib.rs", PROC_MACRO_LIB),
    ]);
    let view = tree.root(&[
        ("Cargo.toml", b"[package]\nname = \"m\"\n\n[lib]\nproc-macro = true\n"),
        ("src/lib.rs", MACROS),
    ]);
    let (core, proc_macro, m) =
        ("path+file:///rustc/abc/library/core#0.0.0", "path+file:///rustc/abc/library/proc_macro#0.0.0", "path+file:///w#0.1.0");
    let std_dep = |index: usize, name: &str| UnitDep {
        index,
        extern_crate_name: name.to_owned(),
        public: None,
        noprelude: Some(true),
        nounused: None,
        dev: false,
    };
    let made = |pkg: &str, target: Target, is_std: bool, dependencies: Vec<UnitDep>| Unit {
        profile: Profile { incremental: true, strip: Strip::Resolved(Stripped::None), ..Profile::dev() },
        is_std,
        dependencies,
        ..unit(pkg, target, Mode::Build, &[])
    };
    let mut macro_target = target("m", "lib", "/w/src/lib.rs");
    macro_target.kind = vec!["proc-macro".to_owned()];
    macro_target.crate_types = vec!["proc-macro".to_owned()];
    let units = Units {
        graph: UnitGraph {
            version: 1,
            units: vec![
                made(core, target("core", "lib", "/rustc/abc/library/core/src/lib.rs"), true, Vec::new()),
                made(proc_macro, target("proc_macro", "lib", "/rustc/abc/library/proc_macro/src/lib.rs"), true, vec![std_dep(0, "core")]),
                made(m, macro_target, false, vec![std_dep(0, "core"), std_dep(1, "proc_macro")]),
            ],
            roots: vec![2],
        },
        plans: vec![plan("", &[]), plan("", &[]), plan("", &[])],
        packages: vec![
            PackagePlan { is_member: false, ..package(core, "core", "/rustc/abc/library/core") },
            PackagePlan { is_member: false, ..package(proc_macro, "proc_macro", "/rustc/abc/library/proc_macro") },
            package(m, "m", "/w"),
        ],
        lock: None,
        warnings: Vec::new(),
        target_dir: "/w/target".to_owned(),
        build_dir: "/w/target".to_owned(),
        finished: String::new(),
        link_dirs: Vec::new(),
        sandboxed_tests: false,
    };
    let config = BuildConfig {
        invocation: Invocation {
            command: Command::Build,
            args: Vec::new(),
            root: "/w".to_owned(),
            cwd: "/w".to_owned(),
            env: Vec::new(),
            files: Vec::new(),
        },
        mounts: vec![
            Mount { at: "/rustc/abc/library".to_owned(), origin: Origin::Root(library), under: String::new() },
            Mount { at: "/w".to_owned(), origin: Origin::View, under: String::new() },
        ],
        host: HOST.to_owned(),
        library: "/rustc/abc/library".to_owned(),
        std: "/s/std".to_owned(),
        builds: "/s/build".to_owned(),
        sources: "/s/sources".to_owned(),
    };
    blaze_lower::lower(blaze_lower::Lower { units: &units, config: &config, files: &tree, root: view, host_cpu: None }).unwrap()
}

/// A host-built proc macro's lib builds no code: its expanders run its MIR and find its macros
/// from its def map (`blaze_macro_eval::load`), so nothing is appended to its root, which is
/// the file on disk itself, and the def map has each macro.
#[test]
fn a_proc_macros_root_is_its_file_on_disk_and_its_def_map_has_its_macros() {
    let lowered = lowered_proc_macro();
    assert!(lowered.crates().is_proc_macro(2));
    let krate = lowered.krate(2).unwrap();
    let db = lowered.db();
    let root = krate.data(db).root_file_id;
    assert_eq!(db.files().alias_of(root), None, "a proc macro's root is no alias");
    let input = db.file_text(root);
    assert_eq!(input.text(db).as_bytes(), MACROS);
    assert_eq!(input.appended_root_module(db), None);
    let def_map = hir_def::nameres::crate_def_map(db, krate);
    assert!(def_map.is_proc_macro());
    let macros = def_map
        .module(db, def_map.crate_root(db))
        .scope(db)
        .declarations()
        .filter(|def| matches!(def, hir_def::ModuleDefId::FunctionId(it) if def_map.fn_as_proc_macro(*it).is_some()))
        .count();
    assert_eq!(macros, 2, "`shout` and `tag`");
}

/// `cargo check` and `cargo clippy` stop every unit after the front end but a build script's
/// compile, which is linked as under `cargo build` so that it can run.
#[test]
fn a_check_links_its_build_scripts_and_checks_the_rest() {
    use crate::serve::work::Goal;
    let script = unit(P, target("build-script-build", "custom-build", "/w/p/build.rs"), Mode::Build, &[]);
    let lib = unit(P, target("p", "lib", "/w/p/src/lib.rs"), Mode::Check, &[]);
    let bin = unit(P, target("p", "bin", "/w/p/src/main.rs"), Mode::Build, &[]);
    assert!(!super::checks_only(Goal::Check, &script));
    assert!(super::checks_only(Goal::Check, &lib));
    assert!(super::checks_only(Goal::Check, &bin));
    assert!(super::checks_only(Goal::Build, &lib));
    assert!(!super::checks_only(Goal::Build, &script));
    assert!(!super::checks_only(Goal::Build, &bin));
}

/// `blaze check` links a proc macro a crate uses as its wasm32-wasip1 module, as `blaze build`
/// does, so the crates that use it expand its macros; only checked, its `.wasm` would be a
/// crate record. A proc-macro crate checked for itself (a member, `Mode::Check`) is only checked.
#[test]
fn a_check_links_its_proc_macros_as_wasm_modules() {
    use crate::serve::work::Goal;
    let mut macros = target("m", "proc-macro", "/w/m/src/lib.rs");
    macros.crate_types = vec!["proc-macro".to_owned()];
    let mut used = unit(P, macros.clone(), Mode::Build, &[]);
    used.macro_wasm = true;
    let mut checked = unit(P, macros, Mode::Check, &[]);
    checked.macro_wasm = true;
    assert!(!super::checks_only(Goal::Check, &used));
    assert_eq!(super::phases(Goal::Check, &used), &crate::serve::work::Phase::BUILD);
    assert!(!super::checks_only(Goal::Build, &used));
    assert!(super::checks_only(Goal::Check, &checked));
}

/// A proc macro's `.wasm` output that holds a library's crate record (an `ar` archive) is
/// refused before Wasmtime parses it, the error naming the file and both kinds.
#[test]
fn an_archive_where_a_wasm_module_is_expected_is_refused_naming_both_kinds() {
    let mut macros = target("m", "proc-macro", "/w/m/src/lib.rs");
    macros.crate_types = vec!["proc-macro".to_owned()];
    let record = compile::crate_record(&unit(P, macros, Mode::Build, &[]), None, &[]);
    let path = "/t/wasm32-wasip1+macros/debug/build/m/0/out/m-0.wasm";
    let error = compile::module_at(path, record).unwrap_err();
    assert_eq!(error, compile::ModuleError::NotAModule { path: path.to_owned(), found: compile::FileKind::Archive });
    let said = error.to_string();
    assert!(said.contains(path) && said.contains("an `ar` archive") && said.contains("a linked wasm32-wasip1 module"), "{said}");
    let module = b"\0asm\x01\0\0\0".to_vec();
    assert_eq!(compile::module_at(path, module.clone()), Ok(module));
}

/// A front end held on the loop whose drop panics (a storage with a table it can't walk) lets
/// go of nothing past the catch: dropped, its holder goes on; let go of by a refusal, the job
/// is given the panic.
#[test]
fn a_front_end_whose_drop_panics_fails_its_holder_not_the_loop() {
    struct Breaks;
    impl Drop for Breaks {
        fn drop(&mut self) {
            panic!("a table with an Id of 0");
        }
    }
    let dropped = std::panic::catch_unwind(|| drop(super::Held::new(Breaks, "a test's front end")));
    assert!(dropped.is_ok(), "the drop of a held front end unwound its holder");
    let panic = super::Held::new(Breaks, "a test's front end").let_go().unwrap_err();
    assert!(panic.to_string().contains("a table with an Id of 0"), "{panic}");
    assert!(super::Held::new(3_u8, "a test's front end").let_go().is_ok());
}

/// Every build script's target is `build-script-build`: the phase spans' `unit` names the
/// package, its version and the target, so the profile tells each script apart.
#[test]
fn a_units_phase_spans_name_its_package_version_and_target() {
    let script = |pkg: &str| unit(pkg, target("build-script-build", "custom-build", "build.rs"), Mode::Build, &[]);
    let serde = script("registry+https://github.com/rust-lang/crates.io-index#serde@1.0.219");
    let libc = script("registry+https://github.com/rust-lang/crates.io-index#libc@0.2.170");
    assert_eq!(compile::span_unit(&serde), "serde v1.0.219 (build script)");
    assert_eq!(compile::span_unit(&libc), "libc v0.2.170 (build script)");
    let lib = target("blaze_api", "lib", "src/lib.rs");
    let api = "path+file:///w/crates/blaze-api#0.1.0";
    assert_eq!(compile::span_unit(&unit(api, lib.clone(), Mode::Build, &[])), "blaze-api v0.1.0 (lib)");
    assert_eq!(compile::span_unit(&unit(api, lib, Mode::Test, &[])), "blaze-api v0.1.0 (lib test)");
    let bin = target("blaze", "bin", "src/main.rs");
    assert_eq!(compile::span_unit(&unit(api, bin, Mode::Build, &[])), "blaze-api v0.1.0 (bin \"blaze\")");
    let tests = target("e2e", "test", "tests/e2e.rs");
    assert_eq!(compile::span_unit(&unit(api, tests, Mode::Test, &[])), "blaze-api v0.1.0 (test \"e2e\")");
}

/// A library's doctests are named as cargo names the unit, and each example's program and
/// the verdicts are kept beside the runner, in its `.doctests` directory only
/// (docs/v2/plan/doctests.md).
#[test]
fn doctests_are_named_and_kept_beside_their_runner() {
    let lib = target("blaze_api", "lib", "src/lib.rs");
    let doctests = unit("path+file:///w/crates/blaze-api#0.1.0", lib, Mode::Doctest, &[]);
    assert_eq!(compile::span_unit(&doctests), "blaze-api v0.1.0 (lib doctest)");
    let main = "/t/debug/deps/blaze_api-0123";
    assert!(compile::doctest::beside(main, "/t/debug/deps/blaze_api-0123.doctests/src_lib_rs_3_0"));
    assert!(compile::doctest::beside(main, "/t/debug/deps/blaze_api-0123.doctests/verdicts"));
    assert!(!compile::doctest::beside(main, "/t/debug/deps/blaze_api-0123.doctestsx/verdicts"));
    assert!(!compile::doctest::beside(main, "/t/debug/deps/blaze_api-0123.d"));
}

/// A build script that reads `FOO` of the process through `option_env!` and nothing else.
const SCRIPT: &[u8] = b"#[rustc_builtin_macro]\nmacro_rules! option_env { () => {} }\n\n\
fn main() { let _ = option_env!(\"FOO\"); }\n";

/// [`SCRIPT`] with its `main` edited.
const SCRIPT_EDITED: &[u8] = b"#[rustc_builtin_macro]\nmacro_rules! option_env { () => {} }\n\n\
fn main() { let _ = option_env!(\"FOO\"); let _ = 1; }\n";

/// One run of blaze compiling build script `text` (unit 1, on `core` when `on_core`) for a
/// process with the variables `env`: lowered over files in memory, its crate resolved so the
/// `env!` log has what it read, and its record's base key worked out as `compile` works it out,
/// with what its code read.
fn script_run(text: &'static [u8], on_core: bool, env: &[(&str, &str)]) -> (blaze_cas::Key, super::reads::Reads) {
    let (lowered, base) = script_lowered(text, on_core, env);
    let krate = lowered.krate(1).unwrap();
    (base, super::reads::Reads::of(lowered.db(), krate).unwrap())
}

/// [`script_run`]'s front end, its crate resolved, and the record's base key.
fn script_lowered(text: &'static [u8], on_core: bool, env: &[(&str, &str)]) -> (blaze_lower::Lowered, blaze_cas::Key) {
    let front = script_front(text, on_core, env);
    (front.lowered, front.base)
}

/// [`script_lowered`]'s front end with what it was lowered from, for a later build to lower
/// into it.
struct ScriptFront {
    lowered: blaze_lower::Lowered,
    base: blaze_cas::Key,
    units: Units,
    config: blaze_units::query::BuildConfig,
    tree: blaze_query::MemTree,
    view: ContentId,
}

impl ScriptFront {
    /// The build config of this front end's build run with the variables `env` in place of
    /// its own.
    fn config_with(&self, env: &[(&str, &str)]) -> blaze_units::query::BuildConfig {
        let mut config = self.config.clone();
        config.invocation.env = super::reads::tests::env(env);
        config
    }
}

fn script_front(text: &'static [u8], on_core: bool, env: &[(&str, &str)]) -> ScriptFront {
    script_front_for(text, on_core, env, true)
}

/// [`script_front`] of a request that asks for the script's package (`primary`) or only for
/// `core`, so the script is not of a primary package.
fn script_front_for(text: &'static [u8], on_core: bool, env: &[(&str, &str)], primary: bool) -> ScriptFront {
    use blaze_units::{
        query::{BuildConfig, Command, Invocation, Mount, Origin},
        schema::{Strip, Stripped},
    };
    let tree = blaze_query::MemTree::default();
    let library = tree.root(&[("core/Cargo.toml", b"[package]\nname = \"core\"\n"), ("core/src/lib.rs", b"pub mod marker {}\n")]);
    let view = tree.root(&[("Cargo.toml", b"[package]\nname = \"s\"\nbuild = \"build.rs\"\n"), ("build.rs", text)]);
    let (core, s) = ("path+file:///rustc/abc/library/core#0.0.0", "path+file:///w#0.1.0");
    let resolved = |mut it: Unit| {
        it.profile = Profile { incremental: true, strip: Strip::Resolved(Stripped::None), ..Profile::dev() };
        it
    };
    let mut core_unit = resolved(unit(core, target("core", "lib", "/rustc/abc/library/core/src/lib.rs"), Mode::Build, &[]));
    core_unit.is_std = true;
    let mut script = resolved(unit(s, target("build-script-build", "custom-build", "/w/build.rs"), Mode::Build, &[]));
    script.dependencies = match on_core {
        true => vec![UnitDep { index: 0, extern_crate_name: "core".to_owned(), public: None, noprelude: Some(true), nounused: None, dev: false }],
        false => Vec::new(),
    };
    let mut script_plan = plan("", &[]);
    script_plan.env = vec![("CARGO_MANIFEST_PATH".to_owned(), "/w/Cargo.toml".to_owned())];
    let units = Units {
        graph: UnitGraph { version: 1, units: vec![core_unit, script], roots: vec![if primary { 1 } else { 0 }] },
        plans: vec![plan("", &[]), script_plan],
        packages: vec![PackagePlan { is_member: false, ..package(core, "core", "/rustc/abc/library/core") }, package(s, "s", "/w")],
        lock: None,
        warnings: Vec::new(),
        target_dir: "/w/target".to_owned(),
        build_dir: "/w/target".to_owned(),
        finished: String::new(),
        link_dirs: Vec::new(),
        sandboxed_tests: false,
    };
    let config = BuildConfig {
        invocation: Invocation {
            command: Command::Build,
            args: Vec::new(),
            root: "/w".to_owned(),
            cwd: "/w".to_owned(),
            env: super::reads::tests::env(env),
            files: Vec::new(),
        },
        mounts: vec![
            Mount { at: "/rustc/abc/library".to_owned(), origin: Origin::Root(library), under: String::new() },
            Mount { at: "/w".to_owned(), origin: Origin::View, under: String::new() },
        ],
        host: HOST.to_owned(),
        library: "/rustc/abc/library".to_owned(),
        std: "/s/std".to_owned(),
        builds: "/s/build".to_owned(),
        sources: "/s/sources".to_owned(),
    };
    let lower = blaze_lower::Lower { units: &units, config: &config, files: &tree, root: view, host_cpu: None };
    let mut lowered = blaze_lower::lower(lower).unwrap();
    let identity = lowered.identity(1).unwrap();
    let krate = lowered.krate(1).unwrap();
    // What its compile does to the front end: every body resolved, `option_env!` expanded.
    hir_def::deps::checked_reads(lowered.db(), krate).unwrap();
    let script = &units.graph.units[1];
    let base = compile::RecordInputs {
        identity,
        unit: compile::KeyedUnit::among(script, |at| Ok(&units.graph.units[at])).unwrap(),
        plan: &units.plans[1],
        run_env: &[],
        script_check_cfgs: Vec::new(),
        makers: one_maker(),
        goal: compile::keyed_goal(crate::serve::work::Goal::Build, script),
        color: false,
        short: false,
        verified: false,
        reached: false,
        pgo: None,
        lint_profile: None,
        layout: None,
        width: None,
        refused: None,
    }
    .key(1)
    .unwrap();
    ScriptFront { lowered, base, units, config, tree, view }
}

/// One made-up maker of records ([`crate::serve::RecordMakers`]), for the keys these tests
/// compare.
fn one_maker() -> crate::serve::RecordMakers {
    let id = |what: &[u8]| ContentId::of("blaze v2 cargo test record makers", what);
    crate::serve::RecordMakers { front_end: id(b"front end"), codegen: id(b"code generator"), record: id(b"record") }
}

/// The key of the record of `base` that `store` has for the variables `env`, as
/// `compile::fresh` looks.
fn look(store: &super::reads::tests::Store, base: blaze_cas::Key, env: &[(&str, &str)]) -> Option<blaze_cas::Key> {
    store.find(base, &super::reads::tests::env(env)).map(|it| it.key)
}

/// A build script that reads `CARGO_PRIMARY_PACKAGE` through `option_env!` and nothing else.
const PRIMARY_SCRIPT: &[u8] = b"#[rustc_builtin_macro]\nmacro_rules! option_env { () => {} }\n\n\
fn main() { let _ = option_env!(\"CARGO_PRIMARY_PACKAGE\"); }\n";

/// `CARGO_PRIMARY_PACKAGE` is of the request, not of the unit: `--workspace` makes every member
/// primary and `-p one` only `one`, so keying every unit by it recompiled every dependency of
/// `one` after a workspace build (sb3slow, the self-build's link step). A record is keyed by it
/// only when a crate of its closure read it (rule 18, both ways): a script that never read it
/// has one base key and one record whether or not its package is primary; one that read it
/// has the same base key, but is filed by it, and misses once its package's primary-ness moves.
#[test]
fn primary_package_keys_only_a_record_whose_code_read_it() {
    let env = [("FOO", "on")];
    let vars = super::reads::tests::env(&env);
    let front = |text, primary| script_front_for(text, true, &env, primary);
    let reads = |front: &ScriptFront| super::reads::Reads::of(front.lowered.db(), front.lowered.krate(1).unwrap()).unwrap();
    let mut store = super::reads::tests::Store::default();

    let (asked, other) = (front(SCRIPT, true), front(SCRIPT, false));
    assert!(asked.units.is_primary(1) && !other.units.is_primary(1));
    assert_eq!(asked.base, other.base, "a crate that never read it: its key holds no primary-ness");
    assert_eq!(reads(&asked), super::reads::Reads::named(&["FOO"]));
    assert_eq!(reads(&other), reads(&asked));
    let filed = store.keep_for(asked.base, &reads(&asked), &vars, &|it| asked.units.is_primary_manifest(it));
    let found = store.find_for(other.base, &vars, &|it| other.units.is_primary_manifest(it));
    assert_eq!(found.map(|it| it.key), Some(filed), "built as primary, found as not");

    let (asked, other) = (front(PRIMARY_SCRIPT, true), front(PRIMARY_SCRIPT, false));
    assert_eq!(asked.base, other.base, "a crate that read it: its base key holds no primary-ness either");
    assert_eq!(reads(&asked), super::reads::Reads::primary("/w/Cargo.toml"), "it is filed by it");
    assert_eq!(reads(&other), reads(&asked));
    let filed = store.keep_for(asked.base, &reads(&asked), &vars, &|it| asked.units.is_primary_manifest(it));
    assert_eq!(store.find_for(asked.base, &vars, &|it| asked.units.is_primary_manifest(it)).map(|it| it.key), Some(filed));
    let found = store.find_for(other.base, &vars, &|it| other.units.is_primary_manifest(it));
    assert_eq!(found, None, "made as primary, its package no longer primary: a miss");
    let not = store.keep_for(other.base, &reads(&other), &vars, &|it| other.units.is_primary_manifest(it));
    assert_ne!(not, filed, "two values, two records");
}

/// Two runs of the same tree differ in the variables of the process that starts them: the
/// coordinator's runs each set their own `BLAZE_PROFILE` directory, and the shell's `OLDPWD`,
/// `SHLVL`, the client's pid and its temp directory differ too. A build script's compile
/// reads none of them, so the second run finds the first run's record. Self-builds 47 to 55
/// recomputed every build script, 1,232 s of `unit names` and 1,855 s of `unit codegen` in
/// build 53, because the whole environment was in every crate's identity.
#[test]
fn build_script_compile_is_reused_across_runs() {
    let first = [
        ("FOO", "on"),
        ("BLAZE_PROFILE", "/tmp/blaze-lanes/profile-0101"),
        ("OLDPWD", "/w"),
        ("SHLVL", "1"),
        ("BLAZE_CLIENT_PID", "4242"),
        ("TMPDIR", "/var/folders/aa/T"),
    ];
    let second = [
        ("FOO", "on"),
        ("BLAZE_PROFILE", "/tmp/blaze-lanes/profile-0242"),
        ("OLDPWD", "/tmp/elsewhere"),
        ("SHLVL", "3"),
        ("BLAZE_CLIENT_PID", "977"),
        ("TMPDIR", "/var/folders/bb/T"),
        ("TERM_SESSION_ID", "w0t0p0"),
    ];
    let (base, read) = script_run(SCRIPT, true, &first);
    assert_eq!(read, super::reads::Reads::named(&["FOO"]), "the script read `FOO` and nothing else of the process");
    let mut store = super::reads::tests::Store::default();
    let filed = store.keep(base, &read, &super::reads::tests::env(&first));
    let (again, read_again) = script_run(SCRIPT, true, &second);
    assert_eq!(again, base, "a variable no crate read is not in the record's key");
    assert_eq!(look(&store, again, &second), Some(filed), "the second run's compile is a store hit");
    let slots = store.0.len();
    assert_eq!(store.keep(again, &read_again, &super::reads::tests::env(&second)), filed);
    assert_eq!(store.0.len(), slots, "the second run files nothing new");
}

/// What a build script's compile is made from makes its record miss when it changes: the
/// value of a variable its code read (another value, or unset), its source, a dependency.
#[test]
fn a_real_input_of_a_build_script_makes_its_record_miss() {
    let run = [("FOO", "on"), ("BLAZE_PROFILE", "/tmp/blaze-lanes/profile-0101")];
    let (base, read) = script_run(SCRIPT, true, &run);
    let mut store = super::reads::tests::Store::default();
    let filed = store.keep(base, &read, &super::reads::tests::env(&run));
    let other_value: &[(&str, &str)] = &[("FOO", "off"), ("BLAZE_PROFILE", "/tmp/blaze-lanes/profile-0101")];
    let unset: &[(&str, &str)] = &[("BLAZE_PROFILE", "/tmp/blaze-lanes/profile-0101")];
    for env in [other_value, unset] {
        let (again, read_again) = script_run(SCRIPT, true, env);
        assert_eq!(again, base, "{env:?}: the base key holds no variable's value");
        assert_eq!(look(&store, again, env), None, "{env:?}: a variable the script read changed");
        let elsewhere = super::reads::tests::Store::default().keep(again, &read_again, &super::reads::tests::env(env));
        assert_ne!(elsewhere, filed, "{env:?}");
    }
    let (edited, _) = script_run(SCRIPT_EDITED, true, &run);
    assert_ne!(edited, base, "its source");
    assert_eq!(look(&store, edited, &run), None, "its source");
    let (alone, _) = script_run(SCRIPT, false, &run);
    assert_ne!(alone, base, "a dependency");
    assert_eq!(look(&store, alone, &run), None, "a dependency");
}

/// A kept front end is keyed by the variables of the environment its crates read, never the
/// whole environment: a no-op build from another shell tab or systemd unit (`SHLVL`,
/// `OLDPWD`, `TERM_SESSION_ID`, `INVOCATION_ID`, `_`), or with a variable only a build
/// script's run reads (`CC`, which its sandbox traces into the run's own key), lowers into it,
/// and the build reads the new values. On a VM, systemd's per-unit `INVOCATION_ID` made every
/// no-op rebuild lower everything again: 86 s in place of about 1 s.
#[test]
fn a_kept_front_end_outlives_a_variable_no_crate_read() {
    let first = [("FOO", "on"), ("SHLVL", "1"), ("OLDPWD", "/w"), ("TERM_SESSION_ID", "w0t0p0"), ("INVOCATION_ID", "5e1f")];
    let second = [
        ("CC", "clang"),
        ("FOO", "on"),
        ("INVOCATION_ID", "77aa"),
        ("OLDPWD", "/tmp/elsewhere"),
        ("SHLVL", "3"),
        ("TERM_SESSION_ID", "w1t2p0"),
        ("_", "/usr/local/bin/blaze"),
    ];
    let mut front = script_front(SCRIPT, true, &first);
    let now = front.config_with(&second);
    assert_eq!(blaze_lower::config_differs(&front.config, &now), None, "the environment is not compared whole");
    let read = blaze_units::query::EnvReads::default();
    assert_eq!(super::env_moved(&front.lowered, &read, &now.invocation.env), None, "no crate read what moved");
    let krate = front.lowered.krate(1).unwrap();
    let lower = blaze_lower::Lower { units: &front.units, config: &now, files: &front.tree, root: front.view, host_cpu: None };
    assert_eq!(front.lowered.refresh(lower, |_| Ok(())), Ok(true));
    let db = front.lowered.db();
    assert_eq!(base_db::process_env_var(db, "SHLVL").as_deref(), Some("3"), "the build reads the request's values");
    assert_eq!(super::reads::Reads::of(db, krate).unwrap(), super::reads::Reads::named(&["FOO"]));
}

/// A variable a crate's `env!` or `option_env!` read lets the kept front end go, named with
/// the crate that read it and never valued, and so does a variable the daemon read into what
/// it keeps with the front end (a store's mode); what read it computes again, as its record
/// misses (`a_real_input_of_a_build_script_makes_its_record_miss`), and nothing else does.
#[test]
fn a_variable_a_crate_read_lets_the_kept_front_end_go_by_name() {
    let first = [("FOO", "value-before"), ("SHLVL", "1")];
    let front = script_front(SCRIPT, true, &first);
    let other: &[(&str, &str)] = &[("FOO", "value-after"), ("SHLVL", "1")];
    let unset: &[(&str, &str)] = &[("SHLVL", "1")];
    let read = blaze_units::query::EnvReads::default();
    for changed in [other, unset] {
        let now = front.config_with(changed);
        assert_eq!(blaze_lower::config_differs(&front.config, &now), None, "{changed:?}");
        let moved = super::env_moved(&front.lowered, &read, &now.invocation.env).expect("`FOO` was read");
        assert_eq!(moved.name, "FOO", "{changed:?}");
        assert!(moved.by.contains("option_env!") && moved.by.contains("build-script-build"), "{}", moved.by);
        let said = format!("{moved} {moved:?}");
        assert!(!said.contains("value-before") && !said.contains("value-after"), "{said}");
    }
    let mut read = blaze_units::query::EnvReads::default();
    let env = super::reads::tests::env(&first);
    read.var(&env, "BLAZE_ITEM_STORE", "the item store's mode").unwrap();
    let now = front.config_with(&[("BLAZE_ITEM_STORE", "off"), ("FOO", "value-before"), ("SHLVL", "2")]);
    let moved = super::env_moved(&front.lowered, &read, &now.invocation.env).expect("the item store's mode moved");
    assert_eq!((moved.name.as_str(), moved.by.as_str()), ("BLAZE_ITEM_STORE", "the item store's mode"));
}

/// Where `d`'s build script's run (unit 1) writes, as cargo names it.
const D_OUT: &str = "/w/target/debug/build/d-0001/out";

/// `d`, a registry dependency whose build script (units 0 and 1) decides by its run's cfg
/// which of `fast` and `slow` its library (unit 2) has, lowered over files in memory.
fn script_cfg_front() -> blaze_lower::Lowered {
    script_lib_front(b"#[cfg(fast)]\npub fn fast() {}\n#[cfg(not(fast))]\npub fn slow() {}\n")
}

/// `d` as [`script_cfg_front`] has it, with `lib` as its library's root.
fn script_lib_front(lib: &[u8]) -> blaze_lower::Lowered {
    use blaze_units::query::{BuildConfig, Command, Invocation, Mount, Origin};
    const D: &str = "registry+https://github.com/rust-lang/crates.io-index#d@0.1.0";
    let tree = blaze_query::MemTree::default();
    let registry = tree.root(&[
        ("d/Cargo.toml", b"[package]\nname = \"d\"\nbuild = \"build.rs\"\n"),
        ("d/build.rs", b"fn main() {}\n"),
        ("d/src/lib.rs", lib),
    ]);
    let view = tree.root(&[]);
    let units = Units {
        graph: UnitGraph {
            version: 1,
            units: vec![
                unit(D, target("build-script-build", "custom-build", "/r/d/build.rs"), Mode::Build, &[]),
                unit(D, target("build-script-build", "custom-build", "/r/d/build.rs"), Mode::RunCustomBuild, &[0]),
                unit(D, target("d", "lib", "/r/d/src/lib.rs"), Mode::Build, &[1]),
            ],
            roots: vec![2],
        },
        plans: vec![
            plan("-0000", &[]),
            UnitPlan { out_dir: Some(D_OUT.to_owned()), ..plan("-0001", &[]) },
            plan("-0002", &[]),
        ],
        packages: vec![PackagePlan {
            is_member: false,
            source: Some("registry+https://github.com/rust-lang/crates.io-index".to_owned()),
            ..package(D, "d", "/r/d")
        }],
        lock: None,
        warnings: Vec::new(),
        target_dir: "/w/target".to_owned(),
        build_dir: "/w/target".to_owned(),
        finished: String::new(),
        link_dirs: Vec::new(),
        sandboxed_tests: false,
    };
    let config = BuildConfig {
        invocation: Invocation {
            command: Command::Build,
            args: Vec::new(),
            root: "/w".to_owned(),
            cwd: "/w".to_owned(),
            env: Vec::new(),
            files: Vec::new(),
        },
        mounts: vec![
            Mount { at: "/r".to_owned(), origin: Origin::Root(registry), under: String::new() },
            Mount { at: "/w".to_owned(), origin: Origin::View, under: String::new() },
        ],
        host: HOST.to_owned(),
        library: "/rustc/abc/library".to_owned(),
        std: "/s/std".to_owned(),
        builds: "/s/build".to_owned(),
        sources: "/s/sources".to_owned(),
    };
    blaze_lower::lower(blaze_lower::Lower { units: &units, config: &config, files: &tree, root: view, host_cpu: None }).unwrap()
}

/// A package of a front lowered over files in memory ([`front_of`]): its name, whether it is at
/// a path (checked whole) or from the registry (reached only), its `src/lib.rs`, and the units it
/// depends on, each with the name it calls it.
struct MemPkg<'a> {
    name: &'a str,
    path: bool,
    lib: &'a str,
    deps: &'a [(usize, &'a str)],
}

/// A front [`front_of`] lowered, with the units it lowered.
struct MemFront {
    lowered: blaze_lower::Lowered,
    units: Units,
}

/// `pkgs`, one lib unit each (unit `i` is `pkgs[i]`) under `/r`, lowered over files in memory as
/// a build lowers them; a unit no other depends on is a root. Each registry package is checked
/// to be reached only and each path package to be checked whole, as the test means them.
fn front_of(pkgs: &[MemPkg<'_>]) -> MemFront {
    use blaze_units::query::{BuildConfig, Command, Invocation, Mount, Origin};
    let tree = blaze_query::MemTree::default();
    let paths: Vec<(String, String)> =
        pkgs.iter().map(|it| (format!("{}/Cargo.toml", it.name), format!("{}/src/lib.rs", it.name))).collect();
    let manifests: Vec<String> = pkgs.iter().map(|it| format!("[package]\nname = \"{}\"\n", it.name)).collect();
    let mut files: Vec<(&str, &[u8])> = Vec::new();
    for ((pkg, (manifest_at, lib_at)), manifest) in pkgs.iter().zip(&paths).zip(&manifests) {
        files.push((manifest_at.as_str(), manifest.as_bytes()));
        files.push((lib_at.as_str(), pkg.lib.as_bytes()));
    }
    let registry = tree.root(&files);
    let view = tree.root(&[]);
    let id = |pkg: &MemPkg<'_>| match pkg.path {
        true => format!("path+file:///r/{}#0.1.0", pkg.name),
        false => format!("registry+https://github.com/rust-lang/crates.io-index#{}@0.1.0", pkg.name),
    };
    let units: Vec<Unit> = pkgs
        .iter()
        .map(|pkg| {
            let mut u = unit(&id(pkg), target(pkg.name, "lib", &format!("/r/{}/src/lib.rs", pkg.name)), Mode::Build, &[]);
            u.dependencies = pkg
                .deps
                .iter()
                .map(|&(index, name)| UnitDep {
                    index,
                    extern_crate_name: name.to_owned(),
                    public: None,
                    noprelude: None,
                    nounused: None,
                    dev: false,
                })
                .collect();
            u
        })
        .collect();
    let roots = (0..pkgs.len()).filter(|at| !pkgs.iter().any(|it| it.deps.iter().any(|dep| dep.0 == *at))).collect();
    let packages: Vec<PackagePlan> = pkgs
        .iter()
        .map(|pkg| PackagePlan {
            is_member: false,
            source: Some(match pkg.path {
                true => format!("(/r/{})", pkg.name),
                false => "(registry+https://github.com/rust-lang/crates.io-index)".to_owned(),
            }),
            ..package(&id(pkg), pkg.name, &format!("/r/{}", pkg.name))
        })
        .collect();
    for (pkg, plan) in pkgs.iter().zip(&packages) {
        let reached = compile::capped(false, plan.is_member, plan.source.as_deref());
        assert_eq!(reached, !pkg.path, "{} is {}", pkg.name, if pkg.path { "checked whole" } else { "reached only" });
    }
    let units = Units {
        graph: UnitGraph { version: 1, units, roots },
        plans: (0..pkgs.len()).map(|at| plan(&format!("-{at:04}"), &[])).collect(),
        packages,
        lock: None,
        warnings: Vec::new(),
        target_dir: "/w/target".to_owned(),
        build_dir: "/w/target".to_owned(),
        finished: String::new(),
        link_dirs: Vec::new(),
        sandboxed_tests: false,
    };
    let config = BuildConfig {
        invocation: Invocation {
            command: Command::Build,
            args: Vec::new(),
            root: "/w".to_owned(),
            cwd: "/w".to_owned(),
            env: Vec::new(),
            files: Vec::new(),
        },
        mounts: vec![
            Mount { at: "/r".to_owned(), origin: Origin::Root(registry), under: String::new() },
            Mount { at: "/w".to_owned(), origin: Origin::View, under: String::new() },
        ],
        host: HOST.to_owned(),
        library: "/rustc/abc/library".to_owned(),
        std: "/s/std".to_owned(),
        builds: "/s/build".to_owned(),
        sources: "/s/sources".to_owned(),
    };
    let lowered =
        blaze_lower::lower(blaze_lower::Lower { units: &units, config: &config, files: &tree, root: view, host_cpu: None }).unwrap();
    MemFront { lowered, units }
}

/// What a reached-only build runs of unit `unit`'s crate: whether its graph has a sealed trait,
/// and [`compile::reached_only_diags`].
fn sealed_diags(lowered: &blaze_lower::Lowered, unit: usize) -> (bool, Vec<blaze_diagnostics::Diag>) {
    let db = lowered.db();
    let krate = lowered.krate(unit).unwrap();
    let files = blaze_diagnostics::Files::new(lowered.paths(), "/");
    let in_graph = blaze_query::db::attach(db, || hir_ty::sealed_in_graph(db, krate));
    let diags = compile::reached_only_diags(db, krate, files, || Ok(blaze_diagnostics::Options::default())).unwrap();
    (in_graph, diags)
}

/// Three registry crates: `l` (unit 0) seals `S`; `d` (unit 1), which depends on `l`,
/// implements `S` and extends `l`'s subtrait of it, both of which rustc accepts
/// (`#[blaze::sealed]` is inert there); `plain` (unit 2) has no sealed trait in its graph.
fn sealed_registry_front() -> blaze_lower::Lowered {
    front_of(&[
        MemPkg {
            name: "l",
            path: false,
            lib: "#[blaze::sealed]\npub trait S {}\npub trait LSub: S {}\npub struct A;\nimpl S for A {}\n",
            deps: &[],
        },
        MemPkg {
            name: "d",
            path: false,
            lib: "pub struct DT;\nimpl dep::S for DT {}\npub trait DSub: dep::LSub {}\n",
            deps: &[(0, "dep")],
        },
        MemPkg { name: "plain", path: false, lib: "pub trait P {}\npub struct X;\nimpl P for X {}\n", deps: &[] },
    ])
    .lowered
}

/// A registry crate that implements, or extends, a trait sealed upstream fails its unit though
/// only what programs reach of it is checked (`BLAZE_DEPS` unset): sealing is `l`'s opt-in and
/// binds every crate that can name `S`, so the sealed checks run for every crate whose graph
/// has a sealed trait, and each refusal names both crates, the trait and the rule. A graph with
/// no `#[blaze::sealed]` anywhere gets no check and no diagnostic, as before.
#[test]
fn a_reached_only_crate_that_implements_a_trait_sealed_upstream_is_refused() {
    let lowered = sealed_registry_front();
    let diags = |unit: usize| sealed_diags(&lowered, unit);

    let (in_graph, d) = diags(1);
    assert!(in_graph, "`d` depends on `l`, which seals `S`");
    let errors: Vec<&str> = d.iter().filter(|it| it.is_error()).map(|it| it.message.as_str()).collect();
    assert!(
        errors.iter().any(|it| it.contains("impl S for DT")
            && it.contains("`S`")
            && it.contains("crate `l`")
            && it.contains("rule: sealed-impl-elsewhere")),
        "{d:#?}"
    );
    assert!(
        errors.iter().any(|it| it.contains("`DSub`") && it.contains("DSub: LSub: S") && it.contains("rule: sealed-supertrait-elsewhere")),
        "{d:#?}"
    );

    let (in_graph, l) = diags(0);
    assert!(in_graph, "`l` seals `S` itself");
    assert!(!l.iter().any(|it| it.is_error()), "`l`'s own impl of `S` is allowed: {l:#?}");

    let (in_graph, plain) = diags(2);
    assert!(!in_graph, "no crate of `plain`'s graph seals a trait");
    assert!(plain.is_empty(), "{plain:#?}");
}

/// `bad`'s source: a body nothing reaches whose macro call no rule of `m` matches (line 3).
const BAD_EXPANSION: &str = "macro_rules! m { (a) => { 1 }; }\npub fn fine() -> i32 { m!(a) }\npub fn unreached() -> i32 { m!(b) }\n";

/// A dependency whose body has a macro call that fails to expand fails its own unit, though
/// only what programs reach of it is type-checked (`BLAZE_DEPS` unset): rustc expands every
/// call of every crate it compiles. The error names the call where it is written (`bad`'s
/// line 3) and the body it is in. Before, `bad` was built, and the error surfaced only in a
/// crate that reached the body, as "MIR that failed to lower (HasErrors)" with no call in it
/// (darling_core's `Initializer::to_tokens`, seen from darling_macro). `user`, which depends on
/// `bad` and expands fine, gets nothing.
#[test]
fn a_dependency_whose_macro_call_fails_to_expand_fails_its_own_unit_naming_the_call() {
    let front = front_of(&[
        MemPkg { name: "bad", path: false, lib: BAD_EXPANSION, deps: &[] },
        MemPkg { name: "user", path: false, lib: "pub fn two() -> i32 { dep::fine() + 1 }\n", deps: &[(0, "dep")] },
    ]);
    let (_, bad) = sealed_diags(&front.lowered, 0);
    let shown: Vec<&blaze_diagnostics::Diag> =
        bad.iter().filter(|it| it.is_error() && !compile::cargo_hides(&it.message)).collect();
    let [error] = shown[..] else { panic!("one error of `bad`'s own, its unit failed: {bad:#?}") };
    let at = error.span.primary.first().unwrap_or_else(|| panic!("the error is placed: {error:#?}"));
    assert!(at.file.ends_with("bad/src/lib.rs"), "{error:#?}");
    let line = BAD_EXPANSION[..at.lo as usize].matches('\n').count() + 1;
    assert_eq!(line, 3, "on the line of the call: {error:#?}");
    let call = BAD_EXPANSION.find("m!(b)").unwrap();
    let within = call as u32..=(call + "m!(b)".len()) as u32;
    assert!(within.contains(&at.lo) && within.contains(&at.hi), "in the call `m!(b)`: {error:#?}");
    assert!(
        error.children.iter().any(|it| it.message == "in the body of `bad::unreached`"),
        "names the body: {error:#?}"
    );
    let (_, user) = sealed_diags(&front.lowered, 1);
    assert!(user.is_empty(), "{user:#?}");
}

/// `up`'s source: a foreign trait with an associated type, a generic one, and a two-argument
/// `#[fundamental]` type, whose every argument rustc's orphan walk looks into.
const ORPHAN_UP: &str = "#![feature(fundamental)]\npub trait Conv { type Out; }\npub trait Gen<X> {}\n\
     #[fundamental]\npub struct Pair<X, Y>(pub X, pub Y);\n";

/// `l`'s source, sealing `S` (with `sealed`) and relaxing the orphan rule for `up::Conv` on it.
fn orphan_l(sealed: bool) -> String {
    let attr = if sealed { "#[blaze::sealed]\n" } else { "" };
    format!("{attr}pub trait S {{}}\npub struct A;\nimpl S for A {{}}\nimpl<T: S> up::Conv for T {{ type Out = u64; }}\n")
}

/// R3-C1's graph: `up` (unit 0, registry) has `Conv`; `l` (unit 1, a path package, so checked
/// whole, and not here: with no `core` its `T` has no `Sized` bound to show) seals `S` when
/// `sealed` and relaxes the orphan rule for `Conv` on it; `d` (unit 2,
/// registry, reached only) implements `Conv` for `l::A`, which rustc refuses (E0117); `ok`
/// (unit 3, registry, reached only) writes only impls of `up`'s traits that rustc's orphan rule
/// allows, one of which (`Conv for Pair<l::A, OkLocal>`) hir's stricter walk would refuse.
fn orphan_front(sealed: bool) -> MemFront {
    let l = orphan_l(sealed);
    front_of(&[
        MemPkg { name: "up", path: false, lib: ORPHAN_UP, deps: &[] },
        MemPkg { name: "l", path: true, lib: &l, deps: &[(0, "up")] },
        MemPkg {
            name: "d",
            path: false,
            lib: "impl up::Conv for l::A { type Out = &'static str; }\n",
            deps: &[(0, "up"), (1, "l")],
        },
        MemPkg {
            name: "ok",
            path: false,
            lib: "pub struct OkLocal;\npub struct Wrap<X>(pub X);\n\
                  impl up::Conv for OkLocal { type Out = u8; }\n\
                  impl up::Conv for &'static OkLocal { type Out = u16; }\n\
                  impl up::Gen<OkLocal> for l::A {}\n\
                  impl<X> up::Gen<X> for Wrap<X> {}\n\
                  impl up::Conv for up::Pair<l::A, OkLocal> { type Out = u32; }\n",
            deps: &[(0, "up"), (1, "l")],
        },
    ])
}

/// A reached-only registry crate in a sealed graph gets rustc's orphan check on every impl of a
/// foreign trait, not only on impls with sealed bounds (R3-C1): the knowability rule and the
/// expansion argument assume every crate that can name `l::S` obeys it, and rustc never checked
/// `d` against a blaze-only `l`. Without the check, `<l::A as up::Conv>::Out` is `u64` in `l` and
/// `&'static str` in `d`: a transmute across the crate boundary. The refusal is rustc's E0117 and
/// names the impl, the trait with its crate, the rule and the crate that seals.
#[test]
fn a_reached_only_orphan_impl_in_a_sealed_graph_is_refused() {
    let front = orphan_front(true);
    let (in_graph, d) = sealed_diags(&front.lowered, 2);
    assert!(in_graph, "`d` depends on `l`, which seals `S`");
    let errors: Vec<&blaze_diagnostics::Diag> = d.iter().filter(|it| it.is_error()).collect();
    assert!(
        errors.iter().any(|it| it.code.as_ref().and_then(|it| it.error()) == Some("E0117")
            && it.message.contains("`impl ")
            && it.message.contains("Conv for ")
            && it.message.contains("A`")
            && it.message.contains("of crate `up`")
            && it.message.contains("rule: orphan")
            && it.message.contains("crate `l` seals a trait")),
        "{d:#?}"
    );
}

/// The other side of R3-C1: a registry crate rustc accepts stays accepted in a sealed graph.
/// Every impl of `ok` is of a foreign trait and allowed by rustc's orphan rule, including
/// `Conv for Pair<l::A, OkLocal>`, which only a walk into every argument of a fundamental type
/// (rustc's) allows; the check is rustc's walker, never hir's stricter `check_orphan_rules`.
#[test]
fn a_reached_only_crate_rustc_accepts_stays_accepted_in_a_sealed_graph() {
    let front = orphan_front(true);
    let (in_graph, ok) = sealed_diags(&front.lowered, 3);
    assert!(in_graph, "`ok` depends on `l`, which seals `S`");
    assert!(!ok.iter().any(|it| it.is_error()), "{ok:#?}");
}

/// A registry crate that seals keeps its relaxed impl in its own sealed graph: `l`, reached only,
/// writes `impl<T: S> up::Conv for T` with `S` sealed and closed, and the orphan check of every
/// foreign impl ([`a_reached_only_orphan_impl_in_a_sealed_graph_is_refused`]) takes `T` for a
/// local type (`hir_ty::sealed_local_bound`), so no E0210. With no `core` here `T` has no `Sized`
/// bound to show, so only the orphan error is looked for.
#[test]
fn a_reached_only_crate_that_seals_keeps_its_relaxed_impl_in_its_sealed_graph() {
    let l = orphan_l(true);
    let front = front_of(&[
        MemPkg { name: "up", path: false, lib: ORPHAN_UP, deps: &[] },
        MemPkg { name: "l", path: false, lib: &l, deps: &[(0, "up")] },
    ]);
    let (in_graph, l) = sealed_diags(&front.lowered, 1);
    assert!(in_graph, "`l` seals `S` itself");
    assert!(!l.iter().any(|it| matches!(it.code.as_ref().and_then(|it| it.error()), Some("E0210" | "E0117"))), "{l:#?}");
}

/// Rule 18 for what makes a record: the front end's, the code generator's and the record's code
/// identities ([`crate::serve::RecordMakers`]) are each in a unit's record key, so a record made
/// by other code of any one of them is never found, and one made by the same code is.
#[test]
fn each_maker_of_a_record_moves_its_key() {
    let key = |front: &mut MemFront, makers: crate::serve::RecordMakers| {
        let identity = front.lowered.identity(0).unwrap();
        let u = &front.units.graph.units[0];
        compile::RecordInputs {
            identity,
            unit: compile::KeyedUnit::among(u, |at| Ok(&front.units.graph.units[at])).unwrap(),
            plan: &front.units.plans[0],
            run_env: &[],
            script_check_cfgs: Vec::new(),
            makers,
            goal: compile::keyed_goal(crate::serve::work::Goal::Build, u),
            color: false,
            short: false,
            verified: false,
            reached: false,
            pgo: None,
            lint_profile: None,
            layout: None,
            width: None,
            refused: None,
        }
        .key(0)
        .unwrap()
    };
    let mut front = orphan_front(false);
    let one = one_maker();
    let base = key(&mut front, one);
    assert_eq!(key(&mut front, one), base, "the same makers, the same key");
    let other = ContentId::of("blaze v2 cargo test record makers", b"another");
    let moved = [
        ("the front end's", crate::serve::RecordMakers { front_end: other, ..one }),
        ("the code generator's", crate::serve::RecordMakers { codegen: other, ..one }),
        ("the record's", crate::serve::RecordMakers { record: other, ..one }),
    ];
    for (what, makers) in moved {
        assert_ne!(key(&mut front, makers), base, "{what} code identity is in the record key");
    }
}

/// Rule 18 for the sealed checks of a reached-only crate: when `l` gains `#[blaze::sealed]`,
/// `d`'s record key moves, so a record of `d` built while its graph had no sealed trait (no check
/// of its own, so no refusal) is never served for the sealed graph, and `d` is checked again and
/// refused. The key moves through the unit's identity, which hashes each dependency's identity
/// and so `l`'s source; `up`, which depends on nothing that changed, keeps its key.
#[test]
fn a_dependency_gaining_sealed_moves_a_reached_only_dependents_key_and_checks_it_again() {
    let key = |front: &mut MemFront, unit: usize| {
        let identity = front.lowered.identity(unit).unwrap();
        let u = &front.units.graph.units[unit];
        compile::RecordInputs {
            identity,
            unit: compile::KeyedUnit::among(u, |at| Ok(&front.units.graph.units[at])).unwrap(),
            plan: &front.units.plans[unit],
            run_env: &[],
            script_check_cfgs: Vec::new(),
            makers: one_maker(),
            goal: compile::keyed_goal(crate::serve::work::Goal::Build, u),
            color: false,
            short: false,
            verified: false,
            reached: true,
            pgo: None,
            lint_profile: None,
            layout: None,
            width: None,
            refused: None,
        }
        .key(unit)
        .unwrap()
    };
    let mut open = orphan_front(false);
    let mut sealed = orphan_front(true);

    let (in_graph, before) = sealed_diags(&open.lowered, 2);
    assert!(!in_graph, "no crate of `d`'s graph seals a trait yet");
    assert!(before.is_empty(), "a reached-only crate with no sealed graph runs no check: {before:#?}");
    let (in_graph, after) = sealed_diags(&sealed.lowered, 2);
    assert!(in_graph, "`l` seals `S` now");
    assert!(after.iter().any(|it| it.is_error() && it.code.as_ref().and_then(|it| it.error()) == Some("E0117")), "{after:#?}");

    assert_ne!(key(&mut open, 2), key(&mut sealed, 2), "`d`'s record key moves when `l` gains `#[blaze::sealed]`");
    assert_eq!(key(&mut open, 0), key(&mut sealed, 0), "`up` depends on nothing that changed");
}

/// `d`'s build script's run, giving the cfgs `cfgs`.
fn d_ran(cfgs: &[&str]) -> blaze_lower::Ran {
    blaze_lower::Ran { cfgs: strings(cfgs), env: Vec::new(), out_dir: D_OUT.to_owned(), files: Vec::new() }
}

/// The value items of `krate`'s root, by name, sorted: what its dependents resolve in it.
fn root_values(lowered: &blaze_lower::Lowered, krate: base_db::Crate) -> Vec<String> {
    let db = lowered.db();
    let def_map = hir_def::nameres::crate_def_map(db, krate);
    let mut names: Vec<String> =
        def_map.module(db, def_map.crate_root(db)).scope(db).values().map(|(name, _)| name.as_str().to_owned()).collect();
    names.sort();
    names
}

/// A dependency read from its image whose build script's output moves while its front end is
/// kept (a variable only the script reads, such as `CC`, changed: the front end is kept, the
/// script runs again and gives another cfg) is served again under its new key, so it resolves
/// with the cfg its run gives now, never as its image of the old cfg has it (rule 18). Without
/// the serve after the run is applied, what a kept job did before, the old cfg's def map is
/// what every dependent resolves against. The new cfg's image is then written, and a run giving
/// either cfg again finds its image.
#[test]
fn a_served_dependency_whose_scripts_cfg_moves_is_served_again_not_stale() {
    let mut sim = blaze_core::sim::Sim::new(1, blaze_core::sim::Faults::default());
    let mut store = super::deps::tests::opened(&mut sim);
    // A reader sees what is put after it is made.
    let reader = store.reader();
    let macros = super::deps::tests::no_macros();
    let build = ContentId::of("blaze v2 cargo test build", b"a served dependency's script moves");
    let mode = hir_def::deps::Mode::Serve;
    let mut keep = |lowered: &blaze_lower::Lowered, session: &mut super::deps::Session, served: hir_def::deps::Served| {
        let kept = super::deps::Kept { served, mode };
        let (puts, _) = super::deps::store(lowered, Some(&kept), session, &[], "/w").unwrap();
        for it in puts {
            store.put(it.put.key, it.put.kind, &it.put.bytes).unwrap();
        }
    };

    // One daemon resolves `d` as its script's first run leaves it, and images it.
    let mut first = script_cfg_front();
    first.ran(1, &d_ran(&[])).unwrap();
    let d = first.krate(2).unwrap();
    let mut session = super::deps::session(&first, &reader, &macros, &[]).unwrap();
    let served = super::deps::images(&mut first, &[d], build, &mut session, mode).unwrap().expect("`d` is keyed");
    assert_eq!(served.hits, Vec::new(), "a cold store");
    assert_eq!(root_values(&first, d), ["slow"]);
    keep(&first, &mut session, served);
    drop((session, first));

    // The next reads `d` from that image.
    let mut kept = script_cfg_front();
    kept.ran(1, &d_ran(&[])).unwrap();
    let d = kept.krate(2).unwrap();
    let mut session = super::deps::session(&kept, &reader, &macros, &[]).unwrap();
    let served = super::deps::images(&mut kept, &[d], build, &mut session, mode).unwrap().expect("`d` is keyed");
    assert_eq!(served.hits, [d], "`d` is read from its image");
    assert_eq!(root_values(&kept, d), ["slow"]);
    assert!(super::deps::images(&mut kept, &[d], build, &mut session, mode).unwrap().is_none(), "no key moved");

    // Its front end is kept for a build whose `CC` moved: the script's run gives `fast` now.
    kept.ran(1, &d_ran(&["fast"])).unwrap();
    let fast = cfg::CfgAtom::Flag(intern::Symbol::intern("fast"));
    assert!(d.cfg_options(kept.db()).check_atom(&fast), "the run's cfg is applied to `d`");
    assert_eq!(root_values(&kept, d), ["slow"], "unserved, `d` resolves as its image of the old cfg has it");
    let served = super::deps::images(&mut kept, &[d], build, &mut session, mode).unwrap().expect("`d`'s key moved");
    assert_eq!(served.hits, Vec::new(), "no image of `d` with `fast`");
    assert!(hir_def::deps::image_of(kept.db(), d).is_none(), "`d` lost the image of the old cfg");
    assert_eq!(root_values(&kept, d), ["fast"], "`d` resolves with the cfg its run gives now");
    keep(&kept, &mut session, served);

    // Either cfg again finds its own image.
    for (cfgs, values) in [(&[][..], ["slow"]), (&["fast"][..], ["fast"])] {
        kept.ran(1, &d_ran(cfgs)).unwrap();
        let served = super::deps::images(&mut kept, &[d], build, &mut session, mode).unwrap().expect("`d`'s key moved");
        assert_eq!(served.hits, [d], "{cfgs:?}");
        assert_eq!(root_values(&kept, d), values, "{cfgs:?}");
    }
}

/// A serve at the view the last serve left walks no key: what that serve wrote is no input of
/// a key, so each crate's key is the one the session holds (the no-op rebuild's 2 or 3 walks
/// per build, spans36). One write in between (the script's run applied again with another
/// cfg) brings the walk back, and a serve that walked and moved nothing is skipped after too.
/// Without the skip in `deps::images`, each of those serves walks again.
#[test]
fn a_serve_at_the_view_the_last_serve_left_walks_no_key_and_a_write_brings_the_walk_back() {
    let mut sim = blaze_core::sim::Sim::new(1, blaze_core::sim::Faults::default());
    let store = super::deps::tests::opened(&mut sim);
    let reader = store.reader();
    let macros = super::deps::tests::no_macros();
    let build = ContentId::of("blaze v2 cargo test build", b"a serve at the view the last one left");
    let mode = hir_def::deps::Mode::Serve;

    let mut front = script_cfg_front();
    front.ran(1, &d_ran(&[])).unwrap();
    let d = front.krate(2).unwrap();
    let mut session = super::deps::session(&front, &reader, &macros, &[]).unwrap();
    assert!(super::deps::images(&mut front, &[d], build, &mut session, mode).unwrap().is_some(), "`d` is keyed");
    assert_eq!(session.walks(), 1);
    // As `deps::serve` ends once it served.
    super::deps::stamp(&front, build, &mut session);
    assert!(super::deps::images(&mut front, &[d], build, &mut session, mode).unwrap().is_none(), "no key moved");
    assert_eq!(session.walks(), 1, "no write since the serve that keyed `d`: no walk");

    front.ran(1, &d_ran(&["fast"])).unwrap();
    assert!(super::deps::images(&mut front, &[d], build, &mut session, mode).unwrap().is_some(), "`d`'s key moved");
    assert_eq!(session.walks(), 2, "a write: the walk runs again");
    super::deps::stamp(&front, build, &mut session);

    // Writes that leave every key as it was: walked once, then skipped at that view.
    front.ran(1, &d_ran(&["fast", "other"])).unwrap();
    front.ran(1, &d_ran(&["fast"])).unwrap();
    assert!(super::deps::images(&mut front, &[d], build, &mut session, mode).unwrap().is_none(), "the cfg is back");
    assert_eq!(session.walks(), 3, "writes came between: walked once");
    assert!(super::deps::images(&mut front, &[d], build, &mut session, mode).unwrap().is_none());
    assert_eq!(session.walks(), 3, "the walk that moved nothing stands at its view");
}

/// `d`'s library, whose items are a file its build script writes to `OUT_DIR`.
const D_INCLUDES: &[u8] = b"#[rustc_builtin_macro]\nmacro_rules! include { () => {} }\n\
#[rustc_builtin_macro]\nmacro_rules! concat { () => {} }\n\
#[rustc_builtin_macro]\nmacro_rules! env { () => {} }\n\
include!(concat!(env!(\"OUT_DIR\"), \"/gen.rs\"));\n";

/// `d`'s build script's run, with the same cfg and variables each time, leaving `generated` as
/// `OUT_DIR/gen.rs`.
fn d_generated(generated: &str) -> blaze_lower::Ran {
    blaze_lower::Ran {
        cfgs: Vec::new(),
        env: Vec::new(),
        out_dir: D_OUT.to_owned(),
        files: vec![("gen.rs".to_owned(), generated.as_bytes().to_vec())],
    }
}

/// A dependency read from its image whose build script runs again with the same cfg and
/// variables and changes only a file in `OUT_DIR` that the crate `include!`s is served again
/// under a new key, never left on its image of the old file (rule 18): a run's files are put
/// in its package's source root (`blaze_lower`'s `apply_run`), each of whose files' text the
/// key covers (`hir_def::deps::key`'s `files`), so the file is in the key, not only checked
/// when the image is fetched.
#[test]
fn a_served_dependency_whose_out_dir_file_alone_moves_is_served_again_not_stale() {
    let mut sim = blaze_core::sim::Sim::new(1, blaze_core::sim::Faults::default());
    let mut store = super::deps::tests::opened(&mut sim);
    let reader = store.reader();
    let macros = super::deps::tests::no_macros();
    let build = ContentId::of("blaze v2 cargo test build", b"a served dependency's OUT_DIR file moves");
    let mode = hir_def::deps::Mode::Serve;

    // One daemon resolves `d` with its first `gen.rs`, and images it.
    let mut first = script_lib_front(D_INCLUDES);
    first.ran(1, &d_generated("pub fn one() {}\n")).unwrap();
    let d = first.krate(2).unwrap();
    let mut session = super::deps::session(&first, &reader, &macros, &[]).unwrap();
    let served = super::deps::images(&mut first, &[d], build, &mut session, mode).unwrap().expect("`d` is keyed");
    assert_eq!(root_values(&first, d), ["one"]);
    let kept = super::deps::Kept { served, mode };
    let (puts, _) = super::deps::store(&first, Some(&kept), &mut session, &[], "/w").unwrap();
    for it in puts {
        store.put(it.put.key, it.put.kind, &it.put.bytes).unwrap();
    }
    drop((session, first));

    // The next reads `d` from that image.
    let mut kept = script_lib_front(D_INCLUDES);
    kept.ran(1, &d_generated("pub fn one() {}\n")).unwrap();
    let d = kept.krate(2).unwrap();
    let mut session = super::deps::session(&kept, &reader, &macros, &[]).unwrap();
    let served = super::deps::images(&mut kept, &[d], build, &mut session, mode).unwrap().expect("`d` is keyed");
    assert_eq!(served.hits, [d], "`d` is read from its image");
    assert_eq!(root_values(&kept, d), ["one"]);
    super::deps::stamp(&kept, build, &mut session);

    // Its script runs again with the same cfg and variables, and writes another `gen.rs`.
    kept.ran(1, &d_generated("pub fn two() {}\n")).unwrap();
    let served = super::deps::images(&mut kept, &[d], build, &mut session, mode)
        .unwrap()
        .expect("`d`'s key moved with its `OUT_DIR` file");
    assert_eq!(served.hits, Vec::new(), "no image of `d` with this `gen.rs`");
    assert!(hir_def::deps::image_of(kept.db(), d).is_none(), "`d` lost the image of the old `gen.rs`");
    assert_eq!(root_values(&kept, d), ["two"], "`d` resolves with the file its run left now");
}

/// A proc macro can read a file outside the source tree (a schema or a `.proto` beside the
/// workspace), which no input of the record's base key names. The record is filed by what the
/// expansion read, so a lookup after the file changed misses, with no macro run to find out,
/// and finds it again once the file is as it was.
#[test]
fn a_file_a_proc_macro_read_outside_the_tree_makes_the_record_miss_once_it_changes() {
    let run = [("BLAZE_PROFILE", "/tmp/blaze-lanes/profile-0101")];
    let (lowered, base) = script_lowered(SCRIPT, true, &run);
    let krate = lowered.krate(1).unwrap();
    let outside = tempfile::tempdir().unwrap();
    let schema = outside.path().join("schema.sql");
    let made: &[u8] = b"create table a (x int);";
    std::fs::write(&schema, made).unwrap();
    let hash = blaze_usercode::inputs::hash_reader(made).unwrap();
    let traced = blaze_core::Traced { files: vec![(schema.clone(), Some(hash))], ..blaze_core::Traced::default() };
    let shape = blaze_usercode::index::Shape::of(&traced).encode();
    // What the daemon's `proc_macro_api::kept::Keeping::reads_now` works out, with no variable passed.
    let now = |env: &base_db::Env, shape: &[u8]| proc_macro_api::kept::reads_in(&[], env, shape);
    let db = lowered.db();
    // As a crate served by its reads notes them (`hir_def::deps::filed::search`): the file's
    // names with the values they had. An expansion that runs notes the same under its call.
    let read = base_db::ExpansionRead::Traced { shape: shape.as_slice().into(), values: now(krate.env(db), &shape).unwrap() };
    let served = base_db::ReadBy::Served(ContentId::of("blaze v2 cargo test image", b"script"));
    db.files().expansions_are_read(krate, served, vec![read]);
    let read = super::reads::Reads::of(db, krate).unwrap();
    let env = super::reads::tests::env(&run);
    let mut store = super::reads::tests::Store::default();
    let filed = store.keep(base, &read, &env);
    assert_ne!(filed, base, "a record whose expansion read a file is filed by it");
    assert_eq!(store.find_now(base, &env, now).map(|it| it.key), Some(filed), "the file as it was");
    std::fs::write(&schema, b"create table a (x int, y int);").unwrap();
    assert_eq!(store.find_now(base, &env, now), None, "the file changed: the record misses");
    std::fs::write(&schema, made).unwrap();
    assert_eq!(store.find_now(base, &env, now).map(|it| it.key), Some(filed), "the file as it was again");
}

/// A span written in blaze-cargo with no target of its own has its module's path for one,
/// `blaze_cargo::..`, and the flight recorder keeps it: a phase span here needs no
/// `target: "blaze"` to be seen.
#[test]
fn a_span_with_no_target_written_in_blaze_cargo_reaches_the_flight_ring() {
    const NAME: &str = "a blaze-cargo span with no target of its own";
    blaze_profile::install();
    std::thread::spawn(|| drop(tracing::info_span!(NAME).entered())).join().unwrap();
    let mut read = blaze_profile::flight::Read::default();
    blaze_profile::flight::window(std::time::Duration::from_secs(60), &mut read);
    let targets: Vec<&str> = read
        .spans
        .iter()
        .filter_map(|row| blaze_profile::flight::meta(row.site))
        .filter(|meta| meta.name() == NAME)
        .map(|meta| meta.target())
        .collect();
    assert_eq!(targets, [module_path!()], "the span is in the ring once, under its module's path");
    assert!(module_path!().starts_with("blaze_cargo::"));
}

/// A `warn!` written in blaze-cargo with no target of its own is kept by the recorder's one
/// rule for spans and events, and reaches the `events` table as one row: its level, its
/// module's path for a target, and its message with its fields. The daemon's watchdog line
/// ("no unit's step ended") is such a `warn!`.
#[test]
fn a_warn_with_no_target_written_in_blaze_cargo_reaches_the_events_table() {
    use arrow_array::{Array as _, cast::AsArray as _, types::UInt32Type};

    const MESSAGE: &str = "a blaze-cargo warn with no target of its own";
    blaze_profile::install();
    std::thread::spawn(|| tracing::warn!(steps = 3, "{MESSAGE}")).join().unwrap();
    let mut read = blaze_profile::flight::Read::default();
    blaze_profile::flight::window(std::time::Duration::from_secs(60), &mut read);
    let batch = blaze_profile::batches::events(&read.events, blaze_profile::batches::Process::this()).unwrap();
    let column = |name: &str| batch.column_by_name(name).unwrap_or_else(|| panic!("the events table has `{name}`"));
    let messages = column("message").as_string::<i32>();
    let rows: Vec<usize> = (0..batch.num_rows()).filter(|&row| messages.is_valid(row) && messages.value(row).contains(MESSAGE)).collect();
    assert_eq!(rows.len(), 1, "the warn is one row of the events table: {:?}", read.events);
    let row = rows[0];
    assert!(messages.value(row).contains("steps=3"), "its fields are in its message: {}", messages.value(row));
    let text = |name: &str| {
        let dict = column(name).as_dictionary::<UInt32Type>();
        dict.values().as_string::<i32>().value(dict.keys().value(row) as usize).to_owned()
    };
    assert_eq!(text("level"), "WARN");
    assert_eq!(text("target"), module_path!());
    assert!(text("name").starts_with("event "), "an event's name is its callsite: {}", text("name"));
    assert!(column("job").is_null(row) && column("span_id").is_null(row), "a thread of no pool, in no span");
}
