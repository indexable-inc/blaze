//! Real proc macros expanded by blaze-macro-eval, held to what the corpus says of them.
//!
//! The fixture workspace (`tests/corpus/ws`) calls serde_derive, thiserror, tokio-macros and its
//! own macros (one of each kind, one that panics, one that reads a file of the calling package,
//! one that reads its variables, one that counts its calls), each from the registry at the
//! version its `Cargo.lock` pins. Its calls are expanded as the front end expands them (each
//! calling crate's def map) with every macro blaze-macro-eval's. These tests check that every
//! call expands, each panic says the macro's message, and the macros that read files,
//! variables and state read the caller's. What each expansion does is held by the fixture's own
//! tests (`ws/calls/src/tests.rs`), which the harness runs on blaze's build.
//!
//! What it needs is one JSON value in `BLAZE_MACRO_CORPUS` (`corpus/config.rs`): the harness
//! fetches the workspace's packages, builds it with `blaze build -p corpus-calls` (so each build
//! script, std's among them, has run) and runs these with `--ignored`.

#[allow(dead_code, reason = "shared with tests/kept.rs, which uses other parts of it")]
#[path = "corpus/config.rs"]
mod config;
#[allow(dead_code, reason = "shared with tests/kept.rs, which uses other parts of it")]
#[path = "corpus/load.rs"]
mod load;
#[allow(dead_code, reason = "shared with tests/kept.rs, which uses other parts of it")]
#[path = "corpus/record.rs"]
mod record;

use std::{
    sync::{Arc, OnceLock, atomic::Ordering},
    time::Instant,
};

use base_db::Crate;
use hir_def::nameres::{crate_def_map, diagnostics::DefDiagnosticKind};
use hir_expand::proc_macro::{ProcMacro, ProcMacroKind};

use config::Corpus;
use record::{Ahead, Outcome, Record, Recorder};

/// Every call of the workspace's two calling crates, in the order the front end made them.
struct Expanded {
    records: Vec<Record>,
    calls: Crate,
    panics: Crate,
    /// The def-map diagnostics of `corpus-calls` but code `cfg` leaves out.
    diagnostics: Vec<String>,
}

fn expanded() -> &'static Expanded {
    static EXPANDED: OnceLock<Result<Expanded, String>> = OnceLock::new();
    match EXPANDED.get_or_init(expand) {
        Ok(expanded) => expanded,
        Err(why) => panic!("{why}"),
    }
}

fn expand() -> Result<Expanded, String> {
    expand_with(&Ahead::On(Arc::default()))
}

/// The workspace's calls expanded, each ordered call run ahead of its turn or not as `ahead`
/// says.
fn expand_with(ahead: &Ahead) -> Result<Expanded, String> {
    let config = Corpus::read()?;
    let records = record::records();
    let keeping = blaze_macro_eval::Macros::nothing(
        "the corpus compares each expansion that runs, so none is kept",
    );
    let workspace = load::lower(
        &config.workspace,
        &config.cargo_home,
        config.homes(),
        &keeping,
        |krate, macros| Ok(wrap(krate, macros, &records, ahead)),
    )?;
    let calls = workspace.krate("corpus-calls")?;
    let panics = workspace.krate("corpus-panics")?;
    let db = workspace.lowered.db();
    let diagnostics = crate_def_map(db, calls)
        .diagnostics(db)
        .iter()
        .filter(|it| !matches!(it.kind, DefDiagnosticKind::UnconfiguredCode { .. }))
        .map(|it| format!("{:?}", it.kind))
        .collect();
    crate_def_map(db, panics);
    let records = std::mem::take(&mut *records.lock());
    Ok(Expanded { records, calls, panics, diagnostics })
}

/// Each of blaze-macro-eval's macros of crate `krate`, recording its calls.
fn wrap(
    krate: &str,
    macros: Vec<ProcMacro>,
    records: &record::Records,
    ahead: &Ahead,
) -> Vec<ProcMacro> {
    macros
        .into_iter()
        .map(|eval| Recorder::wrap_ahead(krate, eval, records, ahead.clone()))
        .collect()
}

/// The calls of macro `name` of crate `krate` from crate `calling`.
fn calls_of<'a>(
    krate: &'a str,
    name: &'a str,
    calling: Crate,
) -> impl Iterator<Item = &'static Record> + 'a {
    expanded().records.iter().filter(move |it| {
        &*it.call.krate == krate && &*it.call.name == name && it.call.calling == Some(calling)
    })
}

/// The one call of macro `name` of crate `krate` from crate `calling` whose subject (the item
/// a derive or an attribute is on, the first identifier a function-like macro is given) is
/// `subject`.
fn call_on(krate: &str, name: &str, calling: Crate, subject: &str) -> &'static Record {
    let found: Vec<&Record> = calls_of(krate, name, calling)
        .filter(|it| it.call.subject().as_deref() == Some(subject))
        .collect();
    match found.as_slice() {
        [one] => one,
        [] => panic!("no call of `{krate}::{name}` on `{subject}`"),
        _ => panic!("`{krate}::{name}` was called on `{subject}` {} times", found.len()),
    }
}

