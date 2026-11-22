use std::collections::BTreeMap;

use syntax::{Edition, SourceFile};

use crate::{
    Decisions, WorkspaceSnapshot, analyze_workspace, build_questions, compile_file_functions,
    compile_struct_sm, convert_workspace, generate_comptime_scaffold,
    questions::AllocStrategy,
    rewrite::{rewrite_manifest, rewrite_rust_source},
};

#[test]
fn compiles_multi_step_io_fn_with_exact_minimal_liveness() {
    let src = r#"
use std::{env, fs};

pub fn load_and_write(prefix: &str, unused_later: u32) -> Result<String, String> {
    let tag = format!("v-{unused_later}");
    let dir = env::var("APP_DIR")?;
    let path = format!("{dir}/{prefix}.txt");
    let text = fs::read_to_string(&path)?;
    let rendered = format!("{tag}: {text}");
    fs::write(&path, &rendered)?;
    Ok(rendered)
}
"#;

    let machines = compile_file_functions(src, None);
    assert_eq!(machines.len(), 1, "expected 1 compiled state machine");
    let sm = &machines[0];
    assert_eq!(sm.fn_name, "load_and_write");
    assert_eq!(sm.machine_name, "LoadAndWriteMachine");
    assert_eq!(sm.state_count, 5, "Start + 3 suspend waits + Done");

    // Verify exact minimal liveness:
    // - At suspend 0 (`env::var`), `unused_later` is ALREADY DEAD because `tag` was built before!
    //   Only `prefix` and `tag` are needed after suspend 0.
    let saved0 = sm.liveness.saved_at_suspend.get(&0).expect("suspend 0");
    assert!(saved0.contains(&"prefix".to_owned()), "prefix must be saved across suspend 0: {saved0:?}");
    assert!(saved0.contains(&"tag".to_owned()), "tag must be saved across suspend 0: {saved0:?}");
    assert!(
        !saved0.contains(&"unused_later".to_owned()),
        "dead variable `unused_later` must be pruned from suspend 0: {saved0:?}"
    );

    // - At suspend 1 (`fs::read_to_string`), `prefix` and `dir` are DEAD; only `path` and `tag` live!
    let saved1 = sm.liveness.saved_at_suspend.get(&1).expect("suspend 1");
    assert!(saved1.contains(&"path".to_owned()), "path live at suspend 1: {saved1:?}");
    assert!(saved1.contains(&"tag".to_owned()), "tag live at suspend 1: {saved1:?}");
    assert!(!saved1.contains(&"prefix".to_owned()), "prefix dead at suspend 1: {saved1:?}");
    assert!(!saved1.contains(&"dir".to_owned()), "dir dead at suspend 1: {saved1:?}");

    // - At suspend 2 (`fs::write`), ONLY `rendered` is live (`path`, `tag`, `text` are all dead)!
    let saved2 = sm.liveness.saved_at_suspend.get(&2).expect("suspend 2");
    assert_eq!(saved2, &vec!["rendered".to_owned()], "only `rendered` is live at suspend 2");

    // Re-parse emitted sans-IO machine, bstd driver, and sim test with `syntax` -> 0 parse errors!
    for (label, code) in [
        ("sans_io_code", &sm.sans_io_code),
        ("bstd_driver_code", &sm.bstd_driver_code),
        ("sim_test_code", &sm.sim_test_code),
    ] {
        let parsed = SourceFile::parse(code, Edition::CURRENT);
        assert!(
            parsed.errors().is_empty(),
            "{label} had parse errors: {:?}\n---\n{code}",
            parsed.errors()
        );
    }
}

#[test]
fn compiles_if_and_loop_control_flow_across_io() {
    let src = r#"
use std::fs;

pub fn sync_files(paths: Vec<String>, write_out: bool) -> Result<usize, String> {
    let mut count: usize = 0;
    for p in paths {
        let content = fs::read_to_string(&p)?;
        if write_out {
            fs::write(&p, &content)?;
        }
        count += 1;
    }
    Ok(count)
}
"#;
    let machines = compile_file_functions(src, None);
    assert_eq!(machines.len(), 1);
    let sm = &machines[0];
    assert!(sm.sans_io_code.contains("enum SyncFilesEffect"));
    assert!(sm.sans_io_code.contains("ReadFileString"));
    assert!(sm.sans_io_code.contains("WriteFile"));

    let parsed = SourceFile::parse(&sm.sans_io_code, Edition::CURRENT);
    assert!(
        parsed.errors().is_empty(),
        "loop/if state machine had parse errors: {:?}\n{}",
        parsed.errors(),
        sm.sans_io_code
    );
}

