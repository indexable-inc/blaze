use std::sync::Arc;

use blaze_ids::ContentId;
use blaze_query::{Database, MemTree};
use blaze_vfs::View;

use super::target::{
    CfgArgs, cargo_cfg_env, data_layout, file_name, split_debuginfo, target_cfg, target_cfg_told, target_facts,
    target_spec_json,
};
use super::*;
use crate::query::{StdGraph, StdKey, StdPlanned, StdUnits};
use crate::schema::{Mode, Unit};

fn recorded() -> serde_json::Value {
    serde_json::from_str(include_str!("rustc.json")).unwrap()
}

fn strings(value: &serde_json::Value) -> Vec<String> {
    value.as_array().unwrap().iter().map(|it| it.as_str().unwrap().to_owned()).collect()
}

/// What `rustc.json` says rustc prints for `--print cfg --target <triple>` at opt-level 0 (so
/// `debug_assertions`, `overflow_checks`, `ub_checks`) with `-C target-feature=+<on>` for each
/// of `on`: each feature's own list and those of the target's features it needs.
fn printed(triple: &str, on: &[&str]) -> Vec<String> {
    let data = recorded();
    let target = &data["targets"][triple];
    let mut cfgs: Vec<String> = strings(&target["cfg"]).into_iter().chain(strings(&target["nightly_cfg"])).collect();
    let listed = |name: &str| {
        target["features"].get(name).is_some()
            || target["llvm_features"].get(name).is_some()
            || target["features_off"].get(name).is_some()
            || strings(&target["other_features"]).iter().any(|it| it == name)
    };
    for feature in on.iter().filter(|it| listed(it)) {
        cfgs.extend(strings(&target["features"][*feature]).iter().map(|it| format!("target_feature=\"{it}\"")));
        for (own, off) in target["features_off"].as_object().unwrap() {
            if strings(off).iter().any(|it| it == feature) {
                cfgs.push(format!("target_feature=\"{own}\""));
            }
        }
    }
    cfgs.extend(["debug_assertions", "overflow_checks", "ub_checks"].map(str::to_owned));
    cfgs.push(format!("panic=\"{}\"", target["panic"].as_str().unwrap()));
    cfgs.sort();
    cfgs.dedup();
    cfgs
}

const TRIPLES: [&str; 4] = ["x86_64-unknown-linux-gnu", "aarch64-apple-darwin", "x86_64-pc-windows-msvc", "wasm32-wasip1"];

#[test]
fn the_cfg_of_each_target_is_what_rustc_printed() {
    for triple in TRIPLES {
        assert_eq!(target_cfg(triple, &CfgArgs::default()).unwrap(), printed(triple, &[]), "{triple}");
    }
    let has = |triple: &str, cfg: &str| target_cfg(triple, &CfgArgs::default()).unwrap().iter().any(|it| it == cfg);
    assert!(has("x86_64-unknown-linux-gnu", "target_os=\"linux\""));
    assert!(has("x86_64-unknown-linux-gnu", "unix"));
    assert!(has("x86_64-pc-windows-msvc", "target_env=\"msvc\""));
    assert!(has("x86_64-pc-windows-msvc", "windows"));
    assert!(has("aarch64-apple-darwin", "target_vendor=\"apple\""));
    assert!(has("wasm32-wasip1", "panic=\"abort\""));
    assert!(has("x86_64-unknown-linux-gnu", "panic=\"unwind\""));
}

#[test]
fn a_target_feature_turns_on_what_rustc_says() {
    let avx2 = CfgArgs::from_flags(&["-C".to_owned(), "target-feature=+avx2".to_owned()]).unwrap();
    assert_eq!(avx2.target_features, [(true, "avx2".to_owned())]);
    for triple in TRIPLES {
        assert_eq!(target_cfg(triple, &avx2).unwrap(), printed(triple, &["avx2"]), "{triple}");
    }
    let linux = target_cfg("x86_64-unknown-linux-gnu", &avx2).unwrap();
    for feature in ["avx", "avx2", "sse3", "sse4.1", "sse4.2", "ssse3"] {
        assert!(linux.contains(&format!("target_feature=\"{feature}\"")), "{feature}");
    }
    // A feature the target does not have changes no cfg: rustc warns of it.
    for triple in ["aarch64-apple-darwin", "wasm32-wasip1"] {
        assert_eq!(target_cfg(triple, &avx2).unwrap(), target_cfg(triple, &CfgArgs::default()).unwrap());
    }
    let joined = CfgArgs::from_flags(&["-Ctarget-feature=+avx2".to_owned()]).unwrap();
    assert_eq!(joined, avx2);
}

