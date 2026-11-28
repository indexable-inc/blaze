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

fn cleanup(dir: &Path) {
    let _ = std::fs::remove_dir_all(dir);
}

fn read_bytes(path: &Path) -> Vec<u8> {
    std::fs::read(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn dir_entry_count(dir: &Path) -> Option<usize> {
    let io = bstd::test::native(dir).ok()?;
    let cx = io.cx(bstd::test::BUDGET, bstd::mem::TASK_FRAME).ok()?;
    let fs = io.need_fs().ok()?;
    Some(fs.root().own_entries(&cx).ok()?.len())
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
    let wasm_a = read_bytes(&mods_a[0].1.join("module.wasm"));
    let checked = check_wasm64(&wasm_a).expect("published build.rs module must be valid wasm64");
    assert!(checked.memory64, "build.rs module must use memory64");
    let identity_a = String::from_utf8(read_bytes(&mods_a[0].1.join("identity"))).expect("utf8 identity");
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
    let wasm_b = read_bytes(&mods_b[0].1.join("module.wasm"));
    assert_eq!(wasm_a, wasm_b, "self-compiled wasm64 build.rs bytes must be byte-identical across workspaces (Rule 25)");

    // 4. Key completeness (Rule 21): editing the path build-dependency invalidates the module key
    //    and recompiles `build.rs` with the updated value.
    write_files(&ws_a, &[("build_helper/src/lib.rs", "pub fn magic() -> u32 { 99 }\n")]);
    let (ok_run2, run_out2) = run_app(&ws_a, &home_a, "run2", "app");
    assert!(ok_run2 && run_out2.contains("RESULT=99:self_compiled_ok"), "blaze run after build_helper edit failed:\n{run_out2}");
    let mods_a2 = published_modules(&home_a.join(".blaze/v2/build-script/modules"));
    assert_eq!(mods_a2.len(), 2, "editing build_helper must publish a new build-script module key: {mods_a2:?}");

    cleanup(&base_a);
    cleanup(&base_b);
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
    let wasm_a = read_bytes(&mods_a[0].1.join("module.wasm"));
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
    let wasm_b = read_bytes(&mods_b[0].1.join("module.wasm"));
    assert_eq!(wasm_a, wasm_b, "self-compiled wasm64 proc-macro bytes must be byte-identical across workspaces (Rule 25)");

    // 4. Key completeness (Rule 21): editing `mac_helper/src/lib.rs` (a path dep under the
    //    proc-macro crate) invalidates the module key and recompiles `mac`.
    write_files(
        &ws_a,
        &[("mac_helper/src/lib.rs", "pub const OFFSET: u32 = 10;\npub const ATTR_VAL: u32 = 11;\n")],
    );
    let (ok_run2, run_out2) = run_app(&ws_a, &home_a, "run2", "app");
    assert!(ok_run2 && run_out2.contains("PM=Widget:11:50"), "blaze run after mac_helper edit failed:\n{run_out2}");
    let mods_a2 = published_modules(&home_a.join(".blaze/v2/proc-macro/modules"));
    assert_eq!(mods_a2.len(), 2, "editing mac_helper must publish a new proc-macro module key: {mods_a2:?}");

    cleanup(&base_a);
    cleanup(&base_b);
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
    cleanup(&base_ffi);

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
    cleanup(&base_nat);
}

/// Blocker C4 end-to-end proof: a build script depends on `rustix_like`, whose host-OS
/// `[target.'cfg(...)'.dependencies]` omits `fake_libc` and renamed `libc_errno` (`fake_errno`),
/// while on `wasm64-blaze-wasip1` (`target_os = "wasi"`) `rustix_like`'s build.rs emits
/// `use_libc_backend` and compiles against `fake_libc` (with its own `build.rs` + weak feature
/// `fake_libc?/std`) and renamed `libc_errno` (`package = "fake_errno"`, with weak feature
/// `libc_errno?/std` and its own WASI dep on `fake_libc`).
#[test]
fn build_script_with_wasi_only_target_specific_renamed_deps_and_dep_build_script_compiles_and_invalidates() {
    let base = fresh_home("wasm64_wasi_target_deps_e2e", true);
    let (home, ws) = (base.join("home"), base.join("ws"));
    let lock = r#"version = 4

[[package]]
name = "app"
version = "0.1.0"
dependencies = [
 "rustix_like",
]

[[package]]
name = "fake_errno"
version = "0.3.14"
dependencies = [
 "fake_libc",
]

[[package]]
name = "fake_libc"
version = "0.2.186"

[[package]]
name = "host_only_dep"
version = "0.1.0"

[[package]]
name = "rustix_like"
version = "1.1.4"
dependencies = [
 "fake_errno",
 "fake_libc",
 "host_only_dep",
]
"#;
    let rustix_like_toml = r#"[package]
name = "rustix_like"
version = "1.1.4"
edition = "2021"

[features]
default = ["std"]
std = ["fake_libc?/std", "libc_errno?/std"]

[target.'cfg(not(target_os = "wasi"))'.dependencies]
host_only_dep = { path = "../host_only_dep" }
fake_libc = { path = "../fake_libc", optional = true, default-features = false }
libc_errno = { package = "fake_errno", path = "../fake_errno", optional = true, default-features = false }

[target.'cfg(target_os = "wasi")'.dependencies]
fake_libc = { path = "../fake_libc", default-features = false }
libc_errno = { package = "fake_errno", path = "../fake_errno", default-features = false }
"#;
    let rustix_like_build_rs = r#"fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("wasi") {
        println!("cargo:rustc-cfg=use_libc_backend");
    }
}
"#;
    let rustix_like_lib_rs = r#"#[cfg(use_libc_backend)]
pub fn combined_value() -> u32 {
    let m: fake_libc::mode_t = fake_libc::mode_val();
    let e: u32 = libc_errno::errno_val();
    m + e
}

#[cfg(not(use_libc_backend))]
pub fn combined_value() -> u32 {
    host_only_dep::host_val()
}
"#;
    let fake_libc_toml = r#"[package]
name = "fake_libc"
version = "0.2.186"
edition = "2021"

[features]
default = ["std"]
std = []
"#;
    let fake_libc_build_rs = r#"fn main() {
    println!("cargo:rustc-cfg=fake_libc_build_ran");
}
"#;
    let fake_libc_lib_rs = r#"#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(all(fake_libc_build_ran, feature = "std"))]
