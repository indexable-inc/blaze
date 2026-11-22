//! The host against small modules written here byte by byte (the vendored Wasmtime takes no
//! WAT text): each limit refused by its own typed error; a fresh instance per call; and, in
//! the simulation, a changed variable or file invalidating exactly the expansions that read
//! it, with the planted bug of a read left out of the record caught.

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
};

use blaze_expand::{Edition, ProcCall, ProcKind, ProcLimit, Produced, Reads, lex};
use blaze_own::Alloc;
use blaze_sim::{Broke, Config, Tape};

use crate::{
    host::{CallError, CompiledFinding, Limits, Loaded, Refused, Runtime, Settings},
    module::{CrateId, Exported, LockedPackages, ModuleRecord, Source, module_key, sha256_hex},
    record::{DirStore, RecordError, base_key, table},
    sandbox::{Files, PACKAGE, Package, Sandbox},
};

const EDITION: Edition = Edition::E2024;

/// `produced` kept as `base`'s answer in `package`'s crate, as a command does it: the crate's
/// results opened, the row taken, the table written.
fn keep(store: &DirStore, base: blaze_ids::ContentId, package: &Package, produced: &Produced<'_>) -> Result<(), RecordError> {
    let results = table::open(store, table::crate_name(package), blaze_own::testing::heap().shared())?;
    results.insert(store, base, produced)?;
    results.keep(store, &[]).map(|_| ())
}

/// What a later command finds for `base` in `package`'s crate: its table opened again.
fn find<'a>(store: &DirStore, base: &blaze_ids::ContentId, package: &Package, edition: Edition, alloc: Alloc<'a>) -> Result<Option<Produced<'a>>, RecordError> {
    let results = table::open(store, table::crate_name(package), blaze_own::testing::heap().shared())?;
    results.find(store, base, package, &table::Verdicts::of(&results), Some(CALL_FILE_SHOWN), edition, alloc)
}

/// The file the tests' calls are written in, as a host shows it (blaze-expand `ProcCall::file`).
const CALL_FILE_SHOWN: &str = "src/lib.rs";

/// The expansion records under the folder `store`, through a world over that folder.
fn records_at(store: &Path) -> DirStore {
    let (io, root) = crate::record::native_world(store).unwrap_or_else(|error| panic!("the records at {}: {error}", store.display()));
    DirStore::expansions(io.spawner(), io.boot().ok(), &root)
}

/// A fresh folder under the system's temporary one, by this process and a count.
fn folder(name: &str) -> PathBuf {
    // One count for the process: tests run on several threads at once.
    static COUNT: AtomicU32 = AtomicU32::new(0);
    let n = COUNT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("blaze-proc-macro-test-{}-{name}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a temporary folder");
    dir
}

/// Code: the bytes of a function body's instructions.
mod op {
    pub fn uleb(mut value: u64, out: &mut Vec<u8>) {
        loop {
            let byte = (value & 0x7f) as u8;
            value >>= 7;
            if value == 0 {
                out.push(byte);
                return;
            }
            out.push(byte | 0x80);
        }
    }

    pub fn sleb(mut value: i64, out: &mut Vec<u8>) {
        loop {
            let byte = (value & 0x7f) as u8;
            value >>= 7;
            let done = (value == 0 && byte & 0x40 == 0) || (value == -1 && byte & 0x40 != 0);
            out.push(if done { byte } else { byte | 0x80 });
            if done {
                return;
            }
        }
    }

    pub fn i32(value: i64) -> Vec<u8> {
        let mut out = vec![0x41];
        sleb(value, &mut out);
        out
    }

    pub fn i64(value: i64) -> Vec<u8> {
        let mut out = vec![0x42];
        sleb(value, &mut out);
        out
    }

    pub fn call(index: u32) -> Vec<u8> {
        let mut out = vec![0x10];
        uleb(u64::from(index), &mut out);
        out
    }

    pub const UNREACHABLE: u8 = 0x00;
    pub const LOOP: [u8; 2] = [0x03, 0x40];
    pub const IF: [u8; 2] = [0x04, 0x40];
    pub const END: u8 = 0x0b;
    pub const BR0: [u8; 2] = [0x0c, 0];
    pub const RETURN: u8 = 0x0f;
    pub const DROP: u8 = 0x1a;
    pub const INDEX: [u8; 2] = [0x20, 1];
    pub const RAN: [u8; 2] = [0x23, 1];
    pub const SET_RAN: [u8; 2] = [0x24, 1];
    pub const GROW: [u8; 2] = [0x40, 0];
    pub const EQ: u8 = 0x46;
    pub const NE: u8 = 0x47;
}

/// The imports, in function-index order.
const OUTPUT: u32 = 0;
const ENV_VAR: u32 = 1;
const PATH_OPEN: u32 = 2;
/// The first of [`wasm_importing`]'s extra imports.
const FIRST_EXTRA: u32 = 3;

fn section(id: u8, body: &[u8], out: &mut Vec<u8>) {
    out.push(id);
    op::uleb(body.len() as u64, out);
    out.extend_from_slice(body);
}

fn name(text: &str, out: &mut Vec<u8>) {
    op::uleb(text.len() as u64, out);
    out.extend_from_slice(text.as_bytes());
}

/// A module whose `blaze_proc_macro_expand` runs `body` (the call's index is local 1; the
/// global `ran`, index 1, starts at 0) and then gives the empty stream: `Ok(None)`, bytes 0 and
/// 1 at address 16. The names to open sit at 32 (`FOO`), 48 (`BAR`), 64 (`package/a.txt`),
/// 96 (`package/b.txt`) and 128 (`etc/passwd`).
fn wasm(body: &[u8]) -> Vec<u8> {
    wasm_importing(body, &[])
}

/// [`wasm`] with more `wasi_snapshot_preview1` imports after `path_open`, each `(i32 x3) ->
/// i32` (`path_unlink_file`'s type): the first is function 3 ([`FIRST_EXTRA`]), and the
/// module's own functions come after them.
fn wasm_importing(body: &[u8], extra: &[&str]) -> Vec<u8> {
    const I32: u8 = 0x7f;
    const I64: u8 = 0x7e;
    let mut out = b"\0asm\x01\0\0\0".to_vec();
    // Types: 0 output (i32 i32), 1 env_var and expand (i32 x4) -> i32, 2 path_open,
    // 3 () -> (), 4 (i32) -> i32, 5 (i32 x3) -> i32.
    let types: [(&[u8], &[u8]); 6] = [
        (&[I32, I32], &[]),
        (&[I32, I32, I32, I32], &[I32]),
        (&[I32, I32, I32, I32, I32, I64, I64, I32, I32], &[I32]),
        (&[], &[]),
        (&[I32], &[I32]),
        (&[I32, I32, I32], &[I32]),
    ];
    let mut s = vec![types.len() as u8];
    for (params, results) in types {
        s.push(0x60);
        s.push(params.len() as u8);
        s.extend_from_slice(params);
        s.push(results.len() as u8);
        s.extend_from_slice(results);
    }
    section(1, &s, &mut out);
    let imports = [("blaze_proc_macro", "output", 0), ("blaze_proc_macro", "env_var", 1), ("wasi_snapshot_preview1", "path_open", 2)];
    let mut s = vec![(imports.len() + extra.len()) as u8];
    for (module, field, ty) in imports.into_iter().chain(extra.iter().map(|field| ("wasi_snapshot_preview1", *field, 5))) {
        name(module, &mut s);
        name(field, &mut s);
        s.extend_from_slice(&[0x00, ty]);
    }
    section(2, &s, &mut out);
    // Functions 3 `_initialize`, 4 `blaze_proc_macro_alloc`, 5 `blaze_proc_macro_expand`,
    // each after the extra imports.
    let own = FIRST_EXTRA as u8 + extra.len() as u8;
    section(3, &[3, 3, 4, 1], &mut out);
    section(5, &[1, 0x00, 1], &mut out);
    // Globals: 0 `decls` = 0; 1 `ran`, mutable, = 0.
    section(6, &[2, I32, 0, 0x41, 0, op::END, I32, 1, 0x41, 0, op::END], &mut out);
    let mut s = vec![5];
    for (field, kind, index) in [("memory", 2u8, 0u8), ("decls", 3, 0), ("_initialize", 0, own), ("blaze_proc_macro_alloc", 0, own + 1), ("blaze_proc_macro_expand", 0, own + 2)] {
        name(field, &mut s);
        s.extend_from_slice(&[kind, index]);
    }
    section(7, &s, &mut out);
    let mut expand = body.to_vec();
    expand.extend(op::i32(16));
    expand.extend(op::i32(2));
    expand.extend(op::call(OUTPUT));
    expand.extend(op::i32(0));
    let mut alloc = op::i32(1024);
    alloc.push(op::END);
    expand.push(op::END);
    let mut s = vec![3];
    for code in [vec![op::END], alloc, expand] {
        let mut f = vec![0];
        f.extend(code);
        op::uleb(f.len() as u64, &mut s);
        s.extend(f);
    }
    section(10, &s, &mut out);
    let data: [(i64, &[u8]); 6] = [(16, b"\x00\x01"), (32, b"FOO"), (48, b"BAR"), (64, b"package/a.txt"), (96, b"package/b.txt"), (128, b"etc/passwd")];
    let mut s = vec![data.len() as u8];
    for (at, bytes) in data {
        s.push(0);
        s.extend(op::i32(at));
        s.push(op::END);
        op::uleb(bytes.len() as u64, &mut s);
        s.extend_from_slice(bytes);
    }
    section(11, &s, &mut out);
    out
}

