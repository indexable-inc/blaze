//! The driver of the home's pass ([`crate::roots`]): it lists what is right under a home, asks
//! the core for the account, and renames what goes into the home's trash. It unlinks nothing:
//! the collector's trash phase does (`fs.rs`), so a crash leaves each entry where it was or
//! whole in trash, as for a build dir.
//!
//! A rename is done while every lock file found in the entry (right inside it and one directory
//! down: `serve.lock`, `sock.lock`, `.blaze-collector`, `.blaze-lease`) is held exclusively by
//! this process, taken without waiting; one that is held keeps the entry.

use std::{
    fs::File,
    path::{Path, PathBuf},
};

use stdx::file_lock::Locked;

use crate::{
    fs::{self, Sizes},
    roots::{self, Account, Entry, Rules},
};

/// What one pass over a home is asked.
#[derive(Clone, Copy, Debug)]
pub struct Pass {
    /// The collector's clock ([`crate::plan::Clock::now`]), never the raw wall clock.
    pub now_ns: u64,
    /// The pressure of the home's volume, permille.
    pub pressure: u32,
    /// Plan and say, rename nothing.
    pub dry_run: bool,
    pub rules: Rules,
}

/// How many of the largest holders a report names.
pub const HOLDERS: usize = 12;

/// Whether `name`, a file in an entry, is a lock some blaze holds while it uses the entry.
fn is_lock(name: &str) -> bool {
    name.ends_with(".lock") || name == crate::lease::COLLECTOR || name == crate::lease::LEASE
}

/// The lock files right inside `dir` and one directory down, sorted.
fn lock_files(dir: &Path, problems: &mut Vec<String>) -> Vec<PathBuf> {
    // alloc: a probe, rarely more than a few
    let mut found = Vec::new();
    let mut look = |at: &Path, down: bool, found: &mut Vec<PathBuf>| -> Vec<PathBuf> {
        // alloc: a probe, the directories one down
        let mut below = Vec::new();
        let read = match std::fs::read_dir(at) {
            Ok(read) => read,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return below,
            Err(error) => {
                problems.push(format!("listing {}: {error}", at.display()));
                return below;
            }
        };
        for entry in read.flatten() {
            let Ok(kind) = entry.file_type() else { continue };
            let name = entry.file_name();
            if kind.is_file() && is_lock(&name.to_string_lossy()) {
                found.push(entry.path());
            } else if kind.is_dir() && down {
                below.push(entry.path());
            }
        }
        below
    };
    for below in look(dir, true, &mut found) {
        look(&below, false, &mut found);
    }
    found.sort();
    found
}

/// Every lock file of `path` held exclusively, taken without waiting: `Ok(None)` when one is
/// held by another (something uses the entry). A file has none.
fn take_locks(path: &Path, problems: &mut Vec<String>) -> Result<Option<Vec<Locked>>, String> {
    // alloc: the locks held over one rename
    let mut held = Vec::new();
    if !path.is_dir() {
        return Ok(Some(held));
    }
    for lock in lock_files(path, problems) {
        let file = match File::open(&lock) {
            Ok(file) => file,
            // Gone since it was listed: its holder, if any, is gone with it.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(format!("opening {}: {error}", lock.display())),
        };
        match Locked::try_lock(file) {
            Ok(Ok(locked)) => held.push(locked),
            Ok(Err(_file)) => return Ok(None),
            Err(error) => return Err(format!("locking {}: {error}", lock.display())),
        }
    }
    Ok(Some(held))
}

/// The newest stamp of `path` and of what is right inside it.
fn used_ns(path: &Path) -> u64 {
    fs::modified_ns(path).max(if path.is_dir() { fs::newest_ns(path) } else { 0 })
}