#[test]
fn a_feature_of_no_cfg_changes_none() {
    // LLVM's, which rustc warns of and passes on.
    let other = CfgArgs::from_flags(&["-Ctarget-feature=+64bit".to_owned()]).unwrap();
    let triple = "x86_64-unknown-linux-gnu";
    assert_eq!(target_cfg(triple, &other).unwrap(), target_cfg(triple, &CfgArgs::default()).unwrap());
    // Turning off what it turned on is the target's own again.
    let both = CfgArgs::from_flags(&["-Ctarget-feature=+avx2,-avx2".to_owned()]).unwrap();
    let cfgs = target_cfg(triple, &both).unwrap();
    assert!(!cfgs.iter().any(|it| it == "target_feature=\"avx2\""), "{cfgs:?}");
    assert!(cfgs.iter().any(|it| it == "target_feature=\"sse2\""), "{cfgs:?}");
}

#[test]
fn the_features_that_change_no_cfg_are_told() {
    let triple = "x86_64-unknown-linux-gnu";
    let args = CfgArgs::from_flags(&["-Ctarget-feature=+blazeunlisted,+64bit,+avx2".to_owned()]).unwrap();
    let (cfgs, skipped) = target_cfg_told(triple, &args).unwrap();
    let avx2 = CfgArgs::from_flags(&["-Ctarget-feature=+avx2".to_owned()]).unwrap();
    assert_eq!(cfgs, target_cfg(triple, &avx2).unwrap());
    // `64bit` is LLVM's, which rustc warns of in words the data does not have.
    assert_eq!(skipped.unknown, ["blazeunlisted"]);
    assert_eq!(skipped.listed, ["64bit"]);
    assert_eq!(target_cfg_told(triple, &avx2).unwrap().1, target::Skipped::default());
    let warning = target::unknown_feature_warning("blazeunlisted").unwrap();
    assert!(warning.contains("`blazeunlisted`") && !warning.contains("{feature}"), "{warning}");
    assert!(warning.ends_with("\n\n"), "{warning:?}");
}

/// `RUSTC_BOOTSTRAP=-1`'s cfgs: `cfg` alone, `debug_assertions` but neither `ub_checks` nor
/// `overflow_checks`.
#[test]
fn a_stable_rustc_prints_no_cfg_of_an_unstable_feature() {
    let data = recorded();
    let stable = CfgArgs { stable: true, ..CfgArgs::default() };
    for triple in TRIPLES {
        let target = &data["targets"][triple];
        let mut want = strings(&target["cfg"]);
        want.push("debug_assertions".to_owned());
        want.push(format!("panic=\"{}\"", target["panic"].as_str().unwrap()));
        want.sort();
        assert_eq!(target_cfg(triple, &stable).unwrap(), want, "{triple}");
    }
    let tuned = CfgArgs { target_features: vec![(true, "avx2".to_owned())], ..stable.clone() };
    assert!(target_cfg("x86_64-unknown-linux-gnu", &tuned).unwrap_err().contains("nightly"));
    let cpu = CfgArgs { target_cpu: Some("x86-64-v3".to_owned()), ..stable };
    assert!(target_cfg("x86_64-unknown-linux-gnu", &cpu).is_err());
}

#[test]
fn a_spec_the_hosts_agree_on_is_every_hosts() {
    let data = recorded();
    let wasm = target_spec_json("wasm32-wasip1", "x86_64-pc-windows-msvc").unwrap().expect("wasm32-wasip1's spec");
    assert_eq!(Some(wasm), target_spec_json("wasm32-wasip1", "aarch64-apple-darwin").unwrap());
    assert_eq!(wasm, data["targets"]["wasm32-wasip1"]["target_spec_json"]["aarch64-apple-darwin"].as_str().unwrap());
    let linux = "x86_64-unknown-linux-gnu";
    assert_eq!(target_spec_json(linux, "x86_64-pc-windows-msvc").unwrap(), None);
    assert_eq!(
        target_spec_json(linux, linux).unwrap(),
        data["targets"][linux]["target_spec_json"][linux].as_str()
    );
    assert!(target_spec_json("riscv64gc-unknown-plan9", linux).is_err());
    let split: Vec<&str> = split_debuginfo(linux).unwrap().iter().map(|it| &**it).collect();
    assert_eq!(split, ["off", "packed", "unpacked"]);
    assert!(target::known(linux).is_ok() && target::known("riscv64gc-unknown-plan9").is_err());
}

