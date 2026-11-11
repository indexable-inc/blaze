//! The counts a lint at `warn` may only lower (AGENTS.md rule fix-through-blaze-lints): one file
//! per lint, `crates/blaze-lints/baselines/<tool>.<lint>.txt`, one `crate<TAB>count` line per
//! package of the workspace that has findings, sorted by package name. A run is held to them both
//! ways: a crate's count over its baseline, a crate the baseline does not have, or a lint with
//! findings and no baseline fails it, and so does a count under its baseline, which is stale until
//! a bless lowers it, so that what one change takes away the next cannot quietly add back. A bless
//! (after `blaze check --fix`, or a port by hand) only lowers counts and drops crates at zero; a
//! baseline that reaches empty is deleted, and its lint moves to `deny`.
//!
//! A line may hold `unmeasured` in place of a count (`crate<TAB>unmeasured`): the crate is held
//! at `warn` with no number yet, as every crate was when the workspace's ratchet turned its lints
//! to `deny` before a run had counted them. Any count passes it, and it is stale, as a count under
//! its baseline is, until a bless records the run's count in its place or drops the crate at zero.
//!
//! A `blaze` lint's file is also what holds a crate at `warn` under the workspace's
//! `[workspace.metadata.blaze.lints-ratchet]`, which denies the lint everywhere else
//! (blaze-units' `flags::ratchet_downgrades` reads the crate names of these files): dropping a
//! crate's line is what turns the lint to `deny` for it.
//!
//! The counts are of one target's cfg, which the directory's `TARGET` file names (a target
//! triple and a newline): a run for another target is refused, never compared.
//!
//! The counting itself is the client's (`blaze-cargo`'s `cli/baselines.rs`), from the findings a
//! run prints: this module is the files and the comparison.

use std::{collections::BTreeMap, path::Path};

/// A lint's counts, by crate (the package's name).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Baseline {
    pub counts: BTreeMap<String, Held>,
}

/// What a baseline holds of one crate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Held {
    /// At most this many findings, above 0.
    Count(u32),
    /// Held at `warn` with no count yet: any count passes until a bless records one.
    Unmeasured,
}

/// The text of [`Held::Unmeasured`] in a baseline's line.
pub const UNMEASURED: &str = "unmeasured";

#[derive(Debug, PartialEq, Eq)]
pub enum BaselineError {
    /// A line that is not `crate<TAB>count`, by its number.
    Line {
        file: String,
        line: usize,
        text: String,
    },
    /// A crate written twice, or out of order.
    Order {
        file: String,
        line: usize,
        krate: String,
    },
    /// A file of the baselines' directory that is not `<tool>.<lint>.txt` of a clippy or blaze
    /// lint this crate knows.
    Lint {
        file: String,
    },
    Read {
        file: String,
        error: String,
    },
    Write {
        file: String,
        error: String,
    },
}

impl std::fmt::Display for BaselineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BaselineError::Line { file, line, text } => {
                write!(
                    f,
                    "{file}:{line}: `{text}` is not `crate<TAB>count` with a count above 0, nor `crate<TAB>{UNMEASURED}`"
                )
            }
            BaselineError::Order { file, line, krate } => {
                write!(
                    f,
                    "{file}:{line}: `{krate}` is not after the crate above it: a baseline is sorted, each crate once"
                )
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

/// Where a run's count is off its baseline.
#[derive(Debug, PartialEq, Eq)]
pub enum Off {
    /// Over: a crate has more findings than its baseline.
    Grew { krate: String, was: u32, now: u32 },
    /// Over: a crate the baseline does not have has findings.
    New { krate: String, now: u32 },
    /// Over: the lint has findings and no baseline at all.
    Unbaselined { now: u32 },
    /// Under: a crate has fewer findings than its baseline (`now` may be 0), so the baseline is
    /// stale until a bless lowers it.
    Shrank { krate: String, was: u32, now: u32 },
    /// A crate held [`Held::Unmeasured`]: stale until a bless records the run's count.
    Unmeasured { krate: String, now: u32 },
}

impl Off {
    /// Whether the run is over the baseline here, which no bless takes away.
    pub fn over(&self) -> bool {
        !matches!(self, Off::Shrank { .. } | Off::Unmeasured { .. })
    }
}

impl std::fmt::Display for Off {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Off::Grew { krate, was, now } => {
                write!(f, "`{krate}`: {now} findings, over the baseline's {was}")
            }
            Off::New { krate, now } => {
                write!(f, "`{krate}`: {now} findings, in a crate the baseline does not have")
            }
            Off::Unbaselined { now } => write!(f, "{now} findings, and the lint has no baseline"),
            Off::Shrank { krate, was, now } => {
                write!(f, "`{krate}`: {now} findings, under the baseline's {was}")
            }
            Off::Unmeasured { krate, now } => {
                write!(f, "`{krate}`: {now} findings, where the baseline holds `{UNMEASURED}`")
            }
        }
    }
}

