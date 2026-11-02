//! `blaze check --baselines DIR`: the run's findings of clippy's lints and blaze's, counted by
//! lint and file, checked against the baselines in `DIR` (`blaze_lints::baseline`), which
//! `--bless-baselines` lowers to the run's and `--new-baseline LINT` starts at it. A finding
//! two units of one crate give (its lib and its lib test) counts once.

use std::path::Path;

use blaze_api::DiagnosticRef;
use blaze_hash::HashSet;
use blaze_lints::baseline::{self, Baseline, Counts};

/// The findings a run printed, counted.
#[derive(Default)]
pub struct Counted {
    /// Each finding once, by lint and its first primary span's file and bytes.
    seen: HashSet<(String, String, u32, u32)>,
    counts: Counts,
}

impl Counted {
    pub fn add(&mut self, diagnostic: &DiagnosticRef<'_>) {
        let Some(lint) = diagnostic.code.filter(|it| it.starts_with("clippy::") || it.starts_with("blaze::")) else {
            return;
        };
        let Some(at) = diagnostic.spans.iter().find(|it| it.primary) else { return };
        // alloc: a finding's key, once per finding of a run
        if self.seen.insert((lint.to_owned(), at.file.to_owned(), at.byte_start, at.byte_end)) {
            *self.counts.entry(lint.to_owned()).or_default().entry(at.file.to_owned()).or_default() += 1;
        }
    }

    /// The baselines in `dir`, from the checkout's `root` when relative, checked against the
    /// run, then lowered to it and each of `new` started at it when `bless`: what to say of
    /// each, or the error naming each count over its baseline.
    pub fn finish(self, root: &Path, dir: &str, bless: bool, new: &[String]) -> Result<Vec<String>, String> {
        let dir = root.join(dir);
        let shown = dir.display();
        let mut said = Vec::new();
        for lint in new {
            let known = baseline::lint_of(&baseline::file_name(lint)).is_some_and(|it| it == *lint);
            if !known {
                return Err(format!("`--new-baseline {lint}`: no clippy or blaze lint blaze has is named so"));
            }
            let path = dir.join(baseline::file_name(lint));
            if path.exists() {
                return Err(format!("`--new-baseline {lint}`: {} is there already", path.display()));
            }
        }
        for lint in new {
            let path = dir.join(baseline::file_name(lint));
            let counts = self.counts.get(lint).cloned().unwrap_or_default(); // clone: one lint's counts, once
            let text = Baseline { counts }.render();
            std::fs::create_dir_all(&dir).map_err(|error| format!("{shown}: {error}"))?;
            std::fs::write(&path, text).map_err(|error| format!("{}: {error}", path.display()))?;
            said.push(format!("{lint}: baseline started at this run's counts"));
        }
        if bless {
            let blessed = baseline::bless(&dir, &self.counts).map_err(|it| it.to_string())?;
            said.extend(blessed.into_iter().map(|lint| format!("{lint}: baseline lowered to this run's counts")));
            return Ok(said);
        }
        let over = baseline::check(&dir, &self.counts).map_err(|it| it.to_string())?;
        if over.is_empty() {
            return Ok(said);
        }
        let lines: Vec<String> = over.iter().map(|(lint, over)| format!("\n  {lint}: {over}")).collect();
        Err(format!("the run's findings are over the baselines in {shown}:{}", lines.concat()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn counted(counts: &[(&str, &str, u32)]) -> Counted {
        let mut it = Counted::default();
        for (lint, path, count) in counts {
            it.counts.entry((*lint).to_owned()).or_default().insert((*path).to_owned(), *count);
        }
        it
    }

    #[test]
    fn a_baseline_is_started_checked_and_lowered() {
        let root = std::env::temp_dir().join(format!("blaze-clippy-baselines-{}", std::process::id()));
        let lint = "clippy::disallowed_types".to_owned();
        let run = counted(&[("clippy::disallowed_types", "src/a.rs", 2), ("clippy::disallowed_types", "src/b.rs", 1)]);
        let said = run.finish(&root, "b", true, std::slice::from_ref(&lint)).unwrap();
        assert_eq!(said, ["clippy::disallowed_types: baseline started at this run's counts"]);
        assert_eq!(std::fs::read_to_string(root.join("b/clippy.disallowed_types.txt")).unwrap(), "src/a.rs\t2\nsrc/b.rs\t1\n");
        let grew = counted(&[("clippy::disallowed_types", "src/a.rs", 3)]).finish(&root, "b", false, &[]).unwrap_err();
        assert!(grew.contains("src/a.rs: 3 findings, over the baseline's 2"), "{grew}");
        let fewer = counted(&[("clippy::disallowed_types", "src/a.rs", 1)]);
        assert_eq!(fewer.finish(&root, "b", true, &[]).unwrap(), ["clippy::disallowed_types: baseline lowered to this run's counts"]);
        assert_eq!(std::fs::read_to_string(root.join("b/clippy.disallowed_types.txt")).unwrap(), "src/a.rs\t1\n");
        let again = counted(&[]).finish(&root, "b", false, std::slice::from_ref(&lint)).unwrap_err();
        assert!(again.contains("is there already"), "{again}");
        let one_wrong = ["blaze::wall_clock".to_owned(), "clippy::no_such_lint".to_owned()];
        assert!(counted(&[]).finish(&root, "b", false, &one_wrong).is_err());
        assert!(!root.join("b/blaze.wall_clock.txt").exists(), "a list with a wrong name writes nothing");
        std::fs::remove_dir_all(&root).unwrap();
    }
}
