#![forbid(unsafe_code)]

use std::{
    ffi::{OsStr, OsString},
    path::PathBuf,
    sync::OnceLock,
};

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
        mode,
        checked_as_test: false,
        features: vec!["default".to_owned()],
        is_std: false,
        dependencies: deps
            .iter()
            .map(|it| UnitDep { index: *it, extern_crate_name: "dep".to_owned(), public: None, noprelude: None, nounused: None })
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

fn slots(units: &Units) -> Vec<OnceLock<script::Applied>> {
    units.graph.units.iter().map(|_| OnceLock::new()).collect()
}

fn value(code: i32, stdout: &str, stderr: &str) -> Value {
    Value { code: Some(code), signal: None, stdout: stdout.as_bytes().to_vec(), stderr: stderr.as_bytes().to_vec(), files: Vec::new() }
}

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

#[test]
fn a_directive_the_sandbox_refuses_fails_the_build() {
    let dir = tempfile::tempdir().unwrap();
    let units = build(dir.path());
    let scripts = slots(&units);
    let tools = PathBuf::from("/state/tools");
    let z = script::script_unit(&of(&units, &scripts, &tools), 1).unwrap();
    let printed = "cargo:rustc-link-search=native=/home/me/lib\n";
    let judged = script::judge(&z, &[], &value(0, printed, ""), Some(&[]), None, false);
    assert!(judged.output.is_none());
    assert!(judged.diagnostics[0].message.starts_with("the build script of `z-sys` printed a directive"));
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
    let [warning, error] = judged.diagnostics.as_slice() else { panic!("{:?}", judged.diagnostics) };
    assert_eq!(warning.rendered, "warning: z-sys@0.1.0: no zlib\n");
    assert_eq!(error.level, Level::Error);
    assert_eq!(
        error.message,
        "failed to run custom build command for `z-sys v0.1.0 (/w/z-sys)`\n\nCaused by:\n  process didn't exit \
         successfully: `/w/target/debug/build/x/build-script-build` (exit status: 101)\n  --- stdout\n  \
         cargo:warning=no zlib\n  \n  --- stderr\n  thread 'main' panicked"
    );
    assert!(error.rendered.starts_with("error: failed to run custom build command for `z-sys v0.1.0 (/w/z-sys)`\n"));

    // An error it logged fails it too, under cargo's cause.
    let judged = script::judge(&z, &[], &value(0, "cargo::error=no zlib\n", ""), Some(&[]), None, true);
    let [logged, error] = judged.diagnostics.as_slice() else { panic!("{:?}", judged.diagnostics) };
    assert_eq!(logged.message, "z-sys@0.1.0: no zlib");
    assert_eq!(logged.rendered, "\x1b[1m\x1b[91merror\x1b[0m\x1b[1m:\x1b[0m z-sys@0.1.0: no zlib\n");
    assert_eq!(
        error.message,
        "failed to run custom build command for `z-sys v0.1.0 (/w/z-sys)`\n\nCaused by:\n  build script logged errors"
    );
}

#[test]
fn a_script_sees_the_jobs_environment_with_cargos_over_it_and_none_of_it_is_kept() {
    let job = vec![
        ("PATH".to_owned(), "/usr/bin".to_owned()),
        ("SECRET_TOKEN".to_owned(), "hunter2".to_owned()),
        ("CARGO_PKG_NAME".to_owned(), "wrong".to_owned()),
    ];
    let dir = tempfile::tempdir().unwrap();
    let units = build(dir.path());
    let env = script::inherited(&job, &units.plans[1]);
    let get = |name: &str| env.iter().find(|(it, _)| it == name).map(|(_, value)| value.clone());
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
    let said = blaze_protocol::cbor::to_vec(&judged.diagnostics).unwrap();
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
    compile::header::write(&mut bytes, &[one, two], &[("/usr/lib/libz.so".to_owned(), lib)]);
    let body = bytes.len();
    bytes.extend_from_slice(b"cbor");
    let same = |path: &std::path::Path| (path == std::path::Path::new("/usr/lib/libz.so")).then_some([7; 32]);
    let changed = |_: &std::path::Path| Some([8; 32]);
    assert_eq!(compile::header::check(&bytes, |_| true, &mut { same }), Some(body));
    assert_eq!(compile::header::check(&bytes, |key| *key != two, &mut { same }), None, "an output gone");
    assert_eq!(compile::header::check(&bytes, |_| true, &mut { changed }), None, "a library changed");
    assert_eq!(compile::header::check(&bytes[..body - 3], |_| true, &mut { same }), None, "cut short");
    assert_eq!(compile::header::check(b"nope", |_| true, &mut { same }), None);
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
    let goals = [Goal::Check, Goal::Build, Goal::Test, Goal::Run, Goal::Reach];
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
/// memory: the proc macro is unit 2.
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
    };
    blaze_lower::lower(blaze_lower::Lower { units: &units, config: &config, files: &tree, root: view, host_cpu: None }).unwrap()
}

/// A proc macro's lib builds no code: its expanders run its MIR and find its macros from its
/// def map (`blaze_macro_eval::load`), so nothing is appended to its root, which is the file on
/// disk itself, and the def map has each macro.
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
