//! The audit's tests, on fixtures written here: no network, no database fetched.

use std::alloc::Global;
use std::collections::BTreeMap;

use blaze_fetch::{Algo, Digest};
use hegel::generators as gs;
use semver::{Op, Version, VersionReq};

use crate::advisory::{Advisory, AdvisoryRefused, Database, MOST_ADVISORY_BYTES};
use crate::audit::{DatabaseInput, Index, Input, audit, verify_database};
use crate::cargo_lock::{Lock, LockRefused, MOST_LOCK_BYTES};
use crate::date::Date;
use crate::fetch::{Effect, Event, Fetch, Refused};
use crate::pins::{DatabasePin, GitPin, SourcePins};
use crate::policy::{Settings, Severity};
use crate::source::{OutsidePath, PinRefused, Trees, check, require_git};
use crate::tree::{Kind, TreeFile, TreeRefused, digest};
use crate::version::{admits, affected, fix};
use crate::{STALE_ACCEPTANCE, UNMAINTAINED, VULNERABLE, WEAK_SOURCE_PIN, YANKED};

// ---------------------------------------------------------------------------------------
// The matcher against a brute-force model.

/// The operators the model knows, as written before a version.
const OPS: [&str; 8] = ["=", ">", ">=", "<", "<=", "~", "^", "*"];

/// One comparator as the model holds it: an operator of [`OPS`] and the parts it gives.
#[derive(Clone, Debug)]
struct Cmp {
    op: &'static str,
    parts: Vec<u64>,
}

impl Cmp {
    fn text(&self) -> String {
        let parts: Vec<String> = self.parts.iter().map(u64::to_string).collect();
        match self.op {
            "*" => format!("{}.*", parts.join(".")),
            op => format!("{op}{}", parts.join(".")),
        }
    }
}

/// The model: what each comparator admits, stated part by part, as cargo's book states it.
fn model_admits(c: &Cmp, v: (u64, u64, u64)) -> bool {
    let v = [v.0, v.1, v.2];
    let n = c.parts.len();
    let prefix = &v[..n];
    let given = c.parts.as_slice();
    let filled = |i: usize| given.get(i).copied().unwrap_or(0);
    let at_least_filled = v >= [filled(0), filled(1), filled(2)];
    match c.op {
        "=" | "*" => prefix == given,
        ">" => prefix > given,
        ">=" => prefix >= given,
        "<" => prefix < given,
        "<=" => prefix <= given,
        "~" if n == 1 => v[0] == given[0],
        "~" => v[0] == given[0] && v[1] == given[1] && at_least_filled,
        "^" => {
            // The left-most non-zero part given, or the last part given when all are zero:
            // every part up to it must equal.
            let keep = given.iter().position(|it| *it != 0).unwrap_or(n - 1);
            v[..=keep] == given[..=keep] && at_least_filled
        }
        _ => unreachable!("an operator of OPS"),
    }
}

fn model_affected(ranges: &[Vec<Cmp>], v: (u64, u64, u64)) -> bool {
    !ranges.iter().any(|range| range.iter().all(|c| model_admits(c, v)))
}

fn req(range: &[Cmp]) -> VersionReq {
    let text: Vec<String> = range.iter().map(Cmp::text).collect();
    let text = text.join(", ");
    VersionReq::parse(&text).unwrap_or_else(|why| panic!("`{text}` parses: {why}"))
}

/// Parts up to this, so small ranges meet small versions often.
const MOST_PART: u64 = 3;

fn draw_cmp(tc: &hegel::TestCase) -> Cmp {
    let op = OPS[tc.draw(gs::integers::<usize>().max_value(OPS.len() - 1))];
    let len = match op {
        "*" => tc.draw(gs::integers::<usize>().min_value(1).max_value(2)),
        _ => tc.draw(gs::integers::<usize>().min_value(1).max_value(3)),
    };
    let parts = (0..len).map(|_| tc.draw(gs::integers::<u64>().max_value(MOST_PART))).collect();
    Cmp { op, parts }
}

fn draw_ranges(tc: &hegel::TestCase) -> Vec<Vec<Cmp>> {
    let ranges = tc.draw(gs::integers::<usize>().max_value(3));
    (0..ranges)
        .map(|_| {
            let wild = |c: &Cmp| c.op == "*";
            let first = draw_cmp(tc);
            // A wildcard stands alone in its range, as people write one.
            if wild(&first) || !tc.draw(gs::booleans()) {
                return vec![first];
            }
            let mut second = draw_cmp(tc);
            while wild(&second) {
                second = draw_cmp(tc);
            }
            vec![first, second]
        })
        .collect()
}