/// Opens the `len`-byte name at `at` for reading, from the root (fd 3).
fn open(at: i64, len: i64) -> Vec<u8> {
    let mut out = Vec::new();
    for arg in [3, 0, at, len, 0] {
        out.extend(op::i32(arg));
    }
    out.extend(op::i64(2));
    out.extend(op::i64(0));
    out.extend(op::i32(0));
    out.extend(op::i32(512));
    out.extend(op::call(PATH_OPEN));
    out.push(op::DROP);
    out
}

/// Reads the variable of the `len`-byte name at `at`.
fn env(at: i64, len: i64) -> Vec<u8> {
    let mut out = Vec::new();
    for arg in [at, len, 256, 64] {
        out.extend(op::i32(arg));
    }
    out.extend(op::call(ENV_VAR));
    out.push(op::DROP);
    out
}

/// `then` when the call's index is `index`.
fn when(index: i64, then: Vec<u8>) -> Vec<u8> {
    let mut out = op::INDEX.to_vec();
    out.extend(op::i32(index));
    out.push(op::EQ);
    out.extend(op::IF);
    out.extend(then);
    out.push(op::END);
    out
}

fn module(wasm: Vec<u8>, macros: u32) -> ModuleRecord {
    module_in(&folder("module"), wasm, macros)
}

#[path = "lazy_wasm_tests.rs"]
mod lazy_wasm;

/// A module record in `dir`: its wasm kept there as `module.wasm`, where a compile reads it
/// (module.rs `ModuleRecord::wasm`: the record holds none).
fn module_in(dir: &Path, wasm: Vec<u8>, macros: u32) -> ModuleRecord {
    let digest = sha256_hex(&wasm);
    std::fs::write(dir.join("module.wasm"), &wasm).expect("the module's wasm in its folder");
    ModuleRecord {
        key: digest.clone(),
        name: "test-macros".to_owned(),
        version: "0.0.0".to_owned(),
        wasm_sha256: digest,
        macros: (0..macros).map(|index| Exported { index, kind: ProcKind::FnLike, name: format!("m{index}"), helpers: Vec::new() }).collect(),
        decls: "decls".to_owned(),
        dir: dir.to_owned(),
        compiled: dir.join("compiled"),
        native_dylib: None,
    }
}

struct Setup {
    runtime: Runtime,
    module: ModuleRecord,
    loaded: Arc<Loaded>,
}

fn setup(body: &[u8], macros: u32) -> Setup {
    let runtime = Runtime::new(blaze_own::testing::heap()).expect("the runtime");
    let module = module(wasm(body), macros);
    let loaded = runtime.load(&module).expect("the module loads");
    Setup { runtime, module, loaded }
}

fn package(root: &Path, env: &[(&str, &str)]) -> Arc<Package> {
    let env = crate::package_env::PackageEnv::set_only(env.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect()).unwrap_or_else(|e| panic!("{e}"));
    Arc::new(Package::new(Files::Folder(root.to_owned()), env).unwrap_or_else(|e| panic!("the test's package at {}: {e}", root.display())))
}

fn run<'a>(setup: &Setup, index: u32, package: Arc<Package>, limits: Limits, alloc: Alloc<'a>) -> Result<Produced<'a>, CallError> {
    let input = lex("struct A;", EDITION, alloc).expect("the input lexes");
    let call = ProcCall { kind: ProcKind::FnLike, input: &input, attr_args: None, file: Ok(CALL_FILE_SHOWN) };
    setup.runtime.expand(&setup.loaded, &setup.module.macros[index as usize], &call, EDITION, package, limits, alloc).map(|ran| ran.produced).map_err(|e| e.error)
}

fn limited(body: &[u8], limits: Limits) -> CallError {
    let setup = setup(body, 1);
    let root = folder("limit");
    match run(&setup, 0, package(&root, &[]), limits, Alloc::global()) {
        Ok(_) => panic!("the module ran to its end"),
        Err(error) => error,
    }
}

#[test]
fn an_empty_module_runs_and_gives_the_empty_stream() {
    let setup = setup(&[], 1);
    let produced = run(&setup, 0, package(&folder("empty"), &[]), Limits::STANDARD, Alloc::global()).expect("it runs");
    assert!(produced.tokens.is_empty());
    assert!(produced.reads.list().is_empty());
}

#[test]
fn a_module_that_loops_is_stopped_by_fuel() {
    let mut body = op::LOOP.to_vec();
    body.extend(op::BR0);
    body.push(op::END);
    let error = limited(&body, Limits { fuel: 1 << 20, ..Limits::STANDARD });
    assert!(matches!(error, CallError::Limit { limit: ProcLimit::Fuel, .. }), "{error:?}");
}

#[test]
fn a_module_that_grows_its_memory_is_stopped_by_the_memory_limit() {
    // loop { if memory.grow(16) == -1 { unreachable } }
    let mut body = op::LOOP.to_vec();
    body.extend(op::i32(16));
    body.extend(op::GROW);
    body.extend(op::i32(-1));
    body.push(op::EQ);
    body.extend(op::IF);
    body.push(op::UNREACHABLE);
    body.push(op::END);
    body.extend(op::BR0);
    body.push(op::END);
    let error = limited(&body, Limits { memory: 16 << 20, ..Limits::STANDARD });
    assert!(matches!(error, CallError::Limit { limit: ProcLimit::Memory, .. }), "{error:?}");
}

#[test]
fn a_path_outside_the_workspace_is_refused() {
    let error = limited(&open(128, 10), Limits::STANDARD);
    assert!(matches!(&error, CallError::Limit { limit: ProcLimit::Paths, why } if why.contains("etc/passwd")), "{error:?}");
}

#[test]
fn huge_output_is_refused() {
    let mut body = op::i32(0);
    body.extend(op::i32(0x7fff_0000));
    body.extend(op::call(OUTPUT));
    body.extend(op::i32(0));
    body.push(op::RETURN);
    let error = limited(&body, Limits { output: 1 << 20, ..Limits::STANDARD });
    assert!(matches!(error, CallError::Limit { limit: ProcLimit::Output, .. }), "{error:?}");
}

/// A module that traps when its global was set: a second call on one `Loaded` passes only if
/// it runs in a fresh instance.
#[test]
fn each_call_runs_in_a_fresh_instance() {
    // if ran { unreachable } ran = 1
    let mut body = op::RAN.to_vec();
    body.extend(op::IF);
    body.push(op::UNREACHABLE);
    body.push(op::END);
    body.extend(op::i32(1));
    body.extend(op::SET_RAN);
    let setup = setup(&body, 1);
    let root = folder("fresh");
    for call in 0..3 {
        let ran = run(&setup, 0, package(&root, &[]), Limits::STANDARD, Alloc::global());
        assert!(ran.is_ok(), "call {call}: {:?}", ran.err());
    }
}

