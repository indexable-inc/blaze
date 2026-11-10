//! blaze-macro-eval's expansions kept and found again: with a store to keep in, a build of the
//! fixture workspace keeps what each stateless macro made; a second build finds every one of
//! them without running it and gives the same output; a call whose variable or file changed
//! finds nothing of the first and runs again. A value verified across checkouts is given to
//! the warm database until the record that retracts it is published, which moves the epoch a
//! kept front end is lowered into at, and the call then expands again.
//!
//! A macro that keeps a session reports what a call read only once its crate settles and the
//! call gave the same from the fresh state as from the settled one: an "emit once" macro, which
//! settles by what it does to its state yet gives otherwise as the first call, never does, and
//! is told to the store as stateful; a macro that settles as proc-macro2's detection does
//! reports every call's reads, keeps its calls from the settled state, and a later build finds
//! them; and a warm revision that edits a call gives what a cold build of the edit gives.
//!
//! It runs on `scratch` of `BLAZE_MACRO_CORPUS` (`corpus/config.rs`), a copy of the workspace
//! built as the first, whose `calls/data.txt` and `sessions/src/lib.rs` it changes and puts
//! back. Its builds share one `Macros`, as a daemon's do; it changes files of the scratch
//! workspace, so its tests take [`SCRATCH`] in turn, in a process of their own.

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

use base_db::Crate;
use blaze_core::sim::{Faults, Sim};
use blaze_usercode::{index, roots::verify};
use hir_def::nameres::crate_def_map;
use hir_expand::proc_macro::{ExpansionReads, ProcMacroCall, ProcMacroExpander};
use proc_macro_api::kept::{self, Environment, Expansions, Keep, Put};

use config::Corpus;
use record::{Record, Recorder, Records};

/// Held by each test while it builds and changes the scratch workspace.
static SCRATCH: blaze_core::lock::Lock<()> = blaze_core::lock::Lock::new("macro.test.kept.scratch", ());