/// The refinement: for every set of `patched` ranges of one or two comparators of every
/// operator, with one, two or three parts, and every version with parts up to
/// [`MOST_PART`] + 1, [`affected`] says what the part-by-part model of cargo's comparators
/// says.
#[hegel::test]
fn the_matcher_agrees_with_the_model(tc: hegel::TestCase) {
    let ranges = draw_ranges(&tc);
    let reqs: Vec<VersionReq> = ranges.iter().map(|it| req(it)).collect();
    let v = (
        tc.draw(gs::integers::<u64>().max_value(MOST_PART + 1)),
        tc.draw(gs::integers::<u64>().max_value(MOST_PART + 1)),
        tc.draw(gs::integers::<u64>().max_value(MOST_PART + 1)),
    );
    let version = Version::new(v.0, v.1, v.2);
    assert_eq!(
        affected(&reqs, &[], &version),
        model_affected(&ranges, v),
        "{version} against {:?}",
        reqs.iter().map(ToString::to_string).collect::<Vec<_>>()
    );
}

/// The matcher with a planted bug: `^0.m.p` taken to keep the major only, as for `^1.m.p`.
fn planted_admits(c: &semver::Comparator, v: &Version) -> bool {
    if c.op == Op::Caret && c.major == 0 {
        return v >= &Version::new(0, c.minor.unwrap_or(0), c.patch.unwrap_or(0)) && v.major == 0;
    }
    admits(c, v)
}

/// The control: the oracle catches the planted bug. Over the same small space the property
/// draws from, some caret range and version split the planted matcher from the model.
#[test]
fn the_oracle_catches_a_caret_that_keeps_only_the_major() {
    let mut caught = None;
    'search: for minor in 0..=MOST_PART {
        for patch in 0..=MOST_PART {
            let c = Cmp { op: "^", parts: vec![0, minor, patch] };
            let parsed = req(std::slice::from_ref(&c));
            for v in [(0, minor + 1, 0), (0, minor, patch + 1), (0, 0, patch)] {
                let version = Version::new(v.0, v.1, v.2);
                let planted = parsed.comparators.iter().all(|it| planted_admits(it, &version));
                if planted != model_admits(&c, v) {
                    caught = Some((c.text(), version));
                    break 'search;
                }
            }
        }
    }
    assert!(caught.is_some(), "the model never told the planted caret from cargo's");
}

#[test]
fn a_pre_release_after_the_fix_has_the_fix() {
    let patched = [VersionReq::parse(">= 1.2.3").unwrap()];
    assert!(!affected(&patched, &[], &Version::parse("1.3.0-rc.1").unwrap()));
    assert!(affected(&patched, &[], &Version::parse("1.2.3-rc.1").unwrap()));
}

#[test]
fn the_fix_is_the_least_patched_start_above_the_version() {
    let patched = [VersionReq::parse(">= 2.0.1").unwrap(), VersionReq::parse("^1.4.2").unwrap()];
    assert_eq!(fix(&patched, &Version::new(1, 4, 0)), Some((Version::new(1, 4, 2), true)));
    assert_eq!(fix(&patched, &Version::new(1, 9, 0)), Some((Version::new(2, 0, 1), false)));
    assert_eq!(fix(&patched, &Version::new(2, 1, 0)), None);
}

// ---------------------------------------------------------------------------------------
// Advisories.

const ADVISORY: &str = r#"```toml
[advisory]
id = "RUSTSEC-2026-0101"
package = "foo"
date = "2026-10-01"
url = "https://example.com/advisories/0101"
categories = ["memory-corruption"]
keywords = ["overflow"]
aliases = ["CVE-2026-0001", "GHSA-xxxx-yyyy-zzzz"]
cvss = "CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H"

[affected]
os = ["linux"]
functions = { "foo::parse" = ["< 1.2.4"] }

[versions]
patched = [">= 1.2.4"]
unaffected = ["< 1.0.0"]
```

# Out-of-bounds write in `foo::parse`

A long input writes past the buffer.
"#;

const UNMAINTAINED_ADVISORY: &str = r#"```toml
[advisory]
id = "RUSTSEC-2025-0007"
package = "bar"
date = "2025-03-02"
informational = "unmaintained"