#[test]
fn detects_and_splits_mixed_io_struct_and_custom_proc_macro() {
    let mut files = BTreeMap::new();
    files.insert(
        "crates/demo-bot/Cargo.toml".to_owned(),
        r#"[package]
name = "demo-bot"
version = "0.1.0"
edition = "2024"

[dependencies]
serde_json = "1"
blake3 = "1"
rusqlite = "0.32"
"#
        .to_owned(),
    );
    files.insert(
        "crates/demo-bot/src/lib.rs".to_owned(),
        r#"
use std::net::TcpStream;
use std::time::Instant;
use std::mem::swap;

pub enum Phase { Connecting, Play }

pub struct Bot {
    sock: TcpStream,
    phase: Phase,
    started: Instant,
    joined: bool,
    loaded: bool,
    id: u32,
}

pub fn hash_it(b: &[u8]) -> [u8; 32] {
    let _ = swap::<u32>;
    *blake3::hash(b).as_bytes()
}
"#
        .to_owned(),
    );
    files.insert(
        "crates/demo-derive/Cargo.toml".to_owned(),
        r#"[package]
name = "demo-derive"
version = "0.1.0"
edition = "2024"

[lib]
proc-macro = true

[dependencies]
syn = "2"
quote = "1"
proc-macro2 = "1"
"#
        .to_owned(),
    );
    files.insert(
        "crates/demo-derive/src/lib.rs".to_owned(),
        r#"
use proc_macro::TokenStream;

#[proc_macro_derive(Component)]
pub fn derive_component(input: TokenStream) -> TokenStream {
    // inspects ItemStruct and Fields::Named
    input
}
"#
        .to_owned(),
    );

    let snap = WorkspaceSnapshot { files };
    let analysis = analyze_workspace(&snap);
    assert_eq!(analysis.crates.len(), 2);
    assert_eq!(analysis.total_sm_struct_candidates, 1);
    assert_eq!(analysis.total_proc_macro_crates, 1);

    let bot_crate = analysis.crates.iter().find(|c| c.name == "demo-bot").unwrap();
    assert_eq!(bot_crate.struct_sm_candidates.len(), 1);
    let struct_code = compile_struct_sm(&bot_crate.struct_sm_candidates[0]);
    let parsed_struct = SourceFile::parse(&struct_code, Edition::CURRENT);
    assert!(parsed_struct.errors().is_empty(), "struct SM parse errors: {:?}", parsed_struct.errors());

    let derive_crate = analysis.crates.iter().find(|c| c.name == "demo-derive").unwrap();
    assert_eq!(derive_crate.proc_macro_entries.len(), 1);
    let comptime_code = generate_comptime_scaffold(&derive_crate.name, &derive_crate.proc_macro_entries);
    let parsed_ct = SourceFile::parse(&comptime_code, Edition::CURRENT);
    assert!(parsed_ct.errors().is_empty(), "comptime scaffold parse errors: {:?}", parsed_ct.errors());

    // Questions should include `dep:rusqlite` and `procmacro:demo-derive`!
    let questions = build_questions(&analysis);
    assert!(questions.iter().any(|q| q.id == "dep:rusqlite"), "must ask about rusqlite");
    assert!(
        questions.iter().any(|q| q.id == "procmacro:demo-derive"),
        "must recommend comptime reflection for custom proc-macro crate"
    );

    // Decisions JSON round-trip via blaze-json!
    let decisions = Decisions::from_recommended(&questions);
    let json = decisions.to_json().expect("serialize decisions");
    let decoded = Decisions::from_json(&json).expect("deserialize decisions");
    assert_eq!(decisions, decoded);

    // Full conversion drives external deps to 0 when recommended decisions are chosen!
    let conv = convert_workspace(&snap, &analysis, &decisions, None);
    assert_eq!(conv.total_external_edges_before, 6);
    assert_eq!(conv.total_external_edges_after, 0);
    assert!(conv.files.contains_key("crates/demo-derive/src/comptime_migration.rs"));
    assert!(conv.files.contains_key("crates/demo-bot/src/sans_io_machines.rs"));
}

#[test]
fn rewrites_manifest_and_rust_paths_cleanly() {
    let manifest = r#"[package]
name = "sample"
version = "0.1.0"
edition = "2024"

[dependencies]
serde_json = "1"
toml = "0.8"
rsa = "0.9"
"#;
    let updated = rewrite_manifest(
        manifest,
        &["serde_json".to_owned(), "toml".to_owned()],
        &[],
        true,
        Some("/path/to/crates/blaze-std"),
        &["requests".to_owned()],
        &["blaze-json".to_owned(), "blaze-toml".to_owned()],
        None,
    );
    assert!(!updated.contains("serde_json"), "serde_json removed:\n{updated}");
    assert!(!updated.contains("\ntoml ="), "toml removed:\n{updated}");
    assert!(updated.contains("blaze-toml = { workspace = true }"), "blaze-toml added:\n{updated}");
    assert!(updated.contains("rsa = \"0.9\""), "kept dep preserved:\n{updated}");
    assert!(updated.contains("bstd = { package = \"blaze-std\""), "bstd added:\n{updated}");
    assert!(updated.contains("features = [\"requests\"]"), "feature added:\n{updated}");

    let rs = r#"
use std::mem::swap;
use std::collections::HashMap;
use blake3::hash;

fn f(alloc: bstd::mem::Alloc<'_>) {
    let mut m: HashMap<u32, u32> = HashMap::new();
    let v = Vec::new();
    let _ = (m, v, hash(b"hi"), swap::<u32>);
}
"#;
    let (rewritten, count, rem_std) = rewrite_rust_source(
        rs,
        &[("blake3::hash", "bstd::digest::blake3")],
        AllocStrategy::ExplicitAllocHandle,
    );
    assert!(count >= 4, "expected >=4 AST edits, got {count}:\n{rewritten}");
    assert!(!rem_std);
    assert!(rewritten.contains("use core::mem::swap;"));
    assert!(rewritten.contains("use bstd::map::HashMap;"));
    assert!(rewritten.contains("bstd::digest::blake3"));
    assert!(rewritten.contains("bstd::vec::Vec::new_in(alloc)"));
    assert!(rewritten.contains("bstd::map::MapOpts"));

    let parsed = SourceFile::parse(&rewritten, Edition::CURRENT);
    assert!(parsed.errors().is_empty(), "rewritten rust had parse errors: {:?}", parsed.errors());
}

