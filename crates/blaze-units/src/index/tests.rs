use blaze_resolve::{DepKind, SourceId};

use super::*;

pub(crate) const SERDE_SHA256: &str =
    "5f0e2c6ed6606019b4e29e69dbaba95b11854410e5347d525002456dbbb786b6";
/// serde 1.0.219's line of `https://index.crates.io/se/rd/serde`.
pub(crate) const SERDE_LINE: &str = r#"{"name":"serde","vers":"1.0.219","deps":[{"name":"serde_derive","req":"=1.0.219","features":[],"optional":true,"default_features":true,"target":null,"kind":"normal"},{"name":"serde_derive","req":"^1","features":[],"optional":false,"default_features":true,"target":"cfg(any())","kind":"normal"}],"cksum":"5f0e2c6ed6606019b4e29e69dbaba95b11854410e5347d525002456dbbb786b6","features":{"alloc":[],"default":["std"],"derive":["serde_derive"],"rc":[],"std":[],"unstable":[]},"yanked":false,"rust_version":"1.31"}"#;

fn status(line: &str) -> Status {
    parse_line(line.as_bytes()).unwrap().status
}

#[test]
fn paths() {
    assert_eq!(dep_path("a", false), "1/a");
    assert_eq!(dep_path("ab", false), "2/ab");
    assert_eq!(dep_path("abc", false), "3/a/abc");
    assert_eq!(dep_path("Abc", false), "3/A/Abc");
    assert_eq!(dep_path("AbCd", false), "Ab/Cd/AbCd");
    assert_eq!(dep_path("aBcDe", false), "aB/cD/aBcDe");
    assert_eq!(dep_path("abc", true), "3/a");
    assert_eq!(dep_path("aBcDe", true), "aB/cD");
}

