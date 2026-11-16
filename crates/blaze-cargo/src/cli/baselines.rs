//! `blaze check --baselines DIR`: the run's findings of clippy's lints and blaze's, counted by
//! lint and crate (the package of the unit that gave each), held to the baselines in `DIR`
//! (`blaze_lints::baseline`) both ways: a count over its baseline, or a lint with findings and no
//! baseline, fails the run, and so does a count under its baseline, which `--bless-baselines`
//! lowers to the run's. `--new-baseline LINT` starts a lint's at the run's. A finding two units of
//! one package give counts once. The run must be of the whole workspace with its default targets,
//! for one named target (`args.rs` refuses `--baselines` without `--workspace` and one
//! `--target`, or with a target selection): its counts are of that target's cfg, which the
//! directory's `TARGET` file names, and a run for another is refused. blaze's own baselines,
//! `crates/blaze-lints/baselines`, are `aarch64-unknown-linux-gnu`'s, which the merge gate's
//! `lint-baselines` job checks.
//!
//! A run that failed is measured in part: a crate whose check did not complete (a unit of it
//! gave an error that is no lint finding, a compiler panic among them; a build script of it
//! failed; or a unit of it was not checked behind a failure) is [`Unmeasured`], and every other
//! crate was checked whole. `--bless-baselines` then records the crates it measured, keeps the
//! rest as the baselines hold them, names each with why, and the command fails.

use std::{fmt, path::Path};

use blaze_api::{DiagnosticRef, Level, Unchecked, UnitOfRef};
use blaze_hash::HashSet;
use blaze_lints::baseline::{self, Baseline, Blessed, Counts, Held, Off, Unmeasured};
use std::collections::BTreeMap;
use std::alloc::Global;

use super::say;

/// The findings a run printed, counted.
#[derive(Default)]
pub struct Counted {
    /// Each finding once, by lint, package, and its first primary span's file and bytes (its
    /// message where it has no primary span).
    seen: HashSet<(String, String, String, u32, u32), Global>,
    counts: Counts,
    /// The lint findings that came with no unit, so no crate to count them under.
    unowned: Vec<String>,
    /// The crates whose check the run did not complete, each with the first cause it gave.
    unmeasured: Unmeasured,
    /// The errors that are no lint finding and came with no unit: the run did not say which
    /// crates' checks they cut short, so it measured none for certain.
    unplaced: Vec<blaze_mem::String<Global>, Global>,
}

/// Whether a diagnostic's code names a lint a baseline counts.
fn is_lint(code: Option<&str>) -> bool {
    code.is_some_and(|it| it.starts_with("clippy::") || it.starts_with("blaze::"))
}

impl Counted {
    pub fn add(&mut self, diagnostic: &DiagnosticRef<'_>) {
        if diagnostic.level == Level::Error && !is_lint(diagnostic.code) {
            self.cut_short(diagnostic);
        }
        let Some(lint) = diagnostic.code.filter(|it| is_lint(Some(it))) else {
            return;
        };
        let at = diagnostic.spans.iter().find(|it| it.primary);
        let Some(unit) = diagnostic.unit else {
            let file = at.map_or("(no span)", |it| it.file);
            // alloc: a finding the run fails on, named once
            self.unowned.push(format!("{lint} at {file}: {}", diagnostic.message));
            return;
        };
        let krate = say::package_name(unit.package_id);
        let (file, start, end) =
            at.map_or((diagnostic.message, 0, 0), |it| (it.file, it.byte_start, it.byte_end));
        // alloc: a finding's key, once per finding of a run
        if self.seen.insert((lint.to_owned(), krate.to_owned(), file.to_owned(), start, end)) {
            *self
                .counts
                .entry(lint.to_owned())
                .or_default()
                .entry(krate.to_owned())
                .or_default() += 1;
        }
    }

    /// An error that is no lint finding: its unit's check stopped short of its end (a panic of
    /// blaze's, an error in its items), so its crate's counts are of part of it.
    fn cut_short(&mut self, diagnostic: &DiagnosticRef<'_>) {
        let first = diagnostic.message.lines().next().unwrap_or("");
        let Some(unit) = diagnostic.unit else {
            self.unplaced.push(blaze_mem::String::from_str_in(first, Global));
            return;
        };
        self.unmeasured.add(say::package_name(unit.package_id), format_args!("error: {first}"));
    }

