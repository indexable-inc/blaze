//! `blaze perf`: the performance history of the programs blaze builds
//! (docs/v2/plan/design/perf-history.md).
//!
//! - `blaze perf record [--target-dir DIR] [--workspace DIR] FILE.ixprof...`: the repetitions
//!   of one run, read through the code maps the links kept in `<target-dir>/perf`, as one row
//!   set in the profile history.
//! - `blaze perf runs [--json]`: the runs recorded, oldest first.
//! - `blaze perf diff [RUN [BASE]] [--json] [--all]`: RUN (the latest when not given) against
//!   BASE (the run of the same program before it when not given), function by function, with
//!   each change's noise bound and cause class.

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};

use blaze_core::exit::Exit;
use blaze_profile::{
    history,
    perf::{
        diff,
        record::{self, Inputs},
        rows,
    },
};

use super::say;

const PERF_USAGE: &str = "blaze perf record [--target-dir DIR] [--workspace DIR] FILE.ixprof... \
                          | blaze perf runs [--json] | blaze perf diff [RUN [BASE]] [--json] [--all]";

type Refused = (i32, String);

pub fn main(words: Vec<OsString>, color: bool) -> i32 {
    match run(words) {
        Ok(()) => 0,
        Err((code, what)) => {
            eprint!("{}", say::error(&what, color));
            code
        }
    }
}

fn usage(what: impl Into<String>) -> Refused {
    (Exit::Usage.code(), format!("{}\nusage: {PERF_USAGE}", what.into()))
}

fn failed(what: impl Into<String>) -> Refused {
    (Exit::Failed.code(), what.into())
}

fn run(words: Vec<OsString>) -> Result<(), Refused> {
    let words: Vec<String> = words
        .into_iter()
        .map(|it| {
            it.into_string().map_err(|it| usage(format!("{} is not UTF-8", it.to_string_lossy())))
        })
        .collect::<Result<_, _>>()?;
    let Some((verb, rest)) = words.split_first() else {
        return Err(usage("`blaze perf` takes record, runs or diff"));
    };
    match verb.as_str() {
        "record" => record(rest),
        "runs" => runs(rest),
        "diff" => compare(rest),
        other => Err(usage(format!("`blaze perf {other}`: give record, runs or diff"))),
    }
}

/// The profile history's directory, made with its tables' schema files.
fn history_dir() -> Result<PathBuf, Refused> {
    let state = crate::serve::state().map_err(|error| failed(format!("{error:#}")))?;
    let dir = history::dir_in(&state);
    std::fs::create_dir_all(&dir).map_err(|error| failed(format!("{}: {error}", dir.display())))?;
    history::ensure_schemas(&dir).map_err(|error| failed(error.to_string()))?;
    Ok(dir)
}

/// The nearest directory at or above `from` with a `Cargo.lock`, else with a `Cargo.toml`.
fn workspace_of(from: &Path) -> Option<PathBuf> {
    for marker in ["Cargo.lock", "Cargo.toml"] {
        if let Some(found) = from.ancestors().find(|it| it.join(marker).is_file()) {
            return Some(found.to_owned());
        }
    }
    None
}

