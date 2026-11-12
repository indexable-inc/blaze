//! `blaze gc [--dry-run]` (also `blaze cache gc`): one run of the collector of what blaze
//! keeps on disk (docs/gc.md), here in the CLI, to its end: what went and why, what stayed and
//! why. `--dry-run` (or `--kept`) plans and removes nothing: it prints what is held, what is
//! kept and why. `blaze gc --last` prints the daemon's last runs from its ledger. `blaze gc
//! --budget <size>` (`64G`, `512M`) sets the home's absolute budget in `<root>/gc.json` and
//! prints what is held against it. `--unkept` is `blaze clean --unkept`: the daemon's store
//! forgets every record no pin and no root keeps.
//!
//! The daemon's store is collected by the daemon alone, which holds it: with none running,
//! the build dirs are collected here and the store is said to be left.
//!
//! `blaze store verify [--dry-run]` checks the whole files the store keeps to place outputs by
//! cloning (`<root>/v2/store/placed/<content>`) against their names and removes the ones that
//! are wrong: what the store names when it refuses a place over such a file
//! (`blaze_cas::VERIFY_PLACED`). It runs with no daemon on the home (the daemon holds those
//! files in memory as it found them); the next build writes what was removed again.
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

const USAGE_TEXT: &str = "blaze gc [--dry-run | --kept] | blaze gc --last | blaze gc --budget <size> (64G, 512M) | blaze clean --unkept | blaze store verify [--dry-run]";