#[test]
fn a_targets_facts_are_read_without_allocating() {
    let linux = "x86_64-unknown-linux-gnu";
    target::known(linux).unwrap();
    let read = blaze_alloc_count::assert_at_most(0, "a target's lookups", || {
        let split = split_debuginfo(linux).unwrap().len();
        (target::known(linux).is_ok(), split, target_spec_json(linux, linux).unwrap().is_some())
    });
    assert_eq!(read, (true, 3, true));
}

/// `--cfg`s rustc prints back as given: a plain name of no builtin cfg, a value with nothing
/// to escape.
#[test]
fn a_cfg_given_is_passed_on_when_plain() {
    let triple = "x86_64-unknown-linux-gnu";
    let given = |cfg: &str| target_cfg(triple, &CfgArgs { cfgs: vec![cfg.to_owned()], ..CfgArgs::default() });
    for plain in ["foo", "feature=\"serde derive\""] {
        assert!(given(plain).unwrap().iter().any(|it| it == plain), "{plain}");
    }
    for refused in ["feature=serde", "feature=\"a\\\"b\"", "debug_assertions", "target_os=\"linux\"", "sanitizer_cfi_generalize_pointers", "9lives"] {
        assert!(given(refused).is_err(), "{refused}");
    }
}

#[test]
fn the_profile_decides_its_cfgs() {
    let triple = "x86_64-unknown-linux-gnu";
    let optimized = CfgArgs::from_flags(&["-Copt-level=3".to_owned()]).unwrap();
    let cfgs = target_cfg(triple, &optimized).unwrap();
    for cfg in ["debug_assertions", "overflow_checks", "ub_checks"] {
        assert!(!cfgs.iter().any(|it| it == cfg), "{cfg}");
    }
    let checked = CfgArgs { overflow_checks: Some(true), ..optimized };
    assert!(target_cfg(triple, &checked).unwrap().iter().any(|it| it == "overflow_checks"));
    let abort = CfgArgs { panic: Some("abort".to_owned()), ..CfgArgs::default() };
    assert!(target_cfg(triple, &abort).unwrap().iter().any(|it| it == "panic=\"abort\""));
    let unwind = CfgArgs { panic: Some("unwind".to_owned()), ..CfgArgs::default() };
    assert!(target_cfg("wasm32-wasip1", &unwind).unwrap_err().contains("panic=unwind"));
    let custom = CfgArgs::from_flags(&["--cfg".to_owned(), "tokio_unstable".to_owned()]).unwrap();
    assert!(target_cfg(triple, &custom).unwrap().iter().any(|it| it == "tokio_unstable"));
    let builtin = CfgArgs::from_flags(&["--cfg".to_owned(), "unix".to_owned()]).unwrap();
    assert!(target_cfg(triple, &builtin).is_err());
}

