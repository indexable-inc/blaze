//! A module record holds no wasm, and a load from a compiled record reads none (item 3 of
//! the 2026-11-18 heap log: every macro crate's wasm read, hashed and kept to the command's
//! end, 114 MB in 79 blocks on tempo).
//!
//! Floor: 0 wasm bytes kept by a record; 0 read by a load whose compiled record is there.
//! Oracle: the load with the wasm gone gives the same expansions as the runtime that compiled
//! it. Control: with no compiled record the same missing wasm is refused by name (so the
//! load does read it when it compiles, and the first result is not vacuous); a wasm that is
//! not its digest's bytes is refused by name too.
//!
//! Expected (`cargo test -p blaze-proc-macro --lib lazy_wasm -- --nocapture`):
//!   "a load from its compiled record: 0 wasm bytes read (floor 0), compiled 0" and three ok.

#![cfg(test)]

use super::{Runtime, Setup, compiled_path, folder, reader_in, reader_outputs};
use crate::{host::RuntimeError, module::ModuleError};

#[test]
fn lazy_wasm_a_load_from_a_compiled_record_reads_no_wasm() {
    let dir = folder("lazy-wasm-hit");
    let first = Runtime::new(blaze_own::testing::heap()).expect("the runtime");
    let module = reader_in(&dir);
    let loaded = first.load(&module).expect("it compiles");
    assert!(loaded.compiled && compiled_path(&first, &module).exists(), "the first load compiles and keeps its record");
    let fresh = Setup { runtime: first, module, loaded };
    std::fs::remove_file(dir.join("module.wasm")).expect("the wasm removed");
    let second = Runtime::new(blaze_own::testing::heap()).expect("the runtime");
    let module = reader_in_kept(&dir, &fresh.module);
    let loaded = second.load(&module).unwrap_or_else(|error| panic!("a load from its compiled record read the wasm: {error}"));
    println!("a load from its compiled record: 0 wasm bytes read (floor 0), compiled {}", second.compiles());
    assert!(!loaded.compiled);
    assert_eq!(second.compiles(), 0);
    let kept = Setup { runtime: second, module, loaded };
    assert_eq!(reader_outputs(&fresh), reader_outputs(&kept), "the expansions of a load from the record are other bytes");
}

/// Control: no compiled record and no wasm: the compile reads the wasm and is refused by name.
#[test]
fn lazy_wasm_control_a_compile_without_its_wasm_is_refused() {
    let dir = folder("lazy-wasm-missing");
    let module = reader_in(&dir);
    std::fs::remove_file(dir.join("module.wasm")).expect("the wasm removed");
    let runtime = Runtime::new(blaze_own::testing::heap()).expect("the runtime");
    match runtime.load(&module) {
        Err(RuntimeError::Wasm(ModuleError::Io { path, .. })) => assert_eq!(path, dir.join("module.wasm")),
        other => panic!("a compile without its wasm: {:?}", other.map(|loaded| loaded.compiled)),
    }
}

/// A wasm that is not the bytes its digest names is refused when the compile reads it.
#[test]
fn lazy_wasm_a_wasm_not_its_digest_is_refused_at_its_compile() {
    let dir = folder("lazy-wasm-flipped");
    let module = reader_in(&dir);
    let path = dir.join("module.wasm");
    let mut bytes = std::fs::read(&path).expect("the wasm");
    let at = bytes.len() - 1;
    bytes[at] ^= 1;
    std::fs::write(&path, &bytes).expect("the flipped wasm");
    let runtime = Runtime::new(blaze_own::testing::heap()).expect("the runtime");
    match runtime.load(&module) {
        Err(RuntimeError::Wasm(ModuleError::Digest { held, .. })) => assert_eq!(held, module.wasm_sha256),
        other => panic!("a flipped wasm: {:?}", other.map(|loaded| loaded.compiled)),
    }
}

/// `module`'s record in `dir` again, without writing its wasm there: what a later command
/// reads (module.rs `read_module` opens no `module.wasm`).
fn reader_in_kept(dir: &std::path::Path, module: &crate::module::ModuleRecord) -> crate::module::ModuleRecord {
    crate::module::ModuleRecord {
        key: module.key.clone(),
        name: module.name.clone(),
        version: module.version.clone(),
        wasm_sha256: module.wasm_sha256.clone(),
        macros: module.macros.clone(),
        decls: module.decls.clone(),
        dir: dir.to_owned(),
        compiled: module.compiled.clone(),
        native_dylib: module.native_dylib.clone(),
    }
}