/// The machine's name, as the `hostname` program prints it: std has no safe call for it, and
/// this crate allows no `unsafe`.
fn host() -> Result<String, String> {
    let out = std::process::Command::new("hostname")
        .output()
        .map_err(|error| format!("running `hostname` to name this machine: {error}"))?;
    if !out.status.success() {
        return Err(format!(
            "`hostname` exited with {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let name = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    if name.is_empty() {
        return Err("`hostname` printed no name".to_owned());
    }
    Ok(name)
}

fn record(words: &[String]) -> Result<(), Refused> {
    let (mut target_dir, mut workspace) = (None, None);
    let mut files = Vec::new();
    let mut at = 0;
    while at < words.len() {
        match words[at].as_str() {
            flag @ ("--target-dir" | "--workspace") => {
                let value = words
                    .get(at + 1)
                    .ok_or_else(|| usage(format!("`{flag}` takes a directory")))?;
                let slot = if flag == "--target-dir" { &mut target_dir } else { &mut workspace };
                *slot = Some(PathBuf::from(value));
                at += 2;
            }
            other if other.starts_with("--") => {
                return Err(usage(format!("`blaze perf record` has no {other}")));
            }
            file => {
                files.push(PathBuf::from(file));
                at += 1;
            }
        }
    }
    if files.is_empty() {
        return Err(usage(
            "`blaze perf record` takes the .ixprof file of each repetition of one run",
        ));
    }
    let cwd = std::env::current_dir()
        .map_err(|error| failed(format!("the current directory: {error}")))?;
    let workspace = workspace.or_else(|| workspace_of(&cwd));
    let target_dir = match (target_dir, std::env::var_os("CARGO_TARGET_DIR"), &workspace) {
        (Some(it), _, _) => it,
        (None, Some(it), _) => PathBuf::from(it),
        (None, None, Some(workspace)) => workspace.join("target"),
        (None, None, None) => {
            return Err(usage(format!(
                "no workspace at or above {}, so no target directory: give --target-dir",
                cwd.display()
            )));
        }
    };
    let tree = match &workspace {
        Some(workspace) => blaze_profile::identity::tree_digest(workspace)
            .map_err(|error| format!("the digest of {}: {error}", workspace.display())),
        None => Err(format!("no workspace at or above {}", cwd.display())),
    };
    let blaze = blaze_profile::identity::binary()
        .map_err(|error| failed(format!("blaze's own executable: {error}")))?;
    let host = host().map_err(failed)?;
    let perf = target_dir.join("perf");
    let mut reps = Vec::with_capacity(files.len());
    for file in &files {
        reps.push(
            std::fs::read(file).map_err(|error| failed(format!("{}: {error}", file.display())))?,
        );
    }
    let (row, functions) = record::record(&reps, &Inputs { perf: perf.clone(), tree, blaze, host })
        .map_err(|why| failed(format!("recording {}: {why}", files[0].display())))?;
    let dir = history_dir()?;
    rows::write(&dir, &row, &functions).map_err(failed)?;
    println!(
        "recorded run {}: {}, {} rep{}, {} samples ({} in code no map names), {} function rows",
        row.run,
        row.target,
        row.reps,
        if row.reps == 1 { "" } else { "s" },
        row.samples,
        row.unmapped,
        functions.len()
    );
    for gap in row.gaps.iter().flat_map(|it| it.split("; ")) {
        println!("  gap: {gap}");
    }
    if !perf.is_dir() {
        println!(
            "  {} does not exist: links write code maps only while it does; make it and build again to name functions \
             by identity",
            perf.display()
        );
    }
    Ok(())
}

fn runs(words: &[String]) -> Result<(), Refused> {
    let json = match words {
        [] => false,
        [it] if it == "--json" => true,
        _ => return Err(usage("`blaze perf runs` takes only --json")),
    };
    let runs = rows::runs(&history_dir()?).map_err(failed)?;
    if json {
        let text =
            serde_json::to_string_pretty(&runs).map_err(|error| failed(error.to_string()))?;
        println!("{text}");
        return Ok(());
    }
    if runs.is_empty() {
        println!("no runs recorded: `blaze perf record FILE.ixprof...` records one");
        return Ok(());
    }
    println!(
        "{:<16}  {:>19}  {:>4}  {:>9}  {:<12}  target",
        "run", "started (unix s)", "reps", "samples", "tree"
    );
    for it in &runs {
        println!(
            "{:<16}  {:>19}  {:>4}  {:>9}  {:<12}  {}{}",
            it.run,
            it.started_unix_ns / 1_000_000_000,
            it.reps,
            it.samples,
            it.tree.as_deref().map_or("unknown", |it| &it[..it.len().min(12)]),
            it.target,
            if it.gaps.is_some() { "  (gaps: `--json` lists them)" } else { "" }
        );
    }
    Ok(())
}

fn compare(words: &[String]) -> Result<(), Refused> {
    let (mut json, mut all) = (false, false);
    let mut named = Vec::new();
    for word in words {
        match word.as_str() {
            "--json" => json = true,
            "--all" => all = true,
            other if other.starts_with("--") => {
                return Err(usage(format!("`blaze perf diff` has no {other}")));
            }
            other => named.push(other),
        }
    }
    if named.len() > 2 {
        return Err(usage("`blaze perf diff` takes a run and a base"));
    }
    let dir = history_dir()?;
    let runs = rows::runs(&dir).map_err(failed)?;
    let run = match named.first() {
        Some(name) => diff::find(&runs, name).map_err(failed)?,
        None => runs.last().ok_or_else(|| {
            failed("no runs recorded: `blaze perf record FILE.ixprof...` records one")
        })?,
    };
    let base = match named.get(1) {
        Some(name) => diff::find(&runs, name).map_err(failed)?,
        None => diff::default_base(&runs, run).ok_or_else(|| {
            failed(format!(
                "run {} is the first of {} recorded: name a base run to compare it with",
                run.run, run.target
            ))
        })?,
    };
    let run_rows = rows::functions(&dir, &run.run).map_err(failed)?;
    let base_rows = rows::functions(&dir, &base.run).map_err(failed)?;
    let diff = diff::compare((run, &run_rows), (base, &base_rows)).map_err(failed)?;
    if json {
        let text =
            serde_json::to_string_pretty(&diff).map_err(|error| failed(error.to_string()))?;
        println!("{text}");
    } else {
        print!("{}", diff::render(&diff, all));
    }
    Ok(())
}
