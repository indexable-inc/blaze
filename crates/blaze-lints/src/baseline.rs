//! The counts a lint at `warn` may only lower: one file per lint,
//! `crates/blaze-lints/baselines/<tool>.<lint>.txt`, one `path<TAB>count` line per file of the
//! workspace that has findings, sorted by path. A check fails when a file's count grows or a file
//! not in the baseline has a finding; a bless (after `blaze check --fix`, or a port by hand) only lowers
//! counts and drops files at zero. A lint whose baseline is empty moves to `deny` and its file is
//! deleted.

use std::{collections::BTreeMap, path::Path};

/// A lint's counts, by file.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Baseline {
    pub counts: BTreeMap<String, u32>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum BaselineError {
    /// A line that is not `path<TAB>count`, by its number.
    Line { file: String, line: usize, text: String },
    /// A path written twice, or out of order.
    Order { file: String, line: usize, path: String },
    /// A file of the baselines' directory that is not `<tool>.<lint>.txt` of a clippy or blaze
    /// lint this crate knows.
    Lint { file: String },
    Read { file: String, error: String },
    Write { file: String, error: String },
}

impl std::fmt::Display for BaselineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BaselineError::Line { file, line, text } => {
                write!(f, "{file}:{line}: `{text}` is not `path<TAB>count` with a count above 0")
            }
            BaselineError::Order { file, line, path } => {
                write!(f, "{file}:{line}: `{path}` is not after the path above it: a baseline is sorted, each path once")
            }
            BaselineError::Lint { file } => {
                write!(f, "{file}: not `<tool>.<lint>.txt` of a clippy or blaze lint blaze knows")
            }
            BaselineError::Read { file, error } => write!(f, "{file}: cannot be read: {error}"),
            BaselineError::Write { file, error } => write!(f, "{file}: cannot be written: {error}"),
        }
    }
}

impl std::error::Error for BaselineError {}

/// Where a count is over its baseline.
#[derive(Debug, PartialEq, Eq)]
pub enum Over {
    Grew { path: String, was: u32, now: u32 },
    New { path: String, now: u32 },
}

impl std::fmt::Display for Over {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Over::Grew { path, was, now } => write!(f, "{path}: {now} findings, over the baseline's {was}"),
            Over::New { path, now } => write!(f, "{path}: {now} findings, in a file the baseline does not have"),
        }
    }
}

impl Baseline {
    /// The baseline of the file `name` whose text is `text`.
    pub fn parse(name: &str, text: &str) -> Result<Baseline, BaselineError> {
        let mut counts = BTreeMap::new();
        let mut last: Option<&str> = None;
        for (at, line) in text.lines().enumerate() {
            let line_error = || BaselineError::Line { file: name.to_owned(), line: at + 1, text: line.to_owned() };
            let (path, count) = line.split_once('\t').ok_or_else(line_error)?;
            let count: u32 = count.parse().ok().filter(|it| *it > 0).ok_or_else(line_error)?;
            if path.is_empty() {
                return Err(line_error());
            }
            if last.is_some_and(|it| it >= path) {
                return Err(BaselineError::Order { file: name.to_owned(), line: at + 1, path: path.to_owned() });
            }
            last = Some(path);
            counts.insert(path.to_owned(), count);
        }
        Ok(Baseline { counts })
    }

    /// The file's text: what [`Baseline::parse`] reads back.
    pub fn render(&self) -> String {
        let mut text = String::new();
        for (path, count) in &self.counts {
            text.push_str(path);
            text.push('\t');
            text.push_str(&count.to_string());
            text.push('\n');
        }
        text
    }

    /// Where `now`, the counts of a run, are over the baseline, in the order of the paths.
    pub fn over(&self, now: &BTreeMap<String, u32>) -> Vec<Over> {
        let mut over = Vec::new();
        for (path, &now) in now {
            match self.counts.get(path) {
                None if now > 0 => over.push(Over::New { path: path.clone(), now }),
                Some(&was) if now > was => over.push(Over::Grew { path: path.clone(), was, now }),
                _ => {}
            }
        }
        over
    }