    /// A build script's failure ([`blaze_api::Response::Report`] of a unit): its package was not
    /// checked.
    pub fn report(&mut self, unit: &UnitOfRef<'_>, title: &str) {
        let krate = say::package_name(unit.package_id);
        self.unmeasured.add(krate, format_args!("its build failed: {title}"));
    }

    /// What a failed build left unchecked ([`blaze_api::Response::Unchecked`]): each unit that
    /// failed with units behind it, and each unit not checked behind it, by its package (a unit
    /// is named `package v1.2.3 (lib)`, its first word, [`say::unit_package`]). A unit that
    /// failed only in its bodies or lints (`open`) was checked whole, and its findings counted.
    pub fn unchecked(&mut self, unchecked: &Unchecked) {
        for behind in &unchecked.behind {
            if behind.id.is_some() {
                let why = &behind.why;
                self.unmeasured.add(say::unit_package(&behind.unit), format_args!("it failed: {why}"));
            }
            for unit in &behind.units {
                let (failed, why) = (&behind.unit, &behind.why);
                self.unmeasured.add(
                    say::unit_package(unit),
                    format_args!("`{unit}` was not checked behind `{failed}`, which failed: {why}"),
                );
            }
        }
    }

    /// The baselines in `dir`, from the checkout's `root` when relative, of the run's `target`:
    /// each of `new` started at the run's counts, then all of them checked against the run, or
    /// lowered to it when `bless`. What to say of each, or the error naming every count off its
    /// baseline.
    pub fn finish(
        self,
        root: &Path,
        dir: &str,
        target: &str,
        bless: bool,
        new: &[String],
    ) -> Result<Vec<String>, String> {
        if !self.unowned.is_empty() {
            let lines: Vec<String> = self.unowned.iter().map(|it| format!("\n  {it}")).collect();
            return Err(format!(
                "the run gave lint findings with no unit, so no crate to count them under in the baselines:{}",
                lines.concat()
            ));
        }
        let dir = root.join(dir);
        let shown = dir.display();
        let theirs = baseline::target_of(&dir).map_err(|it| it.to_string())?;
        match &theirs {
            Some(theirs) if theirs != target => {
                return Err(format!(
                    "the baselines in {shown} count `{theirs}`'s findings and this run checks `{target}`'s: \
                     cfg'd code differs, so run it with `--target {theirs}`"
                ));
            }
            Some(_) => {}
            None if !baseline::read(&dir).map_err(|it| it.to_string())?.is_empty() => {
                let file = baseline::TARGET_FILE;
                return Err(format!(
                    "{shown} has baselines and no `{file}` file naming the target they count"
                ));
            }
            None => {}
        }
        if !self.unplaced.is_empty() {
            return Err(format!(
                "the run gave errors with no unit, so it did not say whose check they cut short and \
                 measured no crate for certain; the baselines in {shown} were not compared or written:{}",
                Lines(&self.unplaced)
            ));
        }
        if !new.is_empty() && !self.unmeasured.crates.is_empty() {
            return Err(format!(
                "`--new-baseline` starts a baseline at the run's counts, and this run did not measure \
                 every crate:{}",
                UnmeasuredLines(&self.unmeasured)
            ));
        }
        let mut said = Vec::new();
        for lint in new {
            let known = baseline::lint_of(&baseline::file_name(lint)).is_some_and(|it| it == *lint);
            if !known {
                return Err(format!(
                    "`--new-baseline {lint}`: no clippy or blaze lint blaze has is named so"
                ));
            }
            let path = dir.join(baseline::file_name(lint));
            if path.exists() {
                return Err(format!(
                    "`--new-baseline {lint}`: {} is there already",
                    path.display()
                ));
            }
            if self.counts.get(lint).is_none_or(|it| it.values().all(|count| *count == 0)) {
                return Err(format!(
                    "`--new-baseline {lint}`: the run has no finding of it, so set it to `deny` instead"
                ));
            }
        }
        for lint in new {
            let path = dir.join(baseline::file_name(lint));
            let counts = self.counts.get(lint).map_or_else(BTreeMap::new, |it| {
                // alloc: one lint's counts, once
                it.iter().map(|(krate, count)| (krate.clone(), Held::Count(*count))).collect()
            });
            let text = Baseline { counts }.render();
            std::fs::create_dir_all(&dir).map_err(|error| format!("{shown}: {error}"))?;
            std::fs::write(&path, text).map_err(|error| format!("{}: {error}", path.display()))?;
            said.push(format!("{lint}: baseline started at this run's counts"));
        }
        if theirs.is_none() && !new.is_empty() {
            let path = dir.join(baseline::TARGET_FILE);
            std::fs::write(&path, format!("{target}\n"))
                .map_err(|error| format!("{}: {error}", path.display()))?;
        }
        let unmeasured = &self.unmeasured;
        let off =
            baseline::check(&dir, &self.counts, unmeasured, Global).map_err(|it| it.to_string())?;
        let (over, under): (Vec<_>, Vec<_>) = off.iter().partition(|(_, off)| off.over());
        let listed = |offs: &[&(blaze_mem::String<Global>, Off)]| -> String {
            offs.iter().map(|(lint, off)| format!("\n  {lint}: {off}")).collect::<Vec<_>>().concat()
        };
        let over_said = |then: &str| {
            format!(
                "the run's findings are over the baselines in {shown}, which may only shrink (AGENTS.md rule \
                 fix-through-blaze-lints): fix the findings the change added (`blaze check --fix` makes the \
                 machine-applicable ones). A lint with no baseline is fixed, set to `deny`, or, when its \
                 findings are being moved off, started with `--new-baseline LINT`{then}:{}",
                listed(&over)
            )
        };
        if bless {
            // Each count the run measured is lowered to it, the counts over their baselines and
            // the crates it did not measure kept as they are: one finding over, or one broken
            // unit, holds no other crate's count back. The command still fails on either.
            let blessed = baseline::bless(&dir, &self.counts, unmeasured, Global)
                .map_err(|it| it.to_string())?;
            said.extend(blessed.into_iter().map(|(lint, blessed)| match blessed {
                Blessed::Lowered => format!("{lint}: baseline lowered to this run's counts"),
                Blessed::Emptied => format!(
                    "{lint}: no finding left, baseline deleted: it is `deny` in every crate where the \
                     workspace's `lints-ratchet` denies it; elsewhere set it to `deny`"
                ),
            }));
            if over.is_empty() && unmeasured.crates.is_empty() {
                return Ok(said);
            }
            // The error's text, on the heap once: the command fails with it.
            let mut failed = blaze_mem::String::new_in(Global);
            if !over.is_empty() {
                failed.push_str(&over_said(
                    "; a bless only lowers a baseline, so these stay as they are and every other count \
                     the run measured was recorded",
                ));
            }
            if !unmeasured.crates.is_empty() {
                if !failed.is_empty() {
                    failed.push('\n');
                }
                blaze_mem::format_into!(
                    &mut failed,
                    "the run did not complete the check of {} crate(s), which the baselines in {shown} keep \
                     as they hold them (`unmeasured` stays so): every other crate's count was recorded. Fix \
                     what stopped each and bless again:{}",
                    unmeasured.crates.len(),
                    UnmeasuredLines(unmeasured)
                );
            }
            for it in &said {
                blaze_mem::format_into!(&mut failed, "\n  recorded: {it}");
            }
            return Err(failed.as_str().to_owned());
        }
        if !over.is_empty() {
            return Err(over_said(""));
        }
        if !unmeasured.crates.is_empty() {
            return Err(format!(
                "the run did not complete the check of {} crate(s), so their counts were not compared with \
                 the baselines in {shown}:{}",
                unmeasured.crates.len(),
                UnmeasuredLines(unmeasured)
            ));
        }
        if !under.is_empty() {
            return Err(format!(
                "the baselines in {shown} are stale: the run has fewer findings than they hold. Lower them by \
                 running this command again with `--bless-baselines`, and commit the files:{}",
                listed(&under)
            ));
        }
        Ok(said)
    }
}

