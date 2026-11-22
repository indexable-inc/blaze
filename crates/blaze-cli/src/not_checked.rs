//! What was not checked, named by cause at crate grain (drive.md D115 ruling (6), the
//! coordinator's wording of 2026-11-16 21:1x): one line a cause,
//!
//! "not checked because <cause> failed: crate `a`, crate `b`, .. (and N more), and M other task(s)"
//!
//! The drive groups the tasks that failed `Waited` by the task their chain ends at
//! (`blaze_drive::not_run::by_cause`, transitively); this file names each cause by its item and
//! each waited `CrateScope` by its crate. One shape for every cause: a failed build script reads
//! "not checked because the build script `build.rs` of crate `X` failed: crate `X`". A waited
//! task of another kind is counted, never listed (its crate is not known per failure yet: the
//! Owners input names it, a later step). A need dropped this run is a cause of its own.

use blaze_drive::not_run::{by_cause, Cause};
use blaze_drive::{Kind, Name, TaskFailure};
use bstd::mem::Frame;

use crate::names::Names;
use crate::work::Failures;
use crate::CliError;

/// The most crates (or items) named under one cause; the count past them stays exact. Few: a
/// line names what to look at first, and tempo's one cause named hundreds on one line at 20
/// (2026-11-17); `--each` lists every one. One home, the findings' query module's (D164).
pub(crate) const CRATES_SHOWN: usize = blaze_report::query::SHOWN;

/// The items not checked because a task failed, as the lines under the failures counted them
/// (waited.rs `line`), and how many of those lines counted an item.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Behind {
    pub items: u32,
    pub failures: u32,
}

impl Behind {
    /// One failure's line, counting `items`.
    pub(crate) fn add(&mut self, items: u32) {
        if items > 0 {
            self.items = self.items.saturating_add(items);
            self.failures += 1;
        }
    }
}

/// Each line of what was not checked, in cause order, handed to `each` (the caller prints it as
/// a note). `failures` is the drive's whole list (`Request::failures`: the machine's own
/// `Waited` too); `failed` the workers' entries, for a failed task's path; `names` the
/// command's names step, `None` when it did not run (then crates are not named, only counted).
pub(crate) fn lines(
    failures: &[TaskFailure],
    failed: &Failures<'_>,
    names: Option<&Names<'_>>,
    frame: &Frame,
    each: &mut dyn FnMut(&str) -> Result<(), CliError>,
) -> Result<(), CliError> {
    let erased = blaze_own::Erased::new(frame);
    let groups = by_cause(failures, erased.handle()).map_err(|_| CliError::Text {
        what: "the failed tasks wait on each other in a ring: no cause can be named (blaze-drive not_run.rs, rule 3)",
    })?;
    for group in groups.iter() {
        let mut words = bstd::mem::String::new_in(frame);
        let write = |words: &mut bstd::mem::String<_>, args: core::fmt::Arguments<'_>| {
            core::fmt::write(words, args).map_err(|_| CliError::Text { what: "a not-checked line's words" })
        };
        match group.cause {
            Cause::Failed(cause) => {
                words.push_str("not checked because ");
                cause_words(&mut words, cause, failed, names)?;
                words.push_str(" failed: ");
            }
            Cause::Dropped(cause) => write(&mut words, format_args!("not checked because `{cause}` is no longer made (dropped this run): "))?,
        }
        // The crates: each waited `CrateScope` the names step knows, by name and side, sorted
        // and without repeats; every other waited task counted.
        let mut crates: blaze_own::Vec<(&str, bool), _> = blaze_own::Vec::with_capacity_in(group.waited.len(), erased.handle());
        let mut others = 0usize;
        for &task in group.waited.iter() {
            match (task.kind, names.and_then(|names| names.crate_at(task.subject).map(|at| names.crate_label(at)))) {
                (Kind::CrateScope, Some(label)) => crates.push(label),
                _ => others += 1,
            }
        }
        crates.sort_unstable();
        crates.dedup();
        for (i, (name, macro_side)) in crates.iter().take(CRATES_SHOWN).enumerate() {
            if i > 0 {
                words.push_str(", ");
            }
            let side = if *macro_side { " (macro side)" } else { "" };
            write(&mut words, format_args!("crate `{name}`{side}"))?;
        }
        if crates.len() > CRATES_SHOWN {
            write(&mut words, format_args!(" (and {} more)", crates.len() - CRATES_SHOWN))?;
        }
        match (crates.is_empty(), others) {
            (_, 0) => {}
            (true, n) => write(&mut words, format_args!("{n} task(s)"))?,
            (false, n) => write(&mut words, format_args!(", and {n} other task(s)"))?,
        }
        each(words.as_str())?;
    }
    Ok(())
}

/// The failed task `cause` named by its item: a crate's scope by its crate, a build script by
/// its file and the crate that reads it, any other by its kind and file, else the drive's name.
fn cause_words(words: &mut bstd::mem::String<impl core::alloc::Allocator>, cause: Name, failed: &Failures<'_>, names: Option<&Names<'_>>) -> Result<(), CliError> {
    let text = |_: core::fmt::Error| CliError::Text { what: "a not-checked line's words" };
    match (cause.kind, names) {
        (Kind::CrateScope, Some(names)) => {
            if let Some(at) = names.crate_at(cause.subject) {
                let (name, macro_side) = names.crate_label(at);
                let side = if macro_side { " (macro side)" } else { "" };
                return core::fmt::write(words, format_args!("crate `{name}`{side}")).map_err(text);
            }
        }
        (Kind::Script, Some(names)) => {
            if let Some(at) = names.script_readers(cause).next()
                && let Some(file) = names.script_root(at)
            {
                let (name, macro_side) = names.crate_label(at);
                let side = if macro_side { " (macro side)" } else { "" };
                return core::fmt::write(words, format_args!("the build script `{file}` of crate `{name}`{side}")).map_err(text);
            }
        }
        // A build script's module lookup (script_modules.rs): named by the script it is for.
        (Kind::MacroModule, Some(names)) => {
            if let Some(module) = names.script_module(cause.subject)
                && let Some(file) = names.script_root(module.crate_at)
            {
                let (name, macro_side) = names.crate_label(module.crate_at);
                let side = if macro_side { " (macro side)" } else { "" };
                return core::fmt::write(words, format_args!("the module of the build script `{file}` of crate `{name}`{side}")).map_err(text);
            }
        }
        _ => {}
    }
    match failed.iter().find(|entry| entry.failure.name == cause).and_then(|entry| entry.path) {
        Some(path) => core::fmt::write(words, format_args!("{} of `{path}`", cause.kind.name())).map_err(text),
        None => core::fmt::write(words, format_args!("`{cause}`")).map_err(text),
    }
}

/// The items the check could not check, by cause: the same line, the same cap.
pub(crate) mod items;

/// The tasks not run because an input they read could not be made, by cause: one line a cause.
pub(crate) mod tasks;

/// The tasks that failed only because a task they need failed: counted under their root.
pub(crate) mod waited;

#[cfg(test)]
#[path = "not_checked_tests.rs"]
mod tests;