#[test]
fn emitted_sans_io_state_machine_compiles_and_runs_under_rustc() {
    let src = r#"
use std::{env, fs};

pub fn load_and_write(prefix: &str, unused_later: u32) -> Result<String, String> {
    let tag = format!("v-{unused_later}");
    let dir = env::var("APP_DIR")?;
    let path = format!("{dir}/{prefix}.txt");
    let text = fs::read_to_string(&path)?;
    let rendered = format!("{tag}: {text}");
    fs::write(&path, &rendered)?;
    Ok(rendered)
}
"#;
    let machines = compile_file_functions(src, None);
    assert_eq!(machines.len(), 1);
    let sm = &machines[0];

    // Write a standalone test harness that drives the ACTUAL generated `LoadAndWriteMachine`
    // step-by-step and asserts every Effect, Event, and final Result — then compile & run it!
    let harness = format!(
        "{}\n\n\
         fn main() {{\n\
             let mut m = LoadAndWriteMachine::new(\"cfg\".to_owned(), 7);\n\
             assert_eq!(\n\
                 m.step(LoadAndWriteEvent::Start),\n\
                 LoadAndWriteStep::Need(LoadAndWriteEffect::ReadEnv {{ name: \"APP_DIR\".to_owned() }})\n\
             );\n\
             assert_eq!(\n\
                 m.step(LoadAndWriteEvent::ReadEnvDone(Ok(\"/etc/app\".to_owned()))),\n\
                 LoadAndWriteStep::Need(LoadAndWriteEffect::ReadFileString {{ path: \"/etc/app/cfg.txt\".to_owned() }})\n\
             );\n\
             assert_eq!(\n\
                 m.step(LoadAndWriteEvent::ReadFileStringDone(Ok(\"hello\".to_owned()))),\n\
                 LoadAndWriteStep::Need(LoadAndWriteEffect::WriteFile {{ path: \"/etc/app/cfg.txt\".to_owned(), data: \"v-7: hello\".to_owned() }})\n\
             );\n\
             assert_eq!(\n\
                 m.step(LoadAndWriteEvent::WriteFileDone(Ok(()))),\n\
                 LoadAndWriteStep::Done(Ok(\"v-7: hello\".to_owned()))\n\
             );\n\
             assert!(m.is_done());\n\
         }}\n",
        sm.sans_io_code
    );

    let tmp_dir = std::env::temp_dir().join(format!("blaze_bstd_convert_sm_{}", std::process::id()));
    std::fs::create_dir_all(&tmp_dir).expect("mkdir tmp");
    let rs_path = tmp_dir.join("sm_test.rs");
    let bin_path = tmp_dir.join("sm_test");
    std::fs::write(&rs_path, &harness).expect("write harness");

    let compile = std::process::Command::new("rustc")
        .args(["--edition=2024", "-o"])
        .arg(&bin_path)
        .arg(&rs_path)
        .output()
        .expect("run rustc");
    assert!(
        compile.status.success(),
        "generated sans-IO state machine failed to compile!\nstderr:\n{}\n---\nsource:\n{harness}",
        String::from_utf8_lossy(&compile.stderr)
    );

    let run = std::process::Command::new(&bin_path).output().expect("run compiled SM");
    let _ = std::fs::remove_dir_all(&tmp_dir);
    assert!(
        run.status.success(),
        "generated sans-IO state machine failed at runtime!\nstderr:\n{}",
        String::from_utf8_lossy(&run.stderr)
    );
}

