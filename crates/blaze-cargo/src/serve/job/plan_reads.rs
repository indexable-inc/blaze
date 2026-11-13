//! The plan kept by what it read: a checkout's plan is a function of its config (its request
//! shape, which holds the mounts and the environment it reads) and of the files and listings of
//! the view it read through the engine's inputs (`Cx::file`, `Cx::dir`; `file_at` and `dir_at`
//! read roots that never change). So a view whose difference from the plan's view leaves every
//! one of those reads equal has the plan already: its units are the same `Arc`, and what is kept
//! by their identity (`LastPlan::outputs`, `sized`, `stood`) goes on with them. No IO, no clock:
//! the reads the engine recorded and the VFS's difference of two roots.
//!
//! Until 2026-11-12 a kept plan was matched by its view's root and config as one key, so a body
//! edit moved the key and the plan was asked again (1.97 ms on six dependencies, a body edit,
//! build 58 of /tmp/plan/12-edit-floor.md), came back equal and was found equal by a walk of
//! every unit.

use blaze_ids::ContentId;
use blaze_vfs::{Change, ChangeKind};

/// The inputs the plan read, as the engine recorded them (`blaze_query::Pinned::inputs_of`):
/// each path of the view whose bytes it read, and each directory whose listing it read, with
/// their ids then. Paths are relative to the view's root, as `Vfs::diff` names them (`""` is the
/// root's listing); each list sorted by its paths' bytes, one entry a path.
pub(crate) struct PlanReads {
    pub files: Box<[(Box<str>, ContentId)]>,
    pub listings: Box<[(Box<str>, ContentId)]>,
}

impl PlanReads {
    /// The reads of `inputs`, sorted and deduplicated. A path that is not relative to the view
    /// as the VFS names one (a leading or trailing `/`, an empty, `.` or `..` part), or read at
    /// two ids, is refused naming it: [`holds`] compares paths as the VFS writes them, and a
    /// path written otherwise would never meet its change.
    pub(crate) fn of(inputs: blaze_query::Inputs) -> Result<PlanReads, String> {
        let blaze_query::Inputs { files, listings } = inputs;
        Ok(PlanReads { files: sorted(files, false)?, listings: sorted(listings, true)? })
    }

    /// How many reads it holds, for a span.
    pub(crate) fn len(&self) -> usize {
        self.files.len() + self.listings.len()
    }
}

fn sorted(
    mut reads: Vec<(Box<str>, ContentId)>,
    listing: bool,
) -> Result<Box<[(Box<str>, ContentId)]>, String> {
    let what = if listing { "listing" } else { "file" };
    for (path, _) in &reads {
        // The root's listing is `""`; no file is.
        let root = listing && path.is_empty();
        if !root && path.split('/').any(|part| part.is_empty() || part == "." || part == "..") {
            // alloc: an error, once
            return Err(blaze_report::say!(
                "the plan read the ",
                blaze_report::say::Words(what),
                " ",
                blaze_report::say::Code(path),
                ", which is no path relative to the view's root: its changes could not be told"
            )
            .flat());
        }
    }
    reads.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    for pair in reads.windows(2) {
        if pair[0].0 == pair[1].0 && pair[0].1 != pair[1].1 {
            // alloc: an error, once
            return Err(blaze_report::say!(
                "the plan read the ",
                blaze_report::say::Words(what),
                " ",
                blaze_report::say::Code(&pair[0].0),
                " at two ids, ",
                blaze_report::say::Words(&format!("{} and {}", pair[0].1, pair[1].1)),
                ", in one view"
            )
            .flat());
        }
    }
    reads.dedup_by(|a, b| a.0 == b.0);
    Ok(reads.into_boxed_slice())
}

/// Whether a view whose difference from the plan's view is `changed` (`Vfs::diff`, every file
/// that differs) and `relisted` (`Vfs::relisted`, every directory whose listing differs, sorted)
/// leaves every read of `reads` equal: no changed path is a file it read or a listing it read,
/// and no relisted directory is a listing it read.
///
/// A listing is names and whether each is a directory, so it differs exactly where `relisted`
/// says: a file modified relists nothing, a file added to an existing `src/a/` relists `src/a`
/// alone (a plan that listed only `src` holds), and a new `src/a/` relists `src` and `src/a`.
/// The tree keeps no empty directory, so a directory made or emptied is made or emptied by the
/// files under it, and is in `relisted` with them. Each path is looked up in the sorted reads:
/// O((changes + relisted) x log reads), where a one-file body edit is one lookup.
pub(crate) fn holds(reads: &PlanReads, changed: &[Change], relisted: &[String]) -> bool {
    for change in changed {
        let path = change.path.as_str();
        if read(&reads.files, path) {
            return false;
        }
        if change.kind != ChangeKind::Modified && read(&reads.listings, path) {
            return false;
        }
    }
    if planted(Plant::ListingBlind) {
        return true;
    }
    !relisted.iter().any(|dir| read(&reads.listings, dir))
}

fn read(reads: &[(Box<str>, ContentId)], path: &str) -> bool {
    reads.binary_search_by(|it| (*it.0).cmp(path)).is_ok()
}

/// A bug planted here, for a test's control: each makes [`holds`] keep a plan a fresh plan of the
/// new view differs from. Never planted outside a test.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(in crate::serve) enum Plant {
    None,
    /// The relisted directories are left out: a file or a directory added under a listed
    /// directory keeps the plan.
    ListingBlind,
}

#[cfg(test)]
thread_local! {
    pub(in crate::serve) static PLANT: std::cell::Cell<Plant> =
        const { std::cell::Cell::new(Plant::None) };
}

#[cfg(test)]
fn planted(plant: Plant) -> bool {
    PLANT.get() == plant
}

#[cfg(not(test))]
fn planted(_: Plant) -> bool {
    false
}

#[cfg(test)]
mod tests;
