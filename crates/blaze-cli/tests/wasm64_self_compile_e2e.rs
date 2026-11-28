//! End-to-end proof of Blaze's self-compiled `wasm64` (`memory64`) usercode stack:
//! real Rust source text -> `blaze-ladder` front-end + `Walk` + `Maker` -> `compile_walk_to_wasm64`
//! -> `blaze_export::lir_wasm64::compile_lir_wasm64` -> LIVE Wasmtime `memory64` execution inside
//! BOTH `blaze_proc_macro::script::ScriptRuntime` (build scripts) AND `blaze_proc_macro::host::Runtime`
//! (proc macros) — zero `rustc`, zero `wasm-lld`, zero `wasi-libc`, zero hand-built LIR.

mod support;

use std::collections::HashMap;
use std::sync::Arc;

use blaze_build_script::{
    Env,
    key::{CMode, RunLimits},
};
use blaze_emit::{Isa, Os};
use blaze_expand::{Edition, ProcCall, ProcKind, lex};
use blaze_export::wasm64::{Wasm64Opts, check_wasm64};
use blaze_id::{CrateItemKey, ItemKey as LibKey};
use blaze_ids::Hasher;
use blaze_ids::producer::{CodeBuilder, ProducerId, Producers};
use blaze_ladder::body_ids::{BodyId, BodyIds, Grant, Read};
use blaze_ladder::code_records::{Builder, MemoryStore, Settings, TierName};
use blaze_ladder::crates::{Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::keys::checker_of;
use blaze_ladder::front2::source::{Load, LoadedExpansion, Log, NamesSource, Records};
use blaze_ladder::run::OverflowChecks;
use blaze_ladder::run::make_one::wasm64::{
    Wasm64BuildOpts, compile_roots_to_wasm64, compile_walk_to_wasm64, default_wasm64_import,
};
use blaze_ladder::run::make_one::{Maker, NamesRecords, SecondCheck, walk_records};
use blaze_ladder::run::special::MakerStart;
use blaze_ladder::symbols::static_symbol;
use blaze_own::{AVec, Alloc, Vec as OwnVec};
use blaze_proc_macro::{
    host::{CallError, Limits, Runtime},
    module::{Exported, ModuleRecord, sha256_hex},
    package_env::PackageEnv,
    sandbox::{Files, Package},
    script::ScriptRuntime,
    script_sandbox::{Script, ScriptRoot, Tree},
};
use blaze_scope::resolve::aligned;
use blaze_scope::{items_record, names_record};
use blaze_surface::ItemKind;
use blaze_toolchain::ToolchainId;
use blaze_typeck::ItemKey;

const DIR: &str = "/wasm64_cli_e2e/";
const ROOT: &str = "/wasm64_cli_e2e/src/main.rs";
const LANG_CRATES: [usize; 1] = [0];
const READS: [Read; 1] = [Read { fact: [5; 32], digest: [6; 32] }];

#[derive(Clone, Copy)]
struct TestGrant;

impl Grant for TestGrant {}

fn alloc() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

fn leak(s: &str) -> &'static str {
    Box::leak(s.to_string().into_boxed_str())
}

fn producer() -> ProducerId<CodeBuilder> {
    Producers::for_tests(b"wasm64 cli e2e").get::<CodeBuilder>()
}

fn builder() -> Builder {
    Builder {
        isa: Isa { lse: true, os: Os::MacOs },
        tier: TierName::Reference,
        settings: Settings::baseline(OverflowChecks::Off, blaze_ladder::run::Panic::Unwind),
        toolchain: ToolchainId([9; 32]),
        producer: producer(),
        edition: edition::PackageEdition::of_manifest("2024").expect("2024 is an edition"),
    }
}

#[derive(Clone, Default)]
struct Kept {
    names: Vec<&'static [u8]>,
    items: Vec<&'static [u8]>,
    files: Vec<HashMap<String, &'static str>>,
    expansions: Vec<HashMap<LibKey, &'static str>>,
    origins: Vec<HashMap<LibKey, &'static [u8]>>,
}

impl Load<'static> for Kept {
    fn names(&self, krate: usize) -> Option<&'static [u8]> {
        self.names.get(krate).copied()
    }

    fn items(&self, krate: usize) -> Option<&'static [u8]> {
        self.items.get(krate).copied()
    }

    fn file(
        &self,
        krate: usize,
        path: blaze_scope::package_path::PackagePath<'_>,
    ) -> Option<&'static str> {
        self.files.get(krate)?.get(path.as_str()).copied()
    }

    fn expansion(&self, krate: usize, call: LibKey) -> Option<LoadedExpansion<'static>> {
        let text = self.expansions.get(krate)?.get(&call).copied()?;
        let origins = self.origins.get(krate).and_then(|m| m.get(&call)).copied().map(|bytes| {
            if bytes.is_empty() {
                (&[][..], &[][..])
            } else {
                blaze_scope::expansion_origins::split(bytes).expect("expansion origins")
            }
        });
        Some(LoadedExpansion { text, origins })
    }
}

fn made_origins(
    call: LibKey,
    origins: &Result<&[u8], blaze_scope::expansion_origins::OriginsError>,
) -> &'static [u8] {
    use blaze_scope::expansion_origins::OriginsError;
    match origins {
        Ok(bytes) => Box::leak(bytes.to_vec().into_boxed_slice()),
        Err(OriginsError::Rewritten { .. } | OriginsError::Unplaced { .. }) => &[],
        Err(e) => panic!("origins of expansion {call:x}: {e:?}"),
    }
}

struct Fixture {
    kept: Kept,
    keys: &'static [CrateItemKey],
    crate_names: &'static [&'static str],
}

fn fixture(text: &str) -> Fixture {
    let a = alloc();
    let edition = edition::PackageEdition::of_manifest("2024").expect("2024 is an edition");
    let files = vec![(String::from(ROOT), String::from(text))];
    let user = UserCrate {
        name: "wasm64_cli_e2e",
        dir: DIR,
        root: ROOT,
        files: &files,
        edition,
        panic: blaze_ladder::run::Panic::Unwind,
    };
    let tc = Toolchain::none(a);
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("inputs: {e}"));
    let crates = inputs.crates();
    let keys: &'static [CrateItemKey] = Vec::leak(crates.iter().map(|c| c.key).collect());
    let crate_names: &'static [&'static str] =
        Vec::leak(crates.iter().map(|c| leak(c.name)).collect());
    let names = inputs
        .resolve(&crates, blaze_own::testing::bump())
        .unwrap_or_else(|e| panic!("resolve: {e}"));
    let mut kept = Kept::default();
    for c in names.crates().iter() {
        kept.names.push(aligned(
            &names_record::encode(c, a).unwrap_or_else(|e| panic!("names record: {e:?}")),
            blaze_own::testing::bump(),
        ));
        kept.items.push(aligned(
            &items_record::encode_with(c, &names, a)
                .unwrap_or_else(|e| panic!("items record: {e:?}")),
            blaze_own::testing::bump(),
        ));
        kept.files
            .push(c.files.iter().map(|f| (f.package.as_str().to_string(), leak(f.text))).collect());
        kept.expansions.push(
            c.expansions
                .iter()
                .filter_map(|(call, e)| e.made.map(|m| (*call, leak(m.text))))
                .collect(),
        );
        kept.origins.push(
            c.expansions
                .iter()
                .filter_map(|(call, e)| {
                    e.made.as_ref().map(|m| (*call, made_origins(*call, &m.origins)))
                })
                .collect(),
        );
    }
    Fixture { kept, keys, crate_names }
}

fn reader(f: &Fixture) -> NamesRecords<'static> {
    let a = alloc();
    let bump = blaze_own::testing::bump();
    let frame: &'static blaze_mem::Frame =
        bump.alloc_val(blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("reader frame"));
    let log: &'static Log<'static> = bump.alloc_val(Log::new(f.crate_names, a));
    let load: &'static Kept = bump.alloc_val(f.kept.clone());
    let records: &'static Records<'static> = bump.alloc_val(Records::new(f.keys, load, log, frame, a));
    NamesRecords { records, frame, lang_crates: &LANG_CRATES, body_sources: None }
}

fn item_named(f: &Fixture, kind: ItemKind, name: &str) -> ItemKey {
    let records = reader(f).records;
    let mut found = None;
    assert!(
        records.each_item(0, &mut |key, _| {
            if found.is_none()
                && records.kind_of_item(key) == Some(kind)
                && records.name_of(key) == Some(name)
            {
                found = Some(checker_of(key));
            }
        }),
        "items record should list items"
    );
    found.unwrap_or_else(|| panic!("no item `{name}` of kind {kind:?}"))
}

struct EveryItem;

impl BodyIds for EveryItem {
    fn body_check(&self, item: ItemKey) -> Option<BodyId<'_>> {
        Some(BodyId {
            key: Hasher::new("wasm64 cli e2e item").part(&item.0.to_le_bytes()[..]).finish(),
            reads: &READS,
        })
    }

    fn today(&self, fact: &[u8; 32]) -> Option<[u8; 32]> {
        (*fact == READS[0].fact).then_some(READS[0].digest)
    }

    fn crate_fact(&self, _item: ItemKey) -> Option<Read> {
        Some(Read { fact: [8; 32], digest: [9; 32] })
    }
}