#[test]
fn bstd_explicit_alloc_machine_and_grouped_use_and_capability_threading() {
    let src = r#"
use std::{collections::HashMap, mem::swap, time::Instant};

pub fn record_metric(label: &str) -> u64 {
    let mut m = HashMap::new();
    let v = Vec::with_capacity(8);
    let msg = format!("metric:{label}");
    let now = Instant::now();
    let _ = (m, v, msg, swap::<u64>);
    now.elapsed().as_nanos() as u64
}
"#;
    let (rewritten, count, rem_std) = rewrite_rust_source(src, &[], AllocStrategy::ExplicitAllocHandle);
    assert!(count >= 5, "expected grouped use + alloc + clock edits, got {count}:\n{rewritten}");
    assert!(!rem_std, "all std imports should have been split into core + bstd:\n{rewritten}");
    assert!(rewritten.contains("use core::mem::swap;"), "core leaf split:\n{rewritten}");
    assert!(
        rewritten.contains("use bstd::{map::HashMap, clock::Instant};"),
        "bstd leaves grouped:\n{rewritten}"
    );
    assert!(rewritten.contains("alloc: bstd::mem::Alloc<'_>"), "alloc param threaded:\n{rewritten}");
    assert!(rewritten.contains("clock: &bstd::clock::Clock"), "clock capability threaded:\n{rewritten}");
    assert!(rewritten.contains("clock.now()"), "Instant::now rewritten to clock.now():\n{rewritten}");
    assert!(rewritten.contains("bstd::mem::format_in!(alloc,"), "format! rewritten to format_in!:\n{rewritten}");
    assert!(
        rewritten.contains("bstd::vec::Vec::with_capacity_in(8, alloc)"),
        "Vec::with_capacity rewritten:\n{rewritten}"
    );
    let parsed = SourceFile::parse(&rewritten, Edition::CURRENT);
    assert!(parsed.errors().is_empty(), "rewritten file parse errors: {:?}\n{rewritten}", parsed.errors());

    // Verify `SmAllocMode::BstdExplicitAlloc` emits `Machine<'a>`, explicit `bstd::mem::Alloc<'a>`,
    // and parses cleanly with 0 syntax errors!
    let io_src = r#"
use std::{env, fs};

pub fn sync_item(key: &str) -> Result<String, String> {
    let dir = env::var("DATA_DIR")?;
    let path = format!("{dir}/{key}");
    let body = fs::read_to_string(&path)?;
    Ok(body)
}
"#;
    let machines = crate::compile_file_functions_opts(
        io_src,
        None,
        None,
        crate::SmAllocMode::BstdExplicitAlloc,
    );
    assert_eq!(machines.len(), 1);
    let sm = &machines[0];
    assert!(sm.sans_io_code.contains("alloc: bstd::mem::Alloc<'a>"), "machine holds explicit Alloc:\n{}", sm.sans_io_code);
    assert!(sm.sans_io_code.contains("bstd::mem::String<bstd::mem::Alloc<'a>>"), "uses bstd String:\n{}", sm.sans_io_code);
    assert!(sm.sans_io_code.contains("bstd::mem::format_in!(alloc,"), "uses format_in!:\n{}", sm.sans_io_code);
    assert!(sm.bstd_driver_code.contains("alloc: bstd::mem::Alloc<'a>"), "driver threads explicit Alloc:\n{}", sm.bstd_driver_code);
    assert!(sm.bstd_driver_code.contains("fs: &bstd::fs::Dir<bstd::fs::ReadOnly>"), "driver narrows Dir<ReadOnly>:\n{}", sm.bstd_driver_code);
    assert!(sm.bstd_driver_code.contains("env: &bstd::env::Env"), "driver takes Env capability:\n{}", sm.bstd_driver_code);

    for (lbl, code) in [
        ("bstd_sans_io", &sm.sans_io_code),
        ("bstd_driver", &sm.bstd_driver_code),
        ("bstd_sim_test", &sm.sim_test_code),
    ] {
        let p = SourceFile::parse(code, Edition::CURRENT);
        assert!(p.errors().is_empty(), "{lbl} parse errors: {:?}\n{code}", p.errors());
    }
}

#[test]
fn cyclic_loop_with_mut_var_compiles_and_runs_under_rustc() {
    let src = r#"
use std::fs;

pub fn count_lines(paths: Vec<String>) -> Result<usize, String> {
    let mut total: usize = 0;
    for p in paths {
        let text = fs::read_to_string(&p)?;
        if !text.is_empty() {
            total += 1;
        }
    }
    Ok(total)
}
"#;
    let machines = compile_file_functions(src, None);
    assert_eq!(machines.len(), 1);
    let sm = &machines[0];
    assert!(!sm.sans_io_code.contains("unreachable!(\"bounded CFG"), "no depth limit fallback:\n{}", sm.sans_io_code);

    let harness = format!(
        "{}\n\n\
         fn main() {{\n\
             let mut m = CountLinesMachine::new(vec![\"a\".to_owned(), \"b\".to_owned(), \"c\".to_owned()]);\n\
             assert_eq!(m.step(CountLinesEvent::Start), CountLinesStep::Need(CountLinesEffect::ReadFileString {{ path: \"a\".to_owned() }}));\n\
             assert_eq!(m.step(CountLinesEvent::ReadFileStringDone(Ok(\"hi\".to_owned()))), CountLinesStep::Need(CountLinesEffect::ReadFileString {{ path: \"b\".to_owned() }}));\n\
             assert_eq!(m.step(CountLinesEvent::ReadFileStringDone(Ok(String::new()))), CountLinesStep::Need(CountLinesEffect::ReadFileString {{ path: \"c\".to_owned() }}));\n\
             assert_eq!(m.step(CountLinesEvent::ReadFileStringDone(Ok(\"ok\".to_owned()))), CountLinesStep::Done(Ok(2)));\n\
             assert!(m.is_done());\n\
         }}\n",
        sm.sans_io_code
    );

    let tmp_dir = std::env::temp_dir().join(format!("blaze_bstd_convert_loop_{}", std::process::id()));
    std::fs::create_dir_all(&tmp_dir).expect("mkdir tmp");
    let rs_path = tmp_dir.join("loop_sm.rs");
    let bin_path = tmp_dir.join("loop_sm");
    std::fs::write(&rs_path, &harness).expect("write loop harness");

    let compile = std::process::Command::new("rustc")
        .args(["--edition=2024", "-o"])
        .arg(&bin_path)
        .arg(&rs_path)
        .output()
        .expect("run rustc");
    assert!(
        compile.status.success(),
        "cyclic loop SM failed to compile!\nstderr:\n{}\n---\nsource:\n{harness}",
        String::from_utf8_lossy(&compile.stderr)
    );
    let run = std::process::Command::new(&bin_path).output().expect("run loop SM");
    let _ = std::fs::remove_dir_all(&tmp_dir);
    assert!(run.status.success(), "cyclic loop SM runtime failed: {}", String::from_utf8_lossy(&run.stderr));
}