/// A store in `sim`, open and ready.
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
        store.complete(completion, effects);
    }).expect("the effects end");
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
    let workspace =
        load::lower(dir, &config.cargo_home, config.homes(), keeping, |krate, macros| {
            Ok(macros
                .into_iter()
                .map(|eval| {
                    loaded.push((
                        krate.to_owned(),
                        eval.name.as_str().to_owned(),
                        eval.expander.clone(),
                    ));
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
    let found: Vec<&Record> = records
        .iter()
        .filter(|it| &*it.call.name == name && it.call.subject().as_deref() == Some(subject))
        .collect();
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
    for put in
        puts.iter_mut().filter(|it| it.what.starts_with(named) && !it.value.starts_with(COPY))
    {
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
        assert_ne!(
            was.len(),
            text.len(),
            "a changed length tells the change apart from the file's stamp alone"
        );
        std::fs::write(path, text).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        let file = std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        file.set_modified(SystemTime::now() + Duration::from_secs(5))
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
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
    let _scratch = SCRATCH.lock();
    let config = Corpus::read().unwrap_or_else(|why| panic!("{why}"));
    let dir = config.scratch().unwrap_or_else(|why| panic!("{why}")).to_owned();
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

    // A first build runs every macro and keeps what each stateless one made.
    let records = record::records();
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
        assert!(
            !puts_of(&puts, name).is_empty(),
            "the first build keeps nothing of `{name}`: {:?}",
            whats(&puts)
        );
    }
    assert!(
        puts_of(&puts, "counter").is_empty(),
        "`counter` keeps state, and was kept: {:?}",
        whats(&puts)
    );
    let verdict = |puts: &[Put], krate: &str| {
        // The crate's display name is its package's or its target's.
        let names = [krate.to_owned(), krate.replace('_', "-")]
            .map(|name| format!("the verdict of the proc macros of `{name}`"));
        puts.iter()
            .filter(|it| names.contains(&it.what))
            .map(|it| it.value.clone())
            .collect::<Vec<_>>()
    };
    // Its first call changed the fresh state (it settles), its second what the first left.
    assert_eq!(
        verdict(&puts, "corpus_state"),
        [b"settles".to_vec(), b"stateful".to_vec()],
        "`counter` was seen to keep state: {:?}",
        whats(&puts)
    );
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
        assert_eq!(
            (&one.call.krate, &one.call.name),
            (&two.call.krate, &two.call.name),
            "both builds call in one order"
        );
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
        (record::literals(&output.tree), kept::take_puts())
    };

    // A call whose variable changed finds nothing of the first and runs again.
    let package = the_call(&secondly, "env_name", "PACKAGE");
    let mut env = package.call.env.clone();
    env.set("CARGO_PKG_NAME", "corpus-renamed");
    let (said, puts) = replay("env_name", ProcMacroCall { env: &env, ..package.call.call() });
    assert_eq!(said, ["\"corpus-renamed\""], "`env_name!` with CARGO_PKG_NAME changed");
    assert_eq!(
        puts_of(&puts, "env_name").len(),
        1,
        "with CARGO_PKG_NAME changed it ran: {:?}",
        whats(&puts)
    );
    let (said, puts) = replay("env_name", package.call.call());
    assert_eq!(said, ["\"corpus-calls\""], "`env_name!` as it was called");
    assert!(puts.is_empty(), "as it was called it is found: {:?}", whats(&puts));

    // A call whose file changed finds nothing of the first and reads it again.
    let data = the_call(&secondly, "include_data", "DATA");
    let changed =
        Changed::write(&Path::new(&dir).join("calls/data.txt"), "changed after the first build\n");
    let (said, puts) = replay("include_data", data.call.call());
    assert!(
        said.iter().any(|it| it.contains("changed after the first build")),
        "`include_data!` read the old file: {said:?}"
    );
    assert_eq!(
        puts_of(&puts, "include_data").len(),
        1,
        "with its file changed it ran: {:?}",
        whats(&puts)
    );
    drop(changed);

    // M1 (round 8): the warm database was given `Serialize` verified across checkouts. Once
    // the record that retracts it is published, the epoch a kept front end is lowered into at
    // moves (what `blaze-cargo`'s `unit.rs` compares before it reuses a front end), and the
    // call looks it up again, finds it retracted, and expands.
    let serialize = (secondly.iter())
        .find(|it| &*it.call.name == "Serialize" && it.call.calling == Some(calls))
        .unwrap_or_else(|| panic!("`corpus-calls` calls no `Serialize`"));
    let again = |call: ProcMacroCall<'_>| {
        let output =
            expander_of(&loaded, &serialize.call.krate, "Serialize").expand_in(db, calls, call);
        output.unwrap_or_else(|error| panic!("`Serialize` again: {error:?}"));
        kept::take_puts()
    };
    let puts = again(serialize.call.call());
    assert!(puts.is_empty(), "verified and not retracted, it is found: {:?}", whats(&puts));
    let at = macros.keeps().retractions_now();
    let keeping = macros.keeps().keeping().unwrap_or_else(|not| panic!("{not}"));
    for (run, unmarked) in &verified {
        keeping.put(Put::of(
            verify::retraction(run, unmarked),
            "the record of `Serialize`".to_owned(),
        ));
    }
    assert_eq!(
        macros.keeps().retractions_now(),
        at,
        "put, not published: nothing is retracted yet"
    );
    let published = macros.keeps().publish(&mut store, Vec::new());
    assert!(published.refused.is_empty(), "{:?}", published.refused);
    assert!(!published.durable.is_empty(), "a record is durable before its job is done");
    assert_ne!(
        macros.keeps().retractions_now(),
        at,
        "retracted: a front end kept at the old epoch is lowered afresh"
    );
    let puts = again(serialize.call.call());
    assert!(
        !puts_of(&puts, "Serialize").is_empty(),
        "retracted, it expanded again: {:?}",
        whats(&puts)
    );
}

/// The values of the store's verdict of the proc-macro crate `krate` that `puts` put.
fn verdicts(puts: &[Put], krate: &str) -> Vec<Vec<u8>> {
    // The crate's display name is its package's or its target's.
    let names = [krate.to_owned(), krate.replace('_', "-")]
        .map(|name| format!("the verdict of the proc macros of `{name}`"));
    puts.iter().filter(|it| names.contains(&it.what)).map(|it| it.value.clone()).collect()
}

/// What each call in crate `krate` of a macro of crate `of` read, as its expansion reported it
/// to the front end (`hir_expand`'s `expansion_reads`), its def map made first.
fn reads_of(db: &base_db::Db, krate: Crate, of: Crate) -> Vec<ExpansionReads> {
    let def_map = crate_def_map(db, krate);
    let calls: Vec<_> =
        def_map.modules(db).flat_map(|(_, module)| module.scope(db).all_macro_calls()).collect();
    calls
        .into_iter()
        .filter(|call| call.loc(db).def.krate == of)
        .map(|call| call.expansion_reads(db).clone())
        .collect()
}

/// The one recorded call of `name` on `subject`.
fn call_on<'a>(records: &'a [Record], name: &str, subject: &str) -> &'a Record {
    let found: Vec<&Record> = records
        .iter()
        .filter(|it| &*it.call.name == name && it.call.subject().as_deref() == Some(subject))
        .collect();
    match found.as_slice() {
        [one] => one,
        _ => panic!("{} calls of `{name}` on `{subject}`", found.len()),
    }
}

