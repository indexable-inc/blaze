//! End-to-end integration proof of `MacroModule` self-compilation via `blaze-lir -> wasm64`
//! (`wasm64-blaze-wasip1`) with ZERO prebuilt `~/.blaze/v2/{build-script,proc-macro}/modules`
//! and ZERO `rustc` (`PATH=""`):
//!
//! 1. Pure-Rust `build.rs` with a workspace path build-dependency (`OUT_DIR`, `include!`,
//!    `cargo:rustc-cfg`, `cargo:rustc-env`), `blaze check` + `blaze run`, byte-identical
//!    determinism across distinct workspaces/UnitIds (Rule 25), and cache invalidation when the
//!    path build-dependency changes (Rule 21).
//! 2. Pure-Rust proc-macro crate with a workspace path library dependency exporting all three
//!    proc-macro kinds (`#[proc_macro]`, `#[proc_macro_attribute]`,
//!    `#[proc_macro_derive(..., attributes(...))]`), `blaze check` + `blaze run`, byte-identical
//!    determinism across workspaces (Rule 25), and invalidation when the path dep changes
//!    (Rule 21).
//! 3. Tier-2 refusals (unsupported native C/FFI symbol in `build.rs` and `RanAs::Native` `cc`
//!    build script) fail with clear, typed errors naming what and where — zero fallback to
//!    `rustc` or `toolrun.sh` (Rules 1 & 3).

mod support;

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use blaze_export::wasm64::check_wasm64;
use support::fresh_home;

const DEADLINE: Duration = Duration::from_secs(300);
const REAP: Duration = Duration::from_secs(10);

fn write_files(dir: &Path, files: &[(&str, &str)]) {
    for &(rel, text) in files {
        let at = dir.join(rel);
        std::fs::create_dir_all(at.parent().expect("parent dir"))
            .unwrap_or_else(|e| panic!("mkdir {}: {e}", at.display()));
        std::fs::write(&at, text)
            .unwrap_or_else(|e| panic!("write {}: {e}", at.display()));
    }
}

fn macos_sdkroot() -> Option<String> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    if let Ok(sdk) = std::env::var("SDKROOT")
        && !sdk.is_empty()
    {
        return Some(sdk);
    }
    let out = Command::new("/usr/bin/xcrun")
        .args(["--sdk", "macosx", "--show-sdk-path"])
        .output()
        .ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned()).filter(|s| !s.is_empty())
}

fn has_blaze_launcher() -> bool {
    Path::new(env!("CARGO_BIN_EXE_blaze"))
        .parent()
        .is_some_and(|dir| dir.join("blaze-launcher").is_file())
}

/// Executes package `pkg` via `blaze run -p <pkg>` when `blaze-launcher` is installed beside
/// `blaze`, and ALWAYS exports a standalone native binary via `blaze export -p <pkg> -o <bin>`
/// and runs it directly so this test stays self-contained in single-crate `cargo test -p blaze-cli`.
fn run_app(dir: &Path, home: &Path, tag: &str, pkg: &str) -> (bool, String) {
    if has_blaze_launcher() {
        let (ok, out) = run_blaze(dir, home, tag, &["run", "-p", pkg]);
        if !ok {
            return (false, out);
        }
        return (true, out);
    }
    let bin = dir.with_extension(format!("{tag}.bin"));
    let bin_str = bin.to_str().expect("utf8 bin path");
    let (ok, exp_out) = run_blaze(dir, home, tag, &["export", "-p", pkg, "-o", bin_str]);
    if !ok {
        return (false, exp_out);
    }
    let output = Command::new(&bin)
        .env_clear()
        .stdin(Stdio::null())
        .output()
        .unwrap_or_else(|e| panic!("exec {}: {e}", bin.display()));
    let _ = std::fs::remove_file(&bin);
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    (output.status.success(), combined)
}