/// The subjects the four macros read, one each: `FOO`, `BAR`, `a.txt`, `b.txt`.
const SUBJECTS: [&str; 4] = ["env:FOO", "env:BAR", "file:a.txt", "file:b.txt"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Plant {
    None,
    /// The record is kept without the run's reads.
    UnrecordedRead,
}

/// One case: four expansions, each reading one subject, kept as records; then some subjects
/// change, and every expansion is looked up again. The oracle: a record is found exactly
/// when its one read is unchanged, and each run recorded exactly what it read.
fn invalidation(setup: &Setup, tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let harness = |e: &dyn std::fmt::Debug| Broke::Harness(format!("{e:?}"));
    let root = folder("ws");
    let store = records_at(&folder("records"));
    let mut values = [0u8; 4];
    for value in &mut values {
        *value = tape.int_in(0u8..=3, "value");
    }
    let write = |values: &[u8; 4]| -> Result<(), Broke> {
        std::fs::write(root.join("a.txt"), [b'a', values[2]]).map_err(|e| harness(&e))?;
        std::fs::write(root.join("b.txt"), [b'b', values[3]]).map_err(|e| harness(&e))
    };
    let package_of = |values: &[u8; 4]| {
        let (foo, bar) = (values[0].to_string(), values[1].to_string());
        package(&root, &[("FOO", foo.as_str()), ("BAR", bar.as_str())])
    };
    write(&values)?;
    let before = package_of(&values);
    let input = lex("struct A;", EDITION, Alloc::global()).map_err(|e| harness(&e))?;
    let call = ProcCall { kind: ProcKind::FnLike, input: &input, attr_args: None, file: Ok(CALL_FILE_SHOWN) };
    for index in 0..4u32 {
        let mut produced = run(setup, index, before.clone(), Limits::STANDARD, Alloc::global()).map_err(|e| harness(&e))?;
        let read: Vec<&str> = produced.reads.list().iter().map(|it| produced.reads.subject(it)).collect();
        if read != [SUBJECTS[index as usize]] {
            return Err(Broke::Oracle(format!("macro {index} recorded {read:?}, not [{:?}]", SUBJECTS[index as usize])));
        }
        if plant == Plant::UnrecordedRead {
            produced.reads = Reads::new_in(Alloc::global());
        }
        let base = base_key(&setup.module.wasm_sha256, &setup.module.macros[index as usize], EDITION, &call);
        keep(&store, base, &before, &produced).map_err(|e| harness(&e))?;
    }
    let mut changed = [false; 4];
    for (at, change) in changed.iter_mut().enumerate() {
        *change = tape.bool("change");
        if *change {
            values[at] += 1;
        }
    }
    write(&values)?;
    let after = package_of(&values);
    for (index, changed) in changed.into_iter().enumerate() {
        let base = base_key(&setup.module.wasm_sha256, &setup.module.macros[index], EDITION, &call);
        let found = find(&store, &base, &after, EDITION, Alloc::global()).map_err(|e| harness(&e))?;
        match (found.is_some(), changed) {
            (true, true) => return Err(Broke::Oracle(format!("stale: macro {index}'s record was found after {} changed", SUBJECTS[index]))),
            (false, false) => return Err(Broke::Oracle(format!("macro {index}'s record was lost, though {} did not change", SUBJECTS[index]))),
            _ => {}
        }
    }
    Ok(())
}

fn reader_body() -> Vec<u8> {
    let mut body = when(0, env(32, 3));
    body.extend(when(1, env(48, 3)));
    body.extend(when(2, open(64, 13)));
    body.extend(when(3, open(96, 13)));
    body
}

fn reader() -> Setup {
    setup(&reader_body(), 4)
}

/// The reader's module record, kept in `dir`.
fn reader_in(dir: &Path) -> ModuleRecord {
    module_in(dir, wasm(&reader_body()), 4)
}

/// Each of the reader's four macros run once, its record's bytes.
fn reader_outputs(setup: &Setup) -> Vec<Vec<u8>> {
    let root = folder("compiled-reads");
    std::fs::write(root.join("a.txt"), "alpha").expect("a file");
    let pkg = package(&root, &[("FOO", "1")]);
    (0..4)
        .map(|index| {
            let produced = run(setup, index, Arc::clone(&pkg), Limits::STANDARD, Alloc::global()).expect("it runs");
            crate::record::table::row_bytes(&produced).expect("keepable")
        })
        .collect()
}

/// The compiled record of `module` under `runtime`'s key.
fn compiled_path(runtime: &Runtime, module: &ModuleRecord) -> PathBuf {
    module.compiled.join(runtime.code_key(module))
}

/// A second runtime in this process loads the module from its compiled record, compiling
/// nothing, and its expansions are byte-equal to those of the runtime that compiled it.
#[test]
fn a_second_runtime_loads_the_compiled_record_without_compiling() {
    let dir = folder("compiled");
    let first = Runtime::new(blaze_own::testing::heap()).expect("the runtime");
    let module = reader_in(&dir);
    let loaded = first.load(&module).expect("it compiles");
    assert!(loaded.compiled);
    assert_eq!(first.compiles(), 1);
    assert!(Arc::ptr_eq(&loaded, &first.load(&module).expect("it is loaded")), "one load per runtime");
    assert_eq!(first.compiles(), 1);
    let fresh = Setup { runtime: first, module, loaded };
    let second = Runtime::new(blaze_own::testing::heap()).expect("the runtime");
    let module = reader_in(&dir);
    let loaded = second.load(&module).expect("it loads from its record");
    assert!(!loaded.compiled);
    assert_eq!(second.compiles(), 0);
    assert!(second.take_findings().is_empty());
    let kept = Setup { runtime: second, module, loaded };
    assert_eq!(reader_outputs(&fresh), reader_outputs(&kept));
}

/// A record cut short (not the size its seal names) is refused by name, removed and compiled
/// again. A read checks the seal's size and key, not a hash of the bytes: the seal is the
/// writer's proof (host.rs `seal`, rule 74).
#[test]
fn a_compiled_record_not_its_sealed_size_is_refused_and_compiled_again() {
    let dir = folder("truncated");
    let first = Runtime::new(blaze_own::testing::heap()).expect("the runtime");
    let module = reader_in(&dir);
    first.load(&module).expect("it compiles");
    let path = compiled_path(&first, &module);
    let bytes = std::fs::read(&path).expect("the record");
    std::fs::write(&path, &bytes[..bytes.len() - 1]).expect("the cut record");
    let second = Runtime::new(blaze_own::testing::heap()).expect("the runtime");
    let loaded = second.load(&module).expect("it compiles again");
    assert!(loaded.compiled);
    assert_eq!(second.compiles(), 1);
    assert_eq!(second.module_loads().taken, 0);
    let findings = second.take_findings();
    assert!(matches!(&findings[..], [CompiledFinding { record, why: Refused::Size { .. }, .. }] if *record == path), "{findings:?}");
    let third = Runtime::new(blaze_own::testing::heap()).expect("the runtime");
    assert!(!third.load(&module).expect("it loads").compiled, "the record written again is loaded");
}

/// A record compiled under another engine setting is not loaded: its key differs.
#[test]
fn a_compiled_record_of_another_setting_is_not_loaded() {
    let dir = folder("settings");
    let standard = Runtime::new(blaze_own::testing::heap()).expect("the runtime");
    let module = reader_in(&dir);
    standard.load(&module).expect("it compiles");
    let other = Runtime::with_settings(&Settings { optimize: false, ..Settings::standard() }, blaze_own::testing::heap()).expect("the runtime");
    assert_ne!(other.code_key(&module), standard.code_key(&module));
    let loaded = other.load(&module).expect("it compiles");
    assert!(loaded.compiled);
    assert_eq!(other.compiles(), 1);
    let again = Runtime::new(blaze_own::testing::heap()).expect("the runtime");
    assert!(!again.load(&module).expect("it loads").compiled, "the standard record is still there");
}

/// Two workers of one runtime asking for one module at once: one compiles, the other waits.
#[test]
fn two_workers_loading_one_module_compile_it_once() {
    let runtime = Runtime::new(blaze_own::testing::heap()).expect("the runtime");
    let module = reader_in(&folder("two-loads"));
    let (a, b) = std::thread::scope(|s| {
        let a = s.spawn(|| runtime.load(&module).expect("it loads"));
        let b = s.spawn(|| runtime.load(&module).expect("it loads"));
        (a.join().expect("worker a"), b.join().expect("worker b"))
    });
    assert!(Arc::ptr_eq(&a, &b));
    assert_eq!(runtime.compiles(), 1);
    let all = runtime.load_all(&[&module, &module], 4).expect("the workers start");
    assert!(all.iter().all(Result::is_ok));
    assert_eq!(runtime.compiles(), 1);
}

#[test]
fn a_changed_read_invalidates_exactly_the_expansions_that_read_it() {
    let setup = reader();
    blaze_sim::check(Config::cases(24), |tape| invalidation(&setup, tape, Plant::None));
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ClosurePlant {
    None,
    /// The module's identity is made without its `deps` line: as before identity version 2.
    ClosureLeftOut,
}

/// A small lock: the macro crate `m` on one of two syns, each on `unicode-ident`, and an
/// `other` package outside the closure. `syn`: which syn `m` takes; `ident` and `other`: the
/// checksums' last digits.
fn small_lock(syn: u8, ident: u8, other: u8) -> String {
    let syn_version = if syn == 0 { "1.0.0" } else { "3.0.0" };
    format!(
        "version = 4\n\n[[package]]\nname = \"m\"\nversion = \"1.0.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"m0\"\ndependencies = [\n \"syn {syn_version}\",\n]\n\n\
         [[package]]\nname = \"syn\"\nversion = \"1.0.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"s1\"\ndependencies = [\n \"unicode-ident\",\n]\n\n\
         [[package]]\nname = \"syn\"\nversion = \"3.0.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"s3\"\ndependencies = [\n \"unicode-ident\",\n]\n\n\
         [[package]]\nname = \"unicode-ident\"\nversion = \"1.0.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"u{ident}\"\n\n\
         [[package]]\nname = \"other\"\nversion = \"1.0.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"o{other}\"\n"
    )
}

/// One case: two locks drawn from the tape, the macro crate's module key under each. The
/// oracle: the keys are equal exactly when `m`'s closure is (the same syn, the same
/// `unicode-ident`); a change outside it (`other`) keeps the key.
fn closure_keys(tape: &Tape, plant: ClosurePlant) -> Result<(), Broke> {
    let harness = |e: &dyn std::fmt::Debug| Broke::Harness(format!("{e:?}"));
    let draw = |tape: &Tape| (tape.int_in(0u8..=1, "syn"), tape.int_in(0u8..=1, "ident"), tape.int_in(0u8..=1, "other"));
    let (first, second) = (draw(tape), draw(tape));
    let key = |(syn, ident, other): (u8, u8, u8)| -> Result<String, Broke> {
        let deps = match plant {
            ClosurePlant::None => {
                let text = small_lock(syn, ident, other);
                let lock = blaze_manifest::lock::Lock::read(text.as_bytes(), blaze_own::testing::heap().alloc()).map_err(|e| harness(&e))?;
                LockedPackages::of(&lock).closure("m", "1.0.0").map_err(|e| harness(&e))?
            }
            ClosurePlant::ClosureLeftOut => String::new(),
        };
        let id = CrateId { name: "m", version: "1.0.0", source: Source::Registry { checksum: "m0" }, deps: &deps, features: &["default"] };
        Ok(module_key(&id))
    };
    let same_closure = first.0 == second.0 && first.1 == second.1;
    match (key(first)? == key(second)?, same_closure) {
        (true, false) => Err(Broke::Oracle(format!("same module for two closures: {first:?} and {second:?} (syn, unicode-ident, other)"))),
        (false, true) => Err(Broke::Oracle(format!("two modules for one closure: {first:?} and {second:?}"))),
        _ => Ok(()),
    }
}

#[test]
fn the_module_key_follows_exactly_the_lock_closure() {
    blaze_sim::check(Config::cases(32), |tape| closure_keys(tape, ClosurePlant::None));
}

/// A source correction (D58) is in the module key: asn1-rs-derive 0.6.0 over synstructure
/// 0.13.2 (tempo's checksum) keys differently with no row, the checked-in row, and that row
/// edited; a lock without synstructure keys the same under all three.
#[test]
fn a_source_correction_row_is_in_the_module_key() {
    use blaze_fetch::correct::{TABLE, Table};
    let lock = "version = 4\n\n[[package]]\nname = \"asn1-rs-derive\"\nversion = \"0.6.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"a0\"\ndependencies = [\n \"synstructure\",\n]\n\n\
                [[package]]\nname = \"synstructure\"\nversion = \"0.13.2\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"728a70f3dbaf5bab7f0c4b1ac8d7ae5ea60a4b5549c8a5914361c99147a709d2\"\n";
    let read = |text: &str| Table::read(text, blaze_lex::Cpu::SCALAR, std::alloc::Global).expect("a table");
    let lock = blaze_manifest::lock::Lock::read(lock.as_bytes(), blaze_own::testing::heap().alloc()).expect("the lock reads");
    let key = |table| {
        let deps = crate::module::LockedPackages::of_with(&lock, table).closure("asn1-rs-derive", "0.6.0").expect("a closure");
        module_key(&CrateId { name: "asn1-rs-derive", version: "0.6.0", source: Source::Registry { checksum: "a0" }, deps: &deps, features: &["default"] })
    };
    let none = key(read("{\"rows\": []}"));
    let row = key(read(TABLE));
    let edited = key(read(&TABLE.replace("\"replace\": \"any()\"", "\"replace\": \"all(any())\"")));
    println!("module key, no row: {none}\nmodule key, the row: {row}\nmodule key, the row edited: {edited}");
    assert!(none != row && row != edited && none != edited);
}

#[test]
fn control_the_closure_left_out_of_the_identity_is_caught() {
    blaze_sim::control(Config::cases(32), ClosurePlant::None, ClosurePlant::ClosureLeftOut, "same module for two closures", closure_keys);
}

#[test]
fn control_a_read_left_out_of_the_record_is_caught() {
    let setup = reader();
    blaze_sim::control(Config::cases(24), Plant::None, Plant::UnrecordedRead, "stale", |tape, plant| invalidation(&setup, tape, plant));
}

/// [`each_call_runs_in_a_fresh_instance`]'s module, its control: the same module's export called
/// twice on ONE instance (the planted bug: a dirty instance reused) traps on the second call,
/// so the fresh-instance test would fail if the host ever reused one.
#[test]
fn control_a_reused_instance_is_caught() {
    let mut body = op::RAN.to_vec();
    body.extend(op::IF);
    body.push(op::UNREACHABLE);
    body.push(op::END);
    body.extend(op::i32(1));
    body.extend(op::SET_RAN);
    let engine = wasmtime::Engine::default();
    let module = wasmtime::Module::new(&engine, wasm(&body)).expect("the module compiles");
    let mut linker: wasmtime::Linker<()> = wasmtime::Linker::new(&engine);
    linker.func_wrap("blaze_proc_macro", "output", |_: i32, _: i32| {}).expect("output");
    linker.func_wrap("blaze_proc_macro", "env_var", |_: i32, _: i32, _: i32, _: i32| -> i32 { 0 }).expect("env_var");
    linker
        .func_wrap("wasi_snapshot_preview1", "path_open", |_: i32, _: i32, _: i32, _: i32, _: i32, _: i64, _: i64, _: i32, _: i32| -> i32 { 0 })
        .expect("path_open");
    let mut store = wasmtime::Store::new(&engine, ());
    let instance = linker.instantiate(&mut store, &module).expect("it instantiates");
    let expand = instance.get_typed_func::<(i32, i32, i32, i32), i32>(&mut store, "blaze_proc_macro_expand").expect("the export");
    assert!(expand.call(&mut store, (0, 0, 0, 0)).is_ok(), "a clean instance runs");
    assert!(expand.call(&mut store, (0, 0, 0, 0)).is_err(), "a reused instance must trap, or the fresh-instance test proves nothing");
}

/// The whole of WASI preview 1 (`wasi_snapshot_preview1.witx`), by name.
const PREVIEW1: [&str; 46] = [
    "args_get", "args_sizes_get", "environ_get", "environ_sizes_get", "clock_res_get", "clock_time_get",
    "fd_advise", "fd_allocate", "fd_close", "fd_datasync", "fd_fdstat_get", "fd_fdstat_set_flags",
    "fd_fdstat_set_rights", "fd_filestat_get", "fd_filestat_set_size", "fd_filestat_set_times", "fd_pread",
    "fd_prestat_get", "fd_prestat_dir_name", "fd_pwrite", "fd_read", "fd_readdir", "fd_renumber", "fd_seek",
    "fd_sync", "fd_tell", "fd_write", "path_create_directory", "path_filestat_get", "path_filestat_set_times",
    "path_link", "path_open", "path_readlink", "path_remove_directory", "path_rename", "path_symlink",
    "path_unlink_file", "poll_oneoff", "proc_exit", "proc_raise", "sched_yield", "random_get", "sock_accept",
    "sock_recv", "sock_send", "sock_shutdown",
];

/// Every preview 1 call is defined for a proc macro once: served (blaze-wasi), refused or
/// unserved (host.rs), so no import of preview 1 fails a module's load.
#[test]
fn the_proc_macro_host_defines_all_of_wasi_preview1_once() {
    let served = blaze_wasi::FUNCTIONS;
    let (refused, unserved) = (crate::host::REFUSED_WASI, crate::host::UNSERVED_WASI);
    let mut defined: Vec<&str> = served.iter().chain(refused).chain(unserved).copied().collect();
    defined.sort_unstable();
    let mut all = PREVIEW1.to_vec();
    all.sort_unstable();
    assert_eq!(defined, all, "served {served:?}, refused {refused:?}, unserved {unserved:?}");
}

/// What a module of one macro gives, made of `body` with the extra WASI imports `extra`, its
/// wasm in the folder `name`: its load's error, or its call's.
fn expand_importing(name: &str, body: &[u8], extra: &[&str]) -> Result<(), String> {
    let runtime = Runtime::new(blaze_own::testing::heap()).expect("the runtime");
    let module = module_in(&folder(name), wasm_importing(body, extra), 1);
    let loaded = runtime.load(&module).map_err(|e| format!("load: {e}"))?;
    let input = lex("struct A;", EDITION, Alloc::global()).expect("the input lexes");
    let call = ProcCall { kind: ProcKind::FnLike, input: &input, attr_args: None, file: Ok(CALL_FILE_SHOWN) };
    runtime.expand(&loaded, &module.macros[0], &call, EDITION, package(&folder(&format!("{name}-pkg")), &[]), Limits::STANDARD, Alloc::global()).map(drop).map_err(|e| format!("call: {e}"))
}

/// Calls the first extra import as `path_unlink_file(3, "package/a.txt")` and traps unless it
/// answered `errno`.
fn unlink_answers(errno: i64) -> Vec<u8> {
    let mut body = Vec::new();
    for arg in [3, 64, 13] {
        body.extend(op::i32(arg));
    }
    body.extend(op::call(FIRST_EXTRA));
    body.extend(op::i32(errno));
    body.push(op::NE);
    body.extend(op::IF);
    body.push(op::UNREACHABLE);
    body.push(op::END);
    body
}

/// Reduced from napi-derive 3.5.2 (turso, 2026-11-21: "unknown import:
/// wasi_snapshot_preview1::path_unlink_file has not been defined", 32 errors): a module that
/// imports `path_unlink_file` loads and expands, and a call of it answers `EPERM` (63), the
/// sandbox giving a proc macro no writable file system (rule 9). The twin: the call checked
/// against success (0) traps, so the errno check runs. CONTROL (rule 23): a module importing a
/// call no host defines still fails its load, naming it, so the load is what was checked.
#[test]
fn a_module_importing_path_unlink_file_loads_and_a_call_answers_eperm() {
    assert_eq!(expand_importing("unlink-linked", &[], &["path_unlink_file"]), Ok(()), "linked, never called");
    assert_eq!(expand_importing("unlink-called", &unlink_answers(63), &["path_unlink_file"]), Ok(()), "called: EPERM");
    let twin = expand_importing("unlink-success", &unlink_answers(0), &["path_unlink_file"]);
    assert!(twin.as_ref().is_err_and(|e| e.starts_with("call: ")), "a call checked against success must trap: {twin:?}");
    let control = expand_importing("unlink-unknown", &[], &["path_unlink_files"]);
    assert!(control.as_ref().is_err_and(|e| e.starts_with("load: ") && e.contains("path_unlink_files")), "{control:?}");
}

/// A module that traps (`unreachable`) is a typed failure naming the crate and the macro, and,
/// through the hook, a finding naming the call's base key.
#[test]
fn a_module_that_traps_fails_its_call_with_a_typed_error() {
    let setup = setup(&[op::UNREACHABLE], 1);
    let input = lex("struct A;", EDITION, Alloc::global()).expect("the input lexes");
    let call = ProcCall { kind: ProcKind::FnLike, input: &input, attr_args: None, file: Ok(CALL_FILE_SHOWN) };
    let error = setup
        .runtime
        .expand(&setup.loaded, &setup.module.macros[0], &call, EDITION, package(&folder("trap"), &[]), Limits::STANDARD, Alloc::global())
        .err()
        .expect("a trap is no expansion");
    assert!(matches!(error.error, CallError::Failed(_)), "{error:?}");
    assert!(error.to_string().contains("m0") && error.to_string().contains("test-macros"), "{error}");

    let store_dir = folder("trap-hook");
    let records = records_at(&store_dir);
    let key = crate::hook::crate_key(&setup.module.key);
    let mut procs = crate::hook::ProcMacros::new(&setup.runtime, &records, Limits::STANDARD);
    procs.add_crate(key, "test-macros", "0.0.0", Ok(module(wasm(&[op::UNREACHABLE]), 1)));
    let def = proc_def(key, "m0");
    let host = procs.run(&def, &call, EDITION, &package(&store_dir, &[]), Alloc::global()).err().expect("refused");
    assert_eq!(host.why, blaze_expand::HostWhy::Failed);
    let findings = procs.take_findings();
    let base = base_key(&setup.module.wasm_sha256, &setup.module.macros[0], EDITION, &call);
    assert!(matches!(&findings[..], [crate::hook::Finding::Call { base: b, .. }] if *b == base), "{findings:?}");
    assert!(findings[0].to_string().contains(&format!("{base:x}")), "{}", findings[0]);
}

/// An attribute macro called as `#[m]` is served two streams, its arguments empty, exactly as
/// `#[m()]` is: tempo's 1,078 argument-less attribute calls panicked in their client when the
/// host served only the item. The planted bug's shape, one stream, is a fn-like call's, so the
/// control is that encoding: it must differ, or the equality proves nothing.
#[test]
fn an_attribute_without_arguments_gets_an_empty_stream() {
    let item = lex("fn f() {}", EDITION, Alloc::global()).expect("the item lexes");
    let empty = blaze_expand::TokenStream::new_in(Alloc::global());
    let served = |kind, attr_args| {
        let mut run_input = Vec::new();
        let call = ProcCall { kind, input: &item, attr_args, file: Ok(CALL_FILE_SHOWN) };
        crate::server::Server::new(EDITION, crate::server::ServerLimits::STANDARD, &call, Alloc::global(), &mut run_input).expect("served");
        run_input
    };
    let bare = served(ProcKind::Attr, None);
    assert_eq!(bare, served(ProcKind::Attr, Some(&empty)), "`#[m]` is served as `#[m()]`");
    assert_ne!(bare, served(ProcKind::FnLike, None), "control: one stream, the planted bug, reads differently");
    // Arguments on a call that takes none are refused, not dropped.
    let mut run_input = Vec::new();
    let call = ProcCall { kind: ProcKind::Derive, input: &item, attr_args: Some(&empty), file: Ok(CALL_FILE_SHOWN) };
    let refused = crate::server::Server::new(EDITION, crate::server::ServerLimits::STANDARD, &call, Alloc::global(), &mut run_input).err();
    assert!(matches!(refused, Some(crate::server::ServerError::ArgumentsOnNonAttribute { kind: ProcKind::Derive })), "{refused:?}");
}

/// An answer kept by another host version is never read: the version is in the base key. The
/// control finds the same answer kept under this version.
#[test]
fn an_answer_of_another_host_version_is_not_read() {
    let setup = setup(&[], 1);
    let root = folder("host-version");
    let pkg = package(&root, &[]);
    let input = lex("struct A;", EDITION, Alloc::global()).expect("the input lexes");
    let call = ProcCall { kind: ProcKind::FnLike, input: &input, attr_args: None, file: Ok(CALL_FILE_SHOWN) };
    let produced = run(&setup, 0, Arc::clone(&pkg), Limits::STANDARD, Alloc::global()).expect("it runs");
    let export = &setup.module.macros[0];
    let old = crate::record::base_key_under(crate::record::HOST_VERSION - 1, &setup.module.wasm_sha256, export, EDITION, &call);
    let now = base_key(&setup.module.wasm_sha256, export, EDITION, &call);
    assert_ne!(old, now);
    let old_store = records_at(&folder("host-version-old"));
    keep(&old_store, old, &pkg, &produced).expect("kept under the old version");
    assert!(find(&old_store, &now, &pkg, EDITION, Alloc::global()).expect("read").is_none(), "the old host's answer was read");
    let store = records_at(&folder("host-version-now"));
    keep(&store, now, &pkg, &produced).expect("kept under this version");
    assert!(find(&store, &now, &pkg, EDITION, Alloc::global()).expect("read").is_some(), "control: this version's answer is found");
}

/// `Span::file`'s read (sandbox.rs `CALL_FILE`) is the call's, not the package's: an answer
/// that read `crates/a/src/lib.rs` is found for a call of the same key in that file, and not
/// for one in another (the item moved to `b`: run again), nor where the host answers no file.
/// In one command, with one package's verdicts, the list is valued per call: the planted bug
/// of keeping the first call's verdict as the package's is caught both ways (`b` after `a`
/// would be found; `a` after `b` would not). The control: an answer that did not read the
/// file is found in both files, as an edit to another line of the file leaves the key and
/// the file the same.
#[test]
fn a_kept_answer_that_read_the_calls_file_is_found_only_in_that_file() {
    use blaze_expand::{ReadAnswer, ReadKind};
    use crate::sandbox::{CALL_FILE, call_file_digest};

    let setup = setup(&[], 1);
    let pkg = package(&folder("call-file"), &[]);
    let input = lex("struct A;", EDITION, Alloc::global()).expect("the input lexes");
    let call = ProcCall { kind: ProcKind::FnLike, input: &input, attr_args: None, file: Ok("crates/a/src/lib.rs") };
    let base = base_key(&setup.module.wasm_sha256, &setup.module.macros[0], EDITION, &call);
    let mut reads = Reads::new_in(Alloc::global());
    reads.push(ReadKind::ProcMacro, CALL_FILE, ReadAnswer::Digest(call_file_digest("crates/a/src/lib.rs"))).expect("the read fits");
    let read_file = Produced { reads, ..run(&setup, 0, Arc::clone(&pkg), Limits::STANDARD, Alloc::global()).expect("it runs") };
    let store = records_at(&folder("call-file-store"));
    keep(&store, base, &pkg, &read_file).expect("kept");

    let results = table::open(&store, table::crate_name(&pkg), blaze_own::testing::heap().shared()).expect("the table opens");
    let verdicts = table::Verdicts::of(&results);
    let found = |file: Option<&str>| results.find(&store, &base, &pkg, &verdicts, file, EDITION, Alloc::global()).expect("read").is_some();
    assert!(found(Some("crates/a/src/lib.rs")), "the real case: a call in the file the answer read");
    assert!(!found(Some("crates/b/src/lib.rs")), "the twin: the call moved to another file runs again");
    assert!(found(Some("crates/a/src/lib.rs")), "a's verdict again: b's was not kept as the package's");
    assert!(!found(None), "a host that answers no file finds nothing that read it");

    // Control: the answer read nothing of the call; found wherever the call is.
    let plain = run(&setup, 0, Arc::clone(&pkg), Limits::STANDARD, Alloc::global()).expect("it runs");
    assert!(plain.reads.list().iter().all(|read| plain.reads.subject(read) != CALL_FILE), "the module asks no file");
    let plain_store = records_at(&folder("call-file-plain"));
    keep(&plain_store, base, &pkg, &plain).expect("kept");
    let results = table::open(&plain_store, table::crate_name(&pkg), blaze_own::testing::heap().shared()).expect("the table opens");
    let verdicts = table::Verdicts::of(&results);
    for file in [Some("crates/a/src/lib.rs"), Some("crates/b/src/lib.rs")] {
        assert!(results.find(&plain_store, &base, &pkg, &verdicts, file, EDITION, Alloc::global()).expect("read").is_some(), "{file:?}");
    }
}

/// `0..=1` reaches a macro as rustc gives it: `0`, then `.` `.` joint and `=` alone, then `1`.
#[test]
fn a_range_in_an_attribute_lexes_as_rustc_lexes_it() {
    let stream = lex("num_args(0..=1)", EDITION, Alloc::global()).expect("lexes");
    let shown: Vec<String> = (0..stream.len()).map(|i| format!("{:?} {:?}", stream.tokens()[i].kind, stream.text_at(i))).collect();
    let text: Vec<&str> = (0..stream.len()).map(|i| stream.text_at(i)).collect();
    assert!(text.contains(&"0") && text.contains(&"1"), "{shown:#?}");
    let dots: Vec<_> = stream.tokens().iter().filter(|t| matches!(t.kind, blaze_expand::TokenKind::Punct { char: '.', .. })).map(|t| t.kind).collect();
    assert!(
        matches!(
            dots[..],
            [blaze_expand::TokenKind::Punct { spacing: blaze_expand::Spacing::Joint, .. }, blaze_expand::TokenKind::Punct { spacing: blaze_expand::Spacing::Joint, .. }]
        ),
        "{shown:#?}"
    );
}

fn proc_def(krate: blaze_id::CrateItemKey, name: &str) -> blaze_expand::MacroDefinition<'static> {
    let at = blaze_expand::DefAt {
        site: blaze_expand::DefSite { krate, module: None },
        edition: EDITION,
        export: blaze_expand::Export::Exported,
        local_inner: blaze_expand::LocalInner::No,
        transparency: None,
    };
    blaze_expand::MacroDefinition::proc_macro(krate, ProcKind::FnLike, name, &[], at, Alloc::global()).expect("a definition")
}

/// A crate with no module is one finding naming it and its version, however many calls meet it.
#[test]
fn a_missing_module_is_one_finding_per_crate() {
    let runtime = Runtime::new(blaze_own::testing::heap()).expect("the runtime");
    let store_dir = folder("missing");
    let records = records_at(&store_dir);
    let key = crate::hook::crate_key("absent");
    let mut procs = crate::hook::ProcMacros::new(&runtime, &records, Limits::STANDARD);
    let missing = crate::module::ModuleError::Missing { name: "gone-macros".to_owned(), version: "1.2.3".to_owned(), key: "absent".to_owned() };
    procs.add_crate(key, "gone-macros", "1.2.3", Err(missing));
    let input = lex("x", EDITION, Alloc::global()).expect("the input lexes");
    let call = ProcCall { kind: ProcKind::FnLike, input: &input, attr_args: None, file: Ok(CALL_FILE_SHOWN) };
    let def = proc_def(key, "m0");
    for _ in 0..5 {
        let host = procs.run(&def, &call, EDITION, &package(&store_dir, &[]), Alloc::global()).err().expect("refused");
        assert_eq!(host.why, blaze_expand::HostWhy::Refused);
    }
    let findings = procs.take_findings();
    assert!(
        matches!(&findings[..], [crate::hook::Finding::MissingModule { name, version, .. }] if name == "gone-macros" && version == "1.2.3"),
        "{findings:?}"
    );
}

/// Default built-in comptime patch library: `serde_derive` expands `Serialize` and `Deserialize`
/// in-process by default even with NO compiled proc-macro module, falls back cleanly on exotic
/// attributes, and can be disabled via `set_comptime_patch(false)`.
#[test]
fn comptime_patch_expands_serde_by_default_without_module_and_tracks_fallbacks() {
    let runtime = Runtime::new(blaze_own::testing::heap()).expect("the runtime");
    let store_dir = folder("comptime-patch-serde");
    let records = records_at(&store_dir);
    let key = crate::hook::crate_key("serde-derive-absent");
    let mut procs = crate::hook::ProcMacros::new(&runtime, &records, Limits::STANDARD);
    assert!(procs.comptime_patch_enabled(), "comptime patch must be enabled by default");
    let missing = crate::module::ModuleError::Missing {
        name: "serde_derive".to_owned(),
        version: "1.0.219".to_owned(),
        key: "serde-derive-absent".to_owned(),
    };
    procs.add_crate(key, "serde_derive", "1.0.219", Err(missing));

    let at = blaze_expand::DefAt {
        site: blaze_expand::DefSite { krate: key, module: None },
        edition: EDITION,
        export: blaze_expand::Export::Exported,
        local_inner: blaze_expand::LocalInner::No,
        transparency: None,
    };
    let ser_def = blaze_expand::MacroDefinition::proc_macro(
        key,
        ProcKind::Derive,
        "Serialize",
        &["serde"],
        at,
        Alloc::global(),
    )
    .expect("Serialize def");
    let de_def = blaze_expand::MacroDefinition::proc_macro(
        key,
        ProcKind::Derive,
        "Deserialize",
        &["serde"],
        at,
        Alloc::global(),
    )
    .expect("Deserialize def");

    assert!(procs.refusal_of(&ser_def).is_none(), "patched Serialize must not be refused");
    assert!(procs.refusal_of(&de_def).is_none(), "patched Deserialize must not be refused");

    let input = lex(
        "#[serde(rename_all = \"camelCase\")] pub struct Config { pub max_retries: u32, pub endpoint: String }",
        EDITION,
        Alloc::global(),
    )
    .expect("input lexes");
    let call = ProcCall { kind: ProcKind::Derive, input: &input, attr_args: None, file: Ok(CALL_FILE_SHOWN) };
    let pkg = package(&store_dir, &[]);

    let ser_out = procs.run(&ser_def, &call, EDITION, &pkg, Alloc::global()).expect("Serialize expands at comptime");
    assert!(!ser_out.tokens.is_empty());
    let de_out = procs.run(&de_def, &call, EDITION, &pkg, Alloc::global()).expect("Deserialize expands at comptime");
    assert!(!de_out.tokens.is_empty());
    assert_eq!(procs.comptime_patch_stats().hits, 2);
    assert_eq!(procs.comptime_patch_stats().fallbacks, 0);

    // Exotic `#[serde(tag = "type")]` cleanly falls back (and since module is missing, refuses).
    let exotic = lex("#[serde(tag = \"type\")] pub enum Msg { A { x: u32 } }", EDITION, Alloc::global()).unwrap();
    let exotic_call = ProcCall { kind: ProcKind::Derive, input: &exotic, attr_args: None, file: Ok(CALL_FILE_SHOWN) };
    assert!(procs.run(&ser_def, &exotic_call, EDITION, &pkg, Alloc::global()).is_err());
    assert_eq!(procs.comptime_patch_stats().fallbacks, 1);

    // Disabling comptime patch restores normal MissingModule refusal.
    procs.set_comptime_patch(false);
    assert!(procs.refusal_of(&ser_def).is_some());
    assert!(procs.run(&ser_def, &call, EDITION, &pkg, Alloc::global()).is_err());
}

/// A module whose output is not the bridge's (ten bytes of a path where a stream's encoding
/// goes) fails its call; it is never read as tokens.
#[test]
fn a_malformed_output_is_refused() {
    let mut body = op::i32(128);
    body.extend(op::i32(10));
    body.extend(op::call(OUTPUT));
    body.extend(op::i32(0));
    body.push(op::RETURN);
    let error = limited(&body, Limits::STANDARD);
    assert!(matches!(error, CallError::Failed(_) | CallError::Refused(_)), "{error:?}");
}

/// One command through the hook: the reader's four macros called in two packages (`one`,
/// `two`) in `order`, then `keep_all`. Each call's row bytes, or `None` for a call the hook
/// refused; and what was written.
fn hook_command(setup: &Setup, store_dir: &Path, packages: &[Arc<Package>; 2], order: &[(usize, u32)], limits: Limits) -> (Vec<Option<Vec<u8>>>, crate::hook::KeptAll) {
    let records = records_at(store_dir);
    let key = crate::hook::crate_key(&setup.module.key);
    let mut procs = crate::hook::ProcMacros::new(&setup.runtime, &records, limits);
    procs.add_crate(key, "test-macros", "0.0.0", Ok(reader_in(&setup.module.dir)));
    let input = lex("struct A;", EDITION, Alloc::global()).expect("the input lexes");
    let call = ProcCall { kind: ProcKind::FnLike, input: &input, attr_args: None, file: Ok(CALL_FILE_SHOWN) };
    let mut rows = vec![None; 8];
    for (package, index) in order {
        let def = proc_def(key, &format!("m{index}"));
        let produced = procs.run(&def, &call, EDITION, &packages[*package], Alloc::global());
        rows[package * 4 + *index as usize] = produced.ok().map(|it| table::row_bytes(&it).expect("keepable"));
    }
    (rows, procs.keep_all().unwrap_or_else(|error| panic!("the tables are kept: {error}")))
}

/// Every file under `dir`, by its path from it, sorted.
fn files_under(dir: &Path) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    let mut folders = vec![dir.to_owned()];
    while let Some(folder) = folders.pop() {
        for entry in std::fs::read_dir(&folder).expect("a folder of the store").map(|it| it.expect("an entry")) {
            let path = entry.path();
            if path.is_dir() {
                folders.push(path);
            } else {
                let name = path.strip_prefix(dir).expect("under the store").to_string_lossy().into_owned();
                out.push((name, std::fs::read(&path).expect("a file of the store")));
            }
        }
    }
    out.sort();
    out
}