impl Baseline {
    /// The baseline of the file `name` whose text is `text`.
    pub fn parse(name: &str, text: &str) -> Result<Baseline, BaselineError> {
        let mut counts = BTreeMap::new();
        let mut last: Option<&str> = None;
        for (at, line) in text.lines().enumerate() {
            let line_error = || BaselineError::Line {
                file: name.to_owned(),
                line: at + 1,
                text: line.to_owned(),
            };
            let (krate, count) = line.split_once('\t').ok_or_else(line_error)?;
            let held = match count {
                UNMEASURED => Held::Unmeasured,
                count => Held::Count(
                    count.parse().ok().filter(|it| *it > 0).ok_or_else(line_error)?,
                ),
            };
            if krate.is_empty() {
                return Err(line_error());
            }
            if last.is_some_and(|it| it >= krate) {
                return Err(BaselineError::Order {
                    file: name.to_owned(),
                    line: at + 1,
                    krate: krate.to_owned(),
                });
            }
            last = Some(krate);
            counts.insert(krate.to_owned(), held);
        }
        Ok(Baseline { counts })
    }

    /// The file's text: what [`Baseline::parse`] reads back.
    pub fn render(&self) -> String {
        let mut text = String::new();
        for (krate, held) in &self.counts {
            text.push_str(krate);
            text.push('\t');
            match held {
                Held::Count(count) => text.push_str(&count.to_string()),
                Held::Unmeasured => text.push_str(UNMEASURED),
            }
            text.push('\n');
        }
        text
    }

    /// Where `now`, the counts of a run, are off the baseline, in the order of the crates.
    pub fn off(&self, now: &BTreeMap<String, u32>) -> Vec<Off> {
        let mut off = Vec::new();
        let mut was = self.counts.iter().peekable();
        let mut now = now.iter().filter(|(_, count)| **count > 0).peekable();
        loop {
            let order = match (was.peek(), now.peek()) {
                (None, None) => return off,
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (Some((a, _)), Some((b, _))) => a.cmp(b),
            };
            match order {
                std::cmp::Ordering::Less => {
                    let (krate, &was) = was.next().unwrap();
                    off.push(match was {
                        Held::Count(was) => Off::Shrank { krate: krate.clone(), was, now: 0 },
                        Held::Unmeasured => Off::Unmeasured { krate: krate.clone(), now: 0 },
                    });
                }
                std::cmp::Ordering::Greater => {
                    let (krate, &now) = now.next().unwrap();
                    off.push(Off::New { krate: krate.clone(), now });
                }
                std::cmp::Ordering::Equal => {
                    let ((krate, &was), (_, &now)) = (was.next().unwrap(), now.next().unwrap());
                    match was {
                        Held::Count(was) if now > was => {
                            off.push(Off::Grew { krate: krate.clone(), was, now })
                        }
                        Held::Count(was) if now < was => {
                            off.push(Off::Shrank { krate: krate.clone(), was, now })
                        }
                        Held::Count(_) => {}
                        Held::Unmeasured => off.push(Off::Unmeasured { krate: krate.clone(), now }),
                    }
                }
            }
        }
    }

    /// The baseline lowered to `now`: each crate's count at most what it was, an unmeasured
    /// crate's the run's, a crate at zero dropped, and none added or raised.
    pub fn blessed(&self, now: &BTreeMap<String, u32>) -> Baseline {
        let counts = self
            .counts
            .iter()
            .filter_map(|(krate, &was)| {
                let now = now.get(krate).copied().unwrap_or(0);
                let now = match was {
                    Held::Count(was) => now.min(was),
                    Held::Unmeasured => now,
                };
                (now > 0).then(|| (krate.clone(), Held::Count(now)))
            })
            .collect();
        Baseline { counts }
    }
}