    /// The baseline lowered to `now`: each file's count at most what it was, a file at zero
    /// dropped, and none added or raised.
    pub fn blessed(&self, now: &BTreeMap<String, u32>) -> Baseline {
        let counts = self
            .counts
            .iter()
            .filter_map(|(path, &was)| {
                let now = now.get(path).copied().unwrap_or(0).min(was);
                (now > 0).then(|| (path.clone(), now))
            })
            .collect();
        Baseline { counts }
    }
}

/// The file name of `lint`'s baseline: `clippy.disallowed_methods.txt` for
/// `clippy::disallowed_methods`.
pub fn file_name(lint: &str) -> String {
    format!("{}.txt", lint.replace("::", "."))
}

/// The lint whose baseline is the file `name`, if it is `<tool>.<lint>.txt` of a clippy or blaze
/// lint this crate knows.
pub fn lint_of(name: &str) -> Option<String> {
    let (tool, lint) = name.strip_suffix(".txt")?.split_once('.')?;
    let known = match tool {
        "clippy" => crate::registry::lint(lint).is_some(),
        "blaze" => crate::blaze::Lint::ALL.iter().any(|it| it.name() == lint),
        _ => false,
    };
    known.then(|| format!("{tool}::{lint}"))
}

/// The baselines in `dir`, by lint (`clippy::disallowed_types`); none when `dir` does not exist.
pub fn read(dir: &Path) -> Result<BTreeMap<String, Baseline>, BaselineError> {
    let shown = |path: &Path| path.display().to_string();
    let read_error =
        |path: &Path, error: std::io::Error| BaselineError::Read { file: shown(path), error: error.to_string() };
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => return Err(read_error(dir, error)),
    };
    let mut baselines = BTreeMap::new();
    for entry in entries {
        let path = entry.map_err(|error| read_error(dir, error))?.path();
        let lint = path.file_name().and_then(|it| it.to_str()).and_then(lint_of);
        let lint = lint.ok_or_else(|| BaselineError::Lint { file: shown(&path) })?;
        let text = std::fs::read_to_string(&path).map_err(|error| read_error(&path, error))?;
        baselines.insert(lint, Baseline::parse(&shown(&path), &text)?);
    }
    Ok(baselines)
}

/// Where `counts`, a run's by lint then file as [`counts`] makes them, are over the baselines in
/// `dir`, by lint. A lint without a baseline is not checked here: its level decides.
pub fn check(dir: &Path, counts: &Counts) -> Result<Vec<(String, Over)>, BaselineError> {
    let none = BTreeMap::new();
    let mut over = Vec::new();
    for (lint, baseline) in read(dir)? {
        let now = counts.get(&lint).unwrap_or(&none);
        over.extend(baseline.over(now).into_iter().map(|it| (lint.clone(), it)));
    }
    Ok(over)
}

/// Lowers each baseline in `dir` to `counts` and writes those that changed, returning their
/// lints. `counts` must be of a run over the whole workspace: a file it did not lint reads as at
/// zero and is dropped from the baseline.
pub fn bless(dir: &Path, counts: &Counts) -> Result<Vec<String>, BaselineError> {
    let none = BTreeMap::new();
    let mut written = Vec::new();
    for (lint, baseline) in read(dir)? {
        let blessed = baseline.blessed(counts.get(&lint).unwrap_or(&none));
        if blessed == baseline {
            continue;
        }
        let path = dir.join(file_name(&lint));
        std::fs::write(&path, blessed.render())
            .map_err(|error| BaselineError::Write { file: path.display().to_string(), error: error.to_string() })?;
        written.push(lint);
    }
    Ok(written)
}

/// A run's counts, by lint, then by file.
pub type Counts = BTreeMap<String, BTreeMap<String, u32>>;

