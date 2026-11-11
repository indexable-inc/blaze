//! A wasm module's expansions kept and found again: with a store to keep in, a build of the
//! fixture workspace whose serde derives run in serde_derive's module keeps each expansion and
//! the module's machine code; a second build in the same store, through the same `Macros` and
//! `Modules` as a daemon's builds share them, runs none of them and compiles nothing.

#[allow(dead_code, reason = "shared with tests/corpus.rs, which uses other parts of it")]
#[path = "corpus/config.rs"]
mod config;
#[allow(dead_code, reason = "shared with tests/corpus.rs, which uses other parts of it")]
#[path = "corpus/load.rs"]
mod load;

use std::sync::Arc;

use blaze_core::sim::{Faults, Sim};
use hir_def::nameres::crate_def_map;
use proc_macro_api::kept::{self, Environment, Expansions, Put};

use config::Corpus;

/// The crate whose module runs.
const KRATE: &str = "serde_derive";

fn kept_in(sim: &mut Sim) -> blaze_cas::Store {
    let mut effects = Vec::new();
    let config = blaze_cas::Config {
        dir: blaze_core::FileName::new("store").expect("a plain name"),
        layers: Vec::new(),
        tags: 1 << 48,
        load_tags: (1 << 48) + (1 << 32),
        loader: blaze_cas::Loader::Pumped,
    };
    let mut store = blaze_cas::Store::open(config, &mut effects);
    blaze_core::run_until_idle(sim, &mut effects, |completion, effects| {
        _ = store.complete(completion, effects);
    }).expect("the effects end");
    assert!(store.ready(), "the store in the simulation opens");
    store
}

/// The workspace built with [`KRATE`]'s macros in its module and every other on the evaluator.
fn build(
    config: &Corpus,
    macros: &Arc<blaze_macro_eval::Macros>,
    modules: &blaze_macro_eval::Modules,
    module: &[u8],
) -> load::Workspace {
    let workspace = load::lower_with(
        &config.workspace,
        &config.cargo_home,
        config.homes(),
        |db, krate, toolchain, name| {
            let loaded = match name {
                KRATE => blaze_macro_eval::load_wasm(db, krate, toolchain, macros, modules, module),
                _ => blaze_macro_eval::load(db, krate, toolchain, macros),
            };
            loaded.map_err(|error| format!("loading `{name}`: {error}"))
        },
    )
    .unwrap_or_else(|why| panic!("{why}"));
    let calls = workspace.krate("corpus-calls").unwrap_or_else(|why| panic!("{why}"));
    crate_def_map(workspace.lowered.db(), calls);
    workspace
}

fn of<'a>(puts: &'a [Put], prefix: &str) -> Vec<&'a str> {
    puts.iter().map(|it| it.what.as_str()).filter(|it| it.starts_with(prefix)).collect()
}

#[test]
#[ignore = "needs BLAZE_MACRO_CORPUS with `wasm` naming serde_derive's module, built by blaze for wasm32-wasip1"]
fn a_modules_expansion_and_machine_code_are_found_by_the_next_build() {
    let config = Corpus::read().unwrap_or_else(|why| panic!("{why}"));
    let path = config
        .wasm
        .as_ref()
        .and_then(|it| it.get(KRATE))
        .unwrap_or_else(|| panic!("no `wasm` module of `{KRATE}`"));
    let module = std::fs::read(path).unwrap_or_else(|error| panic!("{path}: {error}"));
    let toolchain = load::toolchain().unwrap_or_else(|why| panic!("{why}"));
    let mut sim = Sim::new(5, Faults::default());
    let mut store = kept_in(&mut sim);
    let keeping = kept::Keeping::new(Expansions {
        store: store.reader(),
        toolchain,
        environment: Arc::new(Environment::default()),
    })
    .unwrap_or_else(|why| panic!("{why}"));
    let macros = blaze_macro_eval::Macros::new(kept::Keeps::In(keeping), None);

    let modules = blaze_macro_eval::Modules::new().unwrap_or_else(|why| panic!("{why}"));
    let first = build(&config, &macros, &modules, &module);
    assert_eq!(modules.loads().compiled, 1, "the first build compiles the module once");
    let mut puts = kept::take_puts();
    macros.flush();
    puts.extend(kept::take_puts());
    // The crate's display name is its package's or its target's.
    let derives = "the proc macro `Serialize` of ";
    assert!(!of(&puts, derives).is_empty(), "the first build keeps no expansion of the module");
    let code = "the machine code of the wasm module of the proc-macro crate ";
    assert_eq!(of(&puts, code).len(), 1, "the first build keeps the module's machine code once");
    let published = macros.keeps().publish(&mut store, puts);
    assert!(published.refused.is_empty(), "{:?}", published.refused);
    drop(first);

    // The second build's process memo still has the module: what it proves is that no
    // expansion runs. A process of its own proves the code is loaded (`src/wasm/tests.rs`).
    let second = build(&config, &macros, &modules, &module);
    assert_eq!(modules.loads().compiled, 1, "the second build compiled the module again");
    let puts = kept::take_puts();
    assert!(
        of(&puts, derives).is_empty(),
        "the second build ran what the first kept: {:?}",
        of(&puts, "")
    );
    drop(second);
}