// rustc sets `cfg(sanitize = "..")` itself for each sanitizer `-Z sanitizer` turns on, lists
// adding up; no `cfg(sanitizer_cfi_*)` without `cfi`, which blaze refuses.
#[test]
fn a_sanitizer_is_a_cfg() {
    let triple = "x86_64-unknown-linux-gnu";
    let flags = |words: &[&str]| CfgArgs::from_flags(&words.iter().map(|it| (*it).to_owned()).collect::<Vec<_>>());
    let sanitized = |words: &[&str]| -> Vec<String> {
        let cfgs = target_cfg(triple, &flags(words).unwrap()).unwrap();
        cfgs.into_iter().filter(|it| it.starts_with("sanitize")).collect()
    };
    assert_eq!(sanitized(&[]), Vec::<String>::new());
    assert_eq!(sanitized(&["-Zsanitizer=address"]), ["sanitize=\"address\""]);
    assert_eq!(sanitized(&["-Z", "sanitizer=thread", "-Zsanitizer=thread"]), ["sanitize=\"thread\""]);
    assert_eq!(
        sanitized(&["-Zsanitizer=memory", "-Zsanitizer-memory-track-origins", "-Z", "sanitizer_recover=memory"]),
        ["sanitize=\"memory\""]
    );
    assert_eq!(sanitized(&["-Zsanitizer-recover=address"]), Vec::<String>::new());
    assert_eq!(
        flags(&["-Zsanitizer=address", "-Zsanitizer=thread"]).unwrap_err(),
        "`-Zsanitizer=address` is incompatible with `-Zsanitizer=thread`"
    );
    assert_eq!(
        flags(&["-Zsanitizer=thread,memory"]).unwrap_err(),
        "`-Zsanitizer=memory` is incompatible with `-Zsanitizer=thread`"
    );
    assert!(flags(&["-Zsanitizer=cfi"]).unwrap_err().contains("the `cfi` sanitizer"));
    assert!(flags(&["-Zsanitizer=nope"]).unwrap_err().contains("no sanitizer `nope`"));
    assert!(flags(&["-Zsanitizer"]).unwrap_err().contains("comma-separated list"));
    // cargo-fuzz's flags beside the sanitizer decide no cfg.
    assert_eq!(
        sanitized(&["-Zsanitizer=address", "-Cpasses=sancov-module", "-Cllvm-args=-sanitizer-coverage-level=4"]),
        ["sanitize=\"address\""]
    );
    // A `--cfg` of it is rustc's to set, not the user's.
    assert!(target_cfg(triple, &flags(&["--cfg", "sanitize=\"address\""]).unwrap()).is_err());
}

#[test]
fn a_proc_macro_of_a_static_target_has_no_crt_static() {
    let triple = "x86_64-unknown-linux-musl";
    let crt = "target_feature=\"crt-static\"".to_owned();
    assert!(target_cfg(triple, &CfgArgs::default()).unwrap().contains(&crt));
    let probe = CfgArgs { proc_macro: true, ..CfgArgs::default() };
    let cfgs = target_cfg(triple, &probe).unwrap();
    assert!(!cfgs.contains(&crt));
    assert!(cfgs.iter().any(|it| it == "proc_macro"));
}

#[test]
fn an_unknown_target_is_refused_naming_it() {
    let error = target_cfg("riscv64gc-unknown-plan9", &CfgArgs::default()).unwrap_err();
    assert!(error.contains("`riscv64gc-unknown-plan9`"), "{error}");
    assert!(target_facts("riscv64gc-unknown-plan9").unwrap_err().contains("riscv64gc-unknown-plan9"));
    assert!(file_name("riscv64gc-unknown-plan9", "bin", "a").is_err());
    let flag = CfgArgs::from_flags(&["-Cno-such-option=1".to_owned()]).unwrap_err();
    assert!(flag.contains("no-such-option"), "{flag}");
}

#[test]
fn a_build_scripts_cfg_variables_are_cargos() {
    let cfgs = target_cfg("x86_64-unknown-linux-gnu", &CfgArgs::default()).unwrap();
    let env = cargo_cfg_env(&cfgs);
    let var = |name: &str| env.iter().find(|(it, _)| it == name).map(|(_, value)| value.as_str());
    assert_eq!(var("CARGO_CFG_TARGET_OS"), Some("linux"));
    assert_eq!(var("CARGO_CFG_TARGET_FAMILY"), Some("unix"));
    assert_eq!(var("CARGO_CFG_UNIX"), Some(""));
    assert_eq!(var("CARGO_CFG_TARGET_FEATURE"), Some("fxsr,sse,sse2,x87"));
    assert_eq!(var("CARGO_CFG_PANIC"), Some("unwind"));
    assert_eq!(var("CARGO_CFG_DEBUG_ASSERTIONS"), None);
    assert!(env.windows(2).all(|it| it[0].0 < it[1].0));
}

#[test]
fn a_targets_facts_and_file_names() {
    let linux = target_facts("x86_64-unknown-linux-gnu").unwrap();
    assert_eq!((linux.panic.as_str(), linux.has_std, linux.needs_builtins_mem), ("unwind", true, false));
    assert_eq!(target_facts("wasm32-wasip1").unwrap().panic, "abort");
    assert_eq!(file_name("x86_64-unknown-linux-gnu", "lib", "core").unwrap().as_deref(), Some("libcore.rlib"));
    assert_eq!(file_name("x86_64-pc-windows-msvc", "bin", "a").unwrap().as_deref(), Some("a.exe"));
    assert_eq!(file_name("wasm32-wasip1", "dylib", "a").unwrap(), None);
    assert!(target::triples().any(|it| it == "aarch64-unknown-linux-gnu"));
}