#[allow(non_camel_case_types)]
pub type mode_t = u32;

#[cfg(all(fake_libc_build_ran, feature = "std"))]
pub fn mode_val() -> mode_t {
    100
}
"#;
    let fake_errno_toml = r#"[package]
name = "fake_errno"
version = "0.3.14"
edition = "2021"

[features]
default = ["std"]
std = ["fake_libc/std"]

[target.'cfg(target_os = "wasi")'.dependencies]
fake_libc = { path = "../fake_libc", default-features = false }
"#;
    let fake_errno_lib_rs = r#"#[cfg(all(target_os = "wasi", feature = "std"))]
pub fn errno_val() -> u32 {
    let base: fake_libc::mode_t = fake_libc::mode_val();
    base + 23
}
"#;
    let app_build_rs = r#"use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    let val = rustix_like::combined_value();
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    fs::write(out_dir.join("gen.rs"), format!("pub const WASI_VAL: u32 = {val};\n"))
        .expect("write gen.rs");
}
"#;
    let app_main_rs = r#"include!(concat!(env!("OUT_DIR"), "/gen.rs"));
fn main() {
    println!("WASI_TARGET_DEPS={}", WASI_VAL);
}
"#;
    write_files(
        &ws,
        &[
            ("Cargo.toml", "[workspace]\nmembers = [\"app\", \"rustix_like\", \"host_only_dep\"]\nresolver = \"2\"\n"),
            ("Cargo.lock", lock),
            ("host_only_dep/Cargo.toml", "[package]\nname = \"host_only_dep\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
            ("host_only_dep/src/lib.rs", "pub fn host_val() -> u32 { 9999 }\n"),
            ("fake_libc/Cargo.toml", fake_libc_toml),
            ("fake_libc/build.rs", fake_libc_build_rs),
            ("fake_libc/src/lib.rs", fake_libc_lib_rs),
            ("fake_errno/Cargo.toml", fake_errno_toml),
            ("fake_errno/src/lib.rs", fake_errno_lib_rs),
            ("rustix_like/Cargo.toml", rustix_like_toml),
            ("rustix_like/build.rs", rustix_like_build_rs),
            ("rustix_like/src/lib.rs", rustix_like_lib_rs),
            ("app/Cargo.toml", "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[build-dependencies]\nrustix_like = { path = \"../rustix_like\" }\n"),
            ("app/build.rs", app_build_rs),
            ("app/src/main.rs", app_main_rs),
        ],
    );

    // 1. Cold `blaze check` with empty module store and PATH="" self-compiles app/build.rs
    //    (plus rustix_like/build.rs and fake_libc/build.rs) using wasm64 target-specific deps.
    let (ok_check, check_out) = run_blaze(&ws, &home, "check1", &["check"]);
    assert!(
        ok_check && check_out.contains("0 errors: blaze check") && !check_out.contains("not checked because"),
        "cold blaze check with wasi-only target-specific renamed deps failed:\n{check_out}"
    );

    // 2. Running `app` confirms `fake_libc::mode_val()` (100) + `libc_errno::errno_val()` (123) = 223!
    let (ok_run, run_out) = run_app(&ws, &home, "run1", "app");
    assert!(ok_run && run_out.contains("WASI_TARGET_DEPS=223"), "blaze run failed:\n{run_out}");

    // 3. Rule 21 cache invalidation: editing `fake_errno/src/lib.rs` (a wasm64-only dep NOT in the
    //    host Linux UnitGraph!) must invalidate `app/build.rs`'s module key and recompile.
    write_files(
        &ws,
        &[(
            "fake_errno/src/lib.rs",
            "#[cfg(all(target_os = \"wasi\", feature = \"std\"))]\npub fn errno_val() -> u32 {\n    let base: fake_libc::mode_t = fake_libc::mode_val();\n    base + 55\n}\n",
        )],
    );
    let (ok_run2, run_out2) = run_app(&ws, &home, "run2", "app");
    assert!(
        ok_run2 && run_out2.contains("WASI_TARGET_DEPS=255"),
        "editing wasm64-only dep fake_errno must invalidate build.rs module:\n{run_out2}"
    );

    cleanup(&base);
}