/// The file of a baselines directory that names the target its counts are of.
pub const TARGET_FILE: &str = "TARGET";

/// The target the baselines in `dir` count, from its [`TARGET_FILE`]; none when it has none.
pub fn target_of(dir: &Path) -> Result<Option<String>, BaselineError> {
    let path = dir.join(TARGET_FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(BaselineError::Read {
                file: path.display().to_string(),
                error: error.to_string(),
            });
        }
    };
    let target = text.strip_suffix('\n').unwrap_or(&text);
    if target.is_empty() || target.contains(char::is_whitespace) {
        let text = text.clone(); // clone: the error's text, once
        return Err(BaselineError::Line { file: path.display().to_string(), line: 1, text });
    }
    Ok(Some(target.to_owned()))
}

/// The file name of `lint`'s baseline: `clippy.disallowed_methods.txt` for
/// `clippy::disallowed_methods`.
pub fn file_name(lint: &str) -> String {
    format!("{}.txt", lint.replace("::", "."))
}

/// The lint whose baseline is the file `name`, if it is `<tool>.<lint>.txt` of a clippy lint this
/// crate knows or a blaze lint of the compile's registry: this crate's, the front end's
/// (`nopanic`, `borrow_workaround`) and the daemon's reachability lints (`dead`, `test_only`,
/// `cfg_only`), all of which a run counts.
pub fn lint_of(name: &str) -> Option<String> {
    use blaze_diagnostics::lints::registry::{Named, named};
    let (tool, lint) = name.strip_suffix(".txt")?.split_once('.')?;
    let known = match tool {
        "clippy" => crate::registry::lint(lint).is_some(),
        // alloc: a lint's full name, once per baseline file read
        "blaze" => matches!(named(&format!("blaze::{lint}")), Named::Lint(_)),
        _ => false,
    };
    known.then(|| format!("{tool}::{lint}"))
}

/// The baselines in `dir`, by lint (`clippy::disallowed_types`); none when `dir` does not exist.
pub fn read(dir: &Path) -> Result<BTreeMap<String, Baseline>, BaselineError> {
    let shown = |path: &Path| path.display().to_string();
    let read_error = |path: &Path, error: std::io::Error| BaselineError::Read {
        file: shown(path),
        error: error.to_string(),
    };
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => return Err(read_error(dir, error)),
    };
    let mut baselines = BTreeMap::new();
    for entry in entries {
        let path = entry.map_err(|error| read_error(dir, error))?.path();
        if path.file_name().is_some_and(|it| it == TARGET_FILE) {
            continue;
        }
        let lint = path.file_name().and_then(|it| it.to_str()).and_then(lint_of);
        let lint = lint.ok_or_else(|| BaselineError::Lint { file: shown(&path) })?;
        let text = std::fs::read_to_string(&path).map_err(|error| read_error(&path, error))?;
        baselines.insert(lint, Baseline::parse(&shown(&path), &text)?);
    }
    Ok(baselines)
}

/// Where `counts`, a run's by lint then crate, are off the baselines in `dir`, by lint, then
/// crate: every lint of the run's and every lint with a baseline. The run must be of the whole
/// workspace, every target: a crate it did not check reads as at zero.
pub fn check(dir: &Path, counts: &Counts) -> Result<Vec<(String, Off)>, BaselineError> {
    let baselines = read(dir)?;
    let none = BTreeMap::new();
    // alloc: the run's and the baselines' lint names, once a run
    let lints: std::collections::BTreeSet<&String> =
        baselines.keys().chain(counts.keys()).collect();
    let mut off = Vec::new();
    for lint in lints {
        let now = counts.get(lint).unwrap_or(&none);
        match baselines.get(lint) {
            Some(baseline) => {
                off.extend(baseline.off(now).into_iter().map(|it| (lint.clone(), it)))
            }
            None => {
                let now = now.values().sum::<u32>();
                if now > 0 {
                    off.push((lint.clone(), Off::Unbaselined { now }));
                }
            }
        }
    }
    Ok(off)
}

/// What a bless did to one lint's baseline.
#[derive(Debug, PartialEq, Eq)]
pub enum Blessed {
    /// Lowered to the run's counts and written.
    Lowered,
    /// Lowered to nothing and deleted: the lint can move to `deny`.
    Emptied,
}