/// Every entry right under `home`, sorted by name: its bytes on disk (through `sizes`, so a
/// directory nothing changed is not walked again), its newest stamp, and whether a lock in it
/// is held. An entry that cannot be measured or probed counts as held, and why is a problem:
/// what cannot be told is never removed.
pub fn list(
    home: &Path,
    sizes: &mut Sizes,
    problems: &mut Vec<String>,
) -> Result<Vec<Entry>, String> {
    let read =
        std::fs::read_dir(home).map_err(|error| format!("listing {}: {error}", home.display()))?;
    // alloc: a pass, one an entry right under the home
    let mut entries = Vec::new();
    for entry in read {
        let entry = entry.map_err(|error| format!("listing {}: {error}", home.display()))?;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        let Ok(meta) = std::fs::symlink_metadata(&path) else { continue };
        let measured = match meta.is_dir() {
            true => sizes.of(&path, problems),
            false => fs::size(&path, problems),
        };
        let (bytes, mut held) = match measured {
            Ok(bytes) => (bytes, false),
            Err(why) => {
                problems.push(why);
                (0, true)
            }
        };
        if !held && roots::owner_of(&name).0.collected_here() {
            // The probe: every lock taken and let go at once.
            held = match take_locks(&path, problems) {
                Ok(locks) => locks.is_none(),
                Err(why) => {
                    problems.push(why);
                    true
                }
            };
        }
        entries.push(Entry { name, bytes, used_ns: used_ns(&path), held });
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(entries)
}

/// One pass over `home`: the account of everything right under it and, unless it is a dry run,
/// each entry that goes renamed into the home's trash under its locks. `to_free` gives the bytes
/// still to free on the home's volume from the bytes the home holds. With the problems met:
/// an entry that could not be measured, probed or renamed, each named.
///
/// # Errors
///
/// The home on a network file system, a home that does not list, or a trash that is not
/// blaze's: nothing was renamed.
pub fn pass(
    home: &Path,
    asked: &Pass,
    to_free: &dyn Fn(u64) -> u64,
    sizes: &mut Sizes,
) -> Result<(Account, Vec<String>), String> {
    let span = tracing::info_span!(
        target: "blaze",
        "gc home pass",
        pressure = asked.pressure,
        bytes = tracing::field::Empty,
        going = tracing::field::Empty,
    );
    let _entered = span.enter();
    fs::check_local(home)?;
    // alloc: a pass's problems, rarely any
    let mut problems = Vec::new();
    let entries = list(home, sizes, &mut problems)?;
    // What is to free depends on all of the home's bytes (blaze's share of the volume), which
    // only the listing knows: every byte under the home, whoever owns it.
    let held: u64 = entries.iter().map(|it| it.bytes).sum();
    let account =
        roots::account(&entries, asked.now_ns, asked.pressure, to_free(held), &asked.rules);
    span.record("bytes", account.total_bytes);
    span.record("going", account.going_bytes);
    if asked.dry_run || account.going().next().is_none() {
        return Ok((account, problems));
    }
    let trash = fs::trash_dir(home)?;
    for (step, row) in account.going().enumerate() {
        let from = home.join(&row.name);
        if !roots::fits_trash(&row.name) {
            problems.push(format!(
                "{} is kept: its name cannot be an entry of trash",
                from.display()
            ));
            continue;
        }
        // Held from here to the rename: a process that takes one of these locks after the
        // rename takes it in trash, where the unlink's own walk finds it.
        let locks = match take_locks(&from, &mut problems) {
            Ok(Some(locks)) => locks,
            Ok(None) => {
                problems.push(format!(
                    "{} is kept: a lock in it was taken since it was listed",
                    from.display()
                ));
                continue;
            }
            Err(why) => {
                problems.push(why);
                continue;
            }
        };
        // Written since it was listed: the account's verdict is about an older state.
        if used_ns(&from) > row.used_ns {
            problems.push(format!("{} is kept: it was written since it was listed", from.display()));
            continue;
        }
        let step = u32::try_from(step).unwrap_or(u32::MAX);
        let to = trash.join(roots::trash_name(asked.now_ns, step, &row.name));
        match fs::rename_no_replace(&from, &to) {
            Ok(()) => {}
            // Gone since it was listed: nothing to remove.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => problems.push(format!(
                "renaming {} to {}: {error}",
                from.display(),
                to.display()
            )),
        }
        drop(locks);
    }
    Ok((account, problems))
}

/// The names right under `home` no blaze makes ([`roots::unregistered`]): the guard a test runs
/// over a home after a build, failing on any.
pub fn unregistered(home: &Path) -> Result<Vec<String>, String> {
    let read =
        std::fs::read_dir(home).map_err(|error| format!("listing {}: {error}", home.display()))?;
    // alloc: a guard's names
    let mut names = Vec::new();
    for entry in read {
        let entry = entry.map_err(|error| format!("listing {}: {error}", home.display()))?;
        names.push(entry.file_name().to_string_lossy().into_owned());
    }
    names.sort();
    // alloc: a guard's answer, rarely any
    Ok(roots::unregistered(names.iter().map(String::as_str)).into_iter().map(str::to_owned).collect())
}
