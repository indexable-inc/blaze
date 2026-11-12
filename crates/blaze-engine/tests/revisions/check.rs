//! A revision's [`Observed`] held to its [`Expect`] and to a clean build of the same
//! revision, and a test's verdict: every failure names its revision, its check, what was
//! wanted and what happened; a failure the test lists as known is said and passes; a known
//! failure that no longer fails is an error, so the list stays true.

use std::{fmt::Write as _, fs, io::Write as _};

use super::spec::{Observed, Revision, Test};

pub struct Failure {
    pub check: &'static str,
    pub wanted: String,
    pub got: String,
}

fn short(text: &str) -> String {
    // A build's progress lines are not what a reader of a failure is after.
    let kept: Vec<&str> = text.lines().filter(|line| !line.trim_start().starts_with("Compiling ")).collect();
    let text = &kept.join("\n");
    match text.len() > 1500 {
        true => format!("{}\n.. ({} bytes)", &text[..text.floor_char_boundary(1500)], text.len()),
        false => text.to_owned(),
    }
}

fn most(check: &'static str, bound: Option<u64>, got: Option<u64>, what: &str, into: &mut Vec<Failure>) {
    let Some(bound) = bound else { return };
    match got {
        Some(got) if got <= bound => {}
        Some(got) => into.push(Failure { check, wanted: format!("at most {bound} {what}"), got: got.to_string() }),
        None => into.push(Failure { check, wanted: format!("at most {bound} {what}"), got: "the driver does not see them".to_owned() }),
    }
}

/// The checks the simulated driver does not see, because its pipeline ends at code records:
/// nothing is linked or run, no unit is counted, and no engine memo counts are read. It
/// skips them and says so in each row; every other check it holds.
pub const SIM_BLIND: &[&str] = &["prints", "says", "max_units", "max_executed", "max_verified", "max_ms", "noop", "link"];

/// Each check of `revision` that `seen` fails, but those in `blind` (what the driver cannot
/// see: said, not taken as met). `clean`: the same revision built from nothing by the same
/// driver.
pub fn held(revision: &Revision, seen: &Observed, clean: Option<&Observed>, blind: &[&str]) -> Vec<Failure> {
    let mut failed = all(revision, seen, clean);
    failed.retain(|it| !blind.contains(&it.check));
    failed
}

