//! End-to-end proof of Blaze's self-compiled `wasm64` (`memory64`) usercode stack:
//! real Rust source text -> `blaze-ladder` front-end + `Walk` + `Maker` -> `compile_walk_to_wasm64`
//! -> `blaze_export::lir_wasm64::compile_lir_wasm64` -> LIVE Wasmtime `memory64` execution inside
//! BOTH `blaze_proc_macro::script::ScriptRuntime` (build scripts) AND `blaze_proc_macro::host::Runtime`
//! (proc macros) — zero `rustc`, zero `wasm-lld`, zero `wasi-libc`, zero hand-built LIR.

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
    let frame: &'static blaze_mem::Frame = Box::leak(Box::new(
        blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("reader frame"),
    ));
    let log: &'static Log<'static> = Box::leak(Box::new(Log::new(f.crate_names, a)));
    let load: &'static Kept = Box::leak(Box::new(f.kept.clone()));
    let records: &'static Records<'static> =
        Box::leak(Box::new(Records::new(f.keys, load, log, frame, a)));
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
    let lang_crates: &'static [usize] = Vec::leak(
        [Some(0), pos("core"), pos("alloc"), pos("std"), pos("panic_abort"), pos("bstd"), pos("blaze_std")]
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