/// The counts of `found` by lint, then by the file of each finding's first primary span.
pub fn counts<'f>(found: impl IntoIterator<Item = &'f crate::Found>) -> Counts {
    let mut counts = Counts::new();
    for found in found {
        let diagnostic = &found.diagnostic;
        let (Some(lint), Some(at)) = (diagnostic.lint_name(), diagnostic.span.primary.first()) else { continue };
        *counts.entry(lint.to_owned()).or_default().entry(at.file.clone()).or_default() += 1;
    }
    counts
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, u32)]) -> BTreeMap<String, u32> {
        pairs.iter().map(|(path, count)| ((*path).to_owned(), *count)).collect()
    }

    #[test]
    fn a_baseline_reads_back_what_it_writes() {
        let text = "crates/a/src/lib.rs\t3\ncrates/b/src/main.rs\t1\n";
        let baseline = Baseline::parse("clippy.disallowed_types.txt", text).unwrap();
        assert_eq!(baseline.counts, map(&[("crates/a/src/lib.rs", 3), ("crates/b/src/main.rs", 1)]));
        assert_eq!(baseline.render(), text);
        assert_eq!(file_name("clippy::disallowed_types"), "clippy.disallowed_types.txt");
        assert_eq!(file_name("blaze::held_across_wait"), "blaze.held_across_wait.txt");
    }

    #[test]
    fn a_line_that_does_not_read_or_a_path_out_of_order_is_an_error_naming_it() {
        for (text, line) in [("a\t0\n", 1), ("a\t1\nb 2\n", 2), ("\t1\n", 1)] {
            let error = Baseline::parse("x.txt", text).unwrap_err();
            assert!(matches!(&error, BaselineError::Line { line: at, .. } if *at == line), "{text:?}: {error}");
        }
        for text in ["b\t1\na\t1\n", "a\t1\na\t2\n"] {
            let error = Baseline::parse("x.txt", text).unwrap_err();
            assert!(matches!(&error, BaselineError::Order { line: 2, .. }), "{text:?}: {error}");
        }
    }

    #[test]
    fn a_count_may_only_shrink_and_a_bless_only_lowers() {
        let baseline = Baseline { counts: map(&[("a", 3), ("b", 2), ("c", 1)]) };
        let now = map(&[("a", 4), ("b", 1), ("d", 2)]);
        assert_eq!(
            baseline.over(&now),
            [Over::Grew { path: "a".to_owned(), was: 3, now: 4 }, Over::New { path: "d".to_owned(), now: 2 }]
        );
        assert_eq!(baseline.blessed(&now).counts, map(&[("a", 3), ("b", 1)]));
        assert!(baseline.over(&map(&[("a", 3), ("b", 0)])).is_empty());
    }

    #[test]
    fn a_run_is_checked_against_and_blesses_the_baselines_of_a_directory() {
        let dir = std::env::temp_dir().join(format!("blaze-lints-baselines-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let types = dir.join("clippy.disallowed_types.txt");
        std::fs::write(&types, "a\t3\nb\t2\n").unwrap();
        std::fs::write(dir.join("blaze.wall_clock.txt"), "c\t1\n").unwrap();
        let counts: Counts = [
            ("clippy::disallowed_types".to_owned(), map(&[("a", 4), ("b", 1)])),
            ("clippy::disallowed_methods".to_owned(), map(&[("z", 9)])),
        ]
        .into_iter()
        .collect();
        assert_eq!(
            check(&dir, &counts).unwrap(),
            [("clippy::disallowed_types".to_owned(), Over::Grew { path: "a".to_owned(), was: 3, now: 4 })]
        );
        assert_eq!(bless(&dir, &counts).unwrap(), ["blaze::wall_clock", "clippy::disallowed_types"]);
        assert_eq!(std::fs::read_to_string(&types).unwrap(), "a\t3\nb\t1\n");
        assert_eq!(std::fs::read_to_string(dir.join("blaze.wall_clock.txt")).unwrap(), "");
        assert!(bless(&dir, &counts).unwrap().is_empty());

        std::fs::write(dir.join("clippy.no_such_lint.txt"), "").unwrap();
        assert!(matches!(check(&dir, &counts), Err(BaselineError::Lint { .. })));
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(check(&dir, &counts).unwrap().is_empty());
    }
}
