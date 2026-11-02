//! blaze-macro-eval's expansions kept and found again: with a store to keep in, a build of the
//! fixture workspace keeps what each stateless macro made; a second build finds every one of
//! them without running it and gives the same output; a call whose variable or file changed
//! finds nothing of the first and runs again. A value verified across checkouts is given to
//! the warm database until the record that retracts it is published, which moves the epoch a
//! kept front end is lowered into at, and the call then expands again.
//!
//! It runs on `scratch` of `BLAZE_MACRO_CORPUS` (`corpus/config.rs`), a copy of the workspace
//! built as the first, whose `calls/data.txt` it changes and puts back. Its builds share one
//! `Macros`, as a daemon's do; it changes a file of the scratch workspace, so it is one test in
//! a process of its own.

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
use blaze_usercode::{index, roots::verify};
use proc_macro_api::kept::{self, Environment, Expansions, Keep, Put};
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
fn build(
    config: &Corpus,
    dir: &str,
    keeping: &Arc<blaze_macro_eval::Macros>,
    records: &Records,
    loaded: &mut Loaded,
) -> load::Workspace {
    let workspace = load::lower(dir, &config.cargo_home, config.homes(), keeping, |krate, macros| {
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
    expander_of(loaded, "corpus_macros", name)
}

/// The expander of macro `name` of crate `of`, as loaded.
fn expander_of<'a>(loaded: &'a Loaded, of: &str, name: &str) -> &'a Arc<dyn ProcMacroExpander> {
    let found: Vec<_> = loaded.iter().filter(|(krate, it, _)| krate == of && it == name).collect();
    match found.as_slice() {
        [(_, _, expander)] => expander,
        _ => panic!("`{of}` has {} macros named `{name}`", found.len()),
    }
}

/// What a checkout's copy of a value is kept as starting with (`verify::copy_of`): not an
/// expansion, so never made a verified value here.
const COPY: &[u8] = b"blaze v2 a checkout's copy 2\0";

/// `puts`, each expansion of `Serialize` kept as a value verified across checkouts, as one is
/// once a second checkout expanded it the same (`verify::keeps`): the runs and the keys of
/// their unmarked results, which a record would retract them by.
fn verified(puts: &mut [Put]) -> Vec<(blaze_usercode::RunKey, blaze_cas::Key)> {
    let named = "the proc macro `Serialize` of ";
    let mut verified = Vec::new();
    for put in puts.iter_mut().filter(|it| it.what.starts_with(named) && !it.value.starts_with(COPY)) {
        let Keep::Run { run, traced } = &put.keep else { continue };
        // Any key stands for the unmarked result: it is what the record is found by.
        let unmarked = index::result_key(run, traced);
        put.value = index::verified_value(&unmarked, &put.value);
        verified.push((*run, unmarked));
    }
    verified
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
    let keeping = kept::Keeping::new(Expansions { store: store.reader(), toolchain, environment: Arc::new(Environment::default()) })
        .unwrap_or_else(|why| panic!("{why}"));
    let macros = blaze_macro_eval::Macros::new(kept::Keeps::In(keeping));

    // A first build runs every macro and keeps what each stateless one made.
    let records = Records::default();
    let mut loaded = Loaded::new();
    let first = build(&config, &dir, &macros, &records, &mut loaded);
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
    macros.flush();
    puts.extend(kept::take_puts());
    assert_eq!(verdict(&puts, "corpus_macros"), [b"stateless".to_vec()], "{:?}", whats(&puts));
    let verified = verified(&mut puts);
    assert!(!verified.is_empty(), "the first build keeps `Serialize`: {:?}", whats(&puts));
    let published = macros.keeps().publish(&mut store, puts);
    assert!(published.refused.is_empty(), "{:?}", published.refused);
    drop(first);
    loaded.clear();

    // A second build finds each, runs none of them, and gives what the first did.
    let second = build(&config, &dir, &macros, &records, &mut loaded);
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

    // M1 (round 8): the warm database was given `Serialize` verified across checkouts. Once
    // the record that retracts it is published, the epoch a kept front end is lowered into at
    // moves (what `blaze-cargo`'s `unit.rs` compares before it reuses a front end), and the
    // call looks it up again, finds it retracted, and expands.
    let serialize = (secondly.iter())
        .find(|it| &*it.call.name == "Serialize" && it.call.calling == Some(calls))
        .unwrap_or_else(|| panic!("`corpus-calls` calls no `Serialize`"));
    let again = |call: ProcMacroCall<'_>| {
        let output = expander_of(&loaded, &serialize.call.krate, "Serialize").expand_in(db, calls, call);
        output.unwrap_or_else(|error| panic!("`Serialize` again: {error:?}"));
        kept::take_puts()
    };
    let puts = again(serialize.call.call());
    assert!(puts.is_empty(), "verified and not retracted, it is found: {:?}", whats(&puts));
    let at = macros.keeps().retractions_now();
    let keeping = macros.keeps().keeping().unwrap_or_else(|not| panic!("{not}"));
    for (run, unmarked) in &verified {
        keeping.put(Put::of(verify::retraction(run, unmarked), "the record of `Serialize`".to_owned()));
    }
    assert_eq!(macros.keeps().retractions_now(), at, "put, not published: nothing is retracted yet");
    let published = macros.keeps().publish(&mut store, Vec::new());
    assert!(published.refused.is_empty(), "{:?}", published.refused);
    assert!(!published.durable.is_empty(), "a record is durable before its job is done");
    assert_ne!(macros.keeps().retractions_now(), at, "retracted: a front end kept at the old epoch is lowered afresh");
    let puts = again(serialize.call.call());
    assert!(!puts_of(&puts, "Serialize").is_empty(), "retracted, it expanded again: {:?}", whats(&puts));
}