/// The hook's round trip. (1) Calls one command ran and kept (`keep_all`) are found by the
/// next command's fresh `ProcMacros` over the same store as the same rows, with no run: the
/// second command has no fuel, so a call it ran would be refused (the control: with an
/// empty store every call of it is). (2) Two commands making the calls in two orders, in two
/// packages of two crate names, leave equal table files. (3) A changed variable misses
/// exactly the call that read it: the crate's table stays where it was.
#[test]
fn the_hook_keeps_calls_for_the_next_command_whatever_their_order() {
    let setup = reader();
    let root = folder("round-ws");
    std::fs::write(root.join("a.txt"), "alpha").expect("a file");
    std::fs::write(root.join("b.txt"), "beta").expect("a file");
    let package_of = |name: &str, foo: &str| package(&root, &[("CARGO_PKG_NAME", name), ("FOO", foo), ("BAR", "2")]);
    let packages = [package_of("one", "1"), package_of("two", "1")];
    let forward: Vec<(usize, u32)> = (0..2).flat_map(|package| (0..4).map(move |index| (package, index))).collect();
    let backward: Vec<(usize, u32)> = forward.iter().rev().copied().collect();
    let no_fuel = Limits { fuel: 0, ..Limits::STANDARD };

    let (store_a, store_b, empty) = (folder("round-a"), folder("round-b"), folder("round-empty"));
    let (ran, kept) = hook_command(&setup, &store_a, &packages, &forward, Limits::STANDARD);
    assert!(ran.iter().all(Option::is_some), "a call of the first command was refused");
    assert_eq!((kept.tables, kept.rows), (2, 8), "two crate names' tables, four rows each: {kept:?}");

    // Control: without fuel and without records, every call is refused.
    let (refused, wrote) = hook_command(&setup, &empty, &packages, &forward, no_fuel);
    assert!(refused.iter().all(Option::is_none), "control: a call ran without fuel");
    assert_eq!(wrote.tables, 0);

    // (1) The next command finds all eight, running none, and writes nothing.
    let (found, wrote) = hook_command(&setup, &store_a, &packages, &backward, no_fuel);
    assert!(found == ran, "a kept call was not found as the row that was kept");
    assert_eq!(wrote.tables, 0, "a command that ran nothing wrote a table");

    // (2) The other order of calls, in another store: equal files.
    let (ran_b, _) = hook_command(&setup, &store_b, &packages, &backward, Limits::STANDARD);
    assert!(ran_b == ran, "two orders of calls gave two rows for one call");
    let (files_a, files_b) = (files_under(&store_a), files_under(&store_b));
    assert_eq!(files_a.iter().filter(|(name, _)| name.contains("crate/")).count(), 2, "two tables: {:?}", files_a.iter().map(|it| &it.0).collect::<Vec<_>>());
    assert!(files_a == files_b, "two orders of calls wrote two stores");

    // (3) `FOO` changes in package one: its m0 (which read FOO) misses, its other three and
    // all of package two's are found.
    let changed = [package_of("one", "9"), Arc::clone(&packages[1])];
    let (after, _) = hook_command(&setup, &store_a, &changed, &forward, no_fuel);
    let missed: Vec<usize> = (0..8).filter(|at| after[*at].is_none()).collect();
    assert_eq!(missed, [0], "a changed variable must miss exactly the call that read it");
    assert!(after[1..] == ran[1..], "an unchanged call was found as another row");
    println!("hook round trip: 8 calls kept in {} tables, {} bytes; found by the next command; one of 8 missed after FOO changed", kept.tables, kept.bytes);
}