/// 1. Real Rust build-script source -> Blaze front-end -> `compile_walk_to_wasm64` ->
///    LIVE Wasmtime `memory64` execution inside `ScriptRuntime`.
#[test]
fn real_rust_build_script_self_compiles_to_wasm64_and_runs_in_script_runtime() {
    const RUST_BUILD_SCRIPT: &str = r#"#![feature(no_core, lang_items)]
#![no_core]
#![no_main]

#[lang = "sized"]
trait Sized {}

#[lang = "copy"]
trait Copy {}

impl Copy for bool {}
impl Copy for u8 {}
impl Copy for i32 {}
impl Copy for u64 {}
impl Copy for usize {}
impl<T> Copy for *const T {}
impl<T> Copy for *mut T {}

#[repr(C)]
struct IoVec {
    buf: *const u8,
    len: u64,
}

unsafe extern "C" {
    fn fd_write(fd: i32, iovs: *const IoVec, iovs_len: u64, nwritten: *mut u64) -> i32;
    fn proc_exit(code: i32);
}

static GOOD_LINES: [u8; 64] = *b"cargo:rustc-cfg=self_compiled_wasm64\ncargo:rustc-env=BLAZE=ok64\n";
static WRONG_LINES: [u8; 25] = *b"cargo:rustc-cfg=bad_path\n";

fn sum_to(n: i32) -> i32 {
    let mut i = 1;
    let mut acc = 0;
    while i <= n {
        acc = acc + i;
        i = i + 1;
    }
    acc
}

fn pick<T>(cond: bool, a: T, b: T) -> T {
    if cond { a } else { b }
}

#[unsafe(no_mangle)]
pub extern "C-unwind" fn main() {
    let ok = sum_to(10) == 55;
    let ptr = pick(ok, &GOOD_LINES as *const [u8; 64] as *const u8, &WRONG_LINES as *const [u8; 25] as *const u8);
    let len = pick(ok, 64u64, 25u64);
    let iov = IoVec { buf: ptr, len };
    let mut written: u64 = 0;
    unsafe {
        fd_write(1, &iov as *const IoVec, 1, &mut written as *mut u64);
        proc_exit(if written == 64 { 0 } else { 1 });
    }
}
"#;

    let f = fixture(RUST_BUILD_SCRIPT);
    let main_item = item_named(&f, ItemKind::Function, "main");
    let b = builder();
    let a = alloc();
    let mut store = MemoryStore::new_in(a);
    let mut walk_maker = Maker::new(reader(&f), Some(main_item), &b, a);
    let (walk, _) = walk_records(main_item, &EveryItem, &mut store, &b, &producer(), &mut walk_maker, a)
        .expect("walk real Rust build script");

    let entry_sym = walk.symbol(0);
    let mut lir_maker = Maker::<TestGrant, SecondCheck>::of_start(
        reader(&f),
        Some(main_item),
        MakerStart::ByRule,
        SecondCheck,
        &b,
        None,
        a,
    );
    let opts = Wasm64BuildOpts {
        module_name: "real_rust_build_script",
        start_symbol: Some(entry_sym),
        exports: &[],
        export_globals: &[],
        include_blaze_host_v1: false,
        opts: Wasm64Opts::DEFAULT,
    };
    let mut wasm: AVec<'_, u8> = OwnVec::new_in(a);
    let built = compile_walk_to_wasm64(
        &walk,
        |_| Some(TestGrant),
        &mut lir_maker,
        &opts,
        default_wasm64_import,
        &mut wasm,
        a,
    )
    .expect("compile real Rust build script to wasm64");

    assert!(built.func_count >= 4);
    assert!(built.static_count >= 2);
    assert_eq!(built.import_count, 2, "only fd_write and proc_exit imported");
    let checked = check_wasm64(wasm.as_slice()).expect("wasm64 validates");
    assert!(checked.memory64);
    assert!(!checked.has_blaze_host_v1);

    // Execute LIVE inside Wasmtime `memory64` via `ScriptRuntime`!
    let dir = std::env::temp_dir().join(format!("blaze-cli-wasm64-bs-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let rt = ScriptRuntime::new(&dir).expect("ScriptRuntime initializes");
    let loaded = rt
        .load("self-compiled-bs 0.1.0", wasm.as_slice())
        .expect("ScriptRuntime loads self-compiled wasm64 build script");

    let vars = vec![("OUT_DIR".to_owned(), "/out".to_owned())];
    let mut env_vars = OwnVec::new_in(a);
    env_vars.push((blaze_own::String::from_str_in("OUT_DIR", a), blaze_own::String::from_str_in("/out", a)));
    let env = Env { vars: env_vars };
    let script = Arc::new(Script {
        package: "self-compiled-bs 0.1.0".to_owned(),
        tree: Tree::Mem { files: Arc::new(Vec::new()), submodules: Arc::from([]), git: None },
        root: ScriptRoot::package(),
        env: vars,
        target: "aarch64-apple-darwin".to_owned(),
        host: "aarch64-apple-darwin".to_owned(),
        c_mode: CMode::Deferred,
        limits: RunLimits {
            fuel: 1 << 26,
            memory: 16 << 20,
            out_dir_bytes: 1 << 20,
            out_dir_files: 16,
            output: 1 << 20,
            line: 256 << 10,
        },
        table: Arc::new(Vec::new()),
        probes: None,
        c_probes: None,
        spawner: None,
        step: blaze_ids::ContentId::default(),
        spawn_setting: Arc::new(blaze_proc_macro::spawn_route::SpawnSetting::off()),
        deps: Arc::new(blaze_proc_macro::script_sandbox::dep_mount::DepViews::none()),
    });

    let ran = rt.run(&loaded, script, &env, a).expect("self-compiled wasm64 build script executes cleanly");
    assert!(ran.fuel > 0, "fuel metered on live self-compiled wasm64 execution");
    assert!(
        ran.outputs.cfgs.iter().any(|(name, value)| name.as_str() == "self_compiled_wasm64" && value.is_none()),
        "expected cargo:rustc-cfg=self_compiled_wasm64, got {:?}",
        ran.outputs.cfgs
    );
    assert!(
        ran.outputs.env.iter().any(|(name, value)| name.as_str() == "BLAZE" && value.as_str() == "ok64"),
        "expected cargo:rustc-env=BLAZE=ok64, got {:?}",
        ran.outputs.env
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// 2. Real Rust proc-macro reactor source -> Blaze front-end -> `compile_walk_to_wasm64` ->
///    LIVE Wasmtime `memory64` execution inside `blaze_proc_macro::host::Runtime`!
#[test]
fn real_rust_proc_macro_self_compiles_to_wasm64_and_runs_in_host_runtime() {
    const RUST_PROC_MACRO: &str = r#"#![feature(no_core, lang_items)]
#![no_core]
#![no_main]

#[lang = "sized"]
trait Sized {}

#[lang = "copy"]
trait Copy {}

impl Copy for bool {}
impl Copy for u8 {}
impl Copy for u32 {}
impl Copy for u64 {}
impl Copy for i64 {}
impl Copy for usize {}
impl<T> Copy for *const T {}
impl<T> Copy for *mut T {}

unsafe extern "C" {
    fn __blaze_wasm64_heap_base() -> u64;
    fn blaze_pm_env_var(name: *const u8, len: u64, into: *mut u8, cap: u64) -> i64;
    fn blaze_pm_output(bytes: *const u8, len: u64);
}

static ENV_KEY: [u8; 3] = *b"FOO";
// Bridge wire encoding of `Ok(None)` (empty token stream): `0u8` (`Ok`), `1u8` (`None`).
static EMPTY_REPLY: [u8; 2] = [0, 1];
static DECLS: [extern "C-unwind" fn(u64, u64) -> u32; 1] = [demo_macro];

extern "C-unwind" fn demo_macro(_input_ptr: u64, _input_len: u64) -> u32 {
    let mut buf: [u8; 64] = [0; 64];
    unsafe {
        let _len = blaze_pm_env_var(&ENV_KEY as *const [u8; 3] as *const u8, 3, &mut buf as *mut [u8; 64] as *mut u8, 64);
        blaze_pm_output(&EMPTY_REPLY as *const [u8; 2] as *const u8, 2);
    }
    0
}

#[unsafe(no_mangle)]
pub extern "C-unwind" fn _initialize() {}

#[unsafe(no_mangle)]
pub extern "C-unwind" fn blaze_proc_macro_alloc(_len: u64) -> u64 {
    unsafe { __blaze_wasm64_heap_base() }
}

#[unsafe(no_mangle)]
pub extern "C-unwind" fn blaze_proc_macro_expand(
    decls: u64,
    index: u32,
    input_ptr: u64,
    input_len: u64,
) -> u32 {
    if index != 0 {
        return 1;
    }
    let table = decls as *const extern "C-unwind" fn(u64, u64) -> u32;
    let f = unsafe { *table };
    f(input_ptr, input_len)
}

// Root anchor so `walk_records` reaches all reactor exports + `DECLS` static.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn main() -> u32 {
    _initialize();
    let p = blaze_proc_macro_alloc(8);
    let d = &DECLS as *const [extern "C-unwind" fn(u64, u64) -> u32; 1] as u64;
    blaze_proc_macro_expand(d, 0, p, 0)
}
"#;

    let f = fixture(RUST_PROC_MACRO);
    let main_item = item_named(&f, ItemKind::Function, "main");
    let init_item = item_named(&f, ItemKind::Function, "_initialize");
    let alloc_item = item_named(&f, ItemKind::Function, "blaze_proc_macro_alloc");
    let expand_item = item_named(&f, ItemKind::Function, "blaze_proc_macro_expand");
    let decls_item = item_named(&f, ItemKind::Static, "DECLS");

    let b = builder();
    let a = alloc();
    let mut store = MemoryStore::new_in(a);
    let mut walk_maker = Maker::new(reader(&f), Some(main_item), &b, a);
    let (walk, _) = walk_records(main_item, &EveryItem, &mut store, &b, &producer(), &mut walk_maker, a)
        .expect("walk real Rust proc-macro reactor");

    let sym_of = |item: ItemKey| {
        (0..walk.len())
            .find(|&r| walk.tabled(r) == item)
            .map(|r| walk.symbol(r))
            .unwrap_or_else(|| panic!("walked symbol missing for item {item:?}"))
    };
    let init_sym = sym_of(init_item);
    let alloc_sym = sym_of(alloc_item);
    let expand_sym = sym_of(expand_item);
    let decls_sym = static_symbol(decls_item);

    let mut lir_maker = Maker::<TestGrant, SecondCheck>::of_start(
        reader(&f),
        Some(main_item),
        MakerStart::ByRule,
        SecondCheck,
        &b,
        None,
        a,
    );
    let exports = [
        (init_sym, "_initialize"),
        (alloc_sym, "blaze_proc_macro_alloc"),
        (expand_sym, "blaze_proc_macro_expand"),
    ];
    let export_globals = [("__rustc_proc_macro_decls_rust__", decls_sym)];
    let opts = Wasm64BuildOpts {
        module_name: "real_rust_proc_macro",
        start_symbol: None,
        exports: &exports,
        export_globals: &export_globals,
        include_blaze_host_v1: false,
        opts: Wasm64Opts::DEFAULT,
    };
    let mut wasm: AVec<'_, u8> = OwnVec::new_in(a);
    let built = compile_walk_to_wasm64(
        &walk,
        |_| Some(TestGrant),
        &mut lir_maker,
        &opts,
        default_wasm64_import,
        &mut wasm,
        a,
    )
    .expect("compile real Rust proc-macro reactor to wasm64");

    assert_eq!(built.import_count, 2, "only env_var and output imported; heap_base inline");
    let checked = check_wasm64(wasm.as_slice()).expect("wasm64 proc-macro reactor validates");
    assert!(checked.memory64);
    assert!(!checked.has_blaze_host_v1);

    // Execute LIVE inside Wasmtime `memory64` via `blaze_proc_macro::host::Runtime`!
    let dir = std::env::temp_dir().join(format!("blaze-cli-wasm64-pm-e2e-{}", std::process::id()));
    let pkg_dir = dir.join("pkg");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&pkg_dir).expect("pkg dir");
    std::fs::write(dir.join("module.wasm"), wasm.as_slice()).expect("write module.wasm");
    let digest = sha256_hex(wasm.as_slice());

    let record = ModuleRecord {
        key: digest.clone(),
        name: "self-compiled-pm".to_owned(),
        version: "0.1.0".to_owned(),
        wasm_sha256: digest,
        macros: vec![
            Exported { index: 0, kind: ProcKind::FnLike, name: "demo".to_owned(), helpers: Vec::new() },
            Exported { index: 1, kind: ProcKind::FnLike, name: "missing".to_owned(), helpers: Vec::new() },
        ],
        decls: "__rustc_proc_macro_decls_rust__".to_owned(),
        dir: dir.clone(),
        compiled: dir.join("compiled"),
        native_dylib: None,
        native_bin: None,
    };

    let rt = Runtime::new(blaze_own::testing::heap()).expect("Runtime initializes");
    let loaded = rt.load(&record).expect("Runtime loads self-compiled wasm64 proc-macro");
    let env = PackageEnv::set_only(vec![("FOO".to_owned(), "from_rust_source_64".to_owned())]).expect("PackageEnv");
    let pkg = Arc::new(Package::new(Files::Folder(pkg_dir), env).expect("Package"));

    let input = lex("struct SelfCompiled;", Edition::E2024, a).expect("lex");
    let call = ProcCall { kind: ProcKind::FnLike, input: &input, attr_args: None, file: Ok("src/lib.rs") };

    let ran = rt
        .expand(&loaded, &record.macros[0], &call, Edition::E2024, Arc::clone(&pkg), Limits::STANDARD, a)
        .expect("self-compiled Rust proc-macro expands live in Wasmtime memory64");
    assert!(ran.produced.tokens.is_empty());
    assert!(ran.fuel > 0);
    assert!(
        ran.produced.reads.list().iter().any(|r| ran.produced.reads.subject(r) == "env:FOO"),
        "expected tracked env:FOO read in {:?}",
        ran.produced.reads.list()
    );

    // Control: out-of-range index returns status 1 cleanly.
    let err = rt
        .expand(&loaded, &record.macros[1], &call, Edition::E2024, pkg, Limits::STANDARD, a)
        .err()
        .expect("index 1 must fail");
    assert!(
        matches!(err.error, CallError::Failed(ref why) if why.contains("no macro at index 1")),
        "expected missing macro index error, got {err:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn real_std_build_script_self_compiles_to_wasm64_and_runs_in_script_runtime() {
    const REAL_STD_SCRIPT: &str = r#"use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rustc-cfg=real_std_wasm64");
    let pkg = env::var("CARGO_PKG_NAME").unwrap_or_default();
    println!("cargo:rustc-env=PKG_WAS={pkg}");
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    let sub = out_dir.join("gen");
    fs::create_dir_all(&sub).expect("create_dir_all in OUT_DIR");
    let gen_file = sub.join("hello.rs");
    fs::write(&gen_file, "pub const ANSWER: u32 = 42;\n").expect("write in OUT_DIR");
    let back = fs::read_to_string(&gen_file).expect("read back from OUT_DIR");
    if back.contains("42") {
        println!("cargo:rustc-cfg=out_dir_roundtrip_ok");
    }
    let cwd = env::current_dir().expect("current_dir");
    println!("cargo:rustc-env=CWD_WAS={}", cwd.display());
    if let Ok(out) = Command::new("rustc").arg("--version").output() {
        if out.status.success() {
            let ver = String::from_utf8_lossy(&out.stdout);
            if ver.contains("rustc") {
                println!("cargo:rustc-cfg=command_rustc_ok");
            }
        }
    }
}
"#;
    let home = std::env::var("HOME").expect("HOME");
    let shelf_dir = std::path::PathBuf::from(home).join(".blaze/v2/toolchain");
    if !shelf_dir.join(blaze_toolchain::PINNED.to_string()).exists() {
        eprintln!("skipping real_std_build_script test: pinned toolchain {} not on shelf {}", blaze_toolchain::PINNED, shelf_dir.display());
        return;
    }
    let a = alloc();
    let edition = edition::PackageEdition::of_manifest("2024").expect("2024");
    let files = vec![(String::from(ROOT), String::from(REAL_STD_SCRIPT))];
    let user = UserCrate {
        name: "wasm64_std_build_rs",
        dir: DIR,
        root: ROOT,
        files: &files,
        edition,
        panic: blaze_ladder::run::Panic::Abort,
    };
    let id = blaze_toolchain::PINNED;
    let mut shelf = blaze_ladder::crates::DirShelf::new(shelf_dir.to_string_lossy().into_owned());
    let tc = Toolchain::open_target(&mut shelf, id, "wasm64-blaze-wasip1").expect("open wasm64 toolchain");
    let inputs = Inputs::new(&user, &tc).expect("inputs");
    let crates = inputs.crates();
    let keys: &'static [CrateItemKey] = Vec::leak(crates.iter().map(|c| c.key).collect());
    let crate_names: &'static [&'static str] = Vec::leak(crates.iter().map(|c| leak(c.name)).collect());
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).expect("resolve");
    let mut kept = Kept::default();
    for c in names.crates().iter() {
        kept.names.push(aligned(&names_record::encode(c, a).expect("names record"), blaze_own::testing::bump()));
        kept.items.push(aligned(&items_record::encode_with(c, &names, a).expect("items record"), blaze_own::testing::bump()));
        kept.files.push(c.files.iter().map(|f| (f.package.as_str().to_string(), leak(f.text))).collect());
        kept.expansions.push(c.expansions.iter().filter_map(|(call, e)| e.made.map(|m| (*call, leak(m.text)))).collect());
        kept.origins.push(c.expansions.iter().filter_map(|(call, e)| e.made.as_ref().map(|m| (*call, made_origins(*call, &m.origins)))).collect());
    }
    let f = Fixture { kept, keys, crate_names };
    let pos = |name: &str| crate_names.iter().position(|&n| n == name);
    let bump = blaze_own::testing::bump();
    let mut lang_vec = OwnVec::new_in(bump);
    lang_vec.extend(
        [Some(0), pos("core"), pos("alloc"), pos("std"), pos("panic_abort"), pos("bstd"), pos("blaze_std")]
            .into_iter()
            .flatten(),
    );
    let lang_crates: &'static [usize] = lang_vec.into_bump_slice();
    let frame: &'static blaze_mem::Frame =
        bump.alloc_val(blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("frame"));
    let log: &'static Log<'static> = bump.alloc_val(Log::new(f.crate_names, a));
    let load: &'static Kept = bump.alloc_val(f.kept.clone());
    let records: &'static Records<'static> = bump.alloc_val(Records::new(f.keys, load, log, frame, a));
    let nr = NamesRecords { records, frame, lang_crates, body_sources: None };

    let mut b = builder();
    b.settings = Settings::baseline(OverflowChecks::Off, blaze_ladder::run::Panic::Abort);
    b.toolchain = id;

    let roots = blaze_ladder::run::special::run_roots(nr.records, nr.lang_crates, b.settings.panic, &[], nr.frame, a, a).expect("run_roots");
    let maker_entry = match roots.shim() {
        Some(shim) => Some(shim.main.tabled),
        None => (!roots.is_empty()).then(|| roots.root(0).tabled),
    };
    let mut build_maker = Maker::of_roots(nr, maker_entry, &roots, SecondCheck, &b, None, a);
    let mut wasm_out: AVec<'static, u8> = OwnVec::new_in(a);
    let out = compile_roots_to_wasm64(
        &roots,
        |_| Some(TestGrant),
        &mut build_maker,
        &Wasm64BuildOpts {
            module_name: "real_std_build_rs",
            start_symbol: Some(roots.entry),
            exports: &[],
            export_globals: &[],
            include_blaze_host_v1: false,
            opts: Wasm64Opts::DEFAULT,
        },
        default_wasm64_import,
        &mut wasm_out,
        a,
    )
    .expect("compile_roots_to_wasm64 for real std build.rs");
    assert!(out.func_count > 100);
    let checked = check_wasm64(wasm_out.as_slice()).expect("valid wasm64");
    assert!(checked.memory64);

    let dir = std::env::temp_dir().join(format!("blaze-wasm64-real-std-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    let rt = ScriptRuntime::new(&dir).expect("ScriptRuntime");
    let loaded = rt.load("real_std_build_rs", wasm_out.as_slice()).expect("ScriptRuntime loads wasm64");
    let mut env_vars = OwnVec::new_in(a);
    for (k, v) in [
        ("CARGO_PKG_NAME", "real_std_pkg"),
        ("CARGO_MANIFEST_DIR", "/package"),
        ("OUT_DIR", "/out"),
        ("RUSTC", "rustc"),
    ] {
        env_vars.push((blaze_own::String::from_str_in(k, a), blaze_own::String::from_str_in(v, a)));
    }
    let env = Env { vars: env_vars };
    let script = Arc::new(Script {
        package: "real_std_pkg".to_owned(),
        tree: Tree::Mem { files: Arc::new(Vec::new()), submodules: Arc::from([]), git: None },
        root: ScriptRoot::package(),
        env: vec![
            ("CARGO_PKG_NAME".to_owned(), "real_std_pkg".to_owned()),
            ("CARGO_MANIFEST_DIR".to_owned(), "/package".to_owned()),
            ("OUT_DIR".to_owned(), "/out".to_owned()),
            ("RUSTC".to_owned(), "rustc".to_owned()),
        ],
        target: "aarch64-apple-darwin".to_owned(),
        host: "aarch64-apple-darwin".to_owned(),
        c_mode: CMode::Deferred,
        limits: RunLimits {
            fuel: 1 << 28,
            memory: 64 << 20,
            out_dir_bytes: 1 << 20,
            out_dir_files: 16,
            output: 1 << 20,
            line: 256 << 10,
        },
        table: Arc::new(Vec::new()),
        probes: None,
        c_probes: None,
        spawner: None,
        step: blaze_ids::ContentId::default(),
        spawn_setting: Arc::new(blaze_proc_macro::spawn_route::SpawnSetting::off()),
        deps: Arc::new(blaze_proc_macro::script_sandbox::dep_mount::DepViews::none()),
    });
    let ran = rt.run(&loaded, script, &env, a).expect("run real std build.rs in Wasmtime memory64");
    assert!(ran.outputs.cfgs.iter().any(|(n, v)| n.as_str() == "real_std_wasm64" && v.is_none()), "cfgs={:?}", ran.outputs.cfgs);
    assert!(ran.outputs.cfgs.iter().any(|(n, v)| n.as_str() == "out_dir_roundtrip_ok" && v.is_none()), "cfgs={:?}", ran.outputs.cfgs);
    assert!(ran.outputs.cfgs.iter().any(|(n, v)| n.as_str() == "command_rustc_ok" && v.is_none()), "cfgs={:?}", ran.outputs.cfgs);
    assert!(ran.outputs.env.iter().any(|(n, v)| n.as_str() == "PKG_WAS" && v.as_str() == "real_std_pkg"), "env={:?}", ran.outputs.env);
    assert!(ran.outputs.env.iter().any(|(n, v)| n.as_str() == "CWD_WAS" && v.as_str() == "/package"), "env={:?}", ran.outputs.env);
    assert!(ran.outputs.files.iter().any(|f| f.name.as_str() == "gen/hello.rs" && f.bytes.as_slice() == b"pub const ANSWER: u32 = 42;\n"), "files={:?}", ran.outputs.files);
    let _ = std::fs::remove_dir_all(&dir);
}

/// 4. Regression for `target-lexicon 0.13.5` `build.rs`: edition 2018 build script with
///    `mod triple { include!("src/triple.rs"); } use self::triple::Triple;` where `src/triple.rs`
///    also defines `#[macro_export] macro_rules! triple { ... $crate::Triple ... }`.
///    Verifies zero `ForeignDollarCrate` notes, valid `wasm64` compilation, AND live execution in
///    `ScriptRuntime`.
#[test]
fn included_inline_mod_with_dollar_crate_macro_compiles_to_wasm64_and_runs() {
    const BUILD_RS: &str = r#"use std::env;
use std::str::FromStr;

mod triple {
    include!("src/triple.rs");
}

use self::triple::Triple;

fn main() {
    let target = env::var("TARGET").expect("TARGET");
    let t = Triple::from_str(&target).expect("valid triple");
    println!("cargo:rustc-env=PARSED_ARCH={}", t.arch);
}
"#;
    const TRIPLE_RS: &str = r#"use core::str::FromStr;

#[derive(Debug)]
pub struct Triple {
    pub arch: String,
}

impl FromStr for Triple {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, ()> {
        let arch = s.split('-').next().ok_or(())?.to_owned();
        Ok(Triple { arch })
    }
}

#[macro_export]
macro_rules! triple {
    ($s:tt) => {
        <$crate::Triple as core::str::FromStr>::from_str($s).expect("invalid triple")
    };
}
"#;
    let home = std::env::var("HOME").expect("HOME");
    let shelf_dir = std::path::PathBuf::from(&home).join(".blaze/v2/toolchain");
    if !shelf_dir.join(blaze_toolchain::PINNED.to_string()).exists() {
        eprintln!("skipping: pinned toolchain not on shelf");
        return;
    }
    let a = alloc();
    let edition = edition::PackageEdition::of_manifest("2018").expect("2018");
    let root = "/wasm64_cli_e2e/build.rs";
    let files = vec![
        (String::from("/wasm64_cli_e2e/build.rs"), String::from(BUILD_RS)),
        (String::from("/wasm64_cli_e2e/src/triple.rs"), String::from(TRIPLE_RS)),
    ];
    let user = UserCrate {
        name: "build_script_build",
        dir: DIR,
        root,
        files: &files,
        edition,
        panic: blaze_ladder::run::Panic::Abort,
    };
    let id = blaze_toolchain::PINNED;
    let mut shelf = blaze_ladder::crates::DirShelf::new(shelf_dir.to_string_lossy().into_owned());
    let tc = Toolchain::open_target(&mut shelf, id, "wasm64-blaze-wasip1").expect("open wasm64 toolchain");
    let inputs = Inputs::new(&user, &tc).expect("inputs");
    let crates = inputs.crates();
    let keys: &'static [CrateItemKey] = Vec::leak(crates.iter().map(|c| c.key).collect());
    let crate_names: &'static [&'static str] = Vec::leak(crates.iter().map(|c| leak(c.name)).collect());
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).expect("resolve");
    assert!(names.notes(0).is_empty(), "expected zero scope notes (no ForeignDollarCrate), got {:?}", names.notes(0));
    let mut kept = Kept::default();
    for c in names.crates().iter() {
        kept.names.push(aligned(&names_record::encode(c, a).expect("names record"), blaze_own::testing::bump()));
        kept.items.push(aligned(&items_record::encode_with(c, &names, a).expect("items record"), blaze_own::testing::bump()));
        kept.files.push(c.files.iter().map(|f| (f.package.as_str().to_string(), leak(f.text))).collect());
        kept.expansions.push(c.expansions.iter().filter_map(|(call, e)| e.made.map(|m| (*call, leak(m.text)))).collect());
        kept.origins.push(c.expansions.iter().filter_map(|(call, e)| e.made.as_ref().map(|m| (*call, made_origins(*call, &m.origins)))).collect());
    }
    let f = Fixture { kept, keys, crate_names };
    let pos = |name: &str| crate_names.iter().position(|&n| n == name);
    let lang_crates: &'static [usize] = Vec::leak(
        [Some(0), pos("core"), pos("alloc"), pos("std"), pos("panic_abort")]
            .into_iter()
            .flatten()
            .collect(),
    );
    let frame: &'static blaze_mem::Frame = Box::leak(Box::new(blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("frame")));
    let log: &'static Log<'static> = Box::leak(Box::new(Log::new(f.crate_names, a)));
    let load: &'static Kept = Box::leak(Box::new(f.kept.clone()));
    let records: &'static Records<'static> = Box::leak(Box::new(Records::new(f.keys, load, log, frame, a)));
    let nr = NamesRecords { records, frame, lang_crates, body_sources: None };

    let mut b = builder();
    b.settings = Settings::baseline(OverflowChecks::Off, blaze_ladder::run::Panic::Abort);
    b.toolchain = id;

    let roots = blaze_ladder::run::special::run_roots(nr.records, nr.lang_crates, b.settings.panic, &[], nr.frame, a, a).expect("run_roots");
    let maker_entry = match roots.shim() {
        Some(shim) => Some(shim.main.tabled),
        None => (!roots.is_empty()).then(|| roots.root(0).tabled),
    };
    let mut build_maker = Maker::of_roots(nr, maker_entry, &roots, SecondCheck, &b, None, a);
    let mut wasm_out: AVec<'static, u8> = OwnVec::new_in(a);
    let out = compile_roots_to_wasm64(
        &roots,
        |_| Some(TestGrant),
        &mut build_maker,
        &Wasm64BuildOpts {
            module_name: "included_inline_mod_build_rs",
            start_symbol: Some(roots.entry),
            exports: &[],
            export_globals: &[],
            include_blaze_host_v1: false,
            opts: Wasm64Opts::USER_PROGRAM,
        },
        default_wasm64_import,
        &mut wasm_out,
        a,
    )
    .expect("compile_roots_to_wasm64 for included inline mod build.rs");
    assert!(out.func_count > 50);
    let checked = check_wasm64(wasm_out.as_slice()).expect("valid wasm64");
    assert!(checked.memory64);

    let dir = std::env::temp_dir().join(format!("blaze-wasm64-incl-mod-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    let rt = ScriptRuntime::new(&dir).expect("ScriptRuntime");
    let loaded = rt.load("included_inline_mod_build_rs", wasm_out.as_slice()).expect("ScriptRuntime loads wasm64");
    let mut env_vars = OwnVec::new_in(a);
    env_vars.push((blaze_own::String::from_str_in("TARGET", a), blaze_own::String::from_str_in("aarch64-apple-darwin", a)));
    let env = Env { vars: env_vars };
    let script = Arc::new(Script {
        package: "included_inline_mod_pkg".to_owned(),
        tree: Tree::Mem { files: Arc::new(Vec::new()), submodules: Arc::from([]), git: None },
        root: ScriptRoot::package(),
        env: vec![("TARGET".to_owned(), "aarch64-apple-darwin".to_owned())],
        target: "aarch64-apple-darwin".to_owned(),
        host: "aarch64-apple-darwin".to_owned(),
        c_mode: CMode::Deferred,
        limits: RunLimits {
            fuel: 1 << 28,
            memory: 64 << 20,
            out_dir_bytes: 1 << 20,
            out_dir_files: 16,
            output: 1 << 20,
            line: 256 << 10,
        },
        table: Arc::new(Vec::new()),
        probes: None,
        c_probes: None,
        spawner: None,
        step: blaze_ids::ContentId::default(),
        spawn_setting: Arc::new(blaze_proc_macro::spawn_route::SpawnSetting::off()),
        deps: Arc::new(blaze_proc_macro::script_sandbox::dep_mount::DepViews::none()),
    });
    let ran = rt.run(&loaded, script, &env, a).expect("run included inline mod build.rs in Wasmtime memory64");
    assert!(
        ran.outputs.env.iter().any(|(n, v)| n.as_str() == "PARSED_ARCH" && v.as_str() == "aarch64"),
        "expected PARSED_ARCH=aarch64, got {:?}",
        ran.outputs.env
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// 5. End-to-end proof for `getrandom 0.3` / `0.4` custom backend on `wasm64-blaze-wasip1`:
///    - `cfg(getrandom_backend = "custom")` is active on `wasm64-blaze-wasip1` via the single
///      canonical `blaze_toolchain::target_cfg` source of truth;
///    - an `extern "Rust"` declaration of `__getrandom_v03_custom` returning `Result<(), Error>`
///      with a newtype wrapper (`struct Error(NonZeroU32)` as in `getrandom 0.3.4` and
///      `struct ErrorI32(NonZeroI32)` as in `library/wasi_rt/src/rt/random.rs`) lowers and links
///      cleanly against `wasi_rt::rt::random::__getrandom_v03_custom` with zero `CallSigMismatch`,
///      `UnresolvedImport`, or ABI mismatch;
///    - executing the emitted `memory64` module under `ScriptRuntime` (`blaze-wasi`) invokes
///      `wasi_snapshot_preview1_64::random_get` and fills entropy cleanly.
#[test]
fn getrandom_v03_custom_backend_compiles_links_wasi_rt_and_runs_in_script_runtime() {
    const GETRANDOM_SCRIPT: &str = r#"#[cfg(not(getrandom_backend = "custom"))]
compile_error!("wasm64-blaze-wasip1 must set getrandom_backend=\"custom\"");

use core::num::{NonZeroI32, NonZeroU32};

#[derive(Copy, Clone, Debug)]
struct ErrorU32(NonZeroU32);

#[derive(Copy, Clone, Debug)]
struct ErrorI32(NonZeroI32);

unsafe extern "Rust" {
    fn __getrandom_v03_custom(dest: *mut u8, len: usize) -> Result<(), ErrorU32>;
    #[link_name = "__getrandom_v03_custom"]
    fn getrandom_v03_custom_i32(dest: *mut u8, len: usize) -> Result<(), ErrorI32>;
}

fn main() {
    // Initialize with non-zero sentinels so we prove `wasi_rt::__getrandom_v03_custom` ->
    // `wasip1::random_get` -> `blaze-wasi` (`script_sandbox::random`, deterministic zero-fill)
    // actually executed and overwrote guest linear memory for both Error newtypes.
    let mut buf1 = [0xAAu8; 32];
    unsafe { __getrandom_v03_custom(buf1.as_mut_ptr(), buf1.len()) }
        .expect("__getrandom_v03_custom (ErrorU32) failed");
    let mut buf2 = [0x55u8; 32];
    unsafe { getrandom_v03_custom_i32(buf2.as_mut_ptr(), buf2.len()) }
        .expect("__getrandom_v03_custom (ErrorI32) failed");
    unsafe { __getrandom_v03_custom(buf1.as_mut_ptr(), 0) }
        .expect("zero-length __getrandom_v03_custom failed");
    assert!(buf1.iter().all(|&b| b == 0), "random_get must overwrite all 32 sentinel bytes of buf1");
    assert!(buf2.iter().all(|&b| b == 0), "random_get must overwrite all 32 sentinel bytes of buf2");
    println!("cargo:rustc-cfg=getrandom_custom_wasm64_ok");
    println!("cargo:rustc-env=GETRANDOM_BYTES={}", buf1.len() + buf2.len());
}
"#;
    let base = support::fresh_home("wasm64_getrandom", true);
    let shelf_dir = base.join("home/.blaze/v2/toolchain");
    if !shelf_dir.join(blaze_toolchain::PINNED.to_string()).exists() {
        eprintln!("skipping getrandom_v03_custom_backend test: pinned toolchain {} not on shelf", blaze_toolchain::PINNED);
        return;
    }
    let a = alloc();
    let bump = blaze_own::testing::bump();
    let edition = edition::PackageEdition::of_manifest("2024").expect("2024");
    let root = "/wasm64_cli_e2e/build.rs";
    let files = [(root.to_owned(), GETRANDOM_SCRIPT.to_owned())];
    let user = UserCrate {
        name: "getrandom_custom_build_rs",
        dir: DIR,
        root,
        files: &files,
        edition,
        panic: blaze_ladder::run::Panic::Abort,
    };
    let id = blaze_toolchain::PINNED;
    let mut shelf = blaze_ladder::crates::DirShelf::new(shelf_dir.to_string_lossy().into_owned());
    let tc = Toolchain::open_target(&mut shelf, id, blaze_toolchain::WASM64_TARGET)
        .expect("open wasm64 toolchain");
    let inputs = Inputs::new(&user, &tc).expect("inputs");
    let crates = inputs.crates();
    let mut keys_vec = OwnVec::new_in(bump);
    keys_vec.extend(crates.iter().map(|c| c.key));
    let keys: &'static [CrateItemKey] = keys_vec.into_bump_slice();
    let mut names_vec = OwnVec::new_in(bump);
    names_vec.extend(crates.iter().map(|c| leak(c.name)));
    let crate_names: &'static [&'static str] = names_vec.into_bump_slice();
    let names = inputs.resolve(&crates, bump).expect("resolve");
    assert!(names.notes(0).is_empty(), "expected zero scope notes, got {:?}", names.notes(0));
    let mut kept = Kept::default();
    for c in names.crates().iter() {
        kept.names.push(aligned(&names_record::encode(c, a).expect("names record"), bump));
        kept.items.push(aligned(&items_record::encode_with(c, &names, a).expect("items record"), bump));
        kept.files.push(c.files.iter().map(|f| (f.package.as_str().to_string(), leak(f.text))).collect());
        kept.expansions.push(c.expansions.iter().filter_map(|(call, e)| e.made.map(|m| (*call, leak(m.text)))).collect());
        kept.origins.push(c.expansions.iter().filter_map(|(call, e)| e.made.as_ref().map(|m| (*call, made_origins(*call, &m.origins)))).collect());
    }
    let f = Fixture { kept, keys, crate_names };
    let pos = |name: &str| crate_names.iter().position(|&n| n == name);
    let mut lang_vec = OwnVec::new_in(bump);
    lang_vec.extend([Some(0), pos("core"), pos("alloc"), pos("std"), pos("panic_abort")].into_iter().flatten());
    let lang_crates: &'static [usize] = lang_vec.into_bump_slice();
    let frame: &'static blaze_mem::Frame =
        bump.alloc_val(blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("frame"));
    let log: &'static Log<'static> = bump.alloc_val(Log::new(f.crate_names, a));
    let load: &'static Kept = bump.alloc_val(f.kept.clone());
    let records: &'static Records<'static> = bump.alloc_val(Records::new(f.keys, load, log, frame, a));
    let nr = NamesRecords { records, frame, lang_crates, body_sources: None };

    let mut b = builder();
    b.settings = Settings::baseline(OverflowChecks::Off, blaze_ladder::run::Panic::Abort);
    b.toolchain = id;

    let roots = blaze_ladder::run::special::run_roots(nr.records, nr.lang_crates, b.settings.panic, &[], nr.frame, a, a).expect("run_roots");
    let maker_entry = match roots.shim() {
        Some(shim) => Some(shim.main.tabled),
        None => (!roots.is_empty()).then(|| roots.root(0).tabled),
    };
    let mut build_maker = Maker::of_roots(nr, maker_entry, &roots, SecondCheck, &b, None, a);
    let mut wasm_out: AVec<'static, u8> = OwnVec::new_in(a);
    let out = compile_roots_to_wasm64(
        &roots,
        |_| Some(TestGrant),
        &mut build_maker,
        &Wasm64BuildOpts {
            module_name: "getrandom_custom_build_rs",
            start_symbol: Some(roots.entry),
            exports: &[],
            export_globals: &[],
            include_blaze_host_v1: false,
            opts: Wasm64Opts::USER_PROGRAM,
        },
        default_wasm64_import,
        &mut wasm_out,
        a,
    )
    .expect("compile_roots_to_wasm64 must link __getrandom_v03_custom against wasi_rt");
    assert!(out.func_count > 50);
    let checked = check_wasm64(wasm_out.as_slice()).expect("valid wasm64");
    assert!(checked.memory64);

    let dir = base.join("home");
    let rt = ScriptRuntime::new(&dir).expect("ScriptRuntime");
    let loaded = rt.load("getrandom_custom_build_rs", wasm_out.as_slice()).expect("ScriptRuntime loads wasm64");
    let env = Env { vars: OwnVec::new_in(a) };
    let script = Arc::new(Script {
        package: "getrandom_custom_pkg".to_owned(),
        tree: Tree::Mem { files: Arc::new(Default::default()), submodules: Arc::from([]), git: None },
        root: ScriptRoot::package(),
        env: Default::default(),
        target: blaze_toolchain::WASM64_TARGET.to_owned(),
        host: "x86_64-unknown-linux-gnu".to_owned(),
        c_mode: CMode::Deferred,
        limits: RunLimits {
            fuel: 1 << 28,
            memory: 64 << 20,
            out_dir_bytes: 1 << 20,
            out_dir_files: 16,
            output: 1 << 20,
            line: 256 << 10,
        },
        table: Arc::new(Default::default()),
        probes: None,
        c_probes: None,
        spawner: None,
        step: blaze_ids::ContentId::default(),
        spawn_setting: Arc::new(blaze_proc_macro::spawn_route::SpawnSetting::off()),
        deps: Arc::new(blaze_proc_macro::script_sandbox::dep_mount::DepViews::none()),
    });
    let ran = rt.run(&loaded, script, &env, a).expect("run getrandom custom build.rs in Wasmtime memory64");
    assert!(
        ran.outputs.cfgs.iter().any(|(n, v)| n.as_str() == "getrandom_custom_wasm64_ok" && v.is_none()),
        "expected getrandom_custom_wasm64_ok cfg, got {:?}",
        ran.outputs.cfgs
    );
    assert!(
        ran.outputs.env.iter().any(|(n, v)| n.as_str() == "GETRANDOM_BYTES" && v.as_str() == "64"),
        "expected GETRANDOM_BYTES=64, got {:?}",
        ran.outputs.env
    );
}

fn read_wasm_u64_leb(bytes: &[u8], pos: &mut usize) -> u64 {
    let mut val = 0u64;
    let mut shift = 0u32;
    loop {
        let b = bytes[*pos];
        *pos += 1;
        val |= u64::from(b & 0x7f) << shift;
        if b & 0x80 == 0 {
            return val;
        }
        shift += 7;
    }
}

fn read_wasm_name<'a>(bytes: &'a [u8], pos: &mut usize) -> &'a str {
    let len = usize::try_from(read_wasm_u64_leb(bytes, pos)).expect("name len");
    let s = core::str::from_utf8(&bytes[*pos..*pos + len]).expect("utf8 wasm name");
    *pos += len;
    s
}