/// Each crate a run left unmeasured with why, a line each, written where the error is.
struct UnmeasuredLines<'a>(&'a Unmeasured);

impl fmt::Display for UnmeasuredLines<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (krate, why) in &self.0.crates {
            write!(f, "\n  `{krate}`: {why}")?;
        }
        Ok(())
    }
}

/// Each of `0` on a line of its own, written where the error is.
struct Lines<'a, T>(&'a [T]);

impl<T: fmt::Display> fmt::Display for Lines<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for it in self.0 {
            write!(f, "\n  {it}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: &str = "x86_64-unknown-linux-gnu";

    fn counted(counts: &[(&str, &str, u32)]) -> Counted {
        let mut it = Counted::default();
        for (lint, krate, count) in counts {
            it.counts.entry((*lint).to_owned()).or_default().insert((*krate).to_owned(), *count);
        }
        it
    }

    #[test]
    fn a_baseline_is_started_checked_both_ways_and_lowered() {
        let made = blaze_core::temp::TempDir::for_test("blaze-clippy-baselines-");
        let root = made.path().to_owned();
        let lint ="clippy::disallowed_types".to_owned();
        let run =
            counted(&[("clippy::disallowed_types", "a", 2), ("clippy::disallowed_types", "b", 1)]);
        let said = run.finish(&root, "b", T, false, std::slice::from_ref(&lint)).unwrap();
        assert_eq!(said, ["clippy::disallowed_types: baseline started at this run's counts"]);
        assert_eq!(
            std::fs::read_to_string(root.join("b/clippy.disallowed_types.txt")).unwrap(),
            "a\t2\nb\t1\n"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("b/TARGET")).unwrap(),
            "x86_64-unknown-linux-gnu\n"
        );
        let other =
            counted(&[]).finish(&root, "b", "aarch64-apple-darwin", false, &[]).unwrap_err();
        assert!(other.contains("--target x86_64-unknown-linux-gnu"), "{other}");
        let same =
            counted(&[("clippy::disallowed_types", "a", 2), ("clippy::disallowed_types", "b", 1)]);
        assert_eq!(same.finish(&root, "b", T, false, &[]).unwrap(), Vec::<String>::new());

        let grew =
            counted(&[("clippy::disallowed_types", "a", 3), ("clippy::disallowed_types", "b", 1)]);
        let grew = grew.finish(&root, "b", T, true, &[]).unwrap_err();
        assert!(
            grew.contains("`a`: 3 findings, over the baseline's 2")
                && grew.contains("these stay as they are"),
            "{grew}"
        );
        let unbaselined = counted(&[
            ("clippy::disallowed_types", "a", 2),
            ("clippy::disallowed_types", "b", 1),
            ("blaze::wall_clock", "c", 1),
        ]);
        let unbaselined = unbaselined.finish(&root, "b", T, false, &[]).unwrap_err();
        assert!(
            unbaselined.contains("blaze::wall_clock: 1 findings, and the lint has no baseline"),
            "{unbaselined}"
        );

        let shrank = counted(&[("clippy::disallowed_types", "a", 1)])
            .finish(&root, "b", T, false, &[])
            .unwrap_err();
        assert!(shrank.contains("stale") && shrank.contains("--bless-baselines"), "{shrank}");
        assert!(
            shrank.contains("`a`: 1 findings, under the baseline's 2")
                && shrank.contains("`b`: 0 findings"),
            "{shrank}"
        );
        let fewer = counted(&[("clippy::disallowed_types", "a", 1)]);
        assert_eq!(
            fewer.finish(&root, "b", T, true, &[]).unwrap(),
            ["clippy::disallowed_types: baseline lowered to this run's counts"]
        );
        assert_eq!(
            std::fs::read_to_string(root.join("b/clippy.disallowed_types.txt")).unwrap(),
            "a\t1\n"
        );
        let none = counted(&[]).finish(&root, "b", T, true, &[]).unwrap();
        assert_eq!(
            none,
            [
                "clippy::disallowed_types: no finding left, baseline deleted: it is `deny` in every crate \
                 where the workspace's `lints-ratchet` denies it; elsewhere set it to `deny`"
            ]
        );
        assert!(!root.join("b/clippy.disallowed_types.txt").exists());
        std::fs::remove_file(root.join("b/TARGET")).unwrap();
        std::fs::write(root.join("b/blaze.wall_clock.txt"), "a\t1\n").unwrap();
        let untargeted = counted(&[]).finish(&root, "b", T, false, &[]).unwrap_err();
        assert!(untargeted.contains("no `TARGET` file"), "{untargeted}");
        std::fs::remove_file(root.join("b/blaze.wall_clock.txt")).unwrap();

        let zero =
            counted(&[]).finish(&root, "b", T, false, std::slice::from_ref(&lint)).unwrap_err();
        assert!(zero.contains("no finding of it"), "{zero}");
        let run = counted(&[("clippy::disallowed_types", "a", 1)]);
        run.finish(&root, "b", T, false, std::slice::from_ref(&lint)).unwrap();
        let again = counted(&[("clippy::disallowed_types", "a", 1)])
            .finish(&root, "b", T, false, std::slice::from_ref(&lint))
            .unwrap_err();
        assert!(again.contains("is there already"), "{again}");
        let one_wrong = ["blaze::wall_clock".to_owned(), "clippy::no_such_lint".to_owned()];
        assert!(
            counted(&[("blaze::wall_clock", "a", 1)])
                .finish(&root, "b", T, false, &one_wrong)
                .is_err()
        );
        assert!(
            !root.join("b/blaze.wall_clock.txt").exists(),
            "a list with a wrong name writes nothing"
        );
    }

