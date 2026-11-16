//! The reader over a tree held in memory: what it asks for, how many passes, and that each
//! span is the line it names.

use blaze_own::Alloc;

use super::{DepKind, Entry, Layout, LayoutError, Need, Pass, Source, Units};

/// A tree: files with their texts, and directories implied by them.
struct Tree<'f> {
    files: &'f [(&'f str, &'f str)],
}

impl<'f> Tree<'f> {
    fn file(&self, path: &str) -> Option<&'f str> {
        self.files.iter().find(|(p, _)| *p == path).map(|(_, text)| *text)
    }

    /// The entries of `dir`: every first component under it.
    fn list(&self, dir: &str) -> Option<std::vec::Vec<Entry<'f>>> {
        let mut out: std::vec::Vec<Entry<'f>> = std::vec::Vec::new();
        for (path, _) in self.files {
            let rest = match dir.is_empty() {
                true => *path,
                false => match path.strip_prefix(dir).and_then(|rest| rest.strip_prefix('/')) {
                    Some(rest) => rest,
                    None => continue,
                },
            };
            let (name, is_dir) = match rest.find('/') {
                Some(i) => (&rest[..i], true),
                None => (rest, false),
            };
            if !out.iter().any(|entry| entry.name == name) {
                out.push(Entry { name, is_dir });
            }
        }
        match out.is_empty() {
            true => None,
            false => Some(out),
        }
    }

    /// The layout read from `start`, and the number of passes it took.
    fn read(&self, start: &str) -> Result<(Units<'static>, usize), LayoutError<'static>> {
        let alloc = Alloc::global();
        let mut layout = Layout::new(start, alloc)?;
        for passes in 1..64 {
            match layout.pass()? {
                Pass::Done(units) => return Ok((units, passes)),
                Pass::Need(needs) => {
                    assert!(!needs.is_empty());
                    for need in needs.iter() {
                        match need {
                            Need::Manifest(path) => layout.manifest(path, self.file(path))?,
                            Need::Listing(dir) => {
                                let entries = self.list(dir);
                                layout.listing(dir, entries.as_deref());
                            }
                        }
                    }
                }
            }
        }
        panic!("the reader did not finish in 64 passes");
    }
}

const ROOT: &str = "[workspace]\nmembers = [\"crates/*\"]\nexclude = [\"crates/skip\"]\n\n[workspace.dependencies]\nshared = { path = \"libs/shared\" }\nserde = { version = \"1\", default-features = false }\n";
const APP: &str = "[package]\nname = \"app\"\nedition = \"blaze-2026.11\"\n\n[dependencies]\nshared.workspace = true\nserde = { workspace = true, features = [\"derive\"] }\nother = { path = \"../other\" }\n\n[dev-dependencies]\nrand = \"0.8\"\n\n[features]\nfast = [\"serde/std\"]\n";
const OTHER: &str = "[package]\nname = \"other\"\nedition = \"2021\"\n\n[[bin]]\nname = \"run\"\npath = \"src/run.rs\"\n";
const SHARED: &str = "[package]\nname = \"shared-lib\"\n";
const SKIP: &str = "[package]\nname = \"skip\"\n";

fn tree() -> Tree<'static> {
    Tree {
        files: &[
            ("ws/Cargo.toml", ROOT),
            ("ws/crates/app/Cargo.toml", APP),
            ("ws/crates/app/src/main.rs", ""),
            ("ws/crates/app/src/lib.rs", ""),
            ("ws/crates/app/src/bin/two.rs", ""),
            ("ws/crates/other/Cargo.toml", OTHER),
            ("ws/crates/other/src/run.rs", ""),
            ("ws/crates/skip/Cargo.toml", SKIP),
            ("ws/crates/skip/src/lib.rs", ""),
            ("ws/libs/shared/Cargo.toml", SHARED),
            ("ws/libs/shared/src/lib.rs", ""),
        ],
    }
}

/// The text a span covers.
fn spanned<'t>(units: &Units<'_>, tree: &Tree<'t>, span: super::Span) -> &'t str {
    let path = units.manifests[span.manifest.0 as usize].as_str();
    &tree.file(path).unwrap()[span.range()]
}