#[test]
fn build_script_wasm64_preserves_workspace_feature_unification_and_excluded_path_deps() {
    let base = fresh_home("wasm64_unify_and_excluded_e2e", true);
    let (home, ws) = (base.join("home"), base.join("ws"));

    // Workspace with two members (`app_a`, `app_b`) sharing `shared_dep`, plus an explicitly
    // excluded non-member path package (`ext_wasi_dep`) that is a direct WASI-only build-dep of
    // BOTH `app_a` (without `wasi_extra`) and `app_b` (initially WITH `features = ["wasi_extra"]`):
    // - `app_a` depends on `shared_dep` WITHOUT `extra_flag` and `ext_wasi_dep` (WASI-only) WITHOUT
    //   `wasi_extra`, observing BOTH unified features enabled by sibling `app_b`!
    // - Removing `features = ["wasi_extra"]` from sibling `app_b/Cargo.toml` (without touching
    //   `app_a` or `Cargo.lock`!) must invalidate `app_a/build.rs`'s cached module because the
    //   resolved WASI-side unified features enter the module key (Rule 21)!
    let lock = r#"# This file is automatically @generated by Cargo.
version = 4

[[package]]
name = "app_a"
version = "0.1.0"
dependencies = [
 "ext_wasi_dep",
 "shared_dep",
]

[[package]]
name = "app_b"
version = "0.1.0"
dependencies = [
 "ext_wasi_dep",
 "shared_dep",
]

[[package]]
name = "ext_wasi_dep"
version = "0.1.0"

[[package]]
name = "shared_dep"
version = "0.1.0"
"#;

    write_files(
        &ws,
        &[
            (
                "Cargo.toml",
                "[workspace]\nmembers = [\"app_a\", \"app_b\", \"shared_dep\"]\nexclude = [\"ext_wasi_dep\"]\nresolver = \"2\"\n",
            ),
            ("Cargo.lock", lock),
            (
                "shared_dep/Cargo.toml",
                "[package]\nname = \"shared_dep\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[features]\ndefault = []\nextra_flag = []\n",
            ),
            (
                "shared_dep/src/lib.rs",
                "pub fn unified_val() -> u32 {\n    if cfg!(feature = \"extra_flag\") { 77 } else { 11 }\n}\n",
            ),
            (
                "ext_wasi_dep/Cargo.toml",
                "[package]\nname = \"ext_wasi_dep\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[features]\ndefault = []\nwasi_extra = []\n",
            ),
            (
                "ext_wasi_dep/src/lib.rs",
                "pub fn ext_val() -> u32 {\n    if cfg!(feature = \"wasi_extra\") { 314 } else { 100 }\n}\n",
            ),
            (
                "app_a/Cargo.toml",
                "[package]\nname = \"app_a\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[build-dependencies]\nshared_dep = { path = \"../shared_dep\" }\n\n[target.'cfg(target_os = \"wasi\")'.build-dependencies]\next_wasi_dep = { path = \"../ext_wasi_dep\" }\n",
            ),
            (
                "app_a/build.rs",
                "use std::{env, fs, path::PathBuf};\nfn main() {\n    #[cfg(target_os = \"wasi\")]\n    let e = ext_wasi_dep::ext_val();\n    #[cfg(not(target_os = \"wasi\"))]\n    let e = 0u32;\n    let v = shared_dep::unified_val() + e;\n    let out = PathBuf::from(env::var(\"OUT_DIR\").expect(\"OUT_DIR\"));\n    fs::write(out.join(\"val.rs\"), format!(\"pub const VAL: u32 = {v};\\n\")).expect(\"write\");\n}\n",
            ),
            (
                "app_a/src/main.rs",
                "include!(concat!(env!(\"OUT_DIR\"), \"/val.rs\"));\nfn main() { println!(\"UNIFIED={}\", VAL); }\n",
            ),
            (
                "app_b/Cargo.toml",
                "[package]\nname = \"app_b\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[build-dependencies]\nshared_dep = { path = \"../shared_dep\", features = [\"extra_flag\"] }\n\n[target.'cfg(target_os = \"wasi\")'.build-dependencies]\next_wasi_dep = { path = \"../ext_wasi_dep\", features = [\"wasi_extra\"] }\n",
            ),
            (
                "app_b/build.rs",
                "use std::{env, fs, path::PathBuf};\nfn main() {\n    #[cfg(target_os = \"wasi\")]\n    let e = ext_wasi_dep::ext_val();\n    #[cfg(not(target_os = \"wasi\"))]\n    let e = 0u32;\n    let v = shared_dep::unified_val() + e;\n    let out = PathBuf::from(env::var(\"OUT_DIR\").expect(\"OUT_DIR\"));\n    fs::write(out.join(\"val.rs\"), format!(\"pub const VAL: u32 = {v};\\n\")).expect(\"write\");\n}\n",
            ),
            (
                "app_b/src/main.rs",
                "include!(concat!(env!(\"OUT_DIR\"), \"/val.rs\"));\nfn main() { println!(\"EXCLUDED={}\", VAL); }\n",
            ),
        ],
    );

    let (ok_check, check_out) = run_blaze(&ws, &home, "check1", &["check"]);
    assert!(
        ok_check && check_out.contains("0 errors: blaze check") && !check_out.contains("not checked because"),
        "cold blaze check with workspace feature unification & excluded wasi-only dep failed:\n{check_out}"
    );

    // 1. `app_a` must see `extra_flag` (77) AND WASI-only `wasi_extra` (314) unified from sibling `app_b` (77 + 314 = 391).
    let (ok_a, out_a) = run_app(&ws, &home, "run_a1", "app_a");
    assert!(ok_a && out_a.contains("UNIFIED=391"), "expected workspace-unified value 391 for app_a:\n{out_a}");

    // 2. `app_b` must resolve excluded `ext_wasi_dep` (`77 + 314 = 391`).
    let (ok_b, out_b) = run_app(&ws, &home, "run_b1", "app_b");
    assert!(ok_b && out_b.contains("EXCLUDED=391"), "expected excluded wasi-only dep value 391 for app_b:\n{out_b}");

    // 3. Removing `wasi_extra` from sibling `app_b/Cargo.toml` (without editing `app_a` or
    //    `Cargo.lock`!) must invalidate `app_a/build.rs`'s cached module (`77 + 100 = 177`)!
    write_files(
        &ws,
        &[(
            "app_b/Cargo.toml",
            "[package]\nname = \"app_b\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[build-dependencies]\nshared_dep = { path = \"../shared_dep\", features = [\"extra_flag\"] }\n\n[target.'cfg(target_os = \"wasi\")'.build-dependencies]\next_wasi_dep = { path = \"../ext_wasi_dep\" }\n",
        )],
    );
    let (ok_a2, out_a2) = run_app(&ws, &home, "run_a2", "app_a");
    assert!(
        ok_a2 && out_a2.contains("UNIFIED=177"),
        "removing sibling WASI-only feature must invalidate app_a/build.rs (expected 177, not stale 391):\n{out_a2}"
    );

    // 4. Editing excluded direct WASI-only build-dep `ext_wasi_dep/src/lib.rs` must invalidate
    //    `app_b/build.rs` even though `ext_wasi_dep` is absent from the host Linux `UnitGraph` (`77 + 415 = 492`).
    write_files(&ws, &[("ext_wasi_dep/src/lib.rs", "pub fn ext_val() -> u32 { 415 }\n")]);
    let (ok_b2, out_b2) = run_app(&ws, &home, "run_b2", "app_b");
    assert!(
        ok_b2 && out_b2.contains("EXCLUDED=492"),
        "editing excluded direct wasi-only build-dep must invalidate app_b/build.rs:\n{out_b2}"
    );

    cleanup(&base);
}

