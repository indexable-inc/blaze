//! blaze-macro-eval's expansions kept and found again: with keeping installed, a build of the
//! fixture workspace keeps what each stateless macro made; a second build finds every one of
//! them without running it and gives the same output; a call whose variable or file changed
//! finds nothing of the first and runs again.
//!
//! It runs on `scratch` of `BLAZE_MACRO_CORPUS` (`corpus/config.rs`), a copy of the workspace
//! built as the first, whose `calls/data.txt` it changes and puts back. Keeping is installed
//! once per process and the store's verdicts are global, so it is one test in a process of
//! its own.

#[allow(dead_code, reason = "shared with tests/corpus.rs, which uses other parts of it")]
#[path = "corpus/config.rs"]
mod config;
#[path = "corpus/difference.rs"]
mod difference;
#[allow(dead_code, reason = "shared with tests/corpus.rs, which uses other parts of it")]
#[path = "corpus/load.rs"]
mod load;
#[allow(dead_code, reason = "shared with tests/corpus.rs, which uses other parts of it")]
#[path = "corpus/record.rs"]
mod record;

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime},
};

use blaze_core::sim::{Faults, Sim};
use proc_macro_api::kept::{self, Environment, Expansions, Put};
use hir_def::nameres::crate_def_map;
use hir_expand::proc_macro::{ProcMacroCall, ProcMacroExpander};

use config::Corpus;
use record::{Record, Recorder, Records};

/// A store in `sim`, open and ready.
fn kept_in(sim: &mut Sim) -> blaze_cas::Store {
    let mut effects = Vec::new();
    let config = blaze_cas::Config {
        dir: blaze_core::FileName::new("store").expect("a plain name"),
        layers: Vec::new(),
        tags: 1 << 48,
    };
    let mut store = blaze_cas::Store::open(config, &mut effects);
    blaze_core::run_until_idle(sim, &mut effects, |completion, effects| {
        store.complete(completion, effects);
    });
    assert!(store.ready(), "the store in the simulation opens");
    store
}

/// blaze-macro-eval's expanders of the workspace by crate and name, as loaded.
type Loaded = Vec<(String, String, Arc<dyn ProcMacroExpander>)>;

/// The workspace at `dir` lowered, every macro blaze-macro-eval's recording into `records`,
/// and `corpus-calls`' def map made.
fn build(config: &Corpus, dir: &str, records: &Records, loaded: &mut Loaded) -> load::Workspace {
    let workspace = load::lower(dir, &config.cargo_home, |krate, macros| {
        Ok(macros
            .into_iter()
            .map(|eval| {
                loaded.push((krate.to_owned(), eval.name.as_str().to_owned(), eval.expander.clone()));
                Recorder::wrap(krate, eval, records)
            })
            .collect())
    })
    .unwrap_or_else(|why| panic!("{why}"));
    let calls = workspace.krate("corpus-calls").unwrap_or_else(|why| panic!("{why}"));
    crate_def_map(workspace.lowered.db(), calls);
    workspace
}

fn taken(records: &Records) -> Vec<Record> {
    std::mem::take(&mut *records.lock())
}

/// The puts of macro `name`.
fn puts_of<'a>(puts: &'a [Put], name: &str) -> Vec<&'a Put> {
    let named = format!("the proc macro `{name}` of ");
    puts.iter().filter(|it| it.what.starts_with(&named)).collect()
}

fn whats(puts: &[Put]) -> Vec<&str> {
    puts.iter().map(|it| it.what.as_str()).collect()
}

/// The one recorded call of `name` on `subject` from `corpus-calls`.
fn the_call<'a>(records: &'a [Record], name: &str, subject: &str) -> &'a Record {
    let found: Vec<&Record> =
        records.iter().filter(|it| &*it.call.name == name && it.call.subject().as_deref() == Some(subject)).collect();
    match found.as_slice() {
        [one] => one,
        _ => panic!("{} calls of `{name}` on `{subject}`", found.len()),
    }
}

fn expander<'a>(loaded: &'a Loaded, name: &str) -> &'a Arc<dyn ProcMacroExpander> {
    let found: Vec<_> = loaded.iter().filter(|(krate, it, _)| krate == "corpus_macros" && it == name).collect();
    match found.as_slice() {
        [(_, _, expander)] => expander,
        _ => panic!("`corpus_macros` has {} macros named `{name}`", found.len()),
    }
}

/// The file `path` with `text` written over it, put back as it was when dropped.
struct Changed {
    path: PathBuf,
    was: Vec<u8>,
}