/// Runs `blaze <args>` in `dir` with `HOME=home` and `PATH=""` (proving zero `rustc`/`toolrun.sh`).
fn run_blaze(dir: &Path, home: &Path, tag: &str, args: &[&str]) -> (bool, String) {
    let said = dir.with_extension(tag);
    let file = File::create(&said).unwrap_or_else(|e| panic!("create {}: {e}", said.display()));
    let file_err = file.try_clone().expect("clone output file");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_blaze"));
    cmd.args(args)
        .current_dir(dir)
        .env("HOME", home)
        .env("CARGO_HOME", home.join(".cargo"))
        .env("PWD", dir)
        .env("PATH", "")
        .stdin(Stdio::null())
        .stdout(Stdio::from(file))
        .stderr(Stdio::from(file_err));
    if let Some(sdk) = macos_sdkroot() {
        cmd.env("SDKROOT", sdk);
    } else {
        cmd.env_remove("SDKROOT");
    }
    let mut child = cmd.spawn().expect("spawn blaze");
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() > DEADLINE => {
                let _ = child.kill();
                let reaped = Instant::now();
                while child.try_wait().ok().flatten().is_none() && reaped.elapsed() < REAP {
                    std::thread::sleep(Duration::from_millis(5));
                }
                panic!("`blaze {:?}` in {} exceeded {DEADLINE:?}", args, dir.display());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("waiting for `blaze {:?}`: {e}", args);
            }
        }
    };
    let text = std::fs::read_to_string(&said)
        .unwrap_or_else(|e| panic!("read {}: {e}", said.display()));
    (status.success(), text)
}

fn published_modules(modules_dir: &Path) -> Vec<(String, PathBuf)> {
    let mut out: Vec<(String, PathBuf)> = std::fs::read_dir(modules_dir)
        .unwrap_or_else(|e| panic!("read_dir {}: {e}", modules_dir.display()))
        .filter_map(Result::ok)
        .filter(|e| e.path().join("module.wasm").is_file())
        .filter_map(|e| Some((e.file_name().into_string().ok()?, e.path())))
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn build_script_workspace(dir: &Path, magic: u32, with_extra_member: bool) {
    let members = if with_extra_member {
        "[\"aaa_extra\", \"build_helper\", \"app\"]"
    } else {
        "[\"build_helper\", \"app\"]"
    };
    let extra_lock = if with_extra_member {
        "[[package]]\nname = \"aaa_extra\"\nversion = \"0.1.0\"\n\n"
    } else {
        ""
    };
    let lock = format!(
        "version = 4\n\n{extra_lock}[[package]]\nname = \"app\"\nversion = \"0.1.0\"\ndependencies = [\n \"build_helper\",\n]\n\n[[package]]\nname = \"build_helper\"\nversion = \"0.1.0\"\n"
    );
    let helper_lib = format!("pub fn magic() -> u32 {{ {magic} }}\n");
    let build_rs = r#"use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    let val = build_helper::magic();
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    fs::write(out_dir.join("gen.rs"), format!("pub const GEN_VAL: u32 = {val};\n"))
        .expect("write gen.rs");
    println!("cargo:rustc-cfg=from_wasm64_bs");
    println!("cargo:rustc-env=WASM64_TAG=self_compiled_ok");
}
"#;
    let main_rs = r#"include!(concat!(env!("OUT_DIR"), "/gen.rs"));

#[cfg(from_wasm64_bs)]
fn tag() -> &'static str {
    env!("WASM64_TAG")
}

fn main() {
    println!("RESULT={}:{}", GEN_VAL, tag());
}
"#;
    let mut files = vec![
        ("Cargo.toml", format!("[workspace]\nmembers = {members}\nresolver = \"2\"\n")),
        ("Cargo.lock", lock),
        ("build_helper/Cargo.toml", "[package]\nname = \"build_helper\"\nversion = \"0.1.0\"\nedition = \"2021\"\n".to_owned()),
        ("build_helper/src/lib.rs", helper_lib),
        ("app/Cargo.toml", "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[build-dependencies]\nbuild_helper = { path = \"../build_helper\" }\n".to_owned()),
        ("app/build.rs", build_rs.to_owned()),
        ("app/src/main.rs", main_rs.to_owned()),
    ];
    if with_extra_member {
        files.push(("aaa_extra/Cargo.toml", "[package]\nname = \"aaa_extra\"\nversion = \"0.1.0\"\nedition = \"2021\"\n".to_owned()));
        files.push(("aaa_extra/src/lib.rs", "pub fn noop() -> u32 { 0 }\n".to_owned()));
    }
    let refs: Vec<(&str, &str)> = files.iter().map(|(k, v)| (*k, v.as_str())).collect();
    write_files(dir, &refs);
}