fn parse_wasm_imports<'a>(wasm: &'a [u8]) -> AVec<'static, (&'a str, &'a str)> {
    assert!(wasm.len() >= 8 && &wasm[..4] == b"\0asm", "valid wasm header");
    let mut pos = 8;
    let mut imports = OwnVec::new_in(alloc());
    while pos < wasm.len() {
        let sec_id = wasm[pos];
        pos += 1;
        let sec_len = usize::try_from(read_wasm_u64_leb(wasm, &mut pos)).expect("section len");
        let sec_end = pos + sec_len;
        if sec_id == 2 {
            let count = read_wasm_u64_leb(wasm, &mut pos);
            for _ in 0..count {
                let module = read_wasm_name(wasm, &mut pos);
                let field = read_wasm_name(wasm, &mut pos);
                imports.push((module, field));
                let kind = wasm[pos];
                pos += 1;
                match kind {
                    0x00 => {
                        let _ = read_wasm_u64_leb(wasm, &mut pos);
                    }
                    0x01 => {
                        pos += 1;
                        let flags = wasm[pos];
                        pos += 1;
                        let _ = read_wasm_u64_leb(wasm, &mut pos);
                        if flags & 0x01 != 0 {
                            let _ = read_wasm_u64_leb(wasm, &mut pos);
                        }
                    }
                    0x02 => {
                        let flags = wasm[pos];
                        pos += 1;
                        let _ = read_wasm_u64_leb(wasm, &mut pos);
                        if flags & 0x01 != 0 {
                            let _ = read_wasm_u64_leb(wasm, &mut pos);
                        }
                    }
                    0x03 => {
                        pos += 2;
                    }
                    other => panic!("unexpected wasm import kind {other:#x}"),
                }
            }
            break;
        }
        pos = sec_end;
    }
    imports
}