/// `rustc +nightly --print target-spec-json -Z unstable-options --target <triple>`'s
/// `data-layout` of the pinned nightly (1.101.0-nightly 16d1ffac1), for each tier-1 target.
#[test]
fn a_targets_data_layout_is_its_specs() {
    let tier_one = [
        ("aarch64-apple-darwin", "e-m:o-p270:32:32-p271:32:32-p272:64:64-i64:64-i128:128-n32:64-S128-Fn32"),
        (
            "aarch64-pc-windows-msvc",
            "e-m:w-p270:32:32-p271:32:32-p272:64:64-p:64:64-i32:32-i64:64-i128:128-n32:64-S128-Fn32",
        ),
        (
            "aarch64-unknown-linux-gnu",
            "e-m:e-p270:32:32-p271:32:32-p272:64:64-i8:8:32-i16:16:32-i64:64-i128:128-n32:64-S128-Fn32",
        ),
        ("x86_64-pc-windows-gnu", "e-m:w-p270:32:32-p271:32:32-p272:64:64-i64:64-i128:128-f80:128-n8:16:32:64-S128"),
        ("x86_64-pc-windows-msvc", "e-m:w-p270:32:32-p271:32:32-p272:64:64-i64:64-i128:128-f80:128-n8:16:32:64-S128"),
        ("x86_64-unknown-linux-gnu", "e-m:e-p270:32:32-p271:32:32-p272:64:64-i64:64-i128:128-f80:128-n8:16:32:64-S128"),
    ];
    for (triple, layout) in tier_one {
        assert_eq!(data_layout(triple).unwrap(), layout, "{triple}");
    }
    assert_eq!(data_layout("wasm32-wasip1").unwrap(), "e-m:e-p:32:32-p10:8:8-p20:8:8-i64:64-i128:128-n32:64-S128-ni:1:10:20");
    let unknown = data_layout("riscv64gc-unknown-plan9").unwrap_err();
    assert!(unknown.contains("riscv64gc-unknown-plan9"), "{unknown}");
}

#[test]
fn the_library_is_named_as_rustcs() {
    let commit = library_commit(include_str!("../../../../library/UPSTREAM")).unwrap();
    assert_eq!(commit, target::rustc_commit());
    assert_eq!(library_at(&commit), format!("/rustc/{commit}/library"));
    assert!(library_commit("commit 0123\n").unwrap_err().contains("0123"));
    assert!(library_commit("repo x\n").is_err());
    assert!(in_library("std/src/lib.rs"));
    assert!(in_library("vendor/libc-0.2.191/Cargo.toml"));
    assert!(in_library(".cargo/config.toml"));
    assert!(!in_library("UPSTREAM"));
    assert!(!in_library("DIGEST"));
    assert!(in_library("std/DIGEST"));
}

#[test]
fn std_brings_the_crates_cargo_builds_with_it() {
    let names = |it: &[&str]| it.iter().map(|it| (*it).to_owned()).collect::<Vec<_>>();
    assert_eq!(
        std_crates(&names(&["std"])),
        names(&["alloc", "compiler_builtins", "core", "panic_unwind", "proc_macro", "std"])
    );
    assert_eq!(std_crates(&names(&["core", "alloc"])), names(&["alloc", "compiler_builtins", "core"]));
    assert!(std_crates(&names(&["std", "test"])).contains(&"test".to_owned()));
    assert_eq!(default_crates("x86_64-unknown-linux-gnu").unwrap(), names(&["std"]));
}

const COMMIT: &str = "16d1ffac17ad8b739e32483ac2e65fe7e1804977";

fn package(name: &str, extra: &str) -> String {
    format!("[package]\nname = \"{name}\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[lib]\ntest = false\nbench = false\n\n{extra}")
}

