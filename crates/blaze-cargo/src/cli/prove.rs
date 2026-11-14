//! `blaze prove [cargo's package and target flags] [--emit] [--subset [--functions]] [--json]`
//! ([`Request::Prove`], docs/cargo.md, docs/v2/plan/design/proofs-for-sans-io.md section 5):
//! every function marked `#[blaze::theorem]` in the library crates `blaze check` with the same
//! flags selects, translated to Lean in the daemon's front end and checked by the pinned Lean
//! (`BLAZE_LEAN_PROVER`) against the crate's proof file under `lean/`. Nothing is compiled.
//! `--emit` writes the models and statements and checks nothing, so needs no Lean; `--subset`
//! adds what the translator takes of every function in a `#![blaze::sans_io]` module: the
//! counts (translated, generic, refused) and the refusals grouped, and with `--functions` one
//! line a function naming its outcome; `--json` always carries every function's outcome.
//!
//! Exit status: 0 when every theorem selected is proved (with `--emit`, emitted), or there is
//! none; 1 otherwise; a command line it refuses is [`Exit::Usage`].

use std::{alloc::Global, ffi::OsString, io::Write as _, path::Path};

use blaze_api::{ProofRecord, Proven, Request, ResponseRef, Subset, SubsetOutcome};
use blaze_core::exit::Exit;

use super::{
    args::{self, Asked},
    daemon::Start,
    observe, say,
};

/// The variable Lean's path is read from: here, where the command's environment is.
const LEAN_VARIABLE: &str = "BLAZE_LEAN_PROVER";

const USAGE: &str =
    "blaze prove [-p PKG]... [--workspace] [cargo's target flags] [--emit] [--subset [--functions]] [--json]";

/// `blaze prove`: its exit code.
pub fn main(words: Vec<OsString>, color: bool) -> i32 {
    match prove(words, color) {
        Ok(code) => code,
        Err((what, code)) => {
            blaze_report::err!("{}", say::error(&what, color));
            code
        }
    }
}

fn prove(words: Vec<OsString>, color: bool) -> Result<i32, (String, i32)> {
    let usage = |what: String| (format!("{USAGE}: {what}"), Exit::Usage.code());
    let failed = |what: String| (what, Exit::Failed.code());
    let (mut emit, mut subset, mut functions, mut json) = (false, false, false, false);
    // The words of the `blaze check` whose targets are proved: `check` and the cargo flags.
    let mut check = vec![OsString::from("check")];
    for word in words {
        match word.to_str() {
            Some("--emit") => emit = true,
            Some("--subset") => subset = true,
            Some("--functions") => functions = true,
            Some("--json") => json = true,
            _ => check.push(word),
        }
    }
    // `--functions` lists the subset's functions: without `--subset` there is none to list, and
    // the flag is refused rather than ignored.
    if functions && !subset {
        return Err(usage("`--functions` lists the functions `--subset` counts; it needs `--subset`".to_owned()));
    }
    let asked = match args::parse(check) {
        Ok(Asked::Cargo(asked)) => asked,
        Ok(_) => return Err(failed("`blaze prove` could not read itself as `blaze check`".to_owned())),
        Err(refused) => return Err(usage(args::refusal(&refused, false))),
    };
    // Read here, where the command's environment is: the daemon may have been started by
    // another, and is given the path in the request.
    let lean = match emit {
        true => None,
        false => match std::env::var(LEAN_VARIABLE) {
            Ok(lean) if Path::new(&lean).is_absolute() => Some(lean),
            Ok(lean) => {
                return Err(failed(format!(
                    "`{LEAN_VARIABLE}` is `{lean}`, which is not an absolute path: it names the \
                     `lean` of Lean {}, the release `lean/lean-toolchain` pins",
                    hir_ty_lean::LEAN_VERSION
                )));
            }
            Err(_) => {
                return Err(failed(format!(
                    "`{LEAN_VARIABLE}` is not set (or is not text): `blaze prove` checks proofs \
                     with Lean {}, the `lean` executable of the release `lean/lean-toolchain` \
                     pins, and looks for it nowhere else (`--emit` writes the models and \
                     statements with no Lean)",
                    hir_ty_lean::LEAN_VERSION
                )));
            }
        },
    };
    let (_, cargo) = super::prepared_cargo(&asked, color).map_err(failed)?;
    let Request::Check { view, targets } = super::request(&asked, cargo).map_err(failed)? else {
        return Err(failed("`blaze prove` made no check's targets of `blaze check`".to_owned()));
    };
    crate::toolchain::ensure().map_err(failed)?;
    let mut proven = None;
    observe::ask(Request::Prove { emit, lean, view, subset, targets }, Start::IfMissing, |response| {
        if let ResponseRef::Proven(it) = response {
            proven = Some(it);
        }
        Ok(())
    })
    .map_err(failed)?;
    let proven =
        proven.ok_or_else(|| failed("the daemon answered `blaze prove` with no answer".to_owned()))?;
    let mut text = Vec::new_in(Global);
    match json {
        true => {
            let _ = writeln!(text, "{}", self::json(&proven));
        }
        false => lines(&mut text, &proven, emit, functions),
    }
    blaze_report::out!("{}", String::from_utf8_lossy(&text));
    // No proof could be checked: the command's failure, after what it did find.
    if let Some(why) = &proven.failed {
        // clone: the error's text, once
        return Err(failed(why.clone()));
    }
    let done = proven.records.iter().all(|it| match it {
        ProofRecord::Proved { .. } => true,
        ProofRecord::Emitted { .. } => emit,
        ProofRecord::Open { .. } | ProofRecord::Refused { .. } => false,
    });
    Ok(if done { 0 } else { 1 })
}