#[test]
fn wasm64_build_script_and_proc_macro_read_std_env_consts_arch() {
    let base = fresh_home("wasm64_std_env_arch_e2e", true);
    let (home, ws) = (base.join("home"), base.join("ws"));
    let lock = r#"version = 4

[[package]]
name = "app"
version = "0.1.0"
dependencies = [
 "mac",
]

[[package]]
name = "mac"
version = "0.1.0"
"#;
    let mac_lib = r#"use proc_macro::TokenStream;

const PM_ARCH_CONST: &str = std::env::consts::ARCH;

#[proc_macro]
pub fn pm_std_arch(_input: TokenStream) -> TokenStream {
    let arch = std::env::consts::ARCH;
    assert_eq!(arch, "wasm64");
    assert_eq!(PM_ARCH_CONST, "wasm64");
    format!("\"{arch}\"").parse().expect("parse arch literal")
}
"#;
    let app_build_rs = r#"const BS_ARCH_CONST: &str = std::env::consts::ARCH;

fn main() {
    let arch = std::env::consts::ARCH;
    assert_eq!(arch, "wasm64");
    assert_eq!(BS_ARCH_CONST, "wasm64");
    println!("cargo:rustc-env=BS_STD_ARCH={arch}");
}
"#;
    let app_main_rs = r#"fn main() {
    println!("STD_ARCH={}:{}", env!("BS_STD_ARCH"), mac::pm_std_arch!());
}
"#;
    write_files(
        &ws,
        &[
            ("Cargo.toml", "[workspace]\nmembers = [\"mac\", \"app\"]\nresolver = \"2\"\n"),
            ("Cargo.lock", lock),
            (
                "mac/Cargo.toml",
                "[package]\nname = \"mac\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\nproc-macro = true\n",
            ),
            ("mac/src/lib.rs", mac_lib),
            (
                "app/Cargo.toml",
                "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\nmac = { path = \"../mac\" }\n",
            ),
            ("app/build.rs", app_build_rs),
            ("app/src/main.rs", app_main_rs),
        ],
    );

    let (_, check_out) = run_blaze(&ws, &home, "check1", &["check"]);
    assert!(
        !check_out.contains("error[") && !check_out.contains("error:") && !check_out.contains("not checked because"),
        "cold blaze check with std::env::consts::ARCH in build.rs + proc-macro failed:\n{check_out}"
    );
    let bs_mods = published_modules(&home.join(".blaze/v2/build-script/modules"));
    assert_eq!(bs_mods.len(), 1, "expected 1 published wasm64 build-script module: {bs_mods:?}");
    let pm_mods = published_modules(&home.join(".blaze/v2/proc-macro/modules"));
    assert_eq!(pm_mods.len(), 1, "expected 1 published wasm64 proc-macro module: {pm_mods:?}");
    assert!(
        home.join(".blaze/v2/wasm64-sysroot-v2").is_dir(),
        "expected wasm64-sysroot-v2 cache directory to be populated"
    );
    assert!(
        !home.join(".blaze/v2/wasm64-sysroot-v1").exists(),
        "stale wasm64-sysroot-v1 directory must not be written"
    );

    let (ok_run, run_out) = run_app(&ws, &home, "run1", "app");
    assert!(
        ok_run && run_out.contains("STD_ARCH=wasm64:wasm64"),
        "expected build.rs and proc-macro std::env::consts::ARCH to evaluate to wasm64:\n{run_out}"
    );

    cleanup(&base);
}

