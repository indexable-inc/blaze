//! A suite's expectations file (`harness/compiletest/<suite>.toml`): the tests known not to
//! pass, grouped by cause, and how many tests the suite has and ran when it was recorded. The
//! ratchet: a test that does not pass and is not listed fails the run, and so does a listed
//! test that passes, no longer runs or is gone, so the list only says what is true. Pure.

use std::collections::BTreeMap;

use blaze_script::toml;

/// The classes a listed test has.
pub(crate) const LISTED: [&str; 3] = ["fail", "panic", "timeout"];

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Expected {
    /// Tests in the suite, and those that ran (passed, failed, panicked or timed out), at the
    /// last whole run recorded.
    pub total: u64,
    pub ran: u64,
    /// Test id to (class, cause).
    pub listed: BTreeMap<String, (String, String)>,
}

/// One test of a run: its id, its class (`pass`, `fail`, `panic`, `timeout`, `unsupported`,
/// `ignored`) and its cause.
pub(crate) struct Seen<'a> {
    pub id: &'a str,
    pub class: &'a str,
    pub cause: &'a str,
}

fn quoted(text: &str) -> String {
    let mut out = String::from("\"");
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c.is_control() => out.push(' '),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

pub(crate) fn read(text: &str) -> Result<Expected, String> {
    let value: toml::Table = text.parse().map_err(|e| format!("not TOML: {e}"))?;
    let mut out = Expected::default();
    for (key, section) in &value {
        let table = section.as_table().ok_or_else(|| format!("`{key}` is not a table"))?;
        match key.as_str() {
            "counts" => {
                let count = |name: &str| table.get(name).and_then(toml::Value::as_integer).and_then(|it| u64::try_from(it).ok()).ok_or_else(|| format!("[counts] has no `{name}`"));
                (out.total, out.ran) = (count("total")?, count("ran")?);
            }
            class if LISTED.contains(&class) => {
                for (cause, tests) in table {
                    let tests = tests.as_array().ok_or_else(|| format!("[{class}] {cause}: not a list of tests"))?;
                    for test in tests {
                        let id = test.as_str().ok_or_else(|| format!("[{class}] {cause}: a test that is not a string"))?;
                        if out.listed.insert(id.to_owned(), (class.to_owned(), cause.clone())).is_some() {
                            return Err(format!("{id} is listed twice"));
                        }
                    }
                }
            }
            other => return Err(format!("no table `{other}` (tables: counts, fail, panic, timeout)")),
        }
    }
    Ok(out)
}

/// The file's text: the same bytes for the same expectations.
pub(crate) fn write(suite: &str, expected: &Expected) -> String {
    let mut out = format!(
        "# rustc's tests/{suite} under blaze: the tests known not to pass, by cause (docs/compiletest.md).\n\
         # Written by `compiletest` with `\"expectations\": \"record\"`; the gate (`\"check\"`) fails when a test\n\
         # that is not here does not pass, and when one that is here passes, no longer runs or is gone.\n\
         \n[counts]\ntotal = {}\nran = {}\n",
        expected.total, expected.ran
    );
    for class in LISTED {
        let mut causes: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for (id, (its, cause)) in &expected.listed {
            if its == class {
                causes.entry(cause.as_str()).or_default().push(id.as_str());
            }
        }
        if causes.is_empty() {
            continue;
        }
        out.push_str(&format!("\n[{class}]\n"));
        for (cause, tests) in causes {
            out.push_str(&format!("{} = [\n", quoted(cause)));
            for test in tests {
                out.push_str(&format!("  {},\n", quoted(test)));
            }
            out.push_str("]\n");
        }
    }
    out
}

/// What of `seen` the expectations do not allow, one line each. `whole`: the run was of the
/// whole suite, so its counts and the listed tests it did not see are held too.
pub(crate) fn check(expected: &Expected, seen: &[Seen<'_>], whole: bool) -> Vec<String> {
    let mut out = Vec::new();
    let mut found = std::collections::BTreeSet::new();
    let mut ran = 0;
    for it in seen {
        found.insert(it.id);
        let listed = expected.listed.get(it.id);
        match (it.class, listed) {
            ("pass", None) => ran += 1,
            ("pass", Some((class, cause))) => {
                ran += 1;
                out.push(format!("passes, and is listed under [{class}] `{cause}`: take its row out: {}", it.id));
            }
            ("fail" | "panic" | "timeout", Some(_)) => ran += 1,
            ("fail" | "panic" | "timeout", None) => {
                ran += 1;
                out.push(format!("{}, and is not listed: {} ({})", it.class, it.id, it.cause));
            }
            (_, Some((class, _))) => out.push(format!("is listed under [{class}] and no longer runs ({}: {}): {}", it.class, it.cause, it.id)),
            (_, None) => {}
        }
    }
    if whole {
        for id in expected.listed.keys().filter(|id| !found.contains(id.as_str())) {
            out.push(format!("is listed and is not in the suite: {id}"));
        }
        if seen.len() as u64 != expected.total {
            out.push(format!("the suite has {} tests, and {} when its expectations were recorded", seen.len(), expected.total));
        }
        if ran < expected.ran {
            out.push(format!("{ran} tests ran, fewer than the {} that ran when the expectations were recorded", expected.ran));
        }
    }
    out
}

/// The expectations after `seen`: each test seen is listed as it came out, the others stay as
/// they were; the counts are a whole run's only.
pub(crate) fn recorded(expected: &Expected, seen: &[Seen<'_>], whole: bool) -> Expected {
    let mut out = if whole { Expected::default() } else { expected.clone() };
    let mut ran = 0;
    for it in seen {
        out.listed.remove(it.id);
        if LISTED.contains(&it.class) {
            out.listed.insert(it.id.to_owned(), (it.class.to_owned(), it.cause.to_owned()));
        }
        if it.class == "pass" || LISTED.contains(&it.class) {
            ran += 1;
        }
    }
    if whole {
        (out.total, out.ran) = (seen.len() as u64, ran);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seen<'a>(rows: &'a [(&'a str, &'a str, &'a str)]) -> Vec<Seen<'a>> {
        rows.iter().map(|it| Seen { id: it.0, class: it.1, cause: it.2 }).collect()
    }

    /// The invariant: a run is accepted exactly when every test that ran came out as the file
    /// says: listed ones still do not pass, unlisted ones pass, and no fewer ran.
    #[test]
    fn the_ratchet_holds_both_ways() {
        let first = [("a.rs", "pass", ""), ("b.rs#x", "fail", "refuses: E0308 \"q\""), ("c.rs", "panic", "panic at x.rs"), ("d.rs", "unsupported", "flag -Z x"), ("e.rs", "timeout", "check")];
        let expected = recorded(&Expected::default(), &seen(&first), true);
        assert_eq!((expected.total, expected.ran, expected.listed.len()), (5, 4, 3));
        assert_eq!(check(&expected, &seen(&first), true), Vec::<String>::new());
        let text = write("ui", &expected);
        assert_eq!(read(&text).unwrap(), expected, "{text}");
        assert_eq!(write("ui", &read(&text).unwrap()), text);
        // A pass that now fails, a listed failure that now passes, a listed test that no longer
        // runs, one gone from the suite, and fewer run.
        let regressed = [("a.rs", "fail", "refuses: x"), ("b.rs#x", "pass", ""), ("c.rs", "unsupported", "flag"), ("d.rs", "unsupported", "flag -Z x")];
        let problems = check(&expected, &seen(&regressed), true);
        assert_eq!(problems.len(), 6, "{problems:#?}");
        assert!(problems[0].starts_with("fail, and is not listed: a.rs"));
        assert!(problems[1].starts_with("passes, and is listed under [fail]"));
        assert!(problems[2].starts_with("is listed under [panic] and no longer runs"));
        assert!(problems[3].starts_with("is listed and is not in the suite: e.rs"));
        // A part of the suite is held to the rows of the tests it ran, not to the counts.
        assert_eq!(check(&expected, &seen(&first[..2]), false), Vec::<String>::new());
        let part = recorded(&expected, &seen(&[("b.rs#x", "pass", "")]), false);
        assert_eq!((part.total, part.ran, part.listed.len()), (5, 4, 2));
    }

    /// The control of `the_ratchet_holds_both_ways`: a run in which nothing ran has no failure,
    /// and is refused.
    #[test]
    fn the_oracle_catches_a_run_that_ran_nothing() {
        let expected = recorded(&Expected::default(), &seen(&[("a.rs", "pass", ""), ("b.rs", "pass", "")]), true);
        let nothing = [("a.rs", "unsupported", "all"), ("b.rs", "unsupported", "all")];
        assert!(seen(&nothing).iter().all(|it| it.class != "fail"), "the reading that looks only for failures passes it");
        assert_eq!(check(&expected, &seen(&nothing), true).len(), 1);
        assert!(read("[oops]\n").is_err() && read("[counts]\ntotal = 1\n").is_err());
    }
}