/// The same call on two workers (two threads, one runtime and module) writes equal records.
#[test]
fn the_same_call_on_two_workers_gives_equal_bytes() {
    let setup = reader();
    let root = folder("two-workers");
    std::fs::write(root.join("a.txt"), "alpha").expect("a file");
    let pkg = package(&root, &[("FOO", "1")]);
    let bytes = |index: u32| {
        let produced = run(&setup, index, Arc::clone(&pkg), Limits::STANDARD, Alloc::global()).expect("it runs");
        crate::record::table::row_bytes(&produced).expect("keepable")
    };
    for index in 0..3 {
        let (a, b) = std::thread::scope(|s| {
            let a = s.spawn(|| bytes(index));
            let b = s.spawn(|| bytes(index));
            (a.join().expect("worker a"), b.join().expect("worker b"))
        });
        assert_eq!(a, b, "macro {index}");
    }
}

/// Output the host gave that a module could have forged: blaze-expand checks it before use.
struct Forged(fn(Alloc<'static>) -> Produced<'static>);

impl blaze_expand::ExpandCx for Forged {
    fn env(&self, _: &str) -> Result<Option<&str>, blaze_expand::HostError> {
        Ok(None)
    }
    fn cfg(&self, _: &blaze_expand::TokenStream<'_>) -> Result<bool, blaze_expand::HostError> {
        Ok(false)
    }
    fn include(&self, _: &str, _: blaze_expand::IncludeAs) -> Result<blaze_expand::Included<'_>, blaze_expand::HostError> {
        Ok(blaze_expand::Included::Missing)
    }
    fn place(&self, _: blaze_expand::PlaceOf) -> Result<blaze_expand::Place<'_>, blaze_expand::HostError> {
        Ok(blaze_expand::Place::Number(1))
    }
    fn run_proc_macro<'a>(&self, _: &blaze_expand::MacroDefinition<'_>, _: ProcCall<'_>, _: Alloc<'a>) -> Result<Produced<'a>, blaze_expand::HostError> {
        Ok((self.0)(Alloc::global()))
    }
}