/// 6. Regression for `#2828`: a self-compiled `wasm64-blaze-wasip1` proc-macro calling
///    `std::env::var("CARGO_PKG_VERSION")` and `std::env::current_dir()` with an injected
///    `blaze_wasm64_pm_shim` crate registered via `Maker::set_export_override_crates` MUST:
///    - bind `libc::getenv` to `blaze_wasm64_pm_shim::getenv` instead of `wasi_rt::rt::env::getenv`,
///    - emit a `.wasm` importing `blaze_proc_macro_64::env_var` (and `current_dir`), and
///    - return the host-provided `CARGO_PKG_VERSION` value when executed through
///      `blaze_proc_macro::host::Runtime` while tracking the `env:CARGO_PKG_VERSION` read.
#[test]
fn real_std_proc_macro_shim_overrides_wasi_rt_getenv_and_imports_env_var() {
    use blaze_ladder::run::make_one::wasm64::{compile_items_to_wasm64, fn_symbol_of_item};
    use blaze_scope::crate_env::{BinNames, CrateEnv, EnvFacts, SeenRoot};
    use blaze_scope::graph::{CrateId, Sysroot};
    use blaze_scope::resolve::{CrateInput, SourceTexts, resolve_crates};
    use blaze_scope::task::{Serial, proc_macro_decls_key};
    use blaze_toolchain::Role;
    use blaze_unit_graph::{UnitKind, implicit};

    const PM_SRC: &str = r#"extern crate proc_macro;
use proc_macro::TokenStream;

#[proc_macro]
pub fn pkg_version(_input: TokenStream) -> TokenStream {
    let ver = std::env::var("CARGO_PKG_VERSION").expect("CARGO_PKG_VERSION must be present");
    let cwd = std::env::current_dir().expect("current_dir must succeed");
    let lit = format!("\"{ver}|{}\"", cwd.display());
    lit.parse().expect("valid token stream")
}
"#;

    const SHIM_SRC: &str = r#"#![allow(unsafe_code)]
use std::{ffi::CStr, mem::ManuallyDrop, panic};

unsafe extern "C" {
    #[link_name = "blaze_pm_dispatch"]
    fn dispatch(request: *const u8, len: usize) -> usize;
    #[link_name = "blaze_pm_reply"]
    fn reply(into: *mut u8);
    #[link_name = "blaze_pm_output"]
    fn output(bytes: *const u8, len: usize);
    #[link_name = "blaze_pm_env_var"]
    fn env_var(name: *const u8, len: usize, into: *mut u8, capacity: usize) -> isize;
    #[link_name = "blaze_pm_current_dir"]
    fn current_dir(into: *mut u8, capacity: usize) -> isize;
}

fn answer(ask: impl Fn(*mut u8, usize) -> isize) -> Option<Vec<u8>> {
    let mut bytes = Vec::<u8>::new();
    loop {
        let len = ask(bytes.as_mut_ptr(), bytes.capacity());
        let len = usize::try_from(len).ok()?;
        if len <= bytes.capacity() {
            unsafe { bytes.set_len(len) };
            return Some(bytes);
        }
        bytes.reserve_exact(len);
    }
}

fn enter_working_directory() -> Result<(), u32> {
    let Some(dir) = answer(|into, capacity| unsafe { current_dir(into, capacity) }) else {
        return Ok(());
    };
    let dir = String::from_utf8(dir).map_err(|_| 0u32)?;
    std::env::set_current_dir(dir)
        .map_err(|error| error.raw_os_error().map_or(0, |errno| errno as u32 & 0xFF_FFFF))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn getenv(name: *const std::ffi::c_char) -> *mut std::ffi::c_char {
    if name.is_null() {
        return std::ptr::null_mut();
    }
    let name = unsafe { CStr::from_ptr(name) }.to_bytes();
    let Some(mut value) =
        answer(|into, capacity| unsafe { env_var(name.as_ptr(), name.len(), into, capacity) })
    else {
        return std::ptr::null_mut();
    };
    value.push(0);
    Box::leak(value.into_boxed_slice()).as_mut_ptr().cast()
}

#[repr(C)]
struct Buffer {
    data: *mut u8,
    len: usize,
    capacity: usize,
    reserve: extern "C" fn(Buffer, usize) -> Buffer,
    drop: extern "C" fn(Buffer),
}

impl Buffer {
    fn of(bytes: Vec<u8>) -> Buffer {
        let mut bytes = ManuallyDrop::new(bytes);
        Buffer { data: bytes.as_mut_ptr(), len: bytes.len(), capacity: bytes.capacity(), reserve, drop: drop_buf }
    }
    unsafe fn into_vec(self) -> Vec<u8> {
        let this = ManuallyDrop::new(self);
        unsafe { Vec::from_raw_parts(this.data, this.len, this.capacity) }
    }
}

extern "C" fn reserve(buffer: Buffer, additional: usize) -> Buffer {
    let mut bytes = unsafe { buffer.into_vec() };
    bytes.reserve(additional);
    Buffer::of(bytes)
}

extern "C" fn drop_buf(buffer: Buffer) {
    std::mem::drop(unsafe { buffer.into_vec() });
}

#[repr(C)]
struct Closure {
    call: extern "C" fn(*mut u8, Buffer) -> Buffer,
    env: *mut u8,
}

#[repr(C)]
struct Config {
    input: Buffer,
    dispatch: Closure,
    force_show_panics: bool,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Client {
    run: extern "C" fn(Config) -> Buffer,
}

extern "C" fn call(_env: *mut u8, buffer: Buffer) -> Buffer {
    let mut bytes = unsafe { buffer.into_vec() };
    let len = unsafe { dispatch(bytes.as_ptr(), bytes.len()) };
    bytes.clear();
    bytes.reserve(len);
    unsafe {
        reply(bytes.as_mut_ptr());
        bytes.set_len(len);
    }
    Buffer::of(bytes)
}

fn hook(info: &panic::PanicHookInfo<'_>) {
    let message = match (info.payload().downcast_ref::<&str>(), info.payload().downcast_ref::<String>()) {
        (Some(it), _) => *it,
        (_, Some(it)) => it.as_str(),
        (None, None) => "panic",
    };
    let mut bytes = Vec::with_capacity(10 + message.len());
    bytes.extend_from_slice(&[1, 0]);
    bytes.extend_from_slice(&(message.len() as u64).to_le_bytes());
    bytes.extend_from_slice(message.as_bytes());
    unsafe { output(bytes.as_ptr(), bytes.len()) };
}

#[unsafe(no_mangle)]
pub extern "C" fn _initialize() {}

#[unsafe(no_mangle)]
pub extern "C" fn blaze_proc_macro_alloc(len: usize) -> *mut u8 {
    let mut bytes = ManuallyDrop::new(Vec::<u8>::with_capacity(len));
    bytes.as_mut_ptr()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn blaze_proc_macro_expand(
    decls: *const &'static [Client],
    index: u32,
    input: *mut u8,
    len: usize,
) -> u32 {
    let input = unsafe { Vec::from_raw_parts(input, len, len) };
    let decls: &'static [Client] = unsafe { *decls };
    let Some(client) = decls.get(index as usize).copied() else { return 1; };
    if let Err(errno) = enter_working_directory() {
        return 2 | (errno << 8);
    }
    panic::set_hook(Box::new(hook));
    let config = Config {
        input: Buffer::of(input),
        dispatch: Closure { call, env: std::ptr::null_mut() },
        force_show_panics: true,
    };
    let out = unsafe { (client.run)(config).into_vec() };
    unsafe { output(out.as_ptr(), out.len()) };
    0
}
"#;

    let base = support::fresh_home("wasm64_pm_getenv", true);
    let shelf_dir = base.join("home/.blaze/v2/toolchain");
    if !shelf_dir.join(blaze_toolchain::PINNED.to_string()).exists() {
        eprintln!("skipping real_std_proc_macro_shim_overrides_wasi_rt_getenv test: pinned toolchain not on shelf");
        return;
    }

    let a = alloc();
    let bump = blaze_own::testing::bump();
    let id = blaze_toolchain::PINNED;
    let mut shelf = blaze_ladder::crates::DirShelf::new(shelf_dir.to_string_lossy().into_owned());
    let tc = Toolchain::open_target(&mut shelf, id, blaze_toolchain::WASM64_TARGET)
        .expect("open wasm64 toolchain");

    // Reuse `Inputs::new` to obtain the resolved wasm64 toolchain crates, then swap crate 0 for
    // our `UnitKind::ProcMacro` crate and inject crate 1 as `blaze_wasm64_pm_shim`.
    let ed24 = edition::PackageEdition::of_manifest("2024").expect("2024");
    let dummy_files = [(ROOT.to_owned(), "fn main() {}".to_owned())];
    let dummy_user = UserCrate {
        name: "dummy",
        dir: DIR,
        root: ROOT,
        files: &dummy_files,
        edition: ed24,
        panic: blaze_ladder::run::Panic::Abort,
    };
    let base_inputs = Inputs::new(&dummy_user, &tc).expect("base inputs");
    let base_crates = base_inputs.crates();
    let tc_base: usize = 2;
    let shim_idx: usize = 1;
    let pm_tc_idx = base_crates
        .iter()
        .skip(1)
        .position(|c| c.name == "proc_macro")
        .map(|i| tc_base + i)
        .expect("wasm64 toolchain includes proc_macro");

    let pm_key = CrateItemKey(blaze_id::ContentId::of("wasm64 e2e pm", b"env_ver_pm"));
    let shim_key = CrateItemKey(blaze_id::ContentId::of("wasm64 e2e pm shim", SHIM_SRC.as_bytes()));
    let pm_deps: &'static [(&'static str, usize)] = bump.alloc_slice_copy(&[("proc_macro", pm_tc_idx)]);
    let shim_deps: &'static [(&'static str, usize)] = bump.alloc_slice_copy(&[("proc_macro", pm_tc_idx)]);

    let mut all_crates: AVec<'_, CrateInput<'_>> = OwnVec::with_capacity_in(base_crates.len() + 1, a);
    all_crates.push(CrateInput {
        name: "env_ver_pm",
        key: pm_key,
        edition: Edition::E2024,
        implicit: implicit(ed24, &UnitKind::ProcMacro),
        root: "/pm/src/lib.rs",
        shown_from: "/pm/",
        features: &[],
        deps: pm_deps,
        cfg: base_crates[0].cfg,
        script: None,
    });
    all_crates.push(CrateInput {
        name: "blaze_wasm64_pm_shim",
        key: shim_key,
        edition: Edition::E2024,
        implicit: implicit(ed24, &UnitKind::Lib),
        root: "/shim/src/lib.rs",
        shown_from: "/shim/",
        features: &[],
        deps: shim_deps,
        cfg: base_crates[0].cfg,
        script: None,
    });
    for tc_crate in base_crates.iter().skip(1) {
        let mut shifted_vec = OwnVec::new_in(bump);
        shifted_vec.extend(tc_crate.deps.iter().map(|&(name, dep)| (leak(name), dep + 1)));
        let shifted_deps: &'static [(&'static str, usize)] = shifted_vec.into_bump_slice();
        all_crates.push(CrateInput { deps: shifted_deps, ..*tc_crate });
    }

    let role_id = |name: &str| {
        all_crates
            .iter()
            .skip(tc_base)
            .position(|c| c.name == name)
            .map(|i| CrateId(u32::try_from(tc_base + i).expect("crate id")))
    };
    let sysroot = Sysroot {
        core: role_id("core"),
        alloc: role_id("alloc"),
        std: role_id("std"),
        proc_macro: role_id("proc_macro"),
        bstd: role_id("bstd"),
    };
    let _ = Role::Core;

    struct PmE2eSources<'a, S> {
        inner: &'a S,
        envs: [Option<CrateEnv<'a>>; 2],
    }
    impl<'a, S: SourceTexts<'a>> SourceTexts<'a> for PmE2eSources<'a, S> {
        fn read(&self, krate: usize, path: &str) -> Option<&'a str> {
            match krate {
                0 => (path == "/pm/src/lib.rs").then_some(PM_SRC),
                1 => (path == "/shim/src/lib.rs").then_some(SHIM_SRC),
                _ => self.inner.read(krate - 1, path),
            }
        }
        fn read_data(&self, krate: usize, from: &str, path: &str) -> Option<&'a [u8]> {
            match krate {
                0 => (path == "/pm/src/lib.rs").then_some(PM_SRC.as_bytes()),
                1 => (path == "/shim/src/lib.rs").then_some(SHIM_SRC.as_bytes()),
                _ => self.inner.read_data(krate - 1, from, path),
            }
        }
        fn crate_env(&self, krate: usize) -> Option<&CrateEnv<'a>> {
            if krate < 2 {
                self.envs.get(krate).and_then(|e| e.as_ref())
            } else {
                self.inner.crate_env(krate - 1)
            }
        }
    }
    let mk_env = |name: &'static str, dir: &'static str| {
        CrateEnv::of(
            EnvFacts {
                crate_name: name,
                package: None,
                bin: None,
                seen: SeenRoot::Package,
                read_folder: dir,
                build_script: false,
                script: None,
                test_or_bench: false,
                bin_exes: BinNames::NONE,
                meta: None,
            },
            a,
        )
        .ok()
    };
    let sources = PmE2eSources {
        inner: base_inputs.texts(),
        envs: [mk_env("env_ver_pm", "/pm/"), mk_env("blaze_wasm64_pm_shim", "/shim/")],
    };
    let names = resolve_crates(all_crates.as_slice(), sysroot, &sources, &Serial, bump)
        .expect("resolve proc-macro + shim + wasm64 sysroot");
    assert!(names.notes(0).is_empty(), "pm notes: {:?}", names.notes(0));
    assert!(names.notes(1).is_empty(), "shim notes: {:?}", names.notes(1));

    let mut keys_vec = OwnVec::new_in(bump);
    keys_vec.extend(all_crates.iter().map(|c| c.key));
    let keys: &'static [CrateItemKey] = keys_vec.into_bump_slice();
    let mut names_vec = OwnVec::new_in(bump);
    names_vec.extend(all_crates.iter().map(|c| leak(c.name)));
    let crate_names: &'static [&'static str] = names_vec.into_bump_slice();
    let mut kept = Kept::default();
    for c in names.crates().iter() {
        kept.names.push(aligned(&names_record::encode(c, a).expect("names record"), bump));
        kept.items.push(aligned(&items_record::encode_with(c, &names, a).expect("items record"), bump));
        kept.files.push(c.files.iter().map(|f| (f.package.as_str().to_string(), leak(f.text))).collect());
        kept.expansions.push(c.expansions.iter().filter_map(|(call, e)| e.made.map(|m| (*call, leak(m.text)))).collect());
        kept.origins.push(c.expansions.iter().filter_map(|(call, e)| e.made.as_ref().map(|m| (*call, made_origins(*call, &m.origins)))).collect());
    }
    let f = Fixture { kept, keys, crate_names };
    let pos = |name: &str| crate_names.iter().position(|&n| n == name);
    let mut lang_vec = OwnVec::new_in(bump);
    lang_vec.extend(
        [Some(0), pos("core"), pos("alloc"), pos("std"), pos("proc_macro"), pos("panic_abort")]
            .into_iter()
            .flatten(),
    );
    let lang_crates: &'static [usize] = lang_vec.into_bump_slice();
    let frame: &'static blaze_mem::Frame =
        bump.alloc_val(blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("frame"));
    let log: &'static Log<'static> = bump.alloc_val(Log::new(f.crate_names, a));
    let load: &'static Kept = bump.alloc_val(f.kept.clone());
    let records: &'static Records<'static> = bump.alloc_val(Records::new(f.keys, load, log, frame, a));
    let nr = NamesRecords { records, frame, lang_crates, body_sources: None };

    let find_shim_fn = |name: &str| -> (LibKey, ItemKey) {
        let mut found = None;
        records.each_item(shim_idx, &mut |key, _| {
            if found.is_none()
                && records.kind_of_item(key) == Some(ItemKind::Function)
                && records.name_of(key) == Some(name)
            {
                found = Some((key, checker_of(key)));
            }
        });
        found.unwrap_or_else(|| panic!("shim missing fn `{name}`"))
    };
    let (_, init_item) = find_shim_fn("_initialize");
    let (_, alloc_item) = find_shim_fn("blaze_proc_macro_alloc");
    let (expand_lib, expand_item) = find_shim_fn("blaze_proc_macro_expand");
    let decls_item = checker_of(proc_macro_decls_key(pm_key));

    let init_sym = fn_symbol_of_item(init_item, a).expect("init sym");
    let alloc_sym = fn_symbol_of_item(alloc_item, a).expect("alloc sym");
    let expand_sym = fn_symbol_of_item(expand_item, a).expect("expand sym");
    let decls_sym = static_symbol(decls_item);

    let mut b = builder();
    b.settings = Settings::baseline(OverflowChecks::Off, blaze_ladder::run::Panic::Abort);
    b.toolchain = id;

    let mut maker = Maker::<TestGrant, SecondCheck>::of_start(
        nr,
        Some(expand_item),
        MakerStart::Direct(expand_lib),
        SecondCheck,
        &b,
        None,
        a,
    );
    maker.set_export_override_crates(&[shim_idx]);

    let exports = [
        (init_sym, "_initialize"),
        (alloc_sym, "blaze_proc_macro_alloc"),
        (expand_sym, "blaze_proc_macro_expand"),
    ];
    let export_globals = [("__rustc_proc_macro_decls_blaze__", decls_sym)];
    let mut wasm_out: AVec<'static, u8> = OwnVec::new_in(a);
    compile_items_to_wasm64(
        &[init_item, alloc_item, expand_item],
        &[decls_item],
        |_| Some(TestGrant),
        &mut maker,
        &Wasm64BuildOpts {
            module_name: "env_ver_pm",
            start_symbol: None,
            exports: &exports,
            export_globals: &export_globals,
            include_blaze_host_v1: false,
            opts: Wasm64Opts::USER_PROGRAM,
        },
        default_wasm64_import,
        &mut wasm_out,
        a,
    )
    .expect("compile_items_to_wasm64 for real std proc-macro with shim override");

    let checked = check_wasm64(wasm_out.as_slice()).expect("valid wasm64 proc-macro");
    assert!(checked.memory64);
    let imports = parse_wasm_imports(wasm_out.as_slice());
    assert!(
        imports.as_slice().contains(&("blaze_proc_macro_64", "env_var")),
        "compiled wasm64 proc-macro MUST import blaze_proc_macro_64::env_var (shim getenv override), got imports={imports:?}"
    );
    assert!(
        imports.as_slice().contains(&("blaze_proc_macro_64", "current_dir")),
        "compiled wasm64 proc-macro MUST import blaze_proc_macro_64::current_dir, got imports={imports:?}"
    );

    // Execute LIVE inside Wasmtime memory64 via `blaze_proc_macro::host::Runtime`!
    let dir = base.join("mod");
    let pkg_dir = dir.join("pkg");
    let io = bstd::test::native(&base).expect("native io");
    let cx = io.cx(bstd::test::BUDGET, bstd::mem::TASK_FRAME).expect("cx");
    let fs = io.need_fs().expect("fs");
    let root = fs.root();
    let mod_dir = root
        .make_dir(&cx, &bstd::path::Path::new("mod").expect("mod"), bstd::fs::Durable::No)
        .expect("mod dir");
    mod_dir
        .make_dir(&cx, &bstd::path::Path::new("pkg").expect("pkg"), bstd::fs::Durable::No)
        .expect("pkg dir");
    let wasm_file = mod_dir
        .create(&cx, &bstd::path::Path::new("module.wasm").expect("module.wasm"))
        .expect("create module.wasm");
    wasm_file
        .write_at(&cx, 0, wasm_out.as_slice(), bstd::fs::Durable::No)
        .expect("write module.wasm");
    let digest = sha256_hex(wasm_out.as_slice());
    let record = ModuleRecord {
        key: digest.clone(),
        name: "env_ver_pm".to_owned(),
        version: "0.1.0".to_owned(),
        wasm_sha256: digest,
        macros: [Exported { index: 0, kind: ProcKind::FnLike, name: "pkg_version".to_owned(), helpers: Default::default() }].into(),
        decls: "__rustc_proc_macro_decls_blaze__".to_owned(),
        dir: dir.clone(),
        compiled: dir.join("compiled"),
        native_dylib: None,
        native_bin: None,
    };
    let rt = Runtime::new(blaze_own::testing::heap()).expect("Runtime");
    let loaded = rt.load(&record).expect("load wasm64 proc-macro");
    let env = PackageEnv::set_only([("CARGO_PKG_VERSION".to_owned(), "9.8.7-e2e".to_owned())].into()).expect("PackageEnv");
    let pkg = Arc::new(Package::new(Files::Folder(pkg_dir), env).expect("Package"));
    let input = lex("", Edition::E2024, a).expect("lex");
    let call = ProcCall { kind: ProcKind::FnLike, input: &input, attr_args: None, file: Ok("src/lib.rs") };
    let ran = rt
        .expand(&loaded, &record.macros[0], &call, Edition::E2024, pkg, Limits::STANDARD, a)
        .expect("real std proc-macro expands live with host CARGO_PKG_VERSION");

    let mut rendered = blaze_own::AString::new_in(a);
    for t in ran.produced.tokens.tokens() {
        rendered.push_str(ran.produced.tokens.text_of(t));
    }
    assert!(
        rendered.as_str().contains("9.8.7-e2e|"),
        "expected host CARGO_PKG_VERSION in expanded tokens, got {:?}",
        rendered.as_str()
    );
    assert!(
        ran.produced.reads.list().iter().any(|r| ran.produced.reads.subject(r) == "env:CARGO_PKG_VERSION"),
        "expected tracked env:CARGO_PKG_VERSION read in {:?}",
        ran.produced.reads.list()
    );
}