[versions]
patched = []
```

# `bar` is unmaintained

Its author archived it.
"#;

#[test]
fn an_advisory_reads_back_what_it_renders() {
    let read = Advisory::parse("crates/foo/RUSTSEC-2026-0101.md", ADVISORY.as_bytes()).unwrap();
    assert_eq!(read.title, "Out-of-bounds write in `foo::parse`");
    assert_eq!(read.functions.len(), 1);
    assert_eq!(read.patched, [">= 1.2.4"]);
    let rendered = read.render();
    let again = Advisory::parse("crates/foo/RUSTSEC-2026-0101.md", rendered.as_bytes()).unwrap();
    assert_eq!(again, read);
    assert_eq!(again.render(), rendered, "a render is a fixed point");

    let informational = Advisory::parse("crates/bar/RUSTSEC-2025-0007.md", UNMAINTAINED_ADVISORY.as_bytes()).unwrap();
    let again = Advisory::parse("crates/bar/RUSTSEC-2025-0007.md", informational.render().as_bytes()).unwrap();
    assert_eq!(again, informational);
}

fn refused(file: &str, text: &str) -> String {
    match Advisory::parse(file, text.as_bytes()) {
        Ok(it) => panic!("`{file}` was read: {it:?}"),
        Err(why) => why.to_string(),
    }
}

#[test]
fn a_malformed_advisory_is_refused_by_name() {
    let file = "crates/foo/RUSTSEC-2026-0101.md";
    let said = refused(file, &ADVISORY.replace("cvss =", "severity ="));
    assert!(said.contains("`advisory.severity`") && said.contains(file), "{said}");
    let said = refused(file, &ADVISORY.replace("[versions]", "[versions]\nfixed = []"));
    assert!(said.contains("`versions.fixed`"), "{said}");
    let said = refused("crates/bar/RUSTSEC-2026-0101.md", ADVISORY);
    assert!(said.contains("belongs at `crates/foo/RUSTSEC-2026-0101.md`"), "{said}");
    let said = refused(file, &ADVISORY.replace("RUSTSEC-2026-0101", "RUSTSEC-26-0101"));
    assert!(said.contains("no RUSTSEC-YYYY-NNNN"), "{said}");
    let said = refused(file, &ADVISORY.replace(">= 1.2.4", ">= one"));
    assert!(said.contains("`versions.patched` range `>= one`"), "{said}");
    let said = refused(file, &ADVISORY.replace("2026-10-01", "2026-13-01"));
    assert!(said.contains("`advisory.date`"), "{said}");
    let said = refused(file, &ADVISORY.replace("```toml", "```"));
    assert!(said.contains("```toml fence"), "{said}");
    let said = refused("crates/bar/RUSTSEC-2025-0007.md", &UNMAINTAINED_ADVISORY.replace("unmaintained\"", "abandoned\""));
    assert!(said.contains("`advisory.informational` = `abandoned`"), "{said}");
}

#[test]
fn an_oversized_advisory_is_refused_before_it_is_read() {
    let mut text = ADVISORY.to_owned();
    text.push_str(&"x".repeat(MOST_ADVISORY_BYTES));
    let said = Advisory::parse("crates/foo/RUSTSEC-2026-0101.md", text.as_bytes());
    assert!(matches!(&said, Err(AdvisoryRefused::TooBig { file, .. }) if file == "crates/foo/RUSTSEC-2026-0101.md"), "{said:?}");
}

#[test]
fn an_oversized_lock_is_refused_before_it_is_read() {
    let text = vec![b'#'; MOST_LOCK_BYTES + 1];
    assert_eq!(Lock::parse(&text), Err(LockRefused::TooBig { bytes: MOST_LOCK_BYTES + 1 }));
}

// ---------------------------------------------------------------------------------------
// A workspace: a lock, a database, settings, pins.

const COMMIT: &str = "575ed6b8ceb79ffaf247ecf7e34333876b0d5be4";
const FORK: &str = "https://example.com/o/forked";
const SUM: &str = "0000000000000000000000000000000000000000000000000000000000000000";