fn forged(tokens: &str, origins: &[blaze_expand::TokenOrigin], text: &[u8], diagnostics: &[blaze_expand::ProcDiagnostic], alloc: Alloc<'static>) -> Produced<'static> {
    let mut o = blaze_own::Vec::new_in(alloc);
    o.extend_from_slice(origins);
    let mut t = blaze_own::Vec::new_in(alloc);
    t.extend_from_slice(text);
    let mut d = blaze_own::Vec::new_in(alloc);
    d.extend_from_slice(diagnostics);
    Produced { tokens: lex(tokens, EDITION, alloc).expect("lexes"), origins: o, reads: Reads::new_in(alloc), diagnostics: d, text: t }
}

/// Every way a host's output can point outside what the call gave: each is an error of the
/// expansion, never an index out of bounds. The input is `a b` (two tokens), no attribute.
#[test]
fn forged_proc_macro_output_is_refused_by_the_expander() {
    use blaze_expand::{ProcDiagnostic as D, ProcLevel, Text, TokenOrigin as O, Transparency};
    let cases: [(&str, fn(Alloc<'static>) -> Produced<'static>); 7] = [
        ("an input index past the input", |a| forged("x", &[O::Input(2)], b"", &[], a)),
        ("attribute arguments on a function-like call", |a| forged("x", &[O::AttrArgs(0)], b"", &[], a)),
        ("the definition's tokens", |a| forged("x", &[O::Def(0)], b"", &[], a)),
        ("fewer origins than tokens", |a| forged("x y", &[O::Made(Transparency::Transparent)], b"", &[], a)),
        ("a diagnostic's text past the text", |a| {
            forged("x", &[O::Made(Transparency::Transparent)], b"ab", &[D::Panicked { message: Text { at: 1, len: 5 } }], a)
        }),
        ("a diagnostic at an input index past the input", |a| {
            forged(
                "x",
                &[O::Made(Transparency::Transparent)],
                b"e",
                &[D::Emitted { level: ProcLevel::Error, message: Text { at: 0, len: 1 }, at: Some(O::Input(99)) }],
                a,
            )
        }),
        ("an unbalanced stream", |a| {
            let mut p = forged("x", &[O::Made(Transparency::Transparent)], b"", &[], a);
            p.tokens.push(blaze_expand::TokenKind::Close(blaze_expand::Delim::Paren), ")", blaze_expand::Ctx::root(EDITION)).expect("pushed");
            p.origins.push(O::Made(Transparency::Transparent));
            p
        }),
    ];
    let key = crate::hook::crate_key("forged");
    let def = proc_def(key, "m");
    let input = lex("a b", EDITION, Alloc::global()).expect("lexes");
    let site = blaze_expand::CallSite { edition: EDITION, panic_edition: EDITION, ctx: blaze_expand::Ctx::root(EDITION) };
    let call = blaze_expand::Call { kind: blaze_expand::CallKind::FnLike { args: &input }, site, eager: blaze_expand::Eager::plain() };
    for (what, make) in cases {
        let result = blaze_expand::expand(&def, &call, &Forged(make), Alloc::global());
        assert!(result.is_err(), "{what}: accepted");
    }
    // The control: a well-formed output of the same shapes is taken.
    let fine = blaze_expand::expand(&def, &call, &Forged(|a| forged("x", &[O::Input(1)], b"", &[], a)), Alloc::global());
    assert!(fine.is_ok(), "{:?}", fine.err());
}

/// The sandbox rule: a macro sees only its own package's files. A fetched package's are its
/// fetch record's, served at [`PACKAGE`], which `CARGO_MANIFEST_DIR` names (cargo's
/// documentation of the variable: "The directory containing the manifest of your package";
/// proc-macro-crate, as jsonrpsee's `#[rpc]` calls it, reads `$CARGO_MANIFEST_DIR/Cargo.toml`).
/// Reduced from reth-rpc-api: its `#[rpc(server)]` emitted `compile_error!` when the manifest
/// was absent. A path that leaves the package by `..` is a refusal naming it.
#[test]
fn a_fetched_package_s_macro_reads_its_own_manifest_and_nothing_above_it() {
    use blaze_wasi::{Open, ROOT_FD, System};
    const MANIFEST: &[u8] = b"[package]\nname = \"reth-rpc-api\"\n[dependencies]\njsonrpsee = \"0.26\"\n";
    let files: Arc<[(String, &'static [u8])]> = Arc::from(vec![("Cargo.toml".to_owned(), MANIFEST), ("src/lib.rs".to_owned(), &b"//"[..])]);
    let env = crate::package_env::PackageEnv::set_only(vec![("CARGO_MANIFEST_DIR".to_owned(), PACKAGE.to_owned())]).unwrap_or_else(|e| panic!("{e}"));
    let package = Arc::new(Package::new(Files::Listed(files), env).unwrap_or_else(|e| panic!("{e}")));
    let mut sandbox = Sandbox::new(package);
    let dir = sandbox.var("CARGO_MANIFEST_DIR").ok().flatten().expect("CARGO_MANIFEST_DIR is given");
    let how = Open { dirflags: 0, oflags: 0, rights_base: 0, rights_inheriting: 0, fdflags: 0 };
    let fd = sandbox.open(ROOT_FD, format!("{dir}/Cargo.toml").as_bytes(), how).expect("not stopped").expect("the manifest opens");
    let mut bytes = vec![0; 256];
    let n = sandbox.read(fd, &mut bytes).expect("not stopped").expect("it reads");
    assert_eq!(&bytes[..n], MANIFEST);
    let src = sandbox.open(ROOT_FD, format!("{dir}/src").as_bytes(), how).expect("not stopped").expect("the folder opens");
    assert_eq!(sandbox.fd_filetype(src).expect("not stopped"), Ok(blaze_wasi::Filetype::Directory));
    let above = sandbox.open(ROOT_FD, format!("{dir}/../Cargo.toml").as_bytes(), how);
    let Err(stop) = above else { panic!("a path above the package was served") };
    assert!(stop.0.contains("/package/../Cargo.toml") && stop.0.contains("outside /package"), "{}", stop.0);
    assert!(matches!(&sandbox.refused, Some((ProcLimit::Paths, _))), "{:?}", sandbox.refused);
}