#[test]
fn a_workspace_from_a_member() {
    let tree = tree();
    let (units, passes) = tree.read("ws/crates/app").unwrap();
    // The start, the manifests above it, `crates/`, then the members' `src/` and their own
    // `src/bin/`, then the path dependency's `src/`.
    assert!(passes <= 6, "{passes} passes");
    assert_eq!(units.manifests[units.root.0 as usize].as_str(), "ws/Cargo.toml");
    let names: std::vec::Vec<&str> = units.members.iter().map(|m| m.name.as_str()).collect();
    assert_eq!(names, ["app", "shared-lib", "other"]);

    let app = &units.members[0];
    assert_eq!(app.dir.as_str(), "ws/crates/app");
    assert_eq!(app.edition.std_crate(), "bstd");
    assert_eq!(spanned(&units, &tree, app.edition_span.unwrap()), "edition = \"blaze-2026.11\"");
    assert_eq!(spanned(&units, &tree, app.span), "[package]");
    let lib = app.lib.as_ref().unwrap();
    assert_eq!((lib.name.as_str(), lib.root.as_str(), lib.found), ("app", "ws/crates/app/src/lib.rs", true));
    let bins: std::vec::Vec<(&str, &str)> = app.bins.iter().map(|b| (b.name.as_str(), b.root.as_str())).collect();
    assert_eq!(bins, [("app", "ws/crates/app/src/main.rs"), ("two", "ws/crates/app/src/bin/two.rs")]);

    let deps = &app.dependencies;
    assert_eq!(deps.len(), 4);
    assert!(matches!(deps[0].source, Source::Member(1)));
    assert_eq!(spanned(&units, &tree, deps[0].span), "shared.workspace = true");
    match &deps[1].source {
        Source::Registry { req: Some(req), registry: None } => assert_eq!(req.as_str(), "1"),
        _ => panic!("serde is from the registry"),
    }
    assert!(!deps[1].default_features);
    let features: std::vec::Vec<&str> = deps[1].features.iter().map(|f| f.as_str()).collect();
    assert_eq!(features, ["derive"]);
    assert!(matches!(deps[2].source, Source::Member(2)));
    assert_eq!(spanned(&units, &tree, deps[2].span), "other = { path = \"../other\" }");
    assert_eq!(deps[3].kind, DepKind::Dev);
    assert_eq!(spanned(&units, &tree, deps[3].span), "rand = \"0.8\"");
    assert_eq!(spanned(&units, &tree, app.features[0].span), "fast = [\"serde/std\"]");

    let other = &units.members[2];
    assert!(other.lib.is_none());
    assert_eq!(other.bins.len(), 1);
    assert_eq!(spanned(&units, &tree, other.bins[0].span), "[[bin]]");
    assert_eq!(other.bins[0].root.as_str(), "ws/crates/other/src/run.rs");
    let shared = &units.members[1];
    assert_eq!(shared.lib.as_ref().unwrap().name.as_str(), "shared_lib");
}

#[test]
fn an_excluded_package_is_its_own_workspace() {
    let (units, _) = tree().read("ws/crates/skip").unwrap();
    assert_eq!(units.members.len(), 1);
    assert_eq!(units.members[0].name.as_str(), "skip");
    assert_eq!(units.manifests[units.root.0 as usize].as_str(), "ws/crates/skip/Cargo.toml");
}

#[test]
fn a_path_dependency_out_of_the_anchor_is_outside() {
    let tree = Tree {
        files: &[
            ("Cargo.toml", "[package]\nname = \"a\"\n[dependencies]\nb = { path = \"../b\" }\nbstd = { package = \"blaze-std\", path = \"/abs/blaze-std\" }\n"),
            ("src/lib.rs", ""),
        ],
    };
    let (units, _) = tree.read("").unwrap();
    assert_eq!(units.members.len(), 1);
    let deps = &units.members[0].dependencies;
    let outside: std::vec::Vec<&str> = deps
        .iter()
        .map(|dep| match &dep.source {
            Source::Outside(path) => path.as_str(),
            _ => panic!("a path out of the anchor is outside"),
        })
        .collect();
    assert_eq!(outside, ["../b", "/abs/blaze-std"]);
    assert_eq!(deps[1].package.as_str(), "blaze-std");
    assert_eq!(spanned(&units, &tree, deps[1].span), "bstd = { package = \"blaze-std\", path = \"/abs/blaze-std\" }");
}

#[test]
fn a_target_path_out_of_the_anchor_is_refused() {
    let tree = Tree { files: &[("Cargo.toml", "[package]\nname = \"a\"\n[lib]\npath = \"../x.rs\"\n"), ("src/lib.rs", "")] };
    assert!(matches!(tree.read(""), Err(LayoutError::OutsideAnchor { .. })));
}

#[test]
fn a_package_under_a_root_that_does_not_list_it() {
    let tree = Tree {
        files: &[
            ("Cargo.toml", "[workspace]\nmembers = [\"a\"]\n"),
            ("a/Cargo.toml", "[package]\nname = \"a\"\n"),
            ("b/Cargo.toml", "[package]\nname = \"b\"\n"),
        ],
    };
    assert!(matches!(tree.read("b"), Err(LayoutError::NotAMember { .. })));
}