/// One line a record, Lean's message under an open one indented, the subset's counts and
/// refusals (with `functions`, a line a function of it), then the counts, written to `text`.
fn lines(text: &mut Vec<u8>, proven: &Proven, emit: bool, functions: bool) {
    if proven.records.is_empty() {
        let _ = writeln!(text, "no function marked `#[blaze::theorem]` in the crates selected");
    }
    let (mut proved, mut open, mut emitted, mut refused) = (0, 0, 0, 0);
    for record in &proven.records {
        match record {
            ProofRecord::Proved { theorem, function, axioms, .. } => {
                proved += 1;
                let _ = writeln!(text, "proved {function}: {theorem} (axioms: {})", axioms.join(", "));
            }
            ProofRecord::Open { why, function, proof_file, .. } => {
                open += 1;
                let _ = writeln!(text, "open {function}: {proof_file}");
                for line in why.lines() {
                    let _ = writeln!(text, "    {line}");
                }
            }
            ProofRecord::Emitted { path, function, statement, .. } => {
                emitted += 1;
                let _ = writeln!(text, "emitted {function}: {statement} ({path})");
            }
            ProofRecord::Refused { at, what, function } => {
                refused += 1;
                let _ = writeln!(text, "refused {function}: {what} at `{at}`");
            }
        }
    }
    if let Some(subset) = &proven.subset {
        subset_lines(text, subset, functions);
    }
    let _ = match emit {
        true => writeln!(text, "{emitted} emitted, {refused} refused"),
        false => writeln!(text, "{proved} proved, {open} open, {refused} refused"),
    };
}

/// The subset's counts, its refusals most first, and with `functions` one line a function by
/// path, written to `text`.
fn subset_lines(text: &mut Vec<u8>, subset: &Subset, functions: bool) {
    let refused: u32 = subset.refused.iter().map(|it| it.count).sum();
    let _ = writeln!(
        text,
        "subset: {} functions in sans-IO modules, {} translated, {} generic (translated by \
         instance), {refused} refused",
        subset.functions, subset.translated, subset.generic
    );
    for it in &subset.refused {
        let _ = writeln!(text, "    {} refused for {}", it.count, it.what);
    }
    if !functions {
        return;
    }
    for it in &subset.outcomes {
        let _ = match &it.outcome {
            SubsetOutcome::Translated => writeln!(text, "translated {}", it.path),
            SubsetOutcome::Generic => writeln!(text, "generic {}", it.path),
            SubsetOutcome::Refused { what } => writeln!(text, "refused {}: {what}", it.path),
        };
    }
}