#[test]
fn pure_rust_build_script_self_compiles_runs_deterministically_and_invalidates_on_dep_edit() {
    let base_a = fresh_home("wasm64_bs_e2e_a", true);
    let (home_a, ws_a) = (base_a.join("home"), base_a.join("ws"));
    build_script_workspace(&ws_a, 42, false);

    // 1. Cold `blaze check` with empty module store and PATH="" self-compiles build.rs to wasm64.
    let (_, check_out) = run_blaze(&ws_a, &home_a, "check1", &["check"]);
    assert!(
        check_out.contains("0 errors: blaze check") && !check_out.contains("not checked because"),
        "cold blaze check with pure-Rust build.rs failed:\n{check_out}"
    );
    let mods_a = published_modules(&home_a.join(".blaze/v2/build-script/modules"));
    assert_eq!(mods_a.len(), 1, "expected 1 published wasm64 build-script module: {mods_a:?}");
    let wasm_a = std::fs::read(mods_a[0].1.join("module.wasm")).expect("read module.wasm");
    let checked = check_wasm64(&wasm_a).expect("published build.rs module must be valid wasm64");
    assert!(checked.memory64, "build.rs module must use memory64");
    let identity_a = std::fs::read_to_string(mods_a[0].1.join("identity")).expect("read identity");
    assert!(identity_a.contains("wasm-target wasm64-blaze-wasip1"), "identity missing wasm-target:\n{identity_a}");
    assert!(identity_a.contains("deps "), "identity missing build-dep digest:\n{identity_a}");
    assert!(
        !home_a.join(".blaze/v2/build-script/modules").read_dir().unwrap().filter_map(Result::ok).any(|e| e.file_name().to_string_lossy().starts_with('.')),
        "modules dir must not contain staging dot-entries"
    );

    // 2. `blaze run` / `blaze export` executes the compiled app using OUT_DIR include!, cfg, and env! from build.rs.
    let (ok_run, run_out) = run_app(&ws_a, &home_a, "run1", "app");
    assert!(ok_run && run_out.contains("RESULT=42:self_compiled_ok"), "blaze run failed:\n{run_out}");

    // 3. Determinism (Rule 25): a second workspace at a different path with an extra member
    //    (shifting UnitIds) emits byte-for-byte identical `module.wasm` under the same key.
    let base_b = fresh_home("wasm64_bs_e2e_b", true);
    let (home_b, ws_b) = (base_b.join("home"), base_b.join("nested/shifted/ws"));
    build_script_workspace(&ws_b, 42, true);
    let (_, check_b) = run_blaze(&ws_b, &home_b, "check_b", &["check"]);
    assert!(check_b.contains("0 errors: blaze check"), "shifted workspace check failed:\n{check_b}");
    let mods_b = published_modules(&home_b.join(".blaze/v2/build-script/modules"));
    assert_eq!(mods_b.len(), 1);
    assert_eq!(mods_a[0].0, mods_b[0].0, "build-script module key must be workspace-path & UnitId independent");
    let wasm_b = std::fs::read(mods_b[0].1.join("module.wasm")).expect("read module.wasm b");
    assert_eq!(wasm_a, wasm_b, "self-compiled wasm64 build.rs bytes must be byte-identical across workspaces (Rule 25)");

    // 4. Key completeness (Rule 21): editing the path build-dependency invalidates the module key
    //    and recompiles `build.rs` with the updated value.
    std::fs::write(ws_a.join("build_helper/src/lib.rs"), "pub fn magic() -> u32 { 99 }\n").expect("edit build_helper");
    let (ok_run2, run_out2) = run_app(&ws_a, &home_a, "run2", "app");
    assert!(ok_run2 && run_out2.contains("RESULT=99:self_compiled_ok"), "blaze run after build_helper edit failed:\n{run_out2}");
    let mods_a2 = published_modules(&home_a.join(".blaze/v2/build-script/modules"));
    assert_eq!(mods_a2.len(), 2, "editing build_helper must publish a new build-script module key: {mods_a2:?}");

    let _ = std::fs::remove_dir_all(base_a);
    let _ = std::fs::remove_dir_all(base_b);
}