/// A standard library of eight crates, laid out and locked as library/ is.
fn library(tree: &MemTree) -> ContentId {
    let manifest = "cargo-features = [\"profile-rustflags\"]\n\n[workspace]\nresolver = \"1\"\nmembers = [\"std\", \"sysroot\"]\n\n\
                    [profile.release.package.compiler_builtins]\ncodegen-units = 10000\n\n\
                    [profile.dev.package.panic_abort]\nrustflags = [\"-Cpanic=abort\"]\n\n\
                    [profile.release.package.panic_abort]\nrustflags = [\"-Cpanic=abort\"]\n";
    let lock = "# This file is automatically @generated by Cargo.\n# It is not intended for manual editing.\nversion = 4\n\n\
                [[package]]\nname = \"alloc\"\nversion = \"0.0.0\"\ndependencies = [\n \"compiler_builtins\",\n \"core\",\n]\n\n\
                [[package]]\nname = \"compiler_builtins\"\nversion = \"0.0.0\"\ndependencies = [\n \"core\",\n]\n\n\
                [[package]]\nname = \"core\"\nversion = \"0.0.0\"\n\n\
                [[package]]\nname = \"panic_abort\"\nversion = \"0.0.0\"\ndependencies = [\n \"alloc\",\n \"core\",\n]\n\n\
                [[package]]\nname = \"panic_unwind\"\nversion = \"0.0.0\"\ndependencies = [\n \"alloc\",\n \"core\",\n]\n\n\
                [[package]]\nname = \"proc_macro\"\nversion = \"0.0.0\"\ndependencies = [\n \"core\",\n \"std\",\n]\n\n\
                [[package]]\nname = \"std\"\nversion = \"0.0.0\"\ndependencies = [\n \"alloc\",\n \"core\",\n \"panic_abort\",\n \"panic_unwind\",\n]\n\n\
                [[package]]\nname = \"sysroot\"\nversion = \"0.0.0\"\ndependencies = [\n \"proc_macro\",\n \"std\",\n]\n";
    let core = package("core", "");
    let builtins = package("compiler_builtins", "[dependencies]\ncore = { path = \"../../core\" }\n\n[features]\nmem = []\n");
    let alloc = package(
        "alloc",
        "[dependencies]\ncore = { path = \"../core\" }\ncompiler_builtins = { path = \"../compiler-builtins/compiler-builtins\" }\n\n\
         [features]\ncompiler-builtins-mem = [\"compiler_builtins/mem\"]\n",
    );
    let panic_abort = package("panic_abort", "[dependencies]\nalloc = { path = \"../alloc\" }\ncore = { path = \"../core\" }\n");
    let panic_unwind = package("panic_unwind", "[dependencies]\nalloc = { path = \"../alloc\" }\ncore = { path = \"../core\" }\n");
    let std = package(
        "std",
        "[dependencies]\nalloc = { path = \"../alloc\" }\ncore = { path = \"../core\" }\n\
         panic_abort = { path = \"../panic_abort\" }\npanic_unwind = { path = \"../panic_unwind\", optional = true }\n\n\
         [features]\npanic-unwind = [\"dep:panic_unwind\"]\nbacktrace = []\n",
    );
    let proc_macro = package("proc_macro", "[dependencies]\nstd = { path = \"../std\" }\ncore = { path = \"../core\" }\n");
    let sysroot = package(
        "sysroot",
        "[dependencies]\nproc_macro = { path = \"../proc_macro\" }\nstd = { path = \"../std\" }\n\n\
         [features]\ndefault = [\"panic-unwind\"]\npanic-unwind = [\"std/panic-unwind\"]\nbacktrace = [\"std/backtrace\"]\n",
    );
    let lib = b"#![no_std]\n".as_slice();
    tree.root(&[
        ("Cargo.toml", manifest.as_bytes()),
        ("Cargo.lock", lock.as_bytes()),
        ("UPSTREAM", format!("commit {COMMIT}\n").as_bytes()),
        ("core/Cargo.toml", core.as_bytes()),
        ("core/src/lib.rs", lib),
        ("compiler-builtins/compiler-builtins/Cargo.toml", builtins.as_bytes()),
        ("compiler-builtins/compiler-builtins/src/lib.rs", lib),
        ("alloc/Cargo.toml", alloc.as_bytes()),
        ("alloc/src/lib.rs", lib),
        ("panic_abort/Cargo.toml", panic_abort.as_bytes()),
        ("panic_abort/src/lib.rs", lib),
        ("panic_unwind/Cargo.toml", panic_unwind.as_bytes()),
        ("panic_unwind/src/lib.rs", lib),
        ("std/Cargo.toml", std.as_bytes()),
        ("std/build.rs", b"fn main() {\n    println!(\"cargo:rustc-cfg=backtrace_in_libstd\");\n}\n".as_slice()),
        ("std/src/lib.rs", b"".as_slice()),
        ("proc_macro/Cargo.toml", proc_macro.as_bytes()),
        ("proc_macro/src/lib.rs", lib),
        ("sysroot/Cargo.toml", sysroot.as_bytes()),
        ("sysroot/src/lib.rs", b"".as_slice()),
    ])
}