#[test]
fn c_and_cpp_interop_recommends_blaze_c_import_and_strips_build_deps() {
    let mut files = BTreeMap::new();
    // 1. A C FFI crate using `bindgen` + `cc` in `build.rs`.
    files.insert(
        "crates/zstd-wrapper/Cargo.toml".to_owned(),
        r#"[package]
name = "zstd-wrapper"
version = "0.1.0"
edition = "2024"

[dependencies]

[build-dependencies]
bindgen = "0.72"
cc = "1.2"
"#
        .to_owned(),
    );
    files.insert(
        "crates/zstd-wrapper/build.rs".to_owned(),
        r#"fn main() {
    cc::Build::new()
        .file("vendor/zstd/zstd.c")
        .include("vendor/zstd/include")
        .define("ZSTD_STATIC_LINKING_ONLY", Some("1"))
        .compile("zstd");
    let bindings = bindgen::Builder::default()
        .header("vendor/zstd/include/zstd.h")
        .clang_arg("-Ivendor/zstd/include")
        .generate()
        .unwrap();
    bindings.write_to_file("bindings.rs").unwrap();
}
"#
        .to_owned(),
    );
    files.insert(
        "crates/zstd-wrapper/src/lib.rs".to_owned(),
        r#"include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
"#
        .to_owned(),
    );

    // 2. A C++ FFI crate using `cxx` + `cxx-build`.
    files.insert(
        "crates/rocks-cpp/Cargo.toml".to_owned(),
        r#"[package]
name = "rocks-cpp"
version = "0.1.0"
edition = "2024"

[dependencies]
cxx = "1.0"

[build-dependencies]
cxx-build = "1.0"
"#
        .to_owned(),
    );
    files.insert(
        "crates/rocks-cpp/build.rs".to_owned(),
        r#"fn main() {
    cxx_build::bridge("src/lib.rs")
        .file("vendor/rocksdb/db.cc")
        .include("vendor/rocksdb/include")
        .std("c++20")
        .compile("rocks-cpp");
}
"#
        .to_owned(),
    );
    files.insert(
        "crates/rocks-cpp/src/lib.rs".to_owned(),
        r#"#[cxx::bridge]
mod ffi {
    unsafe extern "C++" {
        include!("rocksdb/db.h");
    }
}
"#
        .to_owned(),
    );

    let snap = WorkspaceSnapshot { files };
    let analysis = analyze_workspace(&snap);
    assert_eq!(analysis.total_c_interop_crates, 2);

    let questions = build_questions(&analysis);
    let q_c = questions.iter().find(|q| q.id == "c_interop:zstd-wrapper").expect("C interop question");
    assert_eq!(q_c.default_key(), "blaze_c_import");
    assert!(q_c.prompt.contains("blaze::c::import!"));

    let q_cpp = questions.iter().find(|q| q.id == "c_interop:rocks-cpp").expect("C++ interop question");
    assert_eq!(q_cpp.default_key(), "blaze_c_import");
    assert!(q_cpp.prompt.contains("blaze::cpp::import!"));

    let decisions = Decisions::from_recommended(&questions);
    let converted = convert_workspace(&snap, &analysis, &decisions, None);
    assert_eq!(converted.total_c_interop_scaffolds, 2);

    // Check C migration scaffold & Cargo.toml.
    let c_mig = converted
        .files
        .get("crates/zstd-wrapper/src/c_interop_migration.rs")
        .expect("C migration scaffold");
    assert!(c_mig.contains("blaze::c::import! {"), "C macro:\n{c_mig}");
    assert!(c_mig.contains("header \"vendor/zstd/include/zstd.h\";"), "header extracted:\n{c_mig}");
    assert!(c_mig.contains("include \"vendor/zstd/include\";"), "include extracted:\n{c_mig}");
    assert!(c_mig.contains("define ZSTD_STATIC_LINKING_ONLY = \"1\";"), "define extracted:\n{c_mig}");
    assert!(c_mig.contains("crates/zstd-wrapper/src/lib.rs:1"), "OUT_DIR include noted:\n{c_mig}");
    let c_parse = SourceFile::parse(c_mig, Edition::CURRENT);
    assert!(c_parse.errors().is_empty(), "C scaffold parse errors: {:?}\n{c_mig}", c_parse.errors());

    let c_toml = converted.files.get("crates/zstd-wrapper/Cargo.toml").unwrap();
    assert!(!c_toml.contains("bindgen ="), "bindgen stripped:\n{c_toml}");
    assert!(!c_toml.contains("cc ="), "cc stripped:\n{c_toml}");
    assert!(c_toml.contains("[package.metadata.blaze.c]"), "blaze.c table added:\n{c_toml}");
    assert!(c_toml.contains("sources = [\"vendor/zstd/zstd.c\"]"), "sources added:\n{c_toml}");
    assert!(
        blaze_toml::Document::parse_in(c_toml, blaze_toml::Limits::CARGO, blaze_own::Alloc::global()).is_ok(),
        "C Cargo.toml valid:\n{c_toml}"
    );

    // Check C++ migration scaffold & Cargo.toml.
    let cpp_mig = converted
        .files
        .get("crates/rocks-cpp/src/c_interop_migration.rs")
        .expect("C++ migration scaffold");
    assert!(cpp_mig.contains("blaze::cpp::import! {"), "C++ macro:\n{cpp_mig}");
    assert!(cpp_mig.contains("header \"rocksdb/db.h\";"), "C++ header extracted:\n{cpp_mig}");
    assert!(cpp_mig.contains("std \"c++20\";"), "C++20 std extracted:\n{cpp_mig}");
    let cpp_parse = SourceFile::parse(cpp_mig, Edition::CURRENT);
    assert!(cpp_parse.errors().is_empty(), "C++ scaffold parse errors: {:?}\n{cpp_mig}", cpp_parse.errors());

    let cpp_toml = converted.files.get("crates/rocks-cpp/Cargo.toml").unwrap();
    assert!(!cpp_toml.contains("cxx-build ="), "cxx-build stripped:\n{cpp_toml}");
    assert!(cpp_toml.contains("[package.metadata.blaze.c]"), "blaze.c table added:\n{cpp_toml}");
    assert!(cpp_toml.contains("std = \"c++20\""), "std = c++20 in toml:\n{cpp_toml}");
    assert!(
        blaze_toml::Document::parse_in(cpp_toml, blaze_toml::Limits::CARGO, blaze_own::Alloc::global()).is_ok(),
        "C++ Cargo.toml valid:\n{cpp_toml}"
    );
}

