//! `blaze cache gc [--dry-run]`: one run of the collector of what blaze keeps on disk
//! (docs/gc.md), here in the CLI, to its end: what went and why, what stayed and why.
//! `--dry-run` plans and removes nothing. `blaze cache gc --last` prints the daemon's last runs
//! from its ledger.
//!
//! The CLI knows no daemon's live heads, so it keeps more than the daemon would: every build
//! dir whose checkout still exists counts as a head's. Everything else holds as in the daemon:
//! a dir anyone uses holds its lease, and the collector renames a dir only once it takes that
//! lease exclusively. It collects only the areas whose collector lock it takes
//! ([`blaze_gc::lease::collector_lock`]): an area another collector holds is left to it, and
//! said, naming the holder. When it takes none, as a running daemon holds them all, it asks
//! that daemon's collector for the run instead ([`blaze_api::Request::Gc`]) and prints its
//! report, which knows the daemon's live heads (review N14).

use std::{ffi::OsString, path::Path};

use blaze_core::exit::Exit;
use blaze_gc::{Collector, Config, Inventory};

use super::say;

const USAGE_TEXT: &str = "blaze cache gc [--dry-run] | blaze cache gc --last";

/// `blaze cache`, with the words after it.
pub fn main(words: &[OsString], color: bool) -> i32 {
    let refused = |what: &str, code: i32| {
        eprint!("{}", say::error(what, color));
        code
    };
    let words: Option<Vec<&str>> = words.iter().map(|it| it.to_str()).collect();
    let (dry_run, last) = match words.as_deref() {
        Some(["gc"]) => (false, false),
        Some(["gc", "--dry-run" | "-n"]) => (true, false),
        Some(["gc", "--last"]) => (false, true),
        _ => return refused(USAGE_TEXT, Exit::Usage.code()),
    };
    let done = if last { last_runs() } else { collect(dry_run) };
    match done {
        Ok(out) => {
            print!("{out}");
            0
        }
        Err(what) => refused(&what, Exit::Failed.code()),
    }
}

/// The areas, as the daemon's ([`crate::serve::gc`]).
fn areas() -> Result<Vec<String>, String> {
    let builds = crate::serve::builds().map_err(|error| format!("{error:#}"))?;
    let root = builds
        .parent()
        .ok_or_else(|| format!("{} has no parent, blaze's root", builds.display()))?;
    let machine = crate::serve::machine().map_err(|error| format!("{error:#}"))?;
    let mut areas = vec![root.to_string_lossy().into_owned()];
    if machine != root {
        areas.push(machine.to_string_lossy().into_owned());
    }
    Ok(areas)
}

fn collect(dry_run: bool) -> Result<String, String> {
    let areas = areas()?;
    let mut out = String::new();
    // Held to the end of the run: an area another collector holds is given as `""`, left alone.
    // alloc: once a run, an area or two
    let mut locks = Vec::new();
    let mut held = Vec::new();
    let mut skipped = Vec::new();
    let holder = format!("blaze cache gc pid {}", std::process::id());
    for area in &areas {
        match blaze_gc::lease::collector_lock(Path::new(area), &holder)? {
            Ok(lock) => {
                locks.push(lock);
                held.push(area.clone()); // clone: the run's copy, an area or two
            }
            Err(by) => {
                out.push_str(&format!(
                    "{area} is left alone: {by} holds its {}\n",
                    blaze_gc::lease::COLLECTOR
                ));
                skipped.push(format!("{area} (held by {by})"));
                held.push(String::new());
            }
        }
    }
    if locks.is_empty() {
        // A daemon collects them all: its collector runs this, with its live heads.
        return asked_of_daemon(dry_run, out);
    }
    // Every checkout a build dir names that exists: the heads, as the CLI can know them.
    let clock = blaze_gc::plan::Clock::default();
    let found: Inventory =
        blaze_gc::fs::scan(&held, None, u32::MAX, None, &clock, &mut blaze_gc::fs::Sizes::new())?;
    let mut heads: Vec<String> = found
        .objects
        .iter()
        .filter_map(|it| it.checkout.as_ref())
        .filter(|it| it.exists)
        .map(|it| it.path.clone())
        .collect();
    heads.sort_unstable();
    heads.dedup();
    let root = std::path::PathBuf::from(&areas[0]);
    let levels =
        blaze_gc::settings::GcSettings::read(&root, blaze_gc::pace::Levels::default())?.levels;
    let mut collector = Collector::new(Config { levels, ..Config::daemon(areas) });
    collector.heads(&heads);
    collector.skipped(skipped);
    let report = blaze_gc::fs::run_now(&mut collector, dry_run, &held)?;
    drop(locks);
    out.push_str(&report.details());
    Ok(out)
}

/// The run asked of the running daemon, which holds every area's collector lock: its report as
/// it prints, after `out` (which names the holders). No daemon answering, the holder is another
/// `blaze cache gc` or a daemon that runs no collector, and that is the error.
fn asked_of_daemon(dry_run: bool, mut out: String) -> Result<String, String> {
    let mut report = None;
    let asked = super::observe::ask(
        blaze_api::Request::Gc { dry_run },
        super::daemon::Start::Never,
        |response| {
            if let blaze_api::ResponseRef::Collected { report: it } = response {
                report = Some(it.to_owned());
            }
            Ok(())
        },
    );
    match (asked, report) {
        (Ok(()), Some(report)) => {
            out.push_str("every area is the daemon's: its collector ran this\n");
            out.push_str(&report);
            Ok(out)
        }
        (Ok(()), None) => Err(format!("{out}the daemon answered `blaze cache gc` with no report")),
        (Err(why), _) => Err(format!(
            "{out}nothing is collected: every area is another collector's, and the daemon did not run it: {why}"
        )),
    }
}

/// The daemon's last runs, from its ledger.
fn last_runs() -> Result<String, String> {
    let state = crate::serve::state().map_err(|error| format!("{error:#}"))?;
    let path = state.join(blaze_gc::ledger::LEDGER);
    let Some(ledger) = blaze_gc::ledger::read(Path::new(&path))? else {
        return Ok(format!("no collection has run yet: {} is not there\n", path.display()));
    };
    let mut out = String::new();
    for report in &ledger.runs {
        out.push_str(&format!("{report}\n"));
    }
    Ok(out)
}
