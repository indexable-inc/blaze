//! blaze's rules (`tools/astlog/rules/<name>.astlog`) against their fixtures in
//! `rules/tests/<name>/`: `bad.fixture` reports exactly the findings `bad.expected` lists
//! (`line:column rule`, in scan order), fixing it gives `good.fixture` (unchanged when the rules
//! have no rewrites), and every `good*.fixture` reports nothing and needs no fix.

use std::path::{Path, PathBuf};

use astlog_core::Analysis;

type TestResult = Result<(), Box<dyn std::error::Error>>;

/// Every rules file with a test below.
const TESTED: &[&str] = &["expand-hand-extent", "rust-locks"];

fn rules_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../rules")
}

/// `fixture` analyzed by `rules` as the Rust file `place` (a relative path: what a rule's
/// `file-path` sees). The temporary directory holds the file the analysis read, so it lives as
/// long as the analysis.
fn analyze(
    rules: &str,
    fixture: &Path,
    place: &Path,
) -> Result<(tempfile::TempDir, Analysis), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let source = dir.path().join(place);
    if let Some(parent) = source.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::copy(fixture, &source)?;
    let analysis = astlog_core::analyze(rules, &[source])?;
    Ok((dir, analysis))
}

fn findings(analysis: &Analysis) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let found = analysis.findings()?;
    Ok(found.iter().map(|f| format!("{}:{} {}", f.line, f.column, f.rule)).collect())
}

fn check(name: &str) -> TestResult {
    let rules = std::fs::read_to_string(rules_dir().join(format!("{name}.astlog")))?;
    let fixtures = rules_dir().join("tests").join(name);
    let bad = fixtures.join("bad.fixture");
    // Where the fixtures are placed: `fixture.rs`, or the path a `path` file names (a rule
    // scoped by `file-path` to one tree).
    let named = fixtures.join("path");
    let place = if named.exists() {
        PathBuf::from(std::fs::read_to_string(&named)?.trim())
    } else {
        PathBuf::from("fixture.rs")
    };

    let (_dir, analysis) = analyze(&rules, &bad, &place)?;
    let expected = std::fs::read_to_string(fixtures.join("bad.expected"))?;
    assert_eq!(
        findings(&analysis)?,
        expected.lines().collect::<Vec<_>>(),
        "{name}: bad.fixture's findings"
    );

    let good = std::fs::read_to_string(fixtures.join("good.fixture"))?;
    let fixed = analysis.rewritten();
    if analysis.program.rewrites.is_empty() {
        assert!(fixed.is_empty(), "{name} has no rewrites, yet fixing bad.fixture changed it");
    } else {
        let [fixed] = fixed.as_slice() else {
            panic!("{name}: fixing bad.fixture rewrote {} files, not 1", fixed.len());
        };
        assert_eq!(fixed.content, good, "{name}: bad.fixture fixed is not good.fixture");
    }

    let mut goods = 0;
    for entry in std::fs::read_dir(&fixtures)? {
        let path = entry?.path();
        let Some(file) = path.file_name().and_then(|f| f.to_str()) else { continue };
        if !(file.starts_with("good") && file.ends_with(".fixture")) {
            continue;
        }
        let (_dir, analysis) = analyze(&rules, &path, &place)?;
        assert_eq!(findings(&analysis)?, Vec::<String>::new(), "{name}: {file}'s findings");
        assert!(analysis.edits.is_empty(), "{name}: {file} needs a fix: {}", analysis.diff());
        goods += 1;
    }
    assert!(goods >= 1, "{name}: no good fixture");
    Ok(())
}

#[test]
fn expand_hand_extent() -> TestResult {
    check("expand-hand-extent")
}

#[test]
fn rust_locks() -> TestResult {
    check("rust-locks")
}

#[test]
fn every_rules_file_has_a_test() -> TestResult {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(rules_dir())? {
        let path = entry?.path();
        if path.extension().is_some_and(|e| e == "astlog")
            && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
        {
            found.push(stem.to_owned());
        }
    }
    found.sort();
    assert_eq!(
        found, TESTED,
        "a rules file in tools/astlog/rules with no test in this file, or the reverse"
    );
    Ok(())
}