#[test]
fn transitive_whole_crate_alloc_and_capability_threading_rewrites_signatures_returns_and_call_sites() {
    let mut files = BTreeMap::new();
    files.insert(
        "crates/demo/Cargo.toml".to_owned(),
        r#"[package]
name = "demo"
version = "0.1.0"
edition = "2024"

[dependencies]
"#
        .to_owned(),
    );
    files.insert(
        "crates/demo/src/format.rs".to_owned(),
        r#"pub fn format_label(id: u32) -> String {
    format!("item-{id}")
}

pub fn make_boxed(id: u32) -> Box<String> {
    Box::new(format_label(id))
}
"#
        .to_owned(),
    );
    files.insert(
        "crates/demo/src/lib.rs".to_owned(),
        r#"use std::{env, time::Instant};
pub mod format;

pub fn build_labels(n: u32) -> Vec<String> {
    let mut out = Vec::with_capacity(n as usize);
    for i in 0..n {
        out.push(format::format_label(i));
    }
    out
}

pub fn timed_build(n: u32) -> (u64, Vec<String>) {
    let t0 = Instant::now();
    let labels = build_labels(n);
    (t0.elapsed().as_nanos() as u64, labels)
}

pub fn read_tag() -> Result<Option<&'static str>, ()> {
    let _ = env::var("APP_TAG");
    Ok(None)
}