/// `blaze cache`, with the words after it.
pub fn main(words: &[OsString], color: bool) -> i32 {
    let refused = |what: &str, code: i32| {
        blaze_report::err!("{}", say::error(what, color));
        code
    };
    let words: Option<Vec<&str>> = words.iter().map(|it| it.to_str()).collect();
    let done = match words.as_deref() {
        Some(["gc"]) => collect(false, false),
        Some(["gc", "--dry-run" | "-n" | "--kept"]) => collect(true, false),
        Some(["gc", "--unkept"]) => collect(false, true),
        Some(["gc", "--unkept", "--dry-run" | "-n"]) => collect(true, true),
        Some(["gc", "--last"]) => last_runs(),
        // Lane packs-1112: packs out of and into the store (`cli::store`).
        Some(["store", "export" | "import" | "install", ..]) => {
            return super::store::main(&words.as_deref().unwrap_or_default()[1..], color);
        }
        Some(["store", "verify"]) => verify(false),
        Some(["store", "verify", "--dry-run" | "-n"]) => verify(true),
        Some(["gc", "--budget", size]) => budget(size).and_then(|mut said| {
            said.push_str(&collect(true, false)?);
            Ok(said)
        }),
        _ => return refused(USAGE_TEXT, Exit::Usage.code()),
    };
    match done {
        Ok(out) => {
            blaze_report::out!("{out}");
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

/// `size` as bytes: a whole number of `G` (or `GiB`) or `M` (or `MiB`); `None` for anything
/// else, and for none.
fn budget_size(size: &str) -> Option<(u64, &'static str)> {
    let (digits, unit) = size.split_at(size.find(|it: char| !it.is_ascii_digit())?);
    let count: u64 = digits.parse().ok().filter(|&it| it > 0)?;
    match unit {
        "G" | "GiB" | "g" => Some((count, "budget_gib")),
        "M" | "MiB" | "m" => Some((count, "budget_mib")),
        _ => None,
    }
}

/// `blaze gc --budget <size>`: the home's absolute budget written into `<root>/gc.json`, its
/// other fields kept; the file is read back as the daemon reads it before this says it is
/// set, and the daemon reads it again as it is next asked (the report that follows asks).
fn budget(size: &str) -> Result<String, String> {
    let Some((count, field)) = budget_size(size) else {
        return Err(format!(
            "`{size}` is no size: a whole number of G or M, as in `blaze gc --budget 128G`"
        ));
    };
    let areas = areas()?;
    let root = Path::new(&areas[0]);
    let path = root.join(blaze_gc::settings::SETTINGS);
    let mut set = match std::fs::read(&path) {
        Ok(bytes) => match serde_json::from_slice::<serde_json::Value>(&bytes) {
            Ok(serde_json::Value::Object(set)) => set,
            Ok(_) => return Err(format!("{} is not one JSON object", path.display())),
            Err(error) => return Err(format!("{}: {error}", path.display())),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => serde_json::Map::new(),
        Err(error) => return Err(format!("reading {}: {error}", path.display())),
    };
    set.remove("budget_gib");
    set.remove("budget_mib");
    set.insert(field.to_owned(), serde_json::Value::from(count));
    let text = serde_json::to_string_pretty(&serde_json::Value::Object(set))
        .map_err(|error| format!("{}: {error}", path.display()))?;
    let temp = root.join(format!(".gc.json.{}.tmp", std::process::id()));
    std::fs::create_dir_all(root).map_err(|error| format!("{}: {error}", root.display()))?;
    std::fs::write(&temp, format!("{text}\n"))
        .and_then(|()| std::fs::rename(&temp, &path))
        .map_err(|error| format!("writing {}: {error}", path.display()))?;
    let levels =
        blaze_gc::settings::GcSettings::read(root, blaze_gc::pace::Levels::default())?.levels;
    Ok(format!(
        "the budget of {} is {} (and at most {}% of its volume, and what leaves its floor free): written to {}\n",
        root.display(),
        blaze_gc::plan::size_text(levels.budget_bytes),
        levels.share_percent,
        path.display(),
    ))
}

/// What a whole file under `placed/` is, by its name and its bytes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PlacedIs {
    /// Named for its content, and its bytes hash to the name.
    Right,
    /// Named for a content its bytes do not hash to, or no regular file under such a name (a
    /// symbolic link, a directory): what a crash left of a write that was never synced, or
    /// something changed outside the daemon. Removed.
    Wrong,
    /// A derived value's (`<key>.derived`): the store never takes one from an earlier run and
    /// makes it again itself as it opens. Left.
    Derived,
    /// Any other name (a staged file of a place in flight when a daemon died): the store's own
    /// open sweeps those. Left.
    Other,
}

/// What `name` under `placed/` is, read through `path`.
fn placed_is(path: &Path, name: &str) -> Result<PlacedIs, String> {
    if name.ends_with(".derived") {
        return Ok(PlacedIs::Derived);
    }
    if name.len() != 64 || !name.bytes().all(|it| it.is_ascii_hexdigit()) {
        return Ok(PlacedIs::Other);
    }
    let meta = std::fs::symlink_metadata(path).map_err(|error| format!("{}: {error}", path.display()))?;
    if !meta.is_file() {
        return Ok(PlacedIs::Wrong);
    }
    let bytes = std::fs::read(path).map_err(|error| format!("reading {}: {error}", path.display()))?;
    Ok(match format!("{:x}", blaze_cas::content_of(&bytes)) == name {
        true => PlacedIs::Right,
        false => PlacedIs::Wrong,
    })
}

/// The whole files under `placed` checked against their names; the wrong ones removed unless
/// `dry_run`. What it found, as it prints.
fn verify_placed(placed: &Path, dry_run: bool) -> Result<String, String> {
    let entries = match std::fs::read_dir(placed) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(format!("{} is not there: the store keeps no placed file\n", placed.display()));
        }
        Err(error) => return Err(format!("{}: {error}", placed.display())),
    };
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| format!("{}: {error}", placed.display()))?;
        match entry.file_name().into_string() {
            Ok(name) => names.push(name),
            Err(name) => return Err(format!("{}: a name that is not UTF-8: {name:?}", placed.display())),
        }
    }
    names.sort_unstable();
    let mut out = String::new();
    let (mut right, mut wrong, mut left) = (0u64, 0u64, 0u64);
    for name in names {
        let path = placed.join(&name);
        match placed_is(&path, &name)? {
            PlacedIs::Right => right += 1,
            PlacedIs::Derived | PlacedIs::Other => left += 1,
            PlacedIs::Wrong => {
                wrong += 1;
                match dry_run {
                    true => out.push_str(&format!("{} is not what its name says: it would be removed\n", path.display())),
                    false => {
                        let removed = match std::fs::symlink_metadata(&path) {
                            Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(&path),
                            _ => std::fs::remove_file(&path),
                        };
                        removed.map_err(|error| format!("removing {}: {error}", path.display()))?;
                        out.push_str(&format!("{} was not what its name says: removed\n", path.display()));
                    }
                }
            }
        }
    }
    out.push_str(&format!(
        "{}: {right} placed files are what their names say, {wrong} were not and {}, {left} are the store's to make or sweep as it opens; the next build writes again what it needs\n",
        placed.display(),
        if dry_run { "would be removed" } else { "are removed" },
    ));
    Ok(out)
}