/// The records as JSON with their own field names, each `{"outcome": .., ..}`, the key in hex.
fn json(proven: &Proven) -> serde_json::Value {
    let mut records: Vec<serde_json::Value> = Vec::with_capacity_in(proven.records.len(), Global);
    for record in &proven.records {
        records.push(match record {
            ProofRecord::Proved { key, model, axioms, theorem, function } => serde_json::json!({
                "outcome": "Proved", "function": function, "theorem": theorem,
                "key": format!("{key:x}"), "axioms": axioms, "model": model,
            }),
            ProofRecord::Open { why, function, statement, proof_file } => serde_json::json!({
                "outcome": "Open", "function": function, "statement": statement,
                "proof_file": proof_file, "why": why,
            }),
            ProofRecord::Emitted { path, module, function, statement } => serde_json::json!({
                "outcome": "Emitted", "function": function, "module": module,
                "path": path, "statement": statement,
            }),
            ProofRecord::Refused { at, what, function } => serde_json::json!({
                "outcome": "Refused", "function": function, "what": what, "at": at,
            }),
        });
    }
    let subset = proven.subset.as_ref().map(|subset| {
        let mut outcomes: Vec<serde_json::Value> = Vec::with_capacity_in(subset.outcomes.len(), Global);
        for it in &subset.outcomes {
            outcomes.push(match &it.outcome {
                SubsetOutcome::Translated => serde_json::json!({ "path": it.path, "outcome": "Translated" }),
                SubsetOutcome::Generic => serde_json::json!({ "path": it.path, "outcome": "Generic" }),
                SubsetOutcome::Refused { what } => {
                    serde_json::json!({ "path": it.path, "outcome": "Refused", "what": what })
                }
            });
        }
        serde_json::json!({
            "functions": subset.functions, "translated": subset.translated,
            "generic": subset.generic, "refused": subset.refused, "outcomes": outcomes,
        })
    });
    serde_json::json!({ "records": records, "subset": subset, "failed": proven.failed })
}

#[cfg(test)]
mod tests {
    use std::alloc::Global;

    use blaze_api::{Proven, Subset, SubsetFunction, SubsetOutcome, SubsetRefusal};

    /// `--subset` prints the counts and the grouped refusals; `--functions` adds a line a
    /// function, by path, naming its outcome.
    #[test]
    fn the_subset_says_each_function_s_outcome_with_functions() {
        let function = |path: &str, outcome| SubsetFunction { path: path.to_owned(), outcome };
        let raw = "a raw pointer (rule RawPointer)";
        let proven = Proven {
            failed: None,
            subset: Some(Subset {
                refused: vec![SubsetRefusal { what: raw.to_owned(), count: 1 }],
                outcomes: vec![
                    function("cores::addr", SubsetOutcome::Refused { what: raw.to_owned() }),
                    function("cores::double", SubsetOutcome::Translated),
                    function("cores::first", SubsetOutcome::Generic),
                ],
                functions: 3,
                translated: 1,
                generic: 1,
            }),
            records: Vec::new(),
        };
        let counts = "no function marked `#[blaze::theorem]` in the crates selected\n\
                      subset: 3 functions in sans-IO modules, 1 translated, 1 generic (translated \
                      by instance), 1 refused\n    \
                      1 refused for a raw pointer (rule RawPointer)\n";
        let mut text = Vec::new_in(Global);
        super::lines(&mut text, &proven, false, false);
        assert_eq!(String::from_utf8(text).unwrap(), format!("{counts}0 proved, 0 open, 0 refused\n"));
        let mut text = Vec::new_in(Global);
        super::lines(&mut text, &proven, false, true);
        assert_eq!(
            String::from_utf8(text).unwrap(),
            format!(
                "{counts}refused cores::addr: a raw pointer (rule RawPointer)\ntranslated cores::double\n\
                 generic cores::first\n0 proved, 0 open, 0 refused\n"
            )
        );
        let json = super::json(&proven);
        assert_eq!(json["subset"]["outcomes"][0]["outcome"], "Refused", "{json}");
        assert_eq!(json["subset"]["outcomes"][2]["path"], "cores::first", "{json}");
        assert_eq!(json["subset"]["generic"], 1, "{json}");
    }
}