fn key(root: ContentId) -> StdKey {
    StdKey {
        library: library_mount(root, COMMIT),
        triple: "x86_64-unknown-linux-gnu".to_owned(),
        host: "x86_64-unknown-linux-gnu".to_owned(),
        profile: "dev".to_owned(),
        profile_root: "dev".to_owned(),
        panic: "unwind".to_owned(),
        opt_level: "0".to_owned(),
        debug_assertions: true,
        overflow_checks: true,
        debuginfo: "2".to_owned(),
        crates: vec!["std".to_owned()],
        features: vec!["panic-unwind".to_owned(), "backtrace".to_owned()],
        rustflags: Vec::new(),
    }
}

fn setup() -> (Database, Arc<MemTree>, ContentId) {
    RUNS.with(|it| it.set(0));
    let tree = Arc::new(MemTree::default());
    let root = library(&tree);
    let mut db = Database::new(blaze_query::Config { tree: Some(tree.clone()), dir: None, tags: 0 });
    crate::query::register(&mut db);
    (db, tree, root)
}

fn project(db: &Database, tree: &MemTree, name: &str) -> blaze_vfs::ViewId {
    let manifest = format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n");
    db.open(View { root: tree.root(&[("Cargo.toml", manifest.as_bytes()), ("src/main.rs", b"fn main() {}\n".as_slice())]) })
}

fn planned(db: &Database, view: blaze_vfs::ViewId, key: &StdKey) -> StdPlanned {
    db.pin().get::<StdUnits>(view, key).unwrap().get()
}

fn ready(planned: StdPlanned) -> StdGraph {
    match planned {
        StdPlanned::Ready(graph) => graph,
        StdPlanned::Failed(error) => panic!("the standard library did not load: {}", error.what),
    }
}

fn units_of<'a>(graph: &'a StdGraph, name: &str) -> Vec<&'a Unit> {
    let id = &graph.packages.iter().find(|it| it.name == name).unwrap_or_else(|| panic!("no package {name}")).id;
    graph.units.iter().filter(|it| &it.pkg_id == id).collect()
}

fn lib_of<'a>(graph: &'a StdGraph, name: &str) -> &'a Unit {
    units_of(graph, name).into_iter().find(|it| it.target.is_lib() && it.mode == Mode::Build).unwrap()
}

#[test]
fn the_standard_library_is_one_more_graph() {
    let (db, tree, root) = setup();
    let view = project(&db, &tree, "a");
    let graph = ready(planned(&db, view, &key(root)));
    assert!(graph.units.iter().all(|it| it.is_std));
    assert_eq!(graph.plans.len(), graph.units.len());
    let roots: Vec<&str> = graph.roots.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(roots, ["alloc", "compiler_builtins", "core", "panic_unwind", "proc_macro", "std"]);
    for (name, index) in &graph.roots {
        assert_eq!(&graph.units[*index], lib_of(&graph, name));
    }
    assert_eq!(lib_of(&graph, "panic_abort").profile.panic, "abort");
    assert_eq!(lib_of(&graph, "std").profile.panic, "unwind");
    let features = &lib_of(&graph, "std").features;
    assert!(features.iter().any(|it| it == "backtrace"), "{features:?}");
    assert!(features.iter().any(|it| it == "panic-unwind"), "{features:?}");
    assert!(units_of(&graph, "std").iter().any(|it| it.mode == Mode::RunCustomBuild));
    for plan in &graph.plans {
        assert!(plan.outputs.iter().all(|it| it.starts_with(STD_OUT)), "{:?}", plan.outputs);
    }
    // Nothing of the user's profile but what the key says: opt-level 0 with debug assertions.
    assert_eq!(lib_of(&graph, "core").profile.opt_level, "0");
    assert!(lib_of(&graph, "core").profile.debug_assertions);
}

