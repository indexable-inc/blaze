//! Every module in a module store compiled ahead on N workers ([`Runtime::load_all`]), then
//! loaded again by a second runtime from the compiled records: the time of each pass, the
//! records' size on disk, and the slowest and median single load from a record.
//!
//! usage: compile_modules <module store> <workers>

use std::{path::Path, time::Instant};

use blaze_proc_macro::{
    host::Runtime,
    module::{COMPILED_DIR, ModuleRecord, sha256_hex},
};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let [_, store, workers] = &args[..] else { panic!("usage: compile_modules <module store> <workers>") };
    let workers: usize = workers.parse().expect("a worker count");
    let root = Path::new(store).join("proc-macro/modules");
    let mut dirs: Vec<_> = std::fs::read_dir(&root).unwrap_or_else(|e| panic!("{}: {e}", root.display())).map(|it| it.expect("an entry").path()).collect();
    dirs.sort();
    let modules: Vec<ModuleRecord> = dirs
        .iter()
        .filter(|dir| dir.join("wasm-sha256").exists())
        .map(|dir| {
            let read = |file: &str| std::fs::read(dir.join(file)).unwrap_or_else(|e| panic!("{}/{file}: {e}", dir.display()));
            let wasm = read("module.wasm");
            let wasm_sha256 = sha256_hex(&wasm);
            assert_eq!(String::from_utf8_lossy(&read("wasm-sha256")).trim(), wasm_sha256, "{}", dir.display());
            let key = dir.file_name().expect("a key").to_string_lossy().into_owned();
            let decls = String::from_utf8_lossy(&read("decls")).trim().to_owned();
            // The record holds no wasm: a compile reads it from `dir` (module.rs `ModuleRecord::wasm`).
            ModuleRecord { name: key[..12].to_owned(), version: String::new(), key, wasm_sha256, macros: Vec::new(), decls, dir: dir.clone(), compiled: Path::new(store).join(COMPILED_DIR), native_dylib: None }
        })
        .collect();
    let all: Vec<&ModuleRecord> = modules.iter().collect();
    for pass in ["first", "second"] {
        let runtime = Runtime::new(blaze_own::Heap::process()).expect("the runtime");
        let started = Instant::now();
        let loaded = runtime.load_all(&all, workers).expect("the workers start");
        let took = started.elapsed();
        let mut loads: Vec<f64> = Vec::new();
        let mut bytes = 0;
        for (module, loaded) in all.iter().zip(&loaded) {
            let loaded = match loaded {
                Ok(loaded) => loaded,
                Err(e) => {
                    println!("{pass} pass: {} does not load: {e}", module.dir.display());
                    continue;
                }
            };
            bytes += loaded.code_bytes;
            loads.push(loaded.load_time.as_secs_f64() * 1e3);
        }
        loads.sort_by(f64::total_cmp);
        let wasm: usize = all.iter().map(|it| std::fs::metadata(it.dir.join("module.wasm")).map_or(0, |meta| meta.len() as usize)).sum();
        println!(
            "{pass} pass: {} modules on {workers} workers in {:.1} ms; compiled {}; findings {}; compiled records {:.1} MB (wasm {:.1} MB); one load: median {:.2} ms, slowest {:.2} ms",
            all.len(),
            took.as_secs_f64() * 1e3,
            runtime.compiles(),
            runtime.take_findings().len(),
            bytes as f64 / 1e6,
            wasm as f64 / 1e6,
            loads.get(loads.len() / 2).copied().unwrap_or(0.0),
            loads.last().copied().unwrap_or(0.0),
        );
    }
}