/// What blaze-macro-eval gave the call, which must have expanded.
fn output(record: &Record) -> &tt::TopSubtree {
    match &record.eval {
        Outcome::Expanded(tree) => tree,
        Outcome::Panicked(message) => panic!("{} panicked with {message:?}", record.what()),
        Outcome::Failed(why) => panic!("{} failed: {why}", record.what()),
    }
}

const COVERED: &[(&str, &str, &str)] = &[
    ("serde_derive", "Serialize", "Plain"),
    ("serde_derive", "Deserialize", "Plain"),
    ("serde_derive", "Serialize", "Renamed"),
    ("serde_derive", "Deserialize", "Renamed"),
    ("serde_derive", "Serialize", "Outer"),
    ("serde_derive", "Deserialize", "Outer"),
    ("serde_derive", "Serialize", "Wrapper"),
    ("serde_derive", "Deserialize", "Wrapper"),
    ("serde_derive", "Serialize", "Borrowed"),
    ("serde_derive", "Deserialize", "Borrowed"),
    ("serde_derive", "Serialize", "Shape"),
    ("serde_derive", "Deserialize", "Shape"),
    ("serde_derive", "Serialize", "Pair"),
    ("serde_derive", "Deserialize", "Pair"),
    ("thiserror_impl", "Error", "Failure"),
    ("thiserror_impl", "Error", "ParseError"),
    ("thiserror_impl", "Error", "Opaque"),
    ("thiserror_impl", "Error", "Generic"),
    ("tokio_macros", "main", "tokio_main"),
    ("tokio_macros", "test", "tokio_test"),
    ("corpus_macros", "Describe", "Described"),
    ("corpus_macros", "label", "labelled"),
    ("corpus_macros", "label", "labelled_again"),
    ("corpus_macros", "make_fn", "answer"),
    ("corpus_macros", "make_fn", "greeting"),
    ("corpus_macros", "include_data", "DATA"),
    ("corpus_macros", "env_name", "PACKAGE"),
    ("corpus_state", "counter", "FIRST"),
    ("corpus_state", "counter", "SECOND"),
    ("corpus_state", "counter", "THIRD"),
    ("corpus_state", "counter", "FOURTH"),
    ("corpus_state", "counter", "FIFTH"),
];

/// The longest a serde derive of the corpus may take on the evaluator: past it, the
/// evaluator is too slow for builds, whether or not the call ends within its steps.
const SERDE_DERIVE_TIME: std::time::Duration = std::time::Duration::from_secs(30);

/// serde's derives on the corpus' plain struct and on its enum, each timed: what tells
/// an evaluator that is slow from a macro that loops (which stops at its steps, naming them).
#[test]
#[ignore = "needs BLAZE_MACRO_CORPUS: the fixture workspace built by blaze"]
fn a_serde_derive_takes_seconds_on_the_evaluator() {
    let expanded = expanded();
    for subject in ["Plain", "Shape"] {
        for name in ["Serialize", "Deserialize"] {
            let record = call_on("serde_derive", name, expanded.calls, subject);
            output(record);
            eprintln!("serde_derive::{name} on `{subject}`: {:?}", record.took);
            assert!(record.took < SERDE_DERIVE_TIME, "{} took {:?}", record.what(), record.took);
        }
    }
}

#[test]
#[ignore = "needs BLAZE_MACRO_CORPUS: the fixture workspace built by blaze"]
fn every_call_is_expanded() {
    let expanded = expanded();
    assert!(expanded.diagnostics.is_empty(), "corpus-calls' def map: {:#?}", expanded.diagnostics);
    for (krate, name, subject) in COVERED {
        output(call_on(krate, name, expanded.calls, subject));
    }
    let select: Vec<&Record> =
        calls_of("tokio_macros", "select_priv_declare_output_enum", expanded.calls).collect();
    assert_eq!(
        select.len(),
        1,
        "corpus-calls calls tokio's `select_priv_declare_output_enum!` once"
    );
    output(select[0]);
}

#[test]
#[ignore = "needs BLAZE_MACRO_CORPUS: the fixture workspace built by blaze"]
fn a_panic_is_its_message() {
    let panics = expanded().panics;
    let mut boom: Vec<&Record> = calls_of("corpus_macros", "boom", panics).collect();
    boom.sort_by_key(|it| it.call.subtree.as_token_trees().iter_flat_tokens().count());
    let tokio: Vec<&Record> =
        calls_of("tokio_macros", "select_priv_declare_output_enum", panics).collect();
    let expected = [
        (boom.first(), "boom: 0 tokens"),
        (boom.last(), "boom: 3 tokens"),
        (tokio.first(), "unexpected macro input"),
    ];
    assert_eq!(
        (boom.len(), tokio.len()),
        (2, 1),
        "corpus-panics makes two calls of `boom` and one of tokio's"
    );
    // The calls of the same crate around them expand: a panic fails its own call alone.
    for subject in ["before", "between", "after", "last"] {
        output(call_on("corpus_macros", "make_fn", panics, subject));
    }
    for (record, message) in expected {
        let record = record.expect("counted above");
        match &record.eval {
            Outcome::Panicked(said) => assert_eq!(said, message, "{}", record.what()),
            Outcome::Expanded(tree) => panic!(
                "{} expanded to `{tree}`, and the macro panics with {message:?}",
                record.what()
            ),
            Outcome::Failed(why) => {
                panic!("{} failed ({why}), and the macro panics with {message:?}", record.what())
            }
        }
    }
}