/// The macros of `corpus-once` (`once!`, "emit once": settles by its state, gives otherwise as
/// the first call) and `corpus-settles` (`detect!`, settles as proc-macro2's detection does),
/// three calls each in `corpus-sessions`, over two builds sharing a store and a warm revision
/// of the second:
///
/// 1. The first build sees each crate's first call change the fresh state: both are told to
///    settle, every call's reads are unknown (neither was known to settle at load), and the
///    calls from the settled state that give the same from the fresh one are kept: `detect!`'s,
///    and none of `once!`'s.
/// 2. The second loads both as settling. Every `once!` call gives otherwise from the fresh
///    state than from the settled one: its reads are unknown, saying so, the crate is told to
///    be stateful, and none of its calls is kept. Every `detect!` call reports what it read,
///    those from the settled state found as the first build kept them.
/// 3. Its database, warm, with the third call of each edited: `detect!`'s edited call gives
///    what a cold build of the edit gives, byte for byte, and `once!`'s calls' reads are
///    unknown.
#[test]
#[ignore = "needs BLAZE_MACRO_CORPUS with `scratch`: a copy of the fixture workspace built by blaze"]
fn a_session_macro_reports_its_reads_only_once_shown_the_same_from_either_state() {
    let _scratch = SCRATCH.lock();
    let config = Corpus::read().unwrap_or_else(|why| panic!("{why}"));
    let dir = config.scratch().unwrap_or_else(|why| panic!("{why}")).to_owned();
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
    let records = record::records();
    let mut loaded = Loaded::new();
    let crates = |workspace: &load::Workspace| {
        let krate = |name: &str| workspace.krate(name).unwrap_or_else(|why| panic!("{why}"));
        (krate("corpus-sessions"), krate("corpus-once"), krate("corpus-settles"))
    };
    let unknown_saying = |reads: &[ExpansionReads], said: &str| {
        reads.len() == 3
            && reads
                .iter()
                .all(|it| matches!(it, ExpansionReads::Unknown(why) if why.contains(said)))
    };
    let traced = |reads: &[ExpansionReads]| {
        reads.len() == 3 && reads.iter().all(|it| matches!(it, ExpansionReads::Traced { .. }))
    };

    // 1. Unseen: each crate's first call changes the fresh state, and none what it left.
    let first = build(&config, &dir, &macros, &records, &mut loaded);
    let (sessions, once, settles) = crates(&first);
    let db = first.lowered.db();
    for (of, reads) in
        [("once", reads_of(db, sessions, once)), ("detect", reads_of(db, sessions, settles))]
    {
        assert!(unknown_saying(&reads, "does not settle"), "`{of}!`, unseen: {reads:?}");
    }
    let mut puts = kept::take_puts();
    assert_eq!(verdicts(&puts, "corpus_once"), [b"settles".to_vec()], "{:?}", whats(&puts));
    assert_eq!(verdicts(&puts, "corpus_settles"), [b"settles".to_vec()], "{:?}", whats(&puts));
    assert!(
        puts_of(&puts, "once").is_empty(),
        "`once!` was kept, giving otherwise from the fresh state: {:?}",
        whats(&puts)
    );
    assert!(
        !puts_of(&puts, "detect").is_empty(),
        "`detect!`'s calls from the settled state are kept: {:?}",
        whats(&puts)
    );
    macros.flush();
    puts.extend(kept::take_puts());
    let published = macros.keeps().publish(&mut store, puts);
    assert!(published.refused.is_empty(), "{:?}", published.refused);
    drop(first);
    loaded.clear();
    taken(&records);

    // 2. Both settle at load. `once!` gives otherwise from the fresh state than from the
    //    settled one, at each call; `detect!` gives the same, and is found.
    let mut second = build(&config, &dir, &macros, &records, &mut loaded);
    let (sessions, once, settles) = crates(&second);
    let db = second.lowered.db();
    let reads = reads_of(db, sessions, once);
    assert!(unknown_saying(&reads, "gives otherwise from the fresh state"), "{reads:?}");
    let reads = reads_of(db, sessions, settles);
    assert!(traced(&reads), "{reads:?}");
    let puts = kept::take_puts();
    assert_eq!(
        verdicts(&puts, "corpus_once"),
        [b"stateful".to_vec()],
        "`once!` was told to be stateful: {:?}",
        whats(&puts)
    );
    assert!(puts_of(&puts, "once").is_empty(), "`once!` was kept unshown: {:?}", whats(&puts));
    assert!(
        puts_of(&puts, "detect").is_empty(),
        "`detect!` ran what the first build kept: {:?}",
        whats(&puts)
    );
    taken(&records);

    // 3. A warm revision of the second build's database, the third call of each edited.
    let path = Path::new(&dir).join("sessions/src/lib.rs");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    let edited = text
        .replace("once!(ONCE_THIRD)", "once!(ONCE_EDITED)")
        .replace("detect!(DETECT_THIRD)", "detect!(DETECT_EDITED)");
    assert_ne!(
        edited,
        text,
        "{} calls `once!(ONCE_THIRD)` and `detect!(DETECT_THIRD)`",
        path.display()
    );
    let file = (second.lowered.paths().iter())
        .find(|(_, it)| Path::new(it.as_str()) == path)
        .map(|(file, _)| *file)
        .unwrap_or_else(|| panic!("the front end has no file {}", path.display()));
    second.lowered.served_db().set_file_text(file, &edited);
    let db = second.lowered.db();
    let reads = reads_of(db, sessions, once);
    assert!(
        reads.len() == 3 && reads.iter().all(|it| matches!(it, ExpansionReads::Unknown(_))),
        "warm: {reads:?}"
    );
    let reads = reads_of(db, sessions, settles);
    assert!(traced(&reads), "warm: {reads:?}");
    let warm = taken(&records);
    let warm_edited = call_on(&warm, "detect", "DETECT_EDITED");
    drop(second);
    loaded.clear();

    // A cold build of the edit, every call run in order from a fresh session, nothing kept:
    // what rustc gives.
    let changed = Changed::write(&path, &edited);
    let cold_macros =
        blaze_macro_eval::Macros::new(kept::Keeps::Nothing("a cold build keeps nothing"), None);
    let cold = build(&config, &dir, &cold_macros, &records, &mut loaded);
    let (sessions, _, settles) = crates(&cold);
    reads_of(cold.lowered.db(), sessions, settles);
    let coldly = taken(&records);
    let cold_edited = call_on(&coldly, "detect", "DETECT_EDITED");
    match (warm_edited.eval.expanded(), cold_edited.eval.expanded()) {
        (Some(warm), Some(cold)) => {
            if let Some(difference) = difference::difference(cold, warm) {
                panic!("{} expands warm otherwise than cold: {difference}", warm_edited.what());
            }
        }
        _ => panic!("{} did not expand both warm and cold", warm_edited.what()),
    }
    drop(cold);
    drop(changed);
}
