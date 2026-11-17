//! The sans-IO layout reader over each workspace of `tests/fixtures` that cargo read, against
//! what cargo 1.99 printed for it (`expected.json`): the members in cargo's order, each one's
//! edition, library and binary targets with their root files, and its dependencies with their
//! sources, requirements and features. The files are read here, outside the reader, as a
//! driver would.

#![cfg(unix)]

use std::collections::BTreeSet;
use std::path::Path;

use blaze_data::Value;
use blaze_manifest::layout::{DepKind, Entry, GitRef, Layout, Need, Pass, Source, Units};
use blaze_own::Alloc;

type Json = Value<Alloc<'static>>;

fn fixtures() -> &'static Path {
    Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures"))
}

/// The layout of the workspace whose start is `start`, under the anchor `anchor`.
fn read(anchor: &Path, start: &str) -> Units<'static> {
    let alloc = blaze_own::testing::heap().alloc();
    let mut layout = Layout::new(start, alloc).unwrap();
    for _ in 0..64 {
        match layout.pass().unwrap() {
            Pass::Done(units) => return units,
            Pass::Need(needs) => {
                for need in needs.iter() {
                    match need {
                        Need::Manifest(path) => {
                            let text = std::fs::read_to_string(anchor.join(path.as_str())).ok();
                            layout.manifest(path, text.as_deref()).unwrap();
                        }
                        Need::Listing(dir) => {
                            let names: Option<Vec<(String, bool)>> = std::fs::read_dir(anchor.join(dir.as_str())).ok().map(|entries| {
                                entries
                                    .map(|entry| {
                                        let entry = entry.unwrap();
                                        (entry.file_name().into_string().unwrap(), entry.file_type().unwrap().is_dir())
                                    })
                                    .collect()
                            });
                            let entries: Option<Vec<Entry<'_>>> =
                                names.as_ref().map(|names| names.iter().map(|(name, is_dir)| Entry { name, is_dir: *is_dir }).collect());
                            layout.listing(dir, entries.as_deref());
                        }
                    }
                }
            }
        }
    }
    panic!("the reader did not finish in 64 passes");
}

fn text(value: &Json) -> Option<String> {
    value.as_str().map(str::to_owned)
}

/// A target as `(kind, name, root)`, kind `lib`, `proc-macro` or `bin`.
type TargetRow = (String, String, String);
/// A dependency as cargo prints it: package, rename, kind, target, req, optional, default
/// features, features, source, path.
type DepRow = (String, Option<String>, Option<String>, Option<String>, String, bool, bool, Vec<String>, Option<String>, Option<String>);

fn expected_targets(package: &Json) -> BTreeSet<TargetRow> {
    let mut out = BTreeSet::new();
    for target in package.get("targets").unwrap().as_list().unwrap() {
        let kinds: Vec<&str> = target.get("kind").unwrap().as_list().unwrap().iter().map(|k| k.as_str().unwrap()).collect();
        let kind = match kinds.as_slice() {
            ["bin"] => "bin",
            ["proc-macro"] => "proc-macro",
            kinds if kinds.iter().any(|k| ["lib", "rlib", "dylib", "cdylib", "staticlib"].contains(k)) => "lib",
            _ => continue,
        };
        out.insert((kind.to_owned(), text(target.get("name").unwrap()).unwrap(), text(target.get("src_path").unwrap()).unwrap()));
    }
    out
}

fn expected_deps(package: &Json) -> BTreeSet<DepRow> {
    let mut out = BTreeSet::new();
    for dep in package.get("dependencies").unwrap().as_list().unwrap() {
        let mut features: Vec<String> = dep.get("features").unwrap().as_list().unwrap().iter().map(|f| text(f).unwrap()).collect();
        features.sort();
        out.insert((
            text(dep.get("name").unwrap()).unwrap(),
            text(dep.get("rename").unwrap()),
            text(dep.get("kind").unwrap()),
            text(dep.get("target").unwrap()),
            text(dep.get("req").unwrap()).unwrap(),
            dep.get("optional").unwrap().as_bool().unwrap(),
            dep.get("uses_default_features").unwrap().as_bool().unwrap(),
            features,
            text(dep.get("source").unwrap()),
            dep.get("path").and_then(text),
        ));
    }
    out
}

/// cargo's printed requirement: none is `*`; a bare version is a caret one.
fn req(written: Option<&str>) -> String {
    match written {
        None => "*".to_owned(),
        Some(req) if req.starts_with(|c: char| c.is_ascii_digit()) => format!("^{req}"),
        Some(req) => req.to_owned(),
    }
}

fn rooted(dir: &str) -> String {
    match dir.is_empty() {
        true => "$ROOT".to_owned(),
        false => format!("$ROOT/{dir}"),
    }
}