#[test]
#[ignore = "needs BLAZE_MACRO_CORPUS: the fixture workspace built by blaze"]
fn a_static_counts_calls_in_rustcs_order() {
    let calls = expanded().calls;
    let counted: Vec<(String, Vec<String>)> = calls_of("corpus_state", "counter", calls)
        .map(|it| (it.call.subject().unwrap_or_default(), record::literals(output(it))))
        .collect();
    let expected: Vec<(String, Vec<String>)> = ["FIRST", "SECOND", "THIRD", "FOURTH", "FIFTH"]
        .iter()
        .enumerate()
        .map(|(n, name)| (name.to_string(), vec![n.to_string()]))
        .collect();
    assert_eq!(
        counted, expected,
        "`counter!` is called once for each, in the order rustc expands them, its calls after the \
         first run ahead from the state the first left and taken only while that is the state"
    );
}

/// The workspace expanded with each ordered call run ahead of its turn, as a build does, and
/// with each run at its turn alone: every call gives the same tokens, byte for byte, the
/// stateful counter's included, and the panics are the same. Says the time each took.
#[test]
#[ignore = "needs BLAZE_MACRO_CORPUS: the fixture workspace built by blaze"]
fn runs_ahead_give_what_turns_give() {
    let text = |expanded: &Expanded| -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = expanded
            .records
            .iter()
            .map(|it| {
                let said = match &it.eval {
                    Outcome::Expanded(tree) => format!("expanded {tree}"),
                    Outcome::Panicked(message) => format!("panicked {message}"),
                    Outcome::Failed(why) => format!("failed {why}"),
                };
                (format!("{} at {:?}", it.what(), it.call.call_site), said)
            })
            .collect();
        out.sort();
        out
    };
    let count = Arc::default();
    let started = Instant::now();
    let ahead = expand_with(&Ahead::On(Arc::clone(&count))).unwrap_or_else(|why| panic!("{why}"));
    let ahead_took = started.elapsed();
    let started = Instant::now();
    let turns = expand_with(&Ahead::Off).unwrap_or_else(|why| panic!("{why}"));
    let turns_took = started.elapsed();
    let ran_ahead = count.load(Ordering::Relaxed);
    let busy = |expanded: &Expanded| {
        expanded.records.iter().map(|it| it.took).sum::<std::time::Duration>()
    };
    eprintln!(
        "{} calls; runs ahead: {ran_ahead}, wall {ahead_took:?}, the calls' turns {:?}; at their turns alone: \
         wall {turns_took:?}, the calls' turns {:?}",
        ahead.records.len(),
        busy(&ahead),
        busy(&turns),
    );
    assert!(ran_ahead > 0, "no call ran ahead of its turn");
    assert_eq!(
        text(&ahead),
        text(&turns),
        "a call run ahead of its turn gave other than at its turn"
    );
}

#[test]
#[ignore = "needs BLAZE_MACRO_CORPUS: the fixture workspace built by blaze"]
fn a_macro_reads_its_callers_file_and_variables() {
    let calls = expanded().calls;
    let data = record::literals(output(call_on("corpus_macros", "include_data", calls, "DATA")));
    assert!(
        data.iter().any(|it| it.contains("read through std::fs by the calling package")),
        "`include_data!` gave {data:?}, not calls/data.txt"
    );
    let package = output(call_on("corpus_macros", "env_name", calls, "PACKAGE"));
    assert_eq!(
        record::literals(package),
        ["\"corpus-calls\""],
        "`env_name!` reads the calling package's CARGO_PKG_NAME"
    );
    assert!(
        record::has_ident(package, "true"),
        "`env_name!` finds CORPUS_UNSET unset: `{package}`"
    );
}

#[test]
#[ignore = "needs BLAZE_MACRO_CORPUS: the fixture workspace built by blaze"]
fn every_macro_kind_is_called() {
    let expanded = expanded();
    for kind in [ProcMacroKind::CustomDerive, ProcMacroKind::Attr, ProcMacroKind::Bang] {
        assert!(
            expanded
                .records
                .iter()
                .any(|it| it.call.kind == kind && it.call.calling == Some(expanded.calls)),
            "corpus-calls makes no {kind:?} call"
        );
    }
}