fn all(revision: &Revision, seen: &Observed, clean: Option<&Observed>) -> Vec<Failure> {
    let expect = &revision.expect;
    let mut failed = Vec::new();
    match &expect.fails {
        None if !seen.ok => failed.push(Failure {
            check: "outcome",
            wanted: format!("`{}` succeeds", revision.action.word()),
            got: format!("it failed:\n{}", short(&seen.said)),
        }),
        Some(says) if seen.ok => failed.push(Failure {
            check: "outcome",
            wanted: format!("`{}` fails saying `{says}`", revision.action.word()),
            got: "it succeeded".to_owned(),
        }),
        Some(says) if !seen.said.contains(says.as_str()) && !seen.stdout.contains(says.as_str()) => {
            failed.push(Failure {
                check: "outcome",
                wanted: format!("`{}` fails saying `{says}`", revision.action.word()),
                got: format!("it failed saying:\n{}", short(&seen.said)),
            })
        }
        _ => {}
    }
    if !failed.is_empty() {
        // The action did not end as stated: what it made and printed is not held to
        // anything, and the one failure is the finding.
        if seen.panics > 0 {
            failed.push(Failure {
                check: "panics",
                wanted: "no panic caught".to_owned(),
                got: format!("{} caught, the first: {}", seen.panics, seen.first_panic),
            });
        }
        return failed;
    }
    if let Some(prints) = &expect.prints
        && seen.stdout != *prints
    {
        failed.push(Failure { check: "prints", wanted: format!("{prints:?}"), got: format!("{:?}", short(&seen.stdout)) });
    }
    for says in &expect.says {
        if !seen.stdout.contains(says.as_str()) && !seen.said.contains(says.as_str()) {
            failed.push(Failure {
                check: "says",
                wanted: format!("{says:?} in the output"),
                got: format!("{}\n{}", short(&seen.stdout), short(&seen.said)),
            });
        }
    }
    let asks_redone = expect.redone.is_some() || !expect.redone_has.is_empty() || !expect.reused.is_empty();
    match (&seen.redone, asks_redone) {
        (_, false) => {}
        (None, true) => failed.push(Failure {
            check: "redone",
            wanted: "the functions made again".to_owned(),
            got: "the driver does not see them".to_owned(),
        }),
        (Some(redone), true) => {
            if let Some(exactly) = &expect.redone {
                let mut wanted = exactly.clone(); // clone: sorted to compare
                wanted.sort();
                if *redone != wanted {
                    failed.push(Failure { check: "redone", wanted: format!("exactly {wanted:?} made again"), got: format!("{redone:?}") });
                }
            }
            let missing: Vec<&String> = expect.redone_has.iter().filter(|it| !redone.contains(it)).collect();
            if !missing.is_empty() {
                failed.push(Failure { check: "redone_has", wanted: format!("{missing:?} made again"), got: format!("{redone:?}") });
            }
            let again: Vec<&String> = expect.reused.iter().filter(|it| redone.contains(it)).collect();
            if !again.is_empty() {
                failed.push(Failure { check: "reused", wanted: format!("{again:?} not made again"), got: format!("{redone:?}") });
            }
        }
    }
    most("max_units", expect.max_units, seen.units, "units made again", &mut failed);
    most("max_executed", expect.max_executed, seen.executed, "engine memos run", &mut failed);
    most("max_verified", expect.max_verified, seen.verified, "engine memos verified", &mut failed);
    most("max_ms", expect.max_ms, Some(seen.ms as u64), "ms", &mut failed);
    if expect.noop && seen.noop != Some(true) {
        failed.push(Failure {
            check: "noop",
            wanted: "answered as the last build was, with no walk".to_owned(),
            got: match seen.noop {
                Some(_) => "the build walked".to_owned(),
                None => "the driver does not see it".to_owned(),
            },
        });
    }
    if let Some(link) = &expect.link
        && seen.link.as_ref() != Some(link)
    {
        failed.push(Failure { check: "link", wanted: format!("linked `{link}`"), got: format!("{:?}", seen.link) });
    }
    if seen.panics > 0 {
        failed.push(Failure {
            check: "panics",
            wanted: "no panic caught".to_owned(),
            got: format!("{} caught, the first: {}", seen.panics, seen.first_panic),
        });
    }
    if let Some(clean) = clean {
        if clean.ok != seen.ok {
            failed.push(Failure {
                check: "clean outcome",
                wanted: format!("as a clean build: {}", if clean.ok { "success" } else { "failure" }),
                got: format!("{}:\n{}\nthe clean build said:\n{}", if seen.ok { "success" } else { "failure" }, short(&seen.said), short(&clean.said)),
            });
        } else if clean.stdout != seen.stdout {
            failed.push(Failure { check: "clean output", wanted: format!("as a clean build: {:?}", short(&clean.stdout)), got: format!("{:?}", short(&seen.stdout)) });
        }
        if expect.bytes && clean.ok && seen.ok {
            match (&seen.image, &clean.image) {
                (Some(ours), Some(theirs)) if ours == theirs => {}
                (Some(ours), Some(theirs)) => {
                    let first = ours.iter().zip(theirs).position(|(a, b)| a != b).unwrap_or(ours.len().min(theirs.len()));
                    let differing = ours.iter().zip(theirs).filter(|(a, b)| a != b).count();
                    failed.push(Failure {
                        check: "clean bytes",
                        wanted: format!("the clean build's {} bytes", theirs.len()),
                        got: format!("{} bytes, {differing} of them other, the first at {first:#x}", ours.len()),
                    });
                }
                _ => failed.push(Failure { check: "clean bytes", wanted: "the clean build's bytes".to_owned(), got: "the driver holds no bytes of one of the builds".to_owned() }),
            }
        }
    }
    failed
}

/// The cause the revision lists for `check` failing under `driver`: a row named for the check
/// (`redone`) is every driver's, one named `real: redone` only that driver's.
fn known<'a>(revision: &'a Revision, driver: &str, check: &str) -> Option<&'a String> {
    // `sim seed 3` is the driver `sim`.
    let driver = driver.split(' ').next().unwrap_or(driver);
    revision.known.get(&format!("{driver}: {check}")).or_else(|| revision.known.get(check))
}