fn blocks() -> Vec<String> {
    let registry = "registry+https://github.com/rust-lang/crates.io-index";
    vec![
        "[[package]]\nname = \"app\"\nversion = \"0.1.0\"\ndependencies = [\n \"bar\",\n \"forked\",\n \"foo\",\n]\n".to_owned(),
        "[[package]]\nname = \"tool\"\nversion = \"0.1.0\"\ndependencies = [\n \"bar\",\n]\n".to_owned(),
        format!("[[package]]\nname = \"bar\"\nversion = \"0.3.1\"\nsource = \"{registry}\"\nchecksum = \"{SUM}\"\ndependencies = [\n \"foo\",\n]\n"),
        format!("[[package]]\nname = \"foo\"\nversion = \"1.2.0\"\nsource = \"{registry}\"\nchecksum = \"{SUM}\"\n"),
        format!("[[package]]\nname = \"forked\"\nversion = \"0.2.0\"\nsource = \"git+{FORK}?branch=main#{COMMIT}\"\n"),
    ]
}

fn lock_of(order: &[usize]) -> Lock {
    let blocks = blocks();
    let mut text = "version = 4\n".to_owned();
    for at in order {
        text.push('\n');
        text.push_str(&blocks[*at]);
    }
    Lock::parse(text.as_bytes()).unwrap()
}

fn database() -> Database {
    Database::parse(&[
        ("crates/foo/RUSTSEC-2026-0101.md", ADVISORY.as_bytes()),
        ("crates/bar/RUSTSEC-2025-0007.md", UNMAINTAINED_ADVISORY.as_bytes()),
        ("README.md", b"not an advisory"),
    ])
    .unwrap()
}

fn tree_of(text: &str) -> Digest {
    Digest::of(Algo::Blake3, text.as_bytes())
}

/// Trees the test says, by (url, commit).
struct Fixed(BTreeMap<(String, String), Result<Option<Digest>, String>>);

impl Trees for Fixed {
    fn tree(&mut self, url: &str, commit: &str, _: Algo) -> Result<Option<Digest>, String> {
        self.0.get(&(url.to_owned(), commit.to_owned())).cloned().unwrap_or(Ok(None))
    }
}

fn fork_on_disk(tree: Result<Option<Digest>, String>) -> Fixed {
    Fixed(BTreeMap::from([((FORK.to_owned(), COMMIT.to_owned()), tree)]))
}

fn pinned_fork(tree: Digest) -> SourcePins {
    let mut pins = SourcePins::default();
    pins.set_git(GitPin { url: FORK.to_owned(), commit: COMMIT.to_owned(), tree });
    pins.advisories = Some(DatabasePin { from: None, tree: tree_of("db") });
    pins
}

struct NoYanks;

impl Index for NoYanks {
    fn file(&mut self, _: &str, package: &str) -> Option<Vec<u8, Global>> {
        let mut out = Vec::new_in(Global);
        let line = match package {
            "foo" => "{\"name\":\"foo\",\"vers\":\"1.2.0\",\"yanked\":false}\n",
            "bar" => "{\"name\":\"bar\",\"vers\":\"0.3.1\",\"yanked\":true}\n",
            _ => return None,
        };
        out.extend_from_slice(line.as_bytes());
        Some(out)
    }
}

fn settings(accept: &str) -> Settings {
    let text = format!("accept = [{accept}]\n");
    let table: toml::Value = toml::from_str(&text).unwrap();
    Settings::parse(Some(&table)).unwrap()
}

fn report_on(order: &[usize], settings: &Settings, today: &str) -> crate::report::Report {
    let lock = lock_of(order);
    let database = database();
    let pins = pinned_fork(tree_of("fork"));
    let mut trees = fork_on_disk(Ok(Some(tree_of("fork"))));
    let mut index = NoYanks;
    let members = ["app".to_owned(), "tool".to_owned()];
    audit(Input {
        lock: &lock,
        members: &members,
        outside: &[],
        pins: &pins,
        settings,
        database: DatabaseInput::Read { path: "advisory-db".to_owned(), tree: tree_of("db"), database: &database },
        trees: &mut trees,
        index: Some(&mut index),
        today: Date::parse(today).unwrap(),
    })
}