    #[test]
    fn a_finding_with_no_unit_fails_the_run_naming_it() {
        let mut run = Counted::default();
        run.unowned.push("blaze::wall_clock at src/a.rs: a clock read".to_owned());
        let made = blaze_core::temp::TempDir::for_test("blaze-clippy-unowned-");
        let root = made.path().to_owned();
        let error = run.finish(&root,"no-such-baselines", T, false, &[]).unwrap_err();
        assert!(
            error.contains("no unit") && error.contains("blaze::wall_clock at src/a.rs"),
            "{error}"
        );
    }

    /// A failed run's bless records the crates it checked whole and keeps the ones a failure
    /// cut short (the failed unit's and those behind it), naming each, and fails.
    #[test]
    fn a_bless_of_a_failed_run_records_what_it_measured_and_names_the_rest() {
        let made = blaze_core::temp::TempDir::for_test("blaze-clippy-partial-");
        let root = made.path().to_owned();
        std::fs::create_dir_all(root.join("b")).unwrap();
        std::fs::write(root.join("b/TARGET"), format!("{T}\n")).unwrap();
        let file = root.join("b/blaze.nopanic.txt");
        std::fs::write(&file, "a\tunmeasured\np\tunmeasured\nq\t7\n").unwrap();
        let mut run = counted(&[("blaze::nopanic", "a", 3), ("blaze::nopanic", "p", 1)]);
        run.unchecked(&Unchecked {
            open: Vec::new(),
            tests: Vec::new(),
            behind: vec![blaze_api::Behind {
                id: Some(4),
                why: "blaze panicked".to_owned(),
                unit: "p v0.0.0 (lib)".to_owned(),
                units: vec!["q v0.0.0 (lib)".to_owned()],
            }],
            stopped: false,
        });
        let error = run.finish(&root, "b", T, true, &[]).unwrap_err();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "a\t3\np\tunmeasured\nq\t7\n");
        assert!(
            error.contains("`p`: it failed: blaze panicked")
                && error.contains("`q`: `q v0.0.0 (lib)` was not checked behind `p v0.0.0 (lib)`")
                && error.contains("recorded: blaze::nopanic: baseline lowered"),
            "{error}"
        );
        // Without the failure the same counts are a whole run's: `q` at 0 is dropped.
        let whole = counted(&[("blaze::nopanic", "a", 3), ("blaze::nopanic", "p", 1)]);
        whole.finish(&root, "b", T, true, &[]).unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "a\t3\np\t1\n");
    }
}