/// One revision's row of the suite's table.
pub struct Row {
    pub label: String,
    pub seen: Observed,
    pub failed: Vec<Failure>,
    /// Whether the revision built from nothing succeeded, where it was built so.
    pub clean_ok: Option<bool>,
}

/// The test's verdict under `driver`: its rows printed (and appended to the file
/// `BLAZE_REVISIONS_REPORT` names, a JSON line each), and a panic naming every failure that
/// is not known and every known failure that passed. `note`: what the driver adds to each
/// failure (the seed and how to replay, the daemon's log).
pub fn verdict(test: &Test, driver: &str, rows: &[Row], note: &str) {
    let mut wrong = String::new();
    let mut lines = String::new();
    let mut report = String::new();
    for (revision, row) in test.revisions.iter().zip(rows) {
        let mut result = "ok".to_owned();
        let mut known_said = Vec::new();
        for failure in &row.failed {
            match known(revision, driver, failure.check) {
                Some(cause) => known_said.push(format!("{}: {cause} (got: {})", failure.check, short(&failure.got).replace('\n', " "))),
                None => {
                    result = "FAILED".to_owned();
                    _ = writeln!(
                        wrong,
                        "{} {} ({} `{}`), check `{}`:\n  wanted: {}\n  got: {}\n  why: {}",
                        test.name, row.label, driver, revision.action.word(), failure.check, failure.wanted, failure.got, short(&row.seen.why)
                    );
                }
            }
        }
        for (check, cause) in &revision.known {
            // A row of another driver (`real: clean bytes`) is not this one's.
            let check = match check.split_once(": ") {
                Some((of, check)) if Some(of) == driver.split(' ').next() => check,
                Some(_) => continue,
                None => check.as_str(),
            };
            if !row.failed.iter().any(|it| it.check == check) {
                result = "FAILED".to_owned();
                _ = writeln!(
                    wrong,
                    "{} {} ({driver}): the known failure of check `{check}` ({cause}) passes now: remove its `known` row",
                    test.name, row.label
                );
            }
        }
        if result == "ok" && !known_said.is_empty() {
            result = "known".to_owned();
        }
        let n = |it: Option<u64>| it.map_or("-".to_owned(), |it| it.to_string());
        _ = writeln!(
            lines,
            "revisions: {driver} {} {} {} {result} units={} executed={} verified={} redone={} noop={} link={} {} ms{}",
            test.name,
            row.label,
            revision.action.word(),
            n(row.seen.units),
            n(row.seen.executed),
            n(row.seen.verified),
            row.seen.redone.as_ref().map_or("-".to_owned(), |it| match it.len() > 6 {
                true => format!("{} functions", it.len()),
                false => format!("{it:?}"),
            }),
            row.seen.noop.map_or("-".to_owned(), |it| it.to_string()),
            row.seen.link.as_deref().unwrap_or("-"),
            row.seen.ms,
            known_said.iter().map(|it| format!("\n  known: {it}")).collect::<String>(),
        );
        _ = writeln!(
            report,
            "{}",
            serde_json::json!({
                "driver": driver, "test": test.name, "revision": row.label, "action": revision.action.word(),
                "result": result, "known": known_said, "units": row.seen.units, "executed": row.seen.executed,
                "verified": row.seen.verified, "redone": row.seen.redone, "noop": row.seen.noop,
                "link": row.seen.link, "ms": row.seen.ms as u64,
                "failed": row.failed.iter().map(|it| it.check).collect::<Vec<_>>(),
            })
        );
    }
    print!("{lines}");
    if let Ok(path) = std::env::var("BLAZE_REVISIONS_REPORT") {
        // One write of the test's lines: appends of several tests do not interleave.
        let mut file = fs::OpenOptions::new().create(true).append(true).open(&path).unwrap_or_else(|error| panic!("{path}: {error}"));
        file.write_all(report.as_bytes()).unwrap();
    }
    assert!(wrong.is_empty(), "\n{wrong}{note}");
}
