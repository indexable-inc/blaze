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

use std::sync::OnceLock;

use base_db::Crate;
use hir_def::nameres::{crate_def_map, diagnostics::DefDiagnosticKind};
use hir_expand::proc_macro::{ProcMacro, ProcMacroKind};

use config::Corpus;
use record::{Outcome, Record, Recorder};

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
    let config = Corpus::read()?;
    let records = record::Records::default();
    let workspace =
        load::lower(&config.workspace, &config.cargo_home, |krate, macros| Ok(wrap(krate, macros, &records)))?;
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
fn wrap(krate: &str, macros: Vec<ProcMacro>, records: &record::Records) -> Vec<ProcMacro> {
    macros.into_iter().map(|eval| Recorder::wrap(krate, eval, records)).collect()
}

/// The calls of macro `name` of crate `krate` from crate `calling`.
fn calls_of<'a>(krate: &'a str, name: &'a str, calling: Crate) -> impl Iterator<Item = &'static Record> + 'a {
    expanded()
        .records
        .iter()
        .filter(move |it| &*it.call.krate == krate && &*it.call.name == name && it.call.calling == Some(calling))
}

/// The one call of macro `name` of crate `krate` from crate `calling` whose subject (the item
/// a derive or an attribute is on, the first identifier a function-like macro is given) is
/// `subject`.
fn call_on(krate: &str, name: &str, calling: Crate, subject: &str) -> &'static Record {
    let found: Vec<&Record> = calls_of(krate, name, calling).filter(|it| it.call.subject().as_deref() == Some(subject)).collect();
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
    let select: Vec<&Record> = calls_of("tokio_macros", "select_priv_declare_output_enum", expanded.calls).collect();
    assert_eq!(select.len(), 1, "corpus-calls calls tokio's `select_priv_declare_output_enum!` once");
    output(select[0]);
}

#[test]
#[ignore = "needs BLAZE_MACRO_CORPUS: the fixture workspace built by blaze"]
fn a_panic_is_its_message() {
    let panics = expanded().panics;
    let mut boom: Vec<&Record> = calls_of("corpus_macros", "boom", panics).collect();
    boom.sort_by_key(|it| it.call.subtree.as_token_trees().iter_flat_tokens().count());
    let tokio: Vec<&Record> = calls_of("tokio_macros", "select_priv_declare_output_enum", panics).collect();
    let expected = [(boom.first(), "boom: 0 tokens"), (boom.last(), "boom: 3 tokens"), (tokio.first(), "unexpected macro input")];
    assert_eq!((boom.len(), tokio.len()), (2, 1), "corpus-panics makes two calls of `boom` and one of tokio's");
    for (record, message) in expected {
        let record = record.expect("counted above");
        match &record.eval {
            Outcome::Panicked(said) => assert_eq!(said, message, "{}", record.what()),
            Outcome::Expanded(tree) => panic!("{} expanded to `{tree}`, and the macro panics with {message:?}", record.what()),
            Outcome::Failed(why) => panic!("{} failed ({why}), and the macro panics with {message:?}", record.what()),
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
    let expected = [("FIRST".to_owned(), vec!["0".to_owned()]), ("SECOND".to_owned(), vec!["1".to_owned()])];
    assert_eq!(counted, expected, "`counter!` is called once for each, in the order rustc expands them");
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
    assert_eq!(record::literals(package), ["\"corpus-calls\""], "`env_name!` reads the calling package's CARGO_PKG_NAME");
    assert!(record::has_ident(package, "true"), "`env_name!` finds CORPUS_UNSET unset: `{package}`");
}

#[test]
#[ignore = "needs BLAZE_MACRO_CORPUS: the fixture workspace built by blaze"]
fn every_macro_kind_is_called() {
    let expanded = expanded();
    for kind in [ProcMacroKind::CustomDerive, ProcMacroKind::Attr, ProcMacroKind::Bang] {
        assert!(
            expanded.records.iter().any(|it| it.call.kind == kind && it.call.calling == Some(expanded.calls)),
            "corpus-calls makes no {kind:?} call"
        );
    }
}