#[test]
fn proc_macro_dep_build_script_runs_clang_version_and_c_probe_in_wasm64() {
    let base = fresh_home("wasm64_dep_script_cprobes_e2e", true);
    let (home, ws) = (base.join("home"), base.join("ws"));

    // Check whether the host's pinned clang shelf was linked into `fresh_home` (Linux live C probe).
    let has_clang_shelf = cfg!(target_os = "linux")
        && home.join(".blaze/v2/toolchain/clang").is_dir()
        && dir_entry_count(&home.join(".blaze/v2/toolchain/clang")).is_some_and(|n| n > 0);

    let run_c_probe_block = if has_clang_shelf {
        r##"
    let probe_c = out_dir.join("detect_compiler_family.c");
    fs::write(&probe_c, "#ifdef __clang__\nBLAZE_DETECTED_CLANG\n#endif\n").expect("write probe");
    let probe = Command::new("clang")
        .arg("-E")
        .arg(&probe_c)
        .output()
        .expect("spawn clang -E probe");
    assert!(
        probe.status.success(),
        "clang -E probe failed: stderr={}",
        String::from_utf8_lossy(&probe.stderr)
    );
    let probe_out = String::from_utf8_lossy(&probe.stdout);
    assert!(
        probe_out.contains("BLAZE_DETECTED_CLANG"),
        "expected BLAZE_DETECTED_CLANG in clang -E stdout, got: {probe_out}"
    );
"##
    } else {
        ""
    };

    let ring_like_build_rs = format!(
        r#"use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn main() {{
    let ver = Command::new("clang")
        .arg("--version")
        .output()
        .expect("spawn clang --version");
    assert!(
        ver.status.success(),
        "clang --version failed: stderr={{}}",
        String::from_utf8_lossy(&ver.stderr)
    );
    let ver_out = String::from_utf8_lossy(&ver.stdout);
    assert!(
        ver_out.contains("clang version "),
        "expected clang version in stdout, got: {{ver_out}}"
    );

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
{run_c_probe_block}
    fs::write(out_dir.join("probe.rs"), "pub const PROBE_TAG: &str = \"clang_probe_ok\";\n")
        .expect("write probe.rs");
    println!("cargo:rustc-cfg=probed_clang_ok");
}}
"#
    );

    let lock = r#"# This file is automatically @generated by Cargo.