#[test]
fn a_workspace_reports_each_package_once_with_its_chain_and_fix() {
    let report = report_on(&[0, 1, 2, 3, 4], &Settings::default(), "2026-11-13");
    let foo = report.findings.iter().find(|it| it.rule == VULNERABLE).unwrap();
    assert_eq!(foo.package, "foo");
    assert_eq!(foo.advisory.as_deref(), Some("RUSTSEC-2026-0101"));
    assert_eq!(foo.chain, ["app 0.1.0", "foo 1.2.0"], "the shortest chain, app's direct dependency");
    assert_eq!(foo.fix.as_deref(), Some("1.2.4"));
    assert!(foo.compatible);
    assert_eq!(foo.change.as_deref(), Some("in the manifest of `app 0.1.0`, `foo = \"1.2.4\"`"));
    assert_eq!(foo.functions, ["foo::parse"]);
    let bar = report.findings.iter().find(|it| it.rule == UNMAINTAINED).unwrap();
    assert_eq!(bar.severity, Severity::Warning);
    let yanked = report.findings.iter().find(|it| it.rule == YANKED).unwrap();
    assert_eq!((yanked.package.as_str(), yanked.version.as_str()), ("bar", "0.3.1"));
    assert!(!report.ok);
    assert_eq!(report.database.as_ref().and_then(|it| it.age_days), Some(43));
    assert!(report.pinned.iter().any(|it| it.package == "forked" && it.verified));
}

/// The invariant: the report, as JSON and as text, does not depend on the order of the
/// lock's packages.
#[hegel::test]
fn the_report_does_not_depend_on_the_lock_order(tc: hegel::TestCase) {
    let mut order: Vec<usize> = (0..blocks().len()).collect();
    for at in (1..order.len()).rev() {
        let other = tc.draw(gs::integers::<usize>().max_value(at));
        order.swap(at, other);
    }
    let settings = settings("{ id = \"RUSTSEC-2024-0001\", reason = \"gone\", until = \"2027-01-01\" }");
    let base = report_on(&[0, 1, 2, 3, 4], &settings, "2026-11-13");
    let shuffled = report_on(&order, &settings, "2026-11-13");
    assert_eq!(shuffled.json(), base.json(), "order {order:?}");
    assert_eq!(shuffled.text(), base.text());
}

#[test]
fn an_acceptance_holds_through_its_day_and_not_after() {
    let settings = settings("{ id = \"RUSTSEC-2026-0101\", reason = \"parse is fed only our own files\", until = \"2026-11-13\" }");
    let on = report_on(&[0, 1, 2, 3, 4], &settings, "2026-11-13");
    let foo = on.findings.iter().find(|it| it.rule == VULNERABLE).unwrap();
    assert_eq!(foo.severity, Severity::Accepted);
    assert_eq!(foo.expired, None);
    let after = report_on(&[0, 1, 2, 3, 4], &settings, "2026-11-14");
    let foo = after.findings.iter().find(|it| it.rule == VULNERABLE).unwrap();
    assert_eq!(foo.severity, Severity::Error);
    assert_eq!(foo.expired.as_deref(), Some("2026-11-13"));
    assert!(!on.findings.iter().any(|it| it.rule == STALE_ACCEPTANCE));
}

#[test]
fn an_acceptance_that_matches_nothing_is_stale() {
    let settings = settings("{ id = \"RUSTSEC-2024-0001\", reason = \"gone\", until = \"2027-01-01\" }");
    let report = report_on(&[0, 1, 2, 3, 4], &settings, "2026-11-13");
    let stale = report.findings.iter().find(|it| it.rule == STALE_ACCEPTANCE).unwrap();
    assert_eq!(stale.advisory.as_deref(), Some("RUSTSEC-2024-0001"));
}

#[test]
fn an_acceptance_needs_a_reason_and_an_expiry() {
    let parse = |accept: &str| {
        let table: toml::Value = toml::from_str(&format!("accept = [{accept}]\n")).unwrap();
        Settings::parse(Some(&table)).map_err(|it| it.to_string())
    };
    let said = parse("{ id = \"RUSTSEC-2026-0101\", reason = \" \", until = \"2027-01-01\" }").unwrap_err();
    assert!(said.contains("empty reason"), "{said}");
    let said = parse("{ id = \"RUSTSEC-2026-0101\", reason = \"ok\" }").unwrap_err();
    assert!(said.contains("no `until`"), "{said}");
    let said = parse("{ id = \"RUSTSEC-2026-0101\", reason = \"ok\", until = \"soon\" }").unwrap_err();
    assert!(said.contains("YYYY-MM-DD"), "{said}");
}

// ---------------------------------------------------------------------------------------
// Source pins.

