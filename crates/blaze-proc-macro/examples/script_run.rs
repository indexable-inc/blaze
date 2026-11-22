//! Runs one build script's module in the sandbox, twice: each run's outputs and reads (the
//! runner keeps nothing; equal lines twice show the run is a function of its inputs).
//!
//! `script_run <store> <module.wasm> <package folder> <name> <version>`: the package folder is
//! served read-only at `/package` (a dependency, D96); the environment is `blaze_build_script::env`'s for a dev
//! check on aarch64-apple-darwin with no features.

use std::{path::Path, sync::Arc, time::Instant};

use blaze_build_script::{
    env::{Given, PackageFacts},
    key::{CMode, RunLimits},
    probe::{Known, pinned_rows},
};
use blaze_own::Alloc;
use blaze_proc_macro::{
    script::ScriptRuntime,
    script_sandbox::{Script, dir_tree, owned_env},
};

/// The runtime over the folder `store` (its compiled modules).
fn script_runtime(store: &Path) -> ScriptRuntime {
    ScriptRuntime::new(store).unwrap_or_else(|error| panic!("the script runtime: {error}"))
}

const TARGET: &str = "aarch64-apple-darwin";

/// The toy's one try-compile: `rustc - --crate-type=lib` of `fn f() {}`.
const KNOWN: &[Known] = &[Known {
    probe: "toy: a function compiles",
    args: &["--crate-type=lib"],
    snippet: "fn f() {}",
    accepted: true,
    why: "an empty function item is Rust since 1.0",
    needs: &["core", "std"],
}];

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let [_, store, module, folder, name, version] = &args[..] else {
        eprintln!("usage: script_run <store> <module.wasm> <package folder> <name> <version>");
        std::process::exit(2);
    };
    // An entry that takes no grant yet (a plain `fn main`): the run's memory is one frame
    // taken from the OS here and handed down.
    let frame = blaze_mem::Frame::with_reserve(1 << 36, blaze_mem::Tag::Usercode).expect("the entry's frame");
    let erased = blaze_own::Erased::new(&frame);
    let alloc: Alloc<'_> = erased.handle();
    let wasm = std::fs::read(module).unwrap_or_else(|e| panic!("{module}: {e}"));
    let package = PackageFacts { name, version, ..PackageFacts::default() };
    // The folder is a package's copy: it runs as a dependency, at `/package` (D96).
    let given = Given { package, seen: blaze_expand::seen::SeenRoot::Package, features: &[], target_cfg: &[], target: TARGET, host: TARGET, deps: &[] };
    let env = blaze_build_script::env(&given, alloc);
    let table = Arc::new(pinned_rows(TARGET, KNOWN, alloc).to_vec());
    let script = Arc::new(Script {
        package: format!("{name} {version}"),
        tree: dir_tree(&blaze_proc_macro::script_sandbox::RealRoot::resolve(&std::path::absolute(Path::new(folder)).expect("the folder's absolute path")).expect("the folder resolves")),
        root: blaze_proc_macro::script_sandbox::ScriptRoot::package(),
        env: owned_env(&env, alloc),
        target: TARGET.to_owned(),
        host: TARGET.to_owned(),
        c_mode: CMode::Deferred,
        limits: RunLimits::STANDARD,
        table,
        probes: None,
        spawner: None,
        step: blaze_ids::ContentId::default(),
        spawn_setting: Arc::new(blaze_proc_macro::fact_pins::command_spawn_setting(TARGET, alloc).unwrap_or_else(|e| panic!("{e}"))),
        deps: Arc::new(blaze_proc_macro::script_sandbox::dep_mount::DepViews::none()),
    });
    let runtime = script_runtime(Path::new(store));
    let started = Instant::now();
    let loaded = runtime.load(&script.package, &wasm).unwrap_or_else(|e| panic!("{e}"));
    println!("load {:?} (compiled now: {})", started.elapsed(), loaded.compiled);
    for pass in ["first", "second"] {
        let started = Instant::now();
        match runtime.run(&loaded, Arc::clone(&script), &env, alloc) {
            Ok(ran) => {
                let o = &ran.outputs;
                println!(
                    "{pass}: {:?}, fuel {}, peak memory {}, spawns {}, base {:02x?}",
                    started.elapsed(),
                    ran.fuel,
                    ran.peak_memory,
                    ran.spawns,
                    &ran.base.0[..6]
                );
                for (cfg, value) in o.cfgs.iter() {
                    println!("  cfg {}{}", cfg.as_str(), value.as_ref().map_or(String::new(), |v| format!("={v:?}")));
                }
                for (key, value) in o.env.iter() {
                    println!("  env {}={:?}", key.as_str(), value.as_str());
                }
                for file in o.files.iter() {
                    println!("  file {} ({} bytes)", file.name.as_str(), file.bytes.len());
                }
                for request in o.c_requests.iter() {
                    println!("  c request {:?} (OUT_DIR state {})", request.argv.as_str().replace('\0', " "), request.state);
                }
                for read in &ran.reads {
                    println!("  read {} {}", read.subject, if read.digest.is_some() { "found" } else { "absent" });
                }
            }
            Err(error) => println!("{pass}: {error}"),
        }
    }
}