#[test]
fn smoke_root_boundary() {
    let _ = format::format_label(7);
}
"#
        .to_owned(),
    );

    let snap = WorkspaceSnapshot { files };
    let analysis = analyze_workspace(&snap);
    let questions = build_questions(&analysis);
    let decisions = Decisions::from_recommended(&questions);
    let converted = convert_workspace(&snap, &analysis, &decisions, None);

    let fmt_rs = converted.files.get("crates/demo/src/format.rs").expect("format.rs");
    let lib_rs = converted.files.get("crates/demo/src/lib.rs").expect("lib.rs");

    // 1. Leaf function gets `'a`, `alloc: bstd::mem::Alloc<'a>`, and explicit-alloc `String` return type.
    assert!(
        fmt_rs.contains("pub fn format_label<'a>(id: u32, alloc: bstd::mem::Alloc<'a>) -> bstd::mem::String<bstd::mem::Alloc<'a>>"),
        "format_label signature:\n{fmt_rs}"
    );
    // 2. Same-file caller `make_boxed` gets explicit `Box<String>` return type AND forwards `alloc` to `format_label(id, alloc)`.
    assert!(
        fmt_rs.contains("pub fn make_boxed<'a>(id: u32, alloc: bstd::mem::Alloc<'a>) -> bstd::boxed::Box<bstd::mem::String<bstd::mem::Alloc<'a>>, bstd::mem::Alloc<'a>>"),
        "make_boxed signature:\n{fmt_rs}"
    );
    assert!(fmt_rs.contains("format_label(id, alloc)"), "format_label call forwarded:\n{fmt_rs}");

    // 3. Cross-file caller `build_labels` gets explicit `Vec<String>` return type AND forwards `alloc` to `format::format_label(i, alloc)`.
    assert!(
        lib_rs.contains("pub fn build_labels<'a>(n: u32, alloc: bstd::mem::Alloc<'a>) -> bstd::vec::Vec<bstd::mem::String<bstd::mem::Alloc<'a>>, bstd::mem::Alloc<'a>>"),
        "build_labels signature:\n{lib_rs}"
    );
    assert!(lib_rs.contains("format::format_label(i, alloc)"), "cross-file call forwarded:\n{lib_rs}");

    // 4. Transitive caller `timed_build` gets BOTH `alloc` (from `build_labels`) and `clock` (from `Instant::now()`),
    //    rewrites its tuple return type, and forwards `alloc` to `build_labels(n, alloc)`.
    assert!(
        lib_rs.contains("pub fn timed_build<'a>(n: u32, alloc: bstd::mem::Alloc<'a>, clock: &bstd::clock::Clock) -> (u64, bstd::vec::Vec<bstd::mem::String<bstd::mem::Alloc<'a>>, bstd::mem::Alloc<'a>>)"),
        "timed_build signature:\n{lib_rs}"
    );
    assert!(lib_rs.contains("build_labels(n, alloc)"), "transitive call forwarded:\n{lib_rs}");
    assert!(lib_rs.contains("clock.now()"), "clock.now rewritten:\n{lib_rs}");

    // 5. `env::var("APP_TAG")` rewritten to `env.get(&bstd::var!("APP_TAG"))` + threads `env: &bstd::env::Env`.
    assert!(lib_rs.contains("env.get(&bstd::var!(\"APP_TAG\"))"), "env.get rewritten:\n{lib_rs}");
    assert!(lib_rs.contains("env: &bstd::env::Env"), "env param threaded:\n{lib_rs}");

    // 6. Root `#[test]` boundary passes `bstd::mem::Alloc::global()` instead of adding a parameter.
    assert!(
        lib_rs.contains("fn smoke_root_boundary()"),
        "test fn signature kept:\n{lib_rs}"
    );
    assert!(
        lib_rs.contains("format::format_label(7, bstd::mem::Alloc::global())"),
        "test root passes Alloc::global():\n{lib_rs}"
    );

    // 7. Both rewritten files parse with 0 syntax errors!
    for (name, text) in [("format.rs", fmt_rs), ("lib.rs", lib_rs)] {
        let p = SourceFile::parse(text, Edition::CURRENT);
        assert!(p.errors().is_empty(), "{name} parse errors: {:?}\n{text}", p.errors());
    }
}