fn sources(pins: &SourcePins, trees: &mut Fixed) -> (Vec<PinRefused, Global>, Vec<crate::source::Pinned, Global>) {
    check(&lock_of(&[0, 1, 2, 3, 4]), &[], pins, &Settings::default(), trees)
}

#[test]
fn a_git_source_without_its_tree_pinned_is_refused() {
    let (refused, _) = sources(&SourcePins::default(), &mut fork_on_disk(Ok(Some(tree_of("fork")))));
    assert_eq!(refused.len(), 1);
    let said = refused[0].to_string();
    assert!(matches!(refused[0], PinRefused::Missing { .. }));
    for part in ["`forked`", FORK, COMMIT, "`blaze audit pin`", WEAK_SOURCE_PIN] {
        assert!(said.contains(part), "`{said}` names {part}");
    }
}

#[test]
fn a_git_tree_that_is_not_its_pin_is_refused_naming_both() {
    let (refused, _) = sources(&pinned_fork(tree_of("fork")), &mut fork_on_disk(Ok(Some(tree_of("another")))));
    let said = refused[0].to_string();
    assert!(said.contains(&tree_of("fork").to_string()) && said.contains(&tree_of("another").to_string()), "{said}");
}

#[test]
fn a_git_tree_that_is_its_pin_is_accepted() {
    let (refused, passed) = sources(&pinned_fork(tree_of("fork")), &mut fork_on_disk(Ok(Some(tree_of("fork")))));
    assert_eq!(refused, []);
    assert!(matches!(&passed[..], [crate::source::Pinned::Git { verified: true, .. }]));
}

#[test]
fn a_tree_whose_symlink_escapes_is_refused() {
    let files = [
        TreeFile { path: "src/lib.rs", kind: Kind::File, bytes: b"pub fn f() {}" },
        TreeFile { path: "src/secrets", kind: Kind::Link, bytes: b"../../../home/.ssh/id_ed25519" },
    ];
    let said = digest(&files, Algo::Blake3);
    assert!(matches!(&said, Err(TreeRefused::Escapes { path, .. }) if path == "src/secrets"), "{said:?}");
    let inside = [
        TreeFile { path: "src/lib.rs", kind: Kind::File, bytes: b"pub fn f() {}" },
        TreeFile { path: "src/alias.rs", kind: Kind::Link, bytes: b"lib.rs" },
        TreeFile { path: ".git/HEAD", kind: Kind::File, bytes: b"ref: refs/heads/main" },
        TreeFile { path: ".cargo-ok", kind: Kind::File, bytes: b"ok" },
    ];
    let with_marks = digest(&inside, Algo::Blake3).unwrap();
    assert_eq!(with_marks, digest(&inside[..2], Algo::Blake3).unwrap(), "a .git and a fetcher's mark are not hashed");

    let why = digest(&files, Algo::Blake3).unwrap_err().to_string();
    let (refused, _) = sources(&pinned_fork(tree_of("fork")), &mut fork_on_disk(Err(why)));
    assert!(matches!(&refused[0], PinRefused::Tree { why, .. } if why.contains("outside the tree")), "{refused:?}");
}

#[test]
fn a_commit_that_is_not_whole_is_no_pin() {
    let pins = pinned_fork(tree_of("fork"));
    assert!(matches!(require_git(&pins, "forked", FORK, "575ed6b"), Err(PinRefused::NoCommit { .. })));
    assert_eq!(require_git(&pins, "forked", FORK, COMMIT), Ok(&tree_of("fork")));
}

#[test]
fn an_outside_path_is_refused_unless_allowed() {
    let outside = [OutsidePath { package: "local".to_owned(), path: "/elsewhere/local".to_owned() }];
    let lock = lock_of(&[0, 1, 2, 3, 4]);
    let pins = pinned_fork(tree_of("fork"));
    let (refused, _) = check(&lock, &outside, &pins, &Settings::default(), &mut fork_on_disk(Ok(None)));
    assert!(matches!(&refused[..], [PinRefused::OutsidePath { package, .. }] if package == "local"), "{refused:?}");
    let table: toml::Value = toml::from_str("allow-unpinned-paths = [\"local\"]").unwrap();
    let allowed = Settings::parse(Some(&table)).unwrap();
    let (refused, _) = check(&lock, &outside, &pins, &allowed, &mut fork_on_disk(Ok(None)));
    assert_eq!(refused, []);
}