fn actual_deps(units: &Units<'_>, index: usize) -> BTreeSet<DepRow> {
    let mut out = BTreeSet::new();
    for dep in units.members[index].dependencies.iter() {
        let (req_text, source, path) = match &dep.source {
            Source::Member(i) => (req(None), None, Some(rooted(&units.members[*i].dir))),
            Source::Path(dir) => (req(None), None, Some(rooted(dir))),
            Source::Registry { req: written, registry } => {
                assert!(registry.is_none(), "no fixture names a registry");
                (req(written.as_deref()), Some("registry+https://github.com/rust-lang/crates.io-index".to_owned()), None)
            }
            Source::Outside(path) => panic!("no fixture has a path dependency out of its directory: {}", path.as_str()),
            Source::Git { url, reference } => {
                let query = match reference {
                    GitRef::DefaultBranch => String::new(),
                    GitRef::Branch(b) => format!("?branch={}", b.as_str()),
                    GitRef::Tag(t) => format!("?tag={}", t.as_str()),
                    GitRef::Rev(r) => format!("?rev={}", r.as_str()),
                };
                (req(None), Some(format!("git+{}{query}", url.as_str())), None)
            }
        };
        let kind = match dep.kind {
            DepKind::Normal => None,
            DepKind::Dev => Some("dev".to_owned()),
            DepKind::Build => Some("build".to_owned()),
        };
        let rename = (dep.name != dep.package).then(|| dep.name.as_str().to_owned());
        let mut features: Vec<String> = dep.features.iter().map(|f| f.as_str().to_owned()).collect();
        features.sort();
        out.insert((
            dep.package.as_str().to_owned(),
            rename,
            kind,
            dep.target.as_ref().map(|t| t.as_str().to_owned()),
            req_text,
            dep.optional,
            dep.default_features,
            features,
            source,
            path,
        ));
    }
    out
}

fn check(fixture: &str, start: &str) {
    let root = fixtures().join(fixture);
    let expected_text = std::fs::read(root.join("expected.json")).unwrap();
    let expected: Json = blaze_json::read_in(&expected_text, blaze_json::Limits::DEFAULT, blaze_own::testing::heap().alloc()).unwrap();
    let units = read(&root, start);

    let packages = expected.get("packages").unwrap().as_list().unwrap();
    let members: Vec<String> = expected
        .get("workspace_members")
        .unwrap()
        .as_list()
        .unwrap()
        .iter()
        .map(|id| {
            let id = id.as_str().unwrap();
            let package = packages.iter().find(|p| p.get("id").unwrap().as_str() == Some(id)).unwrap();
            text(package.get("name").unwrap()).unwrap()
        })
        .collect();
    let names: Vec<String> = units.members.iter().map(|m| m.name.as_str().to_owned()).collect();
    assert_eq!(names, members, "{fixture}: members");

    for (index, member) in units.members.iter().enumerate() {
        let package = packages.iter().find(|p| p.get("name").unwrap().as_str() == Some(member.name.as_str())).unwrap();
        assert_eq!(
            text(package.get("manifest_path").unwrap()).unwrap(),
            format!("{}/Cargo.toml", rooted(&member.dir)),
            "{fixture}: {} manifest",
            member.name.as_str()
        );
        assert_eq!(
            package.get("edition").unwrap().as_str().unwrap(),
            member.edition.syntax.to_string(),
            "{fixture}: {} edition",
            member.name.as_str()
        );
        let mut targets = BTreeSet::new();
        if let Some(lib) = &member.lib {
            let kind = if lib.proc_macro { "proc-macro" } else { "lib" };
            targets.insert((kind.to_owned(), lib.name.as_str().to_owned(), rooted(&lib.root)));
        }
        for bin in member.bins.iter() {
            targets.insert(("bin".to_owned(), bin.name.as_str().to_owned(), rooted(&bin.root)));
        }
        assert_eq!(targets, expected_targets(package), "{fixture}: {} targets", member.name.as_str());
        assert_eq!(actual_deps(&units, index), expected_deps(package), "{fixture}: {} dependencies", member.name.as_str());

        let features: BTreeSet<(String, Vec<String>)> = member
            .features
            .iter()
            .map(|f| (f.name.as_str().to_owned(), f.enables.iter().map(|e| e.as_str().to_owned()).collect()))
            .collect();
        let expected_features: BTreeSet<(String, Vec<String>)> = package
            .get("features")
            .unwrap()
            .as_map()
            .unwrap()
            .iter()
            .map(|(name, enables)| (name.to_owned(), enables.as_list().unwrap().iter().map(|e| text(e).unwrap()).collect()))
            .collect();
        assert_eq!(features, expected_features, "{fixture}: {} features", member.name.as_str());
    }
}

#[test]
fn discovery() {
    check("discovery", "crates/app");
}

#[test]
fn inherit() {
    check("inherit", "");
}

#[test]
fn single() {
    check("single", "");
}

#[test]
fn not_a_member() {
    let root = fixtures().join("not-a-member");
    let alloc = blaze_own::testing::heap().alloc();
    let mut layout = Layout::new("stray", alloc).unwrap();
    let error = loop {
        match layout.pass() {
            Err(error) => break error,
            Ok(Pass::Done(_)) => panic!("cargo refuses a package its root does not list"),
            Ok(Pass::Need(needs)) => {
                for need in needs.iter() {
                    if let Need::Manifest(path) = need {
                        let text = std::fs::read_to_string(root.join(path.as_str())).ok();
                        layout.manifest(path, text.as_deref()).unwrap();
                    }
                }
            }
        }
    };
    assert!(matches!(error, blaze_manifest::layout::LayoutError::NotAMember { .. }), "{error}");
}