fn proc_macro_workspace(dir: &Path, offset: u32, attr_val: u32, with_extra_member: bool) {
    let members = if with_extra_member {
        "[\"aaa_extra\", \"mac_helper\", \"mac\", \"app\"]"
    } else {
        "[\"mac_helper\", \"mac\", \"app\"]"
    };
    let extra_lock = if with_extra_member {
        "[[package]]\nname = \"aaa_extra\"\nversion = \"0.1.0\"\n\n"
    } else {
        ""
    };
    let lock = format!(
        "version = 4\n\n{extra_lock}[[package]]\nname = \"app\"\nversion = \"0.1.0\"\ndependencies = [\n \"mac\",\n]\n\n[[package]]\nname = \"mac\"\nversion = \"0.1.0\"\ndependencies = [\n \"mac_helper\",\n]\n\n[[package]]\nname = \"mac_helper\"\nversion = \"0.1.0\"\n"
    );
    let helper_lib = format!(
        "pub const OFFSET: u32 = {offset};\npub const ATTR_VAL: u32 = {attr_val};\n"
    );
    let mac_lib = r#"use proc_macro::{TokenStream, TokenTree};

#[proc_macro]
pub fn doubled(input: TokenStream) -> TokenStream {
    let expr = input.to_string();
    let offset = mac_helper::OFFSET;
    format!("(({expr}) * 2 + {offset}u32)").parse().expect("parse doubled expr")
}

#[proc_macro_attribute]
pub fn add_tag(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let val = mac_helper::ATTR_VAL;
    let extra: TokenStream = format!("const ATTR_TAG: u32 = {val};").parse().expect("parse ATTR_TAG");
    let mut out = item;
    out.extend(extra);
    out
}

#[proc_macro_derive(Describe, attributes(describe_tag))]
pub fn derive_describe(input: TokenStream) -> TokenStream {
    let mut saw_struct = false;
    let mut name = String::new();
    for tt in input {
        if let TokenTree::Ident(ident) = tt {
            let s = ident.to_string();
            if saw_struct {
                name = s;
                break;
            }
            if s == "struct" {
                saw_struct = true;
            }
        }
    }
    assert!(!name.is_empty(), "expected struct name in derive(Describe)");
    format!("impl Describe for {name} {{ fn label(&self) -> &'static str {{ \"{name}\" }} }}")
        .parse()
        .expect("parse derive_describe impl")
}
"#;
    let app_main = r#"trait Describe {
    fn label(&self) -> &'static str;
}

#[derive(mac::Describe)]
#[describe_tag]
struct Widget;

#[mac::add_tag]
fn marker() {}