#[test]
fn a_registry_package_without_a_checksum_is_refused() {
    let text = "version = 4\n\n[[package]]\nname = \"foo\"\nversion = \"1.0.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n";
    let lock = Lock::parse(text.as_bytes()).unwrap();
    let (refused, _) = check(&lock, &[], &SourcePins::default(), &Settings::default(), &mut fork_on_disk(Ok(None)));
    assert!(matches!(&refused[..], [PinRefused::NoChecksum { package, .. }] if package == "foo"), "{refused:?}");
}

#[test]
fn the_database_is_read_only_at_its_pin() {
    let pins = pinned_fork(tree_of("fork"));
    assert_eq!(verify_database(&pins, "advisory-db", &tree_of("db")), Ok(()));
    let said = verify_database(&pins, "advisory-db", &tree_of("changed")).unwrap_err().said;
    assert!(said.contains(&tree_of("db").to_string()) && said.contains(&tree_of("changed").to_string()), "{said}");
    assert!(verify_database(&SourcePins::default(), "advisory-db", &tree_of("db")).is_err());
}

#[test]
fn pins_read_back_what_they_render_and_refuse_sha1() {
    let mut pins = pinned_fork(tree_of("fork"));
    pins.set_git(GitPin { url: FORK.to_owned(), commit: "a".repeat(40), tree: tree_of("older") });
    pins.advisories = Some(DatabasePin { from: Some(("https://example.com/db".to_owned(), "b".repeat(40))), tree: tree_of("db") });
    let mut text = String::new();
    pins.render(&mut text);
    let doc: toml::Table = toml::from_str(&text).unwrap();
    let mut again = SourcePins::default();
    let bad = |why: String| why;
    again.parse_git(&doc["git"], &bad).unwrap();
    again.parse_advisories(&doc["advisories"], &bad).unwrap();
    assert_eq!(again, pins);
    let mut twice = String::new();
    again.render(&mut twice);
    assert_eq!(twice, text, "a render is a fixed point");

    let weak = format!("[git.\"{FORK}\"]\n{COMMIT} = \"sha1:{}\"\n", "0".repeat(40));
    let doc: toml::Table = toml::from_str(&weak).unwrap();
    let said = SourcePins::default().parse_git(&doc["git"], &bad).unwrap_err();
    assert!(said.contains("sha1") || said.contains("SHA-1"), "{said}");
}

// ---------------------------------------------------------------------------------------
// The fetch machine and days.

#[test]
fn a_fetch_records_the_tree_it_fetched() {
    let mut fetch = Fetch::new("https://example.com/db", "main", &SourcePins::default());
    assert_eq!(fetch.start(), Effect::Fetch { url: "https://example.com/db".to_owned(), rev: "main".to_owned() });
    let fetched = Event::Fetched { commit: "c".repeat(40), tree: tree_of("db") };
    assert_eq!(fetch.step(fetched), Ok(Effect::Place));
    let pin = DatabasePin { from: Some(("https://example.com/db".to_owned(), "c".repeat(40))), tree: tree_of("db") };
    assert_eq!(fetch.step(Event::Placed), Ok(Effect::Record(pin.clone())));
    assert_eq!(fetch.step(Event::Recorded), Ok(Effect::Done(pin)));
}

#[test]
fn a_fetch_of_a_pinned_commit_with_another_tree_is_refused() {
    let mut pins = SourcePins::default();
    pins.advisories = Some(DatabasePin { from: Some(("https://example.com/db".to_owned(), "c".repeat(40))), tree: tree_of("db") });
    let mut fetch = Fetch::new("https://example.com/db", "main", &pins);
    fetch.start();
    let said = fetch.step(Event::Fetched { commit: "c".repeat(40), tree: tree_of("other") });
    assert!(matches!(said, Err(Refused::Mismatch { .. })), "{said:?}");
}

#[test]
fn days_count_and_turn() {
    assert_eq!(Date::from_unix_days(0).to_string(), "1970-01-01");
    assert_eq!(Date::from_unix_days(11016).to_string(), "2000-02-29");
    assert_eq!(Date::from_unix_days(20770).to_string(), "2026-11-13");
    assert_eq!(Date::parse("2026-11-13").unwrap().ordinal(), 739932);
    assert_eq!(Date::parse("2026-12-31").unwrap().next().to_string(), "2027-01-01");
    assert_eq!(Date::parse("2024-02-28").unwrap().next().to_string(), "2024-02-29");
    assert!(Date::parse("2025-02-29").is_err());
}
