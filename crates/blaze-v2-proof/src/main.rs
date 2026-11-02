//! `blaze-v2-proof '{"incremental": "a.cbor", "scratch": "b.cbor", "before": "p.cbor"}'`: the
//! oracle's comparison of two traces (`blaze_v2_proof::wire`'s files), read in place and joined
//! across cores (`blaze_v2_proof::join`), as JSON on stdout. Its
//! one argument is a JSON value inline, the path of a file holding one, or `-` for stdin;
//! `before`, the trace of the build before the incremental one, adds what it reran for
//! nothing. Exits 1 when anything is stale, 2 when it could not compare. With `"slot"`, the
//! fixture slot the edit moved, and `before`, the slot's case in [`blaze_v2_proof::nameres`] is
//! checked too; a finding over or under exits 1 as well.
//!
//! `blaze-v2-proof '{"dbsp": "impl", "traits": 1, "buckets": 1, "before": "p.cbor", "after": "a.cbor"}'`
//! holds the traces of the builds before and after one edit of `harness/v2/dbsp/edits.py` to
//! the edit kind's case in [`blaze_v2_proof::dbsp`], given how many traits and buckets the edit
//! touched, and prints `{"edit", "claim", "findings": [{"side", "detail"}]}`. A finding over
//! exits 1; an unmeasured one does not.
//!
//! `blaze-v2-proof '{"first": "a", "second": "b"}'` compares two files byte for byte instead
//! (`blaze_v2_proof::bytes`, by the CPU's SIMD) and prints where they first differ.
//!
//! `blaze-v2-proof '{"tree": "src", "skip": ["target"]}'` prints `{"digest": HEX}`, the tree's
//! digest (`blaze_v2_proof::tree`) that `harness/v2/engine.py`'s `tree_digest` compares.
//!
//! `blaze-v2-proof '{"bench": "join"}'` times one of the reference workloads in `bench.rs` and
//! prints `{"value": MS}`, for the bounds `harness/v2/bounds.py` reads.
#![forbid(unsafe_code)]

mod bench;

use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::ExitCode,
};