fn main() {
    marker();
    let w = Widget;
    let v: u32 = mac::doubled!(20);
    println!("PM={}:{}:{}", w.label(), ATTR_TAG, v);
}
"#;
    let mut files = vec![
        ("Cargo.toml", format!("[workspace]\nmembers = {members}\nresolver = \"2\"\n")),
        ("Cargo.lock", lock),
        ("mac_helper/Cargo.toml", "[package]\nname = \"mac_helper\"\nversion = \"0.1.0\"\nedition = \"2021\"\n".to_owned()),
        ("mac_helper/src/lib.rs", helper_lib),
        ("mac/Cargo.toml", "[package]\nname = \"mac\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\nproc-macro = true\n\n[dependencies]\nmac_helper = { path = \"../mac_helper\" }\n".to_owned()),
        ("mac/src/lib.rs", mac_lib.to_owned()),
        ("app/Cargo.toml", "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\nmac = { path = \"../mac\" }\n".to_owned()),
        ("app/src/main.rs", app_main.to_owned()),
    ];
    if with_extra_member {
        files.push(("aaa_extra/Cargo.toml", "[package]\nname = \"aaa_extra\"\nversion = \"0.1.0\"\nedition = \"2021\"\n".to_owned()));
        files.push(("aaa_extra/src/lib.rs", "pub fn noop() -> u32 { 0 }\n".to_owned()));
    }
    let refs: Vec<(&str, &str)> = files.iter().map(|(k, v)| (*k, v.as_str())).collect();
    write_files(dir, &refs);
}

#[test]
fn pure_rust_proc_macro_derive_attr_fn_like_with_path_dep_self_compiles_and_runs() {
    let base_a = fresh_home("wasm64_pm_e2e_a", true);
    let (home_a, ws_a) = (base_a.join("home"), base_a.join("ws"));
    proc_macro_workspace(&ws_a, 5, 7, false);

    // 1. Cold `blaze check` with empty proc-macro store and PATH="" self-compiles `mac` to wasm64.
    let (_, check_out) = run_blaze(&ws_a, &home_a, "check1", &["check"]);
    assert!(
        !check_out.contains("error[") && !check_out.contains("error:") && !check_out.contains("not checked because"),
        "cold blaze check with derive+attr+fn-like proc-macro failed:\n{check_out}"
    );
    let mods_a = published_modules(&home_a.join(".blaze/v2/proc-macro/modules"));
    assert_eq!(mods_a.len(), 1, "expected 1 published wasm64 proc-macro module: {mods_a:?}");
    let wasm_a = std::fs::read(mods_a[0].1.join("module.wasm")).expect("read module.wasm");
    let checked = check_wasm64(&wasm_a).expect("published proc-macro module must be valid wasm64");
    assert!(checked.memory64, "proc-macro module must use memory64");

    // 2. `blaze run` / `blaze export` executes the code generated by all 3 proc-macro kinds!
    let (ok_run, run_out) = run_app(&ws_a, &home_a, "run1", "app");
    assert!(ok_run && run_out.contains("PM=Widget:7:45"), "blaze run with proc-macros failed:\n{run_out}");

    // 3. Determinism (Rule 25): a second workspace at a different path with shifted UnitIds
    //    produces the exact same key and byte-for-byte identical `module.wasm`.
    let base_b = fresh_home("wasm64_pm_e2e_b", true);
    let (home_b, ws_b) = (base_b.join("home"), base_b.join("nested/shifted/ws"));
    proc_macro_workspace(&ws_b, 5, 7, true);
    let (_, check_b) = run_blaze(&ws_b, &home_b, "check_b", &["check"]);
    assert!(
        !check_b.contains("error[") && !check_b.contains("error:") && !check_b.contains("not checked because"),
        "shifted proc-macro check failed:\n{check_b}"
    );
    let mods_b = published_modules(&home_b.join(".blaze/v2/proc-macro/modules"));
    assert_eq!(mods_b.len(), 1);
    assert_eq!(mods_a[0].0, mods_b[0].0, "proc-macro key must be workspace-path & UnitId independent");
    let wasm_b = std::fs::read(mods_b[0].1.join("module.wasm")).expect("read module.wasm b");
    assert_eq!(wasm_a, wasm_b, "self-compiled wasm64 proc-macro bytes must be byte-identical across workspaces (Rule 25)");

    // 4. Key completeness (Rule 21): editing `mac_helper/src/lib.rs` (a path dep under the
    //    proc-macro crate) invalidates the module key and recompiles `mac`.
    std::fs::write(
        ws_a.join("mac_helper/src/lib.rs"),
        "pub const OFFSET: u32 = 10;\npub const ATTR_VAL: u32 = 11;\n",
    )
    .expect("edit mac_helper");
    let (ok_run2, run_out2) = run_app(&ws_a, &home_a, "run2", "app");
    assert!(ok_run2 && run_out2.contains("PM=Widget:11:50"), "blaze run after mac_helper edit failed:\n{run_out2}");
    let mods_a2 = published_modules(&home_a.join(".blaze/v2/proc-macro/modules"));
    assert_eq!(mods_a2.len(), 2, "editing mac_helper must publish a new proc-macro module key: {mods_a2:?}");

    let _ = std::fs::remove_dir_all(base_a);
    let _ = std::fs::remove_dir_all(base_b);
}