/// std's own build scripts are of the library (`is_std`, which the daemon gives their output by)
/// and depend on nothing of it but other scripts: they are never compiled
/// ([`super::scripts`] says what they print), so no host std has to be built for them first.
#[test]
fn std_scripts_are_std_and_need_no_std() {
    let (db, tree, root) = setup();
    let view = project(&db, &tree, "a");
    let graph = ready(planned(&db, view, &key(root)));
    let scripts: Vec<&Unit> = graph.units.iter().filter(|it| it.target.is_custom_build()).collect();
    assert!(scripts.iter().any(|it| it.mode == Mode::RunCustomBuild));
    assert!(scripts.iter().any(|it| it.mode != Mode::RunCustomBuild));
    for unit in scripts {
        assert!(unit.is_std, "{}", unit.target.name);
        for dep in &unit.dependencies {
            let it = &graph.units[dep.index];
            assert!(it.target.is_custom_build(), "{} ({:?}) depends on {}", unit.target.name, unit.mode, it.target.name);
        }
    }
}

#[test]
fn another_panic_strategy_is_another_graph() {
    let (db, tree, root) = setup();
    let view = project(&db, &tree, "a");
    let unwind = ready(planned(&db, view, &key(root)));
    let abort = StdKey { panic: "abort".to_owned(), ..key(root) };
    let aborting = ready(planned(&db, view, &abort));
    assert_ne!(unwind, aborting);
    assert_eq!(lib_of(&aborting, "std").profile.panic, "abort");
    assert_eq!(RUNS.with(|it| it.get()), 2);
}

#[test]
fn every_view_shares_one_standard_library() {
    let (db, tree, root) = setup();
    let first = project(&db, &tree, "a");
    let second = project(&db, &tree, "b");
    let once = planned(&db, first, &key(root));
    let again = planned(&db, second, &key(root));
    assert_eq!(once, again);
    assert_eq!(RUNS.with(|it| it.get()), 1);
}

#[test]
fn a_kept_standard_library_is_read_without_allocating() {
    let (db, tree, root) = setup();
    let view = project(&db, &tree, "a");
    let key = key(root);
    let graph = ready(planned(&db, view, &key));
    let pinned = db.pin();
    let hit = blaze_alloc_count::assert_at_most(0, "a StdUnits hit", || pinned.get::<StdUnits>(view, &key).is_ok());
    assert!(hit);
    assert_eq!(ready(pinned.get::<StdUnits>(view, &key).unwrap().get()), graph);
    assert_eq!(RUNS.with(|it| it.get()), 1);
    // Its key's ID is `StdKey::id`, made without the encoding's buffer once the thread has one.
    let id = blaze_alloc_count::assert_at_most(0, "a std key's ID", || <StdUnits as blaze_query::Query>::input(&key));
    assert_eq!(id, key.id());
}

#[test]
fn a_standard_library_plan_says_what_the_daemon_runs_it_by() {
    let (db, tree, root) = setup();
    let view = project(&db, &tree, "a");
    let graph = ready(planned(&db, view, &key(root)));
    assert!(graph.plans.iter().all(|it| !it.unsandboxed && it.runner.is_empty()), "nothing of library/ runs outside");
    assert!(graph.packages.iter().all(|it| it.default_run.is_none()));
    let std_lib = graph.units.iter().position(|it| std::ptr::eq(it, lib_of(&graph, "std"))).unwrap();
    assert!(graph.plans[std_lib].harness);
}

#[test]
fn what_the_target_cannot_do_is_refused() {
    let (db, tree, root) = setup();
    let view = project(&db, &tree, "a");
    let failed = |key: &StdKey| match planned(&db, view, key) {
        StdPlanned::Failed(error) => error.what,
        StdPlanned::Ready(_) => panic!("{key:?} loaded"),
    };
    let wasm = StdKey { triple: "wasm32-wasip1".to_owned(), ..key(root) };
    assert!(failed(&wasm).contains("panic=unwind"));
    let unknown = StdKey { triple: "riscv64gc-unknown-plan9".to_owned(), ..key(root) };
    assert!(failed(&unknown).contains("riscv64gc-unknown-plan9"));
    let custom = StdKey { profile: "fast".to_owned(), profile_root: "fast".to_owned(), ..key(root) };
    assert!(failed(&custom).contains("fast"));
    let none = StdKey { crates: Vec::new(), ..key(root) };
    assert!(failed(&none).contains("no crate"));
}