#[test]
fn lines() {
    let line = r#"{"name":"a","vers":"0.0.1","deps":[{"name":"b2","package":"b","req":"^1","features":["","x"],"optional":true,"default_features":false,"target":"cfg(unix)","kind":"build","registry":"sparse+https://example.com/index/"}],"cksum":"bae3","features":{"f":["b2/x"]},"features2":{"f":["dep:b2"],"g":[]},"links":"a-sys","rust_version":"1.70","pubtime":"2025-11-12T19:30:12Z","unknown":[1,2]}"#;
    let IndexLine { entry, status: read } = parse_line(line.as_bytes()).unwrap();
    assert_eq!(read, Status::Candidate);
    assert_eq!(entry.features["f"], ["b2/x", "dep:b2"]);
    assert_eq!(entry.features["g"], Vec::<String>::new());
    assert_eq!(entry.features.len(), 2);
    assert_eq!(entry.rust_version.as_ref().unwrap().to_string(), "1.70");
    let dep = &entry.deps[0];
    assert_eq!((dep.name_in_toml.as_str(), dep.package.as_str()), ("b2", "b"));
    assert_eq!(dep.features, ["x"]);
    assert_eq!(dep.kind, DepKind::Build);
    assert!(dep.optional && !dep.default_features);
    assert_eq!(dep.platform.as_ref().unwrap().to_string(), "cfg(unix)");
    assert_eq!(dep.registry.as_ref().unwrap().url().as_str(), "sparse+https://example.com/index/");

    let base = r#""name":"a","vers":"1.0.0","deps":[],"cksum":"00""#;
    assert_eq!(status(&format!("{{{base},\"yanked\":true}}")), Status::Yanked);
    assert_eq!(status(&format!("{{{base},\"v\":2}}")), Status::Candidate);
    assert_eq!(status(&format!("{{{base},\"v\":3}}")), Status::Unsupported(3));
    // Invalid past its name and version, and kept as such with what can still be read.
    let bad = parse_line(br#"{"name":"a","vers":"1.0.0","deps":[{"name":"b","req":"not a req"}],"cksum":"00","rust_version":"1.60"}"#).unwrap();
    assert_eq!(bad.status, Status::Invalid);
    assert_eq!(bad.entry.rust_version.as_ref().unwrap().to_string(), "1.60");
    assert_eq!(status(&format!("{{{base},\"pubtime\":\"2025-11-12T19:30:12-04\"}}")), Status::Invalid);
    assert_eq!(status(&format!("{{{base},\"pubtime\":\"2025-02-29T19:30:12Z\"}}")), Status::Invalid);
    assert_eq!(status(&format!("{{{base},\"rust_version\":\"1.70-beta\"}}")), Status::Invalid);
    assert_eq!(status(&format!("{{{base},\"rust_version\":\"^1.70\"}}")), Status::Invalid);
    assert_eq!(status(r#"{"name":"a","vers":"1.0.0","cksum":"00"}"#), Status::Invalid);
    assert_eq!(status(&format!("{{{base},\"cksum\":\"01\"}}")), Status::Invalid);
    assert_eq!(
        status(&format!("{{{base},\"deps\":[{{\"name\":\"b\",\"req\":\"1\",\"target\":\"cfg(\"}}]}}")),
        Status::Invalid
    );
    assert_eq!(
        status(&format!("{{{base},\"deps\":[{{\"name\":\"b\",\"req\":\"1\",\"artifact\":[\"rlib\"]}}]}}")),
        Status::Invalid
    );
    // A wrong `v` does not keep the version from being read.
    assert_eq!(status(&format!("{{{base},\"v\":-1}}")), Status::Invalid);
    assert!(parse_line(br#"{"name":"a","vers":"one"}"#).is_err());
    assert!(parse_line(b"").is_err());
}

#[test]
fn crates_io_drops_its_own_registry() {
    let line = r#"{"name":"a","vers":"1.0.0","deps":[{"name":"b","req":"1","registry":"https://github.com/rust-lang/crates.io-index"}],"cksum":"00"}"#;
    assert_eq!(parse_line(line.as_bytes()).unwrap().entry.deps[0].registry, None);
}

#[test]
fn an_artifact_dependency_keeps_its_kinds_lib_and_target() {
    let line = r#"{"name":"a","vers":"1.0.0","deps":[{"name":"b","req":"1","artifact":["bin:tool","cdylib"],"lib":true,"bindep_target":"target"}],"cksum":"00"}"#;
    let line = parse_line(line.as_bytes()).unwrap();
    assert_eq!(line.status, Status::Candidate);
    let artifact = line.entry.deps[0].artifact.as_ref().unwrap();
    assert_eq!(artifact.kinds, ["bin:tool", "cdylib"]);
    assert!(artifact.lib);
    assert_eq!(artifact.target.as_deref(), Some("target"));
    // Read, but not yet something the resolver takes.
    assert!(matches!(index_summary(&line, SourceId::crates_io()), Some(IndexSummary::Invalid(_))));
}

#[test]
fn a_line_of_serde_s_entry_reads_as_cargo_reads_it() {
    let line = parse_line(SERDE_LINE.as_bytes()).unwrap();
    assert_eq!(line.status, Status::Candidate);
    let entry = &line.entry;
    assert_eq!((entry.name.as_str(), entry.version.to_string()), ("serde", "1.0.219".to_owned()));
    assert_eq!(entry.checksum, SERDE_SHA256);
    assert_eq!(entry.rust_version.as_ref().map(ToString::to_string).as_deref(), Some("1.31"));
    assert_eq!(entry.features["default"], ["std"]);
    assert_eq!(entry.features["derive"], ["serde_derive"]);
    let [optional, never] = &entry.deps[..] else { panic!("{:?}", entry.deps) };
    assert!(optional.optional && optional.platform.is_none());
    assert_eq!(optional.req.to_string(), "=1.0.219");
    assert_eq!(never.platform.as_ref().map(ToString::to_string).as_deref(), Some("cfg(any())"));

    let base = r#""name":"a","vers":"1.0.0","deps":[],"cksum":"00""#;
    let status = |line: String| parse_line(line.as_bytes()).unwrap().status;
    assert_eq!(status(format!("{{{base},\"yanked\":true}}")), Status::Yanked);
    assert_eq!(status(format!("{{{base},\"v\":3}}")), Status::Unsupported(3));
    assert_eq!(status(format!("{{{base},\"rust_version\":\"1.70-beta\"}}")), Status::Invalid);
    let two = r#"{"name":"a","vers":"0.0.1","deps":[{"name":"b2","package":"b","req":"^1","features":["","x"],"kind":"build","registry":"sparse+https://example.com/index/"}],"cksum":"00","features":{"f":["b2/x"]},"features2":{"f":["dep:b2"]},"links":"a-sys","v":2}"#;
    let line = parse_line(two.as_bytes()).unwrap();
    assert_eq!(line.status, Status::Candidate);
    assert_eq!(line.entry.features["f"], ["b2/x", "dep:b2"]);
    assert_eq!(line.entry.links.as_deref(), Some("a-sys"));
    let dep = &line.entry.deps[0];
    assert_eq!((dep.name_in_toml.as_str(), dep.package.as_str()), ("b2", "b"));
    assert_eq!(dep.features, ["x"]);
    assert_eq!(dep.kind, DepKind::Build);
    assert_eq!(dep.registry.map(|it| it.url().to_string()).as_deref(), Some("sparse+https://example.com/index/"));
    assert!(parse_line(br#"{"name":"a","vers":"one"}"#).is_err());
}

#[test]
fn a_file_s_lines_are_parsed_only_when_a_version_matches() {
    let file = [
        r#"{"name":"a","vers":"1.0.0","deps":[],"cksum":"01"}"#,
        r#"{"name":"a","vers":"one"}"#,
        "",
        r#"{"vers":"1.1.0"}"#,
        r#"{"name":"a","vers":"1.1.0","deps":"not a list","cksum":"02"}"#,
        r#"{"name":"a","vers":"1.2.0","deps":[],"cksum":"03","yanked":true}"#,
    ]
    .join("\n");
    let lines = Lines::new(file.into_bytes());
    assert_eq!(lines.parsed_count(), 0);
    let found = |min: &str| -> Vec<(String, Status)> {
        let min = Version::parse(min).unwrap();
        lines.matching(|it| *it >= min).map(|it| (it.entry.version.to_string(), it.status)).collect()
    };
    assert_eq!(found("1.2.0"), [("1.2.0".to_owned(), Status::Yanked)]);
    assert_eq!(lines.parsed_count(), 1);
    assert_eq!(
        found("0.0.0"),
        [
            ("1.0.0".to_owned(), Status::Candidate),
            ("1.1.0".to_owned(), Status::Invalid),
            ("1.2.0".to_owned(), Status::Yanked),
        ]
    );
    assert_eq!(lines.parsed_count(), 3);
}

#[test]
fn a_summary_s_features_are_in_name_order_whatever_the_line_s_order() {
    let line = r#"{"name":"a","vers":"1.0.0","deps":[],"cksum":"00","features":{"zz":[],"b":["zz"],"a1":[]},"features2":{"m":[],"a0":[]}}"#;
    let line = parse_line(line.as_bytes()).unwrap();
    let summary = index_summary(&line, SourceId::crates_io()).unwrap();
    let names: Vec<&str> = summary.summary().features().keys().map(|it| it.as_str()).collect();
    assert_eq!(names, ["a0", "a1", "b", "m", "zz"]);
}

#[test]
fn prefix_paths_are_cargos() {
    assert_eq!(index_path("a"), "1/a");
    assert_eq!(index_path("ab"), "2/ab");
    assert_eq!(index_path("abc"), "3/a/abc");
    assert_eq!(index_path("serde"), "se/rd/serde");
    assert_eq!(index_path("Serde"), "se/rd/serde");
    assert_eq!(dep_path("Serde", true), "Se/rd");
    assert_eq!(dep_path("abc", true), "3/a");
    assert_eq!(
        crate_url("https://x.example/{prefix}/{lowerprefix}/{crate}", "Serde", "1.0.0", ""),
        "https://x.example/Se/rd/se/rd/Serde"
    );
}

#[test]
fn a_config_json_reads_as_cargo_reads_it() {
    let config: RegistryConfig =
        serde_json::from_str(r#"{"dl":"https://dl.example.com","api":"https://api.example.com","auth-required":true,"other":1}"#)
            .unwrap();
    assert_eq!(config.dl, "https://dl.example.com");
    assert_eq!(config.api.as_deref(), Some("https://api.example.com"));
    assert!(config.auth_required);
    let plain: RegistryConfig = serde_json::from_str(r#"{"dl":"https://dl.example.com","api":null}"#).unwrap();
    assert_eq!((plain.api, plain.auth_required), (None, false));
    assert!(serde_json::from_str::<RegistryConfig>(r#"{"api":"x"}"#).is_err());
}

/// Every string of up to `len` of `pieces`, joined.
fn strings(pieces: &[&str], len: usize) -> Vec<String> {
    let mut out = vec![String::new()];
    let mut last = vec![String::new()];
    for _ in 0..len {
        last = last.iter().flat_map(|it| pieces.iter().map(move |piece| format!("{it}{piece}"))).collect();
        out.extend(last.iter().cloned());
    }
    out
}

/// The probe [`Lines`] and v1's `Summaries` find versions with agrees with [`parse_line`]: the
/// same lines kept, at the same versions.
#[test]
fn a_line_s_version_is_the_one_parse_line_reads() {
    let mut lines: Vec<String> = [
        SERDE_LINE,
        r#"{"name":"a","vers":"1.0.0","deps":[],"cksum":"01"}"#,
        r#"{"name":"a","vers":"one"}"#,
        r#"{"vers":"1.1.0"}"#,
        r#"{"name":"a","vers":"1.1.0","deps":"not a list","cksum":"02"}"#,
        r#"{"name":"a\u0062","vers":"1.0.1+meta","deps":[],"cksum":"03"}"#,
        r#"{"name":1,"vers":"1.0.0"}"#,
        r#"{"name":"a","name":"b","vers":"1.0.0"}"#,
        r#"{"name":"a","vers":"1.0.0-rc.1"}"#,
        r#"["a","1.0.0"]"#,
        "not json",
        "",
    ]
    .iter()
    .map(|it| (*it).to_owned())
    .collect();
    for version in strings(&["1", ".", "0", "-", "a", "+"], 4) {
        lines.push(format!(r#"{{"name":"a","vers":"{version}","deps":[],"cksum":"00"}}"#));
    }
    for line in &lines {
        let parsed = parse_line(line.as_bytes()).ok().map(|it| it.entry.version);
        assert_eq!(line_version(line.as_bytes()), parsed, "{line}");
    }
}

/// v1's `RustVersion` of `crates/blaze-registry/src/index.rs`, kept to check blaze-resolve's,
/// which the index reads now, takes the same strings to the same versions.
mod v1_rust_version {
    use anyhow::bail;
    use semver::{Version, VersionReq};

    pub(super) fn parse(value: &str) -> anyhow::Result<(u64, Option<u64>, Option<u64>)> {
        let (major, minor, patch, pre, build) = match Version::parse(value) {
            Ok(it) => (it.major, Some(it.minor), Some(it.patch), !it.pre.is_empty(), !it.build.is_empty()),
            Err(_) => {
                let mut req = match VersionReq::parse(value) {
                    Ok(req) => req,
                    Err(_) if value.contains('-') => bail!("unexpected prerelease field"),
                    Err(_) if value.contains('+') => bail!("unexpected build field"),
                    Err(_) => bail!("expected a version like \"1.32\""),
                };
                if req.comparators.len() != 1 {
                    bail!("unexpected version requirement");
                }
                let comparator = req.comparators.pop().expect("one comparator");
                if comparator.op != semver::Op::Caret || value.starts_with('^') {
                    bail!("unexpected version requirement");
                }
                let pre = !comparator.pre.is_empty();
                (comparator.major, comparator.minor, comparator.patch, pre, false)
            }
        };
        if pre || build {
            bail!("a prerelease or build");
        }
        Ok((major, minor, patch))
    }

    pub(super) fn display((major, minor, patch): (u64, Option<u64>, Option<u64>)) -> String {
        let mut out = major.to_string();
        for part in [minor, patch].into_iter().flatten() {
            out.push_str(&format!(".{part}"));
        }
        out
    }
}

#[test]
fn a_rust_version_reads_as_v1_read_it() {
    let mut cases: Vec<String> = ["1.70", "1", "1.70.0", " 1.70", "1.70 ", "^1.70", "=1.70", ">=1", "1.x", "1.*"]
        .iter()
        .map(|it| (*it).to_owned())
        .collect();
    cases.extend(strings(&["1", "7", ".", "-", "+", "^", "b", "*", " ", ","], 4));
    for case in &cases {
        let old = v1_rust_version::parse(case).ok();
        let new = case.parse::<RustVersion>().ok();
        assert_eq!(old.is_some(), new.is_some(), "`{case}`");
        if let (Some(old), Some(new)) = (old, new) {
            assert_eq!(v1_rust_version::display(old), new.to_string(), "`{case}`");
        }
    }
}

/// The index's own artifact check before it took blaze-manifest's (`check_artifact` of v2's
/// `load/source/index.rs`, the same rule as v1's `artifact`), kept to check both refuse the same
/// lines. The messages differ on purpose: blaze-manifest's are cargo's.
fn old_check_artifact(kinds: &[String], target: Option<&str>) -> bool {
    for kind in kinds {
        if !matches!(kind.as_str(), "bin" | "cdylib" | "staticlib") && !kind.starts_with("bin:") {
            return false;
        }
    }
    if kinds.iter().any(|it| it == "bin") && kinds.iter().any(|it| it.starts_with("bin:")) {
        return false;
    }
    let mut unique = kinds.to_vec();
    unique.sort();
    unique.dedup();
    if unique.len() != kinds.len() {
        return false;
    }
    if let Some(target) = target {
        let target = target.trim();
        if target.is_empty() || (target != "target" && target.ends_with(".json")) {
            return false;
        }
    }
    true
}

#[test]
fn an_artifact_is_refused_as_the_index_refused_it() {
    let kinds = ["bin", "bin:a", "bin:", "cdylib", "staticlib", "rlib"];
    let mut lists: Vec<Vec<String>> = vec![Vec::new()];
    for _ in 0..3 {
        let longer: Vec<Vec<String>> = lists
            .iter()
            .flat_map(|list| {
                kinds.iter().map(move |kind| {
                    let mut list = list.clone();
                    list.push((*kind).to_owned());
                    list
                })
            })
            .collect();
        lists.extend(longer);
    }
    let targets =
        [None, Some(""), Some(" "), Some("target"), Some(" target "), Some("x86_64-unknown-linux-gnu"), Some("a.json"), Some(" a.json ")];
    for list in &lists {
        for target in targets {
            assert_eq!(
                Artifact::parse(list, false, target).is_ok(),
                old_check_artifact(list, target),
                "{list:?} {target:?}"
            );
        }
    }
}