#[test]
fn tier2_native_build_script_and_unresolved_ffi_fail_with_typed_errors() {
    // 1. Pure-Rust build.rs calling an unsupported native C FFI symbol fails with a typed Tier-2 error.
    let base_ffi = fresh_home("wasm64_tier2_ffi", true);
    let (home_ffi, ws_ffi) = (base_ffi.join("home"), base_ffi.join("ws"));
    write_files(
        &ws_ffi,
        &[
            ("Cargo.toml", "[workspace]\nmembers = [\"app\"]\nresolver = \"2\"\n"),
            ("Cargo.lock", "version = 4\n\n[[package]]\nname = \"app\"\nversion = \"0.1.0\"\n"),
            ("app/Cargo.toml", "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
            (
                "app/build.rs",
                "unsafe extern \"C\" { fn nonexistent_tier2_native_sym() -> i32; }\nfn main() { let _ = unsafe { nonexistent_tier2_native_sym() }; }\n",
            ),
            ("app/src/main.rs", "fn main() {}\n"),
        ],
    );
    let (ok_ffi, out_ffi) = run_blaze(&ws_ffi, &home_ffi, "check_ffi", &["check"]);
    assert!(!ok_ffi, "unresolved native FFI build.rs must fail:\n{out_ffi}");
    assert!(
        out_ffi.contains("Tier-2 unsupported native C/FFI symbol")
            && out_ffi.contains("nonexistent_tier2_native_sym"),
        "expected typed Tier-2 FFI error naming symbol:\n{out_ffi}"
    );
    assert!(!out_ffi.contains("toolrun.sh"), "error must not suggest toolrun.sh:\n{out_ffi}");
    let _ = std::fs::remove_dir_all(base_ffi);

    // 2. Tier-2 native build script (`cc` build-dependency on a known native-every-mode crate
    //    `sha3-asm`) with empty module store fails with a typed Tier-2 native error, no toolrun.sh.
    let base_nat = fresh_home("wasm64_tier2_native", true);
    let (home_nat, ws_nat) = (base_nat.join("home"), base_nat.join("ws"));
    write_files(
        &ws_nat,
        &[
            ("Cargo.toml", "[package]\nname = \"sha3-asm\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[build-dependencies]\ncc = { path = \"cc\" }\n[workspace]\nmembers = [\"cc\"]\nresolver = \"2\"\n"),
            ("Cargo.lock", "version = 4\n[[package]]\nname = \"cc\"\nversion = \"0.1.0\"\n[[package]]\nname = \"sha3-asm\"\nversion = \"0.1.0\"\ndependencies = [\"cc\"]\n"),
            ("cc/Cargo.toml", "[package]\nname = \"cc\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
            ("cc/src/lib.rs", ""),
            ("src/lib.rs", ""),
            ("build.rs", "fn main() {}\n"),
        ],
    );
    let (ok_nat, out_nat) = run_blaze(&ws_nat, &home_nat, "check_nat", &["check"]);
    assert!(!ok_nat, "Tier-2 native build script without binary must fail:\n{out_nat}");
    assert!(
        out_nat.contains("Tier-2 native") && out_nat.contains("sha3-asm 0.1.0"),
        "expected typed Tier-2 native error naming crate:\n{out_nat}"
    );
    assert!(!out_nat.contains("toolrun.sh"), "Tier-2 native error must not suggest toolrun.sh:\n{out_nat}");
    let _ = std::fs::remove_dir_all(base_nat);
}