#[test]
fn cross_crate_propagation_struct_alloc_parameterization_and_collect_in() {
    let mut files = BTreeMap::new();
    // Crate 1: `core-fmt` exports `pub fn make_id(n: u32) -> String`
    files.insert(
        "crates/core-fmt/Cargo.toml".to_owned(),
        r#"[package]
name = "core-fmt"
version = "0.1.0"
edition = "2024"

[dependencies]
"#
        .to_owned(),
    );
    files.insert(
        "crates/core-fmt/src/lib.rs".to_owned(),
        r#"pub fn make_id(n: u32) -> String {
    format!("id-{n}")
}
"#
        .to_owned(),
    );

    // Crate 2: `app` depends on `core-fmt`, calls both `core_fmt::make_id` and imported `make_id`,
    // uses `.collect::<Vec<_>>()`, and defines both a plain heap struct and a `#[repr(C)]` struct.
    files.insert(
        "crates/app/Cargo.toml".to_owned(),
        r#"[package]
name = "app"
version = "0.1.0"
edition = "2024"

[dependencies]
core-fmt = { path = "../core-fmt" }
"#
        .to_owned(),
    );
    files.insert(
        "crates/app/src/lib.rs".to_owned(),
        r#"use std::collections::HashMap;
use core_fmt::make_id;

pub struct Cache {
    pub items: Vec<String>,
    pub counts: HashMap<String, u32>,
}

impl Cache {
    pub fn total(&self) -> usize {
        self.items.len()
    }
}

#[repr(C)]
pub struct FfiHeader {
    pub raw: Vec<u8>,
}

pub fn collect_ids(n: u32) -> Vec<String> {
    (0..n).map(|i| make_id(i)).collect::<Vec<_>>()
}

pub fn make_qualified(n: u32) -> String {
    core_fmt::make_id(n)
}
"#
        .to_owned(),
    );

    let snap = WorkspaceSnapshot { files };
    let analysis = analyze_workspace(&snap);
    let questions = build_questions(&analysis);
    let decisions = Decisions::from_recommended(&questions);
    let converted = convert_workspace(&snap, &analysis, &decisions, None);

    assert_eq!(converted.total_parameterized_structs, 1, "only Cache (not #[repr(C)] FfiHeader) parameterized");
    assert_eq!(converted.total_cross_crate_call_rewrites, 2, "both imported and qualified cross-crate calls forwarded");

    let core_rs = converted.files.get("crates/core-fmt/src/lib.rs").expect("core-fmt/src/lib.rs");
    let app_rs = converted.files.get("crates/app/src/lib.rs").expect("app/src/lib.rs");

    assert!(
        core_rs.contains("pub fn make_id<'a>(n: u32, alloc: bstd::mem::Alloc<'a>) -> bstd::mem::String<bstd::mem::Alloc<'a>>"),
        "core-fmt::make_id rewritten:\n{core_rs}"
    );
    assert!(app_rs.contains("use bstd::vec::CollectIn as _;"), "CollectIn trait imported:\n{app_rs}");
    assert!(app_rs.contains("pub struct Cache<'a>"), "Cache parameterized:\n{app_rs}");
    assert!(
        app_rs.contains("pub items: bstd::vec::Vec<bstd::mem::String<bstd::mem::Alloc<'a>>, bstd::mem::Alloc<'a>>"),
        "Cache.items field rewritten:\n{app_rs}"
    );
    assert!(
        app_rs.contains("pub counts: bstd::map::Map<bstd::mem::String<bstd::mem::Alloc<'a>>, u32, bstd::hash::FixedState, bstd::mem::Alloc<'a>>"),
        "Cache.counts field rewritten:\n{app_rs}"
    );
    assert!(app_rs.contains("impl<'a> Cache<'a>"), "impl<'a> Cache<'a> updated:\n{app_rs}");
    assert!(app_rs.contains("pub struct FfiHeader {"), "#[repr(C)] FfiHeader untouched:\n{app_rs}");
    assert!(app_rs.contains("make_id(i, alloc)"), "imported cross-crate call forwarded:\n{app_rs}");
    assert!(app_rs.contains(".collect_in(alloc)"), ".collect::<Vec<_>>() rewritten to .collect_in(alloc):\n{app_rs}");
    assert!(
        app_rs.contains("pub fn make_qualified<'a>(n: u32, alloc: bstd::mem::Alloc<'a>) -> bstd::mem::String<bstd::mem::Alloc<'a>>"),
        "cross-crate caller signature propagated:\n{app_rs}"
    );
    assert!(app_rs.contains("core_fmt::make_id(n, alloc)"), "qualified cross-crate call forwarded:\n{app_rs}");

    for (name, text) in [("core-fmt/src/lib.rs", core_rs), ("app/src/lib.rs", app_rs)] {
        let p = SourceFile::parse(text, Edition::CURRENT);
        assert!(p.errors().is_empty(), "{name} parse errors: {:?}\n{text}", p.errors());
    }
}

#[test]
fn detects_primitive_obsession_and_emits_explicit_newtypes_module() {
    let mut files = BTreeMap::new();
    files.insert(
        "crates/netbuf/Cargo.toml".to_owned(),
        r#"[package]
name = "netbuf"
version = "0.1.0"
edition = "2024"

[dependencies]
"#
        .to_owned(),
    );
    files.insert(
        "crates/netbuf/src/lib.rs".to_owned(),
        r#"pub struct FrameHeader {
    pub offset: u64,
    pub len: u64,
    pub peer_id: u32,
    pub session_id: u64,
    pub port: u16,
    pub timeout_ms: u64,
    pub align: usize,
}
"#
        .to_owned(),
    );

    let snap = WorkspaceSnapshot { files };
    let analysis = analyze_workspace(&snap);
    assert!(
        analysis.total_newtype_candidates >= 5,
        "expected offset/len, peer_id, session_id, port, timeout_ms, align; got {}",
        analysis.total_newtype_candidates
    );

    let questions = build_questions(&analysis);
    let q = questions
        .iter()
        .find(|q| q.id == "newtype:promote_primitives")
        .expect("newtype promotion question");
    assert_eq!(q.default_key(), "emit_newtypes_module");

    let decisions = Decisions::from_recommended(&questions);
    let converted = convert_workspace(&snap, &analysis, &decisions, None);
    assert_eq!(converted.total_newtype_scaffolds, 1);

    let nt_rs = converted.files.get("crates/netbuf/src/newtypes.rs").expect("src/newtypes.rs");
    assert!(nt_rs.contains("ByteOffset"), "ByteOffset re-exported:\n{nt_rs}");
    assert!(nt_rs.contains("ByteLen"), "ByteLen re-exported:\n{nt_rs}");
    assert!(nt_rs.contains("Alignment"), "Alignment re-exported:\n{nt_rs}");
    assert!(nt_rs.contains("Port"), "Port re-exported:\n{nt_rs}");
    assert!(nt_rs.contains("bstd::id::id!"), "u32 peer_id -> bstd::id::id!:\n{nt_rs}");
    assert!(nt_rs.contains("pub struct PeerId;"), "PeerId declared:\n{nt_rs}");
    assert!(nt_rs.contains("opaque pub struct SessionId(u64);"), "u64 session_id -> opaque newtype:\n{nt_rs}");
    let p = SourceFile::parse(nt_rs, Edition::CURRENT);
    assert!(p.errors().is_empty(), "newtypes.rs parse errors: {:?}\n{nt_rs}", p.errors());
}