impl Changed {
    fn write(path: &Path, text: &str) -> Changed {
        let was = std::fs::read(path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        assert_ne!(was.len(), text.len(), "a changed length tells the change apart from the file's stamp alone");
        std::fs::write(path, text).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        let file = std::fs::File::options().write(true).open(path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        file.set_modified(SystemTime::now() + Duration::from_secs(5)).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        Changed { path: path.to_owned(), was }
    }
}

impl Drop for Changed {
    fn drop(&mut self) {
        if let Err(error) = std::fs::write(&self.path, &self.was) {
            eprintln!("putting back {}: {error}", self.path.display());
        }
    }
}

#[test]
#[ignore = "needs BLAZE_MACRO_CORPUS with `scratch`: a copy of the fixture workspace built by blaze"]
fn a_kept_expansion_is_found_and_one_whose_input_changed_is_not() {
    let config = Corpus::read().unwrap_or_else(|why| panic!("{why}"));
    let dir = config.scratch().unwrap_or_else(|why| panic!("{why}")).to_owned();
    let toolchain = load::toolchain().unwrap_or_else(|why| panic!("{why}"));
    let mut sim = Sim::new(5, Faults::default());
    let mut store = kept_in(&mut sim);
    kept::install(Expansions { store: store.reader(), toolchain, environment: Arc::new(Environment::default()) })
        .unwrap_or_else(|why| panic!("{why}"));

    // A first build runs every macro and keeps what each stateless one made.
    let records = Records::default();
    let mut loaded = Loaded::new();
    let first = build(&config, &dir, &records, &mut loaded);
    let firstly = taken(&records);
    let mut puts = kept::take_puts();
    for name in [
        "Serialize",
        "Deserialize",
        "Error",
        "main",
        "test",
        "select_priv_declare_output_enum",
        "Describe",
        "label",
        "make_fn",
        "include_data",
        "env_name",
    ] {
        assert!(!puts_of(&puts, name).is_empty(), "the first build keeps nothing of `{name}`: {:?}", whats(&puts));
    }
    assert!(puts_of(&puts, "counter").is_empty(), "`counter` keeps state, and was kept: {:?}", whats(&puts));
    let verdict = |puts: &[Put], krate: &str| {
        // The crate's display name is its package's or its target's.
        let names = [krate.to_owned(), krate.replace('_', "-")]
            .map(|name| format!("the verdict of the proc macros of `{name}`"));
        puts.iter().filter(|it| names.contains(&it.what)).map(|it| it.value.clone()).collect::<Vec<_>>()
    };
    assert_eq!(verdict(&puts, "corpus_state"), [b"stateful".to_vec()], "`counter` was seen to keep state: {:?}", whats(&puts));
    // The verdicts of the crates that kept no state are left with the puts of the thread that
    // calls `flush`, as the daemon's loop does, and published with the build's.
    blaze_macro_eval::flush();
    puts.extend(kept::take_puts());
    assert_eq!(verdict(&puts, "corpus_macros"), [b"stateless".to_vec()], "{:?}", whats(&puts));
    let published = kept::publish(&mut store, puts);
    assert!(published.refused.is_empty(), "{:?}", published.refused);
    drop(first);
    loaded.clear();

    // A second build finds each, runs none of them, and gives what the first did.
    let second = build(&config, &dir, &records, &mut loaded);
    let secondly = taken(&records);
    let puts = kept::take_puts();
    assert!(puts.is_empty(), "the second build ran what the first kept: {:?}", whats(&puts));
    assert_eq!(firstly.len(), secondly.len(), "both builds make the same calls");
    for (one, two) in firstly.iter().zip(&secondly) {
        assert_eq!((&one.call.krate, &one.call.name), (&two.call.krate, &two.call.name), "both builds call in one order");
        match (one.eval.expanded(), two.eval.expanded()) {
            (Some(ours), Some(theirs)) => {
                if let Some(difference) = difference::difference(theirs, ours) {
                    panic!("{} is found otherwise than it was made: {difference}", two.what());
                }
            }
            _ => panic!("{} did not expand in both builds", two.what()),
        }
    }

    let db = second.lowered.db();
    let calls = second.krate("corpus-calls").unwrap_or_else(|why| panic!("{why}"));
    let replay = |name: &str, call: ProcMacroCall<'_>| {
        let output = expander(&loaded, name).expand_in(db, calls, call);
        let output = output.unwrap_or_else(|error| panic!("`{name}` again: {error:?}"));
        (record::literals(&output), kept::take_puts())
    };

    // A call whose variable changed finds nothing of the first and runs again.
    let package = the_call(&secondly, "env_name", "PACKAGE");
    let mut env = package.call.env.clone();
    env.set("CARGO_PKG_NAME", "corpus-renamed");
    let (said, puts) = replay("env_name", ProcMacroCall { env: &env, ..package.call.call() });
    assert_eq!(said, ["\"corpus-renamed\""], "`env_name!` with CARGO_PKG_NAME changed");
    assert_eq!(puts_of(&puts, "env_name").len(), 1, "with CARGO_PKG_NAME changed it ran: {:?}", whats(&puts));
    let (said, puts) = replay("env_name", package.call.call());
    assert_eq!(said, ["\"corpus-calls\""], "`env_name!` as it was called");
    assert!(puts.is_empty(), "as it was called it is found: {:?}", whats(&puts));

    // A call whose file changed finds nothing of the first and reads it again.
    let data = the_call(&secondly, "include_data", "DATA");
    let changed = Changed::write(&Path::new(&dir).join("calls/data.txt"), "changed after the first build\n");
    let (said, puts) = replay("include_data", data.call.call());
    assert!(said.iter().any(|it| it.contains("changed after the first build")), "`include_data!` read the old file: {said:?}");
    assert_eq!(puts_of(&puts, "include_data").len(), 1, "with its file changed it ran: {:?}", whats(&puts));
    drop(changed);
}