version = 4

[[package]]
name = "app"
version = "0.1.0"
dependencies = [
 "mac",
]

[[package]]
name = "mac"
version = "0.1.0"
dependencies = [
 "ring_like",
]

[[package]]
name = "ring_like"
version = "0.1.0"
"#;

    write_files(
        &ws,
        &[
            ("Cargo.toml", "[workspace]\nmembers = [\"ring_like\", \"mac\", \"app\"]\nresolver = \"2\"\n"),
            ("Cargo.lock", lock),
            ("ring_like/Cargo.toml", "[package]\nname = \"ring_like\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
            ("ring_like/build.rs", &ring_like_build_rs),
            ("ring_like/src/lib.rs", "include!(concat!(env!(\"OUT_DIR\"), \"/probe.rs\"));\n#[cfg(probed_clang_ok)]\npub fn tag() -> &'static str { PROBE_TAG }\n"),
            ("mac/Cargo.toml", "[package]\nname = \"mac\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\nproc-macro = true\n\n[dependencies]\nring_like = { path = \"../ring_like\" }\n"),
            ("mac/src/lib.rs", "use proc_macro::TokenStream;\n#[proc_macro]\npub fn probe_banner(_input: TokenStream) -> TokenStream {\n    let tag = ring_like::tag();\n    format!(\"{tag:?}\").parse().expect(\"parse banner\")\n}\n"),
            ("app/Cargo.toml", "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\nmac = { path = \"../mac\" }\n"),
            ("app/src/main.rs", "fn main() {\n    println!(\"PROBE_RESULT={}\", mac::probe_banner!());\n}\n"),
        ],
    );

    let (_, check_out) = run_blaze(&ws, &home, "check1", &["check"]);
    assert!(
        !check_out.contains("error[") && !check_out.contains("error:") && !check_out.contains("not checked because"),
        "cold blaze check with proc-macro dep build.rs C probe failed:\n{check_out}"
    );
    assert!(
        !check_out.contains("no C compiler probe setting configured"),
        "dep build script must receive c_probes:\n{check_out}"
    );
    assert!(
        !check_out.contains("the command gave no pins for derived facts"),
        "dep build script must receive derived-fact spawn_setting pins:\n{check_out}"
    );
    let mods = published_modules(&home.join(".blaze/v2/proc-macro/modules"));
    assert_eq!(mods.len(), 1, "expected 1 published wasm64 proc-macro module: {mods:?}");
    let wasm = read_bytes(&mods[0].1.join("module.wasm"));
    let checked = check_wasm64(&wasm).expect("published proc-macro module must be valid wasm64");
    assert!(checked.memory64, "proc-macro module must use memory64");

    let (ok_run, run_out) = run_app(&ws, &home, "run1", "app");
    assert!(
        ok_run && run_out.contains("PROBE_RESULT=clang_probe_ok"),
        "running app built with C-probed proc-macro dep failed:\n{run_out}"
    );

    if has_clang_shelf {
        let probe_cache_dir = home.join(".blaze/v2/build-script/records/c-probe");
        let cached = dir_entry_count(&probe_cache_dir)
            .unwrap_or_else(|| panic!("expected c-probe cache dir {}", probe_cache_dir.display()));
        assert!(
            cached >= 1,
            "expected at least 1 cached c-probe record under {}",
            probe_cache_dir.display()
        );
    }

    cleanup(&base);
}