/// Lowers each baseline in `dir` to `counts`, writes those that changed and deletes those that
/// reach empty, returning what it did, by lint. `counts` must be of a run over the whole
/// workspace, every target: a crate it did not check reads as at zero and is dropped.
pub fn bless(dir: &Path, counts: &Counts) -> Result<Vec<(String, Blessed)>, BaselineError> {
    let none = BTreeMap::new();
    let mut done = Vec::new();
    for (lint, baseline) in read(dir)? {
        let blessed = baseline.blessed(counts.get(&lint).unwrap_or(&none));
        if blessed == baseline {
            continue;
        }
        let path = dir.join(file_name(&lint));
        let write_error = |error: std::io::Error| BaselineError::Write {
            file: path.display().to_string(),
            error: error.to_string(),
        };
        if blessed.counts.is_empty() {
            std::fs::remove_file(&path).map_err(write_error)?;
            done.push((lint, Blessed::Emptied));
        } else {
            std::fs::write(&path, blessed.render()).map_err(write_error)?;
            done.push((lint, Blessed::Lowered));
        }
    }
    Ok(done)
}

/// A run's counts, by lint, then by crate.
pub type Counts = BTreeMap<String, BTreeMap<String, u32>>;

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, u32)]) -> BTreeMap<String, u32> {
        pairs.iter().map(|(krate, count)| ((*krate).to_owned(), *count)).collect()
    }

    fn held(pairs: &[(&str, u32)]) -> BTreeMap<String, Held> {
        pairs.iter().map(|(krate, count)| ((*krate).to_owned(), Held::Count(*count))).collect()
    }

    #[test]
    fn a_baseline_reads_back_what_it_writes() {
        let text = "blaze-cargo\t3\nhir-def\t1\n";
        let baseline = Baseline::parse("clippy.disallowed_types.txt", text).unwrap();
        assert_eq!(baseline.counts, held(&[("blaze-cargo", 3), ("hir-def", 1)]));
        assert_eq!(baseline.render(), text);
        let text = "blaze-cargo\t3\nhir-def\tunmeasured\n";
        let baseline = Baseline::parse("blaze.std_use.txt", text).unwrap();
        assert_eq!(baseline.counts.get("hir-def"), Some(&Held::Unmeasured));
        assert_eq!(baseline.render(), text);
        assert_eq!(file_name("clippy::disallowed_types"), "clippy.disallowed_types.txt");
        assert_eq!(file_name("blaze::held_across_wait"), "blaze.held_across_wait.txt");
        // Every blaze lint a run counts has a file: the daemon's reachability lints too, a
        // group none.
        assert_eq!(lint_of("blaze.dead.txt").as_deref(), Some("blaze::dead"));
        assert_eq!(lint_of("blaze.std_use.txt").as_deref(), Some("blaze::std_use"));
        assert_eq!(lint_of("blaze.internal.txt"), None);
    }

    #[test]
    fn a_line_that_does_not_read_or_a_crate_out_of_order_is_an_error_naming_it() {
        for (text, line) in
            [("a\t0\n", 1), ("a\t1\nb 2\n", 2), ("\t1\n", 1), ("a\tunmeasure\n", 1), ("\tunmeasured\n", 1)]
        {
            let error = Baseline::parse("x.txt", text).unwrap_err();
            assert!(
                matches!(&error, BaselineError::Line { line: at, .. } if *at == line),
                "{text:?}: {error}"
            );
        }
        for text in ["b\t1\na\t1\n", "a\t1\na\t2\n"] {
            let error = Baseline::parse("x.txt", text).unwrap_err();
            assert!(matches!(&error, BaselineError::Order { line: 2, .. }), "{text:?}: {error}");
        }
    }

    #[test]
    fn a_count_off_its_baseline_either_way_is_named_and_a_bless_only_lowers() {
        let baseline = Baseline { counts: held(&[("a", 3), ("b", 2), ("c", 1), ("e", 4)]) };
        let now = map(&[("a", 4), ("b", 1), ("d", 2), ("e", 4), ("f", 0)]);
        assert_eq!(
            baseline.off(&now),
            [
                Off::Grew { krate: "a".to_owned(), was: 3, now: 4 },
                Off::Shrank { krate: "b".to_owned(), was: 2, now: 1 },
                Off::Shrank { krate: "c".to_owned(), was: 1, now: 0 },
                Off::New { krate: "d".to_owned(), now: 2 },
            ]
        );
        assert_eq!(baseline.blessed(&now).counts, held(&[("a", 3), ("b", 1), ("e", 4)]));
        assert!(baseline.off(&map(&[("a", 3), ("b", 2), ("c", 1), ("e", 4)])).is_empty());
        assert!(baseline.off(&map(&[("a", 4)])).iter().any(Off::over));
        assert!(!baseline.off(&map(&[("a", 2)])).iter().any(Off::over));
    }

    /// The invariant: an unmeasured crate passes any count and is never over, is stale until a
    /// bless, and a bless gives it the run's count or drops it at zero, so the ratchet's first
    /// measured run only turns `unmeasured` into numbers or into `deny`.
    #[test]
    fn an_unmeasured_crate_takes_any_count_until_a_bless_records_it() {
        let mut counts = held(&[("a", 2)]);
        counts.insert("b".to_owned(), Held::Unmeasured);
        counts.insert("c".to_owned(), Held::Unmeasured);
        let baseline = Baseline { counts };
        let now = map(&[("a", 2), ("b", 40)]);
        assert_eq!(
            baseline.off(&now),
            [
                Off::Unmeasured { krate: "b".to_owned(), now: 40 },
                Off::Unmeasured { krate: "c".to_owned(), now: 0 },
            ]
        );
        assert!(!baseline.off(&now).iter().any(Off::over), "no count is over `unmeasured`");
        assert_eq!(baseline.blessed(&now).counts, held(&[("a", 2), ("b", 40)]));
        // The control: once measured, the same count over it is over.
        assert!(baseline.blessed(&now).off(&map(&[("b", 41)])).iter().any(Off::over));
    }

    #[test]
    fn a_run_is_checked_against_and_blesses_the_baselines_of_a_directory() {
        let dir =
            std::env::temp_dir().join(format!("blaze-lints-baselines-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let types = dir.join("clippy.disallowed_types.txt");
        std::fs::write(&types, "a\t3\nb\t2\n").unwrap();
        let clock = dir.join("blaze.wall_clock.txt");
        std::fs::write(&clock, "c\t1\n").unwrap();
        let counts: Counts = [
            ("clippy::disallowed_types".to_owned(), map(&[("a", 4), ("b", 1)])),
            ("clippy::disallowed_methods".to_owned(), map(&[("z", 9)])),
        ]
        .into_iter()
        .collect();
        let types_lint = "clippy::disallowed_types".to_owned();
        assert_eq!(
            check(&dir, &counts).unwrap(),
            [
                (
                    "blaze::wall_clock".to_owned(),
                    Off::Shrank { krate: "c".to_owned(), was: 1, now: 0 }
                ),
                ("clippy::disallowed_methods".to_owned(), Off::Unbaselined { now: 9 }),
                (types_lint.clone(), Off::Grew { krate: "a".to_owned(), was: 3, now: 4 }),
                (types_lint.clone(), Off::Shrank { krate: "b".to_owned(), was: 2, now: 1 }),
            ]
        );
        assert_eq!(
            bless(&dir, &counts).unwrap(),
            [
                ("blaze::wall_clock".to_owned(), Blessed::Emptied),
                (types_lint.clone(), Blessed::Lowered)
            ]
        );
        assert_eq!(std::fs::read_to_string(&types).unwrap(), "a\t3\nb\t1\n");
        assert!(!clock.exists(), "a baseline lowered to nothing is deleted");
        assert!(bless(&dir, &counts).unwrap().is_empty());

        assert_eq!(target_of(&dir).unwrap(), None);
        std::fs::write(dir.join(TARGET_FILE), "x86_64-unknown-linux-gnu\n").unwrap();
        assert_eq!(target_of(&dir).unwrap().as_deref(), Some("x86_64-unknown-linux-gnu"));
        assert!(check(&dir, &Counts::new()).is_ok(), "the target file is not a lint's");
        std::fs::write(dir.join(TARGET_FILE), "a b\n").unwrap();
        assert!(matches!(target_of(&dir), Err(BaselineError::Line { .. })));
        std::fs::write(dir.join("clippy.no_such_lint.txt"), "").unwrap();
        assert!(matches!(check(&dir, &counts), Err(BaselineError::Lint { .. })));
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(check(&dir, &Counts::new()).unwrap(), []);
    }
}