use blaze_query::QueryKey;
use blaze_v2_proof::{
    Stale,
    bytes::Mapped,
    dbsp, join,
    nameres::{self, Finding, Side},
    wire::View,
};
use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
#[serde(untagged)]
enum Given {
    Traces(Input),
    Dbsp(DbspInput),
    Files(Files),
    Tree(Tree),
    Bench(Bench),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Tree {
    tree: PathBuf,
    /// Directories at the tree's top left out (build outputs).
    #[serde(default)]
    skip: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Bench {
    bench: bench::Workload,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Files {
    first: PathBuf,
    second: PathBuf,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    incremental: PathBuf,
    scratch: PathBuf,
    #[serde(default)]
    before: Option<PathBuf>,
    /// The fixture slot the edit moved; its `nameres` case, if it has one, is checked.
    #[serde(default)]
    slot: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DbspInput {
    /// The edit kind, a case of [`dbsp::CASES`].
    dbsp: String,
    traits: u32,
    buckets: u32,
    before: PathBuf,
    after: PathBuf,
}

#[derive(Serialize)]
struct DbspOutput {
    edit: &'static str,
    claim: &'static str,
    findings: Vec<NameresFound>,
}

#[derive(Serialize)]
struct Found {
    what: &'static str,
    query: String,
    culprit: bool,
    detail: String,
}

#[derive(Serialize)]
struct Output {
    stale: Vec<Found>,
    culprits: usize,
    work: Option<blaze_v2_proof::Work>,
    needless: Vec<String>,
    nameres: Option<Nameres>,
}

#[derive(Serialize)]
struct Nameres {
    slot: &'static str,
    claim: &'static str,
    findings: Vec<NameresFound>,
}

#[derive(Serialize)]
struct NameresFound {
    side: Side,
    detail: String,
}

fn nameres_found(finding: &Finding) -> NameresFound {
    // alloc: each arm is the CLI's output, written once a finding.
    let detail = match finding {
        Finding::Changed { what, changed, min, max, keys, .. } => {
            // alloc: as above.
            let keys: Vec<String> = keys.iter().take(8).map(name).collect();
            // alloc: as above.
            format!("{changed} {what} changed, the facts allow {min} to {max}: {}", keys.join(", "))
        }
        // alloc: as above.
        Finding::Unrecorded { what, kind } => format!("neither trace has {what} (kind {:#06x})", kind.0),
        Finding::Kept { query, read, saw, now } => {
            // alloc: as above.
            format!("{} was kept though it read {} at {saw:?}, which is now {now:?}", name(query), name(read))
        }
        // alloc: as above.
        Finding::Unexplained { query } => format!("{} reran though nothing it read changed", name(query)),
        // alloc: as above.
        Finding::Needless { query } => format!("{} reran, read and made what it did before", name(query)),
        Finding::Stale(stale) => {
            let f = found(stale);
            // alloc: as above.
            format!("{} {}: {}", f.what, f.query, f.detail)
        }
    };
    NameresFound { side: finding.side(), detail }
}

fn dbsp_found(finding: &dbsp::Finding) -> NameresFound {
    // alloc: each arm is the CLI's output, written once a finding.
    let detail = match finding {
        dbsp::Finding::Reran { what, reran, most, keys, .. } => {
            // alloc: as above.
            let keys: Vec<String> = keys.iter().take(8).map(name).collect();
            // alloc: as above.
            format!("{reran} {what} reran, the edit touched {most}: {}", keys.join(", "))
        }
        // alloc: as above.
        dbsp::Finding::Unrecorded { what, kind } => format!("neither trace has {what} (kind {:#06x})", kind.0),
        dbsp::Finding::Coarse { query, read } => {
            // alloc: as above.
            format!("{} read {}, a whole crate's table, so it reruns with every impl of it", name(query), name(read))
        }
        // alloc: as above.
        dbsp::Finding::Unexplained { query } => format!("{} reran though nothing it read changed", name(query)),
        // alloc: as above.
        dbsp::Finding::Needless { query } => format!("{} reran, read and made what it did before", name(query)),
    };
    NameresFound { side: finding.side(), detail }
}

fn check_dbsp(input: &DbspInput) -> Result<DbspOutput, String> {
    let Some(case) = dbsp::case(&input.dbsp) else {
        // alloc: error path; the CLI exits 2.
        let cases: Vec<&str> = dbsp::CASES.iter().map(|c| c.edit).collect();
        // alloc: as above.
        return Err(format!("no `dbsp` case `{}`; the cases are {}", input.dbsp, cases.join(", ")));
    };
    let (before, after) = (mapped(&input.before)?, mapped(&input.after)?);
    let (before, after) = (view(&input.before, &before)?, view(&input.after, &after)?);
    let touched = dbsp::Touched { traits: input.traits, buckets: input.buckets };
    // alloc: the CLI's output, once a run.
    let findings = dbsp::check(case, touched, &before, &after).iter().map(dbsp_found).collect();
    Ok(DbspOutput { edit: case.edit, claim: case.claim, findings })
}

/// `kkkk:` and the first 8 bytes of the input in hex, written into one 21-byte string.
fn name(key: &QueryKey) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(21);
    for shift in [12, 8, 4, 0] {
        out.push(HEX[usize::from(key.kind.0 >> shift) & 15] as char);
    }
    out.push(':');
    for &b in &key.input.0[..8] {
        out.push(HEX[usize::from(b >> 4)] as char);
        out.push(HEX[usize::from(b & 15)] as char);
    }
    out
}

fn found(stale: &Stale) -> Found {
    // alloc: each arm is the CLI's output, written once a stale result.
    let (what, detail) = match stale {
        Stale::Unverified { read, saw, now, .. } => (
            "unverified",
            // alloc: as above.
            format!("kept though it read {} at {saw:?}, which is now {now:?}", name(read)),
        ),
        Stale::UnrecordedRead { incremental, scratch, .. } => (
            "unrecorded-read",
            // alloc: as above.
            format!("read the same as from scratch; {incremental:?} against {scratch:?}"),
        ),
        // alloc: as above.
        Stale::Absent { .. } => ("absent", "the build from scratch has it, the incremental build not".to_owned()),
        Stale::Downstream { incremental, scratch, .. } => {
            // alloc: as above.
            ("downstream", format!("read stale results; {incremental:?} against {scratch:?}"))
        }
    };
    Found { what, query: name(&stale.query()), culprit: stale.is_culprit(), detail }
}

fn read_input(arg: &str) -> Result<Given, String> {
    let text = if arg == "-" {
        // alloc: the CLI's input, read once a run.
        let mut text = String::new();
        // alloc: error path; the CLI exits 2.
        std::io::stdin().read_to_string(&mut text).map_err(|e| format!("stdin: {e}"))?;
        text
    } else if arg.trim_start().starts_with('{') {
        // alloc: the CLI's input, once a run.
        arg.to_owned()
    } else {
        // alloc: the CLI's input, once a run, or the error that exits 2.
        fs::read_to_string(arg).map_err(|e| format!("{arg}: {e}"))?
    };
    serde_json::from_str(&text).map_err(|e| {
        // alloc: error path; the CLI exits 2.
        format!(
            "the input: {{\"incremental\", \"scratch\", \"before\"?, \"slot\"?}} or {{\"dbsp\", \"traits\", \
             \"buckets\", \"before\", \"after\"}} or {{\"first\", \"second\"}} or {{\"tree\", \"skip\"?}}: {e}"
        )
    })
}

/// A trace's file mapped with no page faulted in: the joins across cores fault in their own.
fn mapped(path: &Path) -> Result<Mapped, String> {
    // alloc: error path; the CLI exits 2.
    Mapped::open_lazily(path).map_err(|e| format!("{}: {e}", path.display()))
}

fn view<'a>(path: &Path, mapped: &'a Mapped) -> Result<View<'a>, String> {
    // alloc: error path; the CLI exits 2.
    View::parse(mapped.bytes()).map_err(|e| format!("{}: {e}", path.display()))
}

fn compare(input: &Input) -> Result<Output, String> {
    let (incremental, scratch) = (mapped(&input.incremental)?, mapped(&input.scratch)?);
    let before = input.before.as_deref().map(mapped).transpose()?;
    let incremental = view(&input.incremental, &incremental)?;
    let scratch = view(&input.scratch, &scratch)?;
    let before = match (input.before.as_deref(), &before) {
        (Some(path), Some(file)) => Some(view(path, file)?),
        _ => None,
    };
    // alloc: the CLI's output, once a run.
    let stale: Vec<Found> = join::under(&incremental, &scratch, &[]).iter().map(found).collect();
    let (work, needless) = match &before {
        Some(before) => {
            let (work, needless) = join::over(before, &incremental);
            // alloc: as above.
            (Some(work), needless.iter().map(name).collect())
        }
        // alloc: none: an empty `Vec`.
        None => (None, Vec::new()),
    };
    let nameres = match (input.slot.as_deref().and_then(nameres::case), &before) {
        (Some(case), Some(before)) => Some(Nameres {
            slot: case.slot,
            claim: case.claim,
            findings: nameres::check(case, &before.to_trace(), &incremental.to_trace(), &scratch.to_trace())
                .iter()
                .map(nameres_found)
                // alloc: the CLI's output, once a run with a fixture slot.
                .collect(),
        }),
        (Some(case), None) => {
            // alloc: error path; the CLI exits 2.
            return Err(format!("the `{}` case needs `before`, the trace of the build before the edit", case.slot));
        }
        (None, _) => None,
    };
    let culprits = stale.iter().filter(|f| f.culprit).count();
    Ok(Output { stale, culprits, work, needless, nameres })
}

fn compare_files(files: &Files) -> ExitCode {
    match blaze_v2_proof::bytes::first_file_difference(&files.first, &files.second) {
        Ok(at) => {
            // alloc: the CLI's output, once a run.
            let simd = format!("{:?}", blaze_v2_proof::bytes::fastest());
            println!("{}", serde_json::json!({ "differs_at": at, "simd": simd }));
            if at.is_some() { ExitCode::from(1) } else { ExitCode::SUCCESS }
        }
        Err(e) => {
            eprintln!("blaze-v2-proof: {} against {}: {e}", files.first.display(), files.second.display());
            ExitCode::from(2)
        }
    }
}

fn tree_digest(tree: &Tree) -> ExitCode {
    match blaze_v2_proof::tree::digest(&tree.tree, &tree.skip) {
        Ok(hash) => {
            // alloc: the CLI's output, once a run.
            let mut digest = Vec::with_capacity(64);
            blaze_v2_proof::tree::hex(&hash, &mut digest);
            let digest = std::str::from_utf8(&digest).expect("hex digits are ASCII");
            println!("{}", serde_json::json!({ "digest": digest }));
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("blaze-v2-proof: the tree {}: {e}", tree.tree.display());
            ExitCode::from(2)
        }
    }
}

fn main() -> ExitCode {
    // alloc: the CLI's arguments, once a run.
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [arg] = args.as_slice() else {
        eprintln!(
            "usage: blaze-v2-proof JSON (inline, a file's path, or -): {{\"incremental\", \"scratch\", \"before\"?, \"slot\"?}} \
             or {{\"dbsp\", \"traits\", \"buckets\", \"before\", \"after\"}} or {{\"first\", \"second\"}} \
             or {{\"tree\", \"skip\"?}} or {{\"bench\": \"first-difference\" or \"join\"}}"
        );
        return ExitCode::from(2);
    };
    let given = match read_input(arg) {
        Ok(given) => given,
        Err(e) => {
            eprintln!("blaze-v2-proof: {e}");
            return ExitCode::from(2);
        }
    };
    let input = match given {
        Given::Traces(input) => input,
        Given::Dbsp(input) => {
            return match check_dbsp(&input) {
                Ok(output) => {
                    let over = output.findings.iter().any(|f| f.side != Side::Unmeasured);
                    println!("{}", serde_json::to_string_pretty(&output).expect("the output is JSON"));
                    if over { ExitCode::from(1) } else { ExitCode::SUCCESS }
                }
                Err(e) => {
                    eprintln!("blaze-v2-proof: {e}");
                    ExitCode::from(2)
                }
            };
        }
        Given::Files(files) => return compare_files(&files),
        Given::Tree(tree) => return tree_digest(&tree),
        Given::Bench(b) => {
            println!("{}", serde_json::json!({ "value": bench::run(b.bench) }));
            return ExitCode::SUCCESS;
        }
    };
    match compare(&input) {
        Ok(output) => {
            let wrong = output.nameres.as_ref().is_some_and(|n| n.findings.iter().any(|f| f.side != Side::Unmeasured));
            let stale = !output.stale.is_empty() || wrong;
            println!("{}", serde_json::to_string_pretty(&output).expect("the output is JSON"));
            if stale { ExitCode::from(1) } else { ExitCode::SUCCESS }
        }
        Err(e) => {
            eprintln!("blaze-v2-proof: {e}");
            ExitCode::from(2)
        }
    }
}