/// `blaze store verify`: [`verify_placed`] of this home's store, with no daemon on it.
fn verify(dry_run: bool) -> Result<String, String> {
    match super::daemon::connect(super::daemon::Start::Never) {
        Ok(None) => {}
        Ok(Some(_)) => {
            return Err(
                "a daemon is running on this home and holds the store's placed files as it found them: `blaze stop`, then `blaze store verify`".to_owned(),
            );
        }
        Err(error) => return Err(format!("{error:#}")),
    }
    let state = crate::serve::state().map_err(|error| format!("{error:#}"))?;
    verify_placed(&state.join("store").join("placed"), dry_run)
}

/// What is said of the store when no daemon holds the home: only its daemon collects it.
const STORE_LEFT: &str = "the daemon's store is left as it is: only the daemon that holds it collects it, and none is running (any build starts one; then `blaze gc` again)\n";

fn collect(dry_run: bool, unkept: bool) -> Result<String, String> {
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
        return asked_of_daemon(dry_run, unkept, out);
    }
    if unkept {
        return Err(format!("{out}{STORE_LEFT}"));
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
    out.push_str(STORE_LEFT);
    Ok(out)
}

/// The run asked of the running daemon, which holds every area's collector lock: its report as
/// it prints, after `out` (which names the holders). No daemon answering, the holder is another
/// `blaze cache gc` or a daemon that runs no collector, and that is the error.
fn asked_of_daemon(dry_run: bool, unkept: bool, mut out: String) -> Result<String, String> {
    let mut report = None;
    let asked = super::observe::ask(
        blaze_api::Request::Gc { dry_run, unkept },
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

#[cfg(test)]
mod tests {
    use super::*;

    /// `blaze store verify`'s rule: a placed file whose bytes hash to its name stays; one
    /// whose bytes do not, and a symbolic link under such a name, are removed (and only said
    /// under `--dry-run`); a derived value's file and any other name are left.
    #[test]
    fn verify_removes_the_placed_files_that_are_not_what_their_names_say() {
        let dir = std::env::temp_dir().join(format!("blaze-store-verify-{}", std::process::id()));
        _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let name_of = |bytes: &[u8]| format!("{:x}", blaze_cas::content_of(bytes));
        let (good, torn, linked) = (b"a whole output".to_vec(), b"another output".to_vec(), b"a third".to_vec());
        std::fs::write(dir.join(name_of(&good)), &good).unwrap();
        std::fs::write(dir.join(name_of(&torn)), &torn[..5]).unwrap();
        std::fs::write(dir.join("elsewhere"), &linked).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(dir.join("elsewhere"), dir.join(name_of(&linked))).unwrap();
        std::fs::write(dir.join(format!("{}.derived", name_of(b"k"))), b"made again").unwrap();
        let said = verify_placed(&dir, true).unwrap();
        assert!(said.contains("would be removed"), "{said}");
        assert!(dir.join(name_of(&torn)).exists(), "a dry run removes nothing");
        let said = verify_placed(&dir, false).unwrap();
        assert!(dir.join(name_of(&good)).exists(), "{said}");
        assert!(!dir.join(name_of(&torn)).exists(), "{said}");
        assert!(std::fs::symlink_metadata(dir.join(name_of(&linked))).is_err(), "{said}");
        assert_eq!(std::fs::read(dir.join("elsewhere")).unwrap(), linked, "what a link named is untouched");
        assert!(dir.join(format!("{}.derived", name_of(b"k"))).exists(), "{said}");
        assert!(said.contains("1 placed files are what their names say"), "{said}");
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(verify_placed(&dir, false).unwrap().contains("is not there"));
    }
}
