//! The reader over a tree held in memory: what it asks for, how many passes, and that each
//! span is the line it names.


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
        self.read_as(start, false)
    }

    /// [`Tree::read`], as the workspace the user builds (`test_targets`) or as a dependency.
    fn read_as(&self, start: &str, test_targets: bool) -> Result<(Units<'static>, usize), LayoutError<'static>> {
        self.read_opts(start, test_targets, false)
    }

    fn read_opts(&self, start: &str, test_targets: bool, path_packages: bool) -> Result<(Units<'static>, usize), LayoutError<'static>> {
        let alloc = blaze_own::testing::heap().alloc();
        let mut layout = Layout::new(start, alloc)?;
        if test_targets {
            layout = layout.with_test_targets();
        }
        if path_packages {
            layout = layout.with_path_packages();
        }
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

/// Integration tests, examples and benches (the Cargo Book, "Cargo Targets", "Target
/// auto-discovery": `tests/<name>.rs` and `tests/<name>/main.rs`, likewise `examples/` and
/// `benches/`; "Setting the keys [..] `autotests`, or `autobenches` to `false` [..] will disable
/// auto-discovery of the corresponding target type"; a `[[test]]` table adds one with its own
/// `path` and `harness`). Prints:
///
/// ```text
/// extras: test a pkg/tests/a.rs harness found | test b pkg/tests/b/main.rs harness found | test c pkg/other/c.rs no-harness written | example e pkg/examples/e.rs harness found | bench x pkg/benches/x.rs harness found
/// autotests = false: test c pkg/other/c.rs no-harness written | example e pkg/examples/e.rs harness found | bench x pkg/benches/x.rs harness found
/// ```
///
/// then, for a dependency (read without `with_test_targets`) whose `[[test]] name = "http"` has
/// no `tests/http.rs`, and for the same manifest as a workspace member:
///
/// ```text
/// as a dependency: 0 test targets, no error
/// as a member: package `cookie-factory` test `http`: no file `pkg/tests/http.rs` for its root
/// ```
///
/// A member's bad target (no file, a `path` out of the package, a second of one name) is a
/// finding in `Member::refused_targets`, never a `LayoutError`: the layout is read whole and the
/// member's other targets stay.
#[test]
fn tests_examples_and_benches_are_targets() {
    const MANIFEST: &str = "[package]\nname = \"pkg\"\nedition = \"2021\"\n\n[[test]]\nname = \"c\"\npath = \"other/c.rs\"\nharness = false\n";
    const NO_AUTO: &str = "[package]\nname = \"pkg\"\nedition = \"2021\"\nautotests = false\n\n[[test]]\nname = \"c\"\npath = \"other/c.rs\"\nharness = false\n";
    let files = |manifest: &'static str| -> std::vec::Vec<(&'static str, &'static str)> {
        std::vec![
            ("pkg/Cargo.toml", manifest),
            ("pkg/src/lib.rs", ""),
            // Listed out of order: the reader sorts by name bytes.
            ("pkg/tests/b/main.rs", ""),
            ("pkg/tests/b/helper.rs", ""),
            ("pkg/tests/a.rs", ""),
            ("pkg/tests/.hidden.rs", ""),
            ("pkg/tests/data.txt", ""),
            ("pkg/examples/e.rs", ""),
            ("pkg/benches/x.rs", ""),
            ("pkg/other/c.rs", ""),
        ]
    };
    let rows = |manifest: &'static str| -> std::string::String {
        let files = files(manifest);
        let (units, _) = Tree { files: &files }.read_as("pkg", true).unwrap();
        let rows: std::vec::Vec<std::string::String> = units.members[0]
            .extras
            .iter()
            .map(|t| {
                let kind = match t.kind {
                    super::ExtraKind::Test => "test",
                    super::ExtraKind::Example => "example",
                    super::ExtraKind::Bench => "bench",
                };
                let harness = if t.harness { "harness" } else { "no-harness" };
                let how = if t.found { "found" } else { "written" };
                std::format!("{kind} {} {} {harness} {how}", t.name.as_str(), t.root.as_str())
            })
            .collect();
        rows.join(" | ")
    };
    let all = rows(MANIFEST);
    std::println!("extras: {all}");
    assert_eq!(
        all,
        "test a pkg/tests/a.rs harness found | test b pkg/tests/b/main.rs harness found | test c pkg/other/c.rs no-harness written | example e pkg/examples/e.rs harness found | bench x pkg/benches/x.rs harness found"
    );
    let no_auto = rows(NO_AUTO);
    std::println!("autotests = false: {no_auto}");
    assert_eq!(no_auto, "test c pkg/other/c.rs no-harness written | example e pkg/examples/e.rs harness found | bench x pkg/benches/x.rs harness found");

    // A dependency (read without `with_test_targets`): no test, example or bench target, and a
    // `[[test]]` whose file its archive left out is no error (cookie-factory 0.3.3's `http`).
    const HTTP: &str = "[package]\nname = \"cookie-factory\"\n\n[[test]]\nname = \"http\"\n";
    let dependency = Tree { files: &[("pkg/Cargo.toml", HTTP), ("pkg/src/lib.rs", "")] };
    let (units, _) = dependency.read("pkg").unwrap();
    assert_eq!(units.members[0].extras.len(), 0, "a dependency's layout lists a test target");
    std::println!("as a dependency: {} test targets, no error", units.members[0].extras.len());
    let doesnt = Tree { files: &[("pkg/Cargo.toml", MANIFEST), ("pkg/src/lib.rs", "")] };
    assert_eq!(doesnt.read("pkg").unwrap().0.members[0].extras.len(), 0, "a dependency's `[[test]]` path was looked for");

    // The same manifests as the workspace the user builds: each bad target is a finding on its
    // member (`refused_targets`, naming the target and the rule), left out of `extras`, and the
    // layout is still read whole: the member's library and its good targets are all there, so
    // one bad `[[example]]` hides nothing else from `check`.
    let refused = |files: &[(&'static str, &'static str)]| {
        let (units, _) = (Tree { files }).read_as("pkg", true).unwrap();
        let member = &units.members[0];
        assert!(member.lib.is_some(), "a refused target cost the member its library");
        let good: std::vec::Vec<std::string::String> = member.extras.iter().map(|t| t.name.as_str().to_owned()).collect();
        let found: std::vec::Vec<(std::string::String, super::TargetRefused<'static>)> =
            member.refused_targets.iter().map(|r| (r.target.as_str().to_owned(), r.why.clone())).collect();
        (member.name.as_str().to_owned(), good, found)
    };
    let (package, good, found) = refused(&[("pkg/Cargo.toml", HTTP), ("pkg/src/lib.rs", ""), ("pkg/examples/e.rs", "")]);
    assert_eq!(good, ["e"], "the good example was dropped with the bad test");
    assert_eq!(found.len(), 1);
    let (target, why) = &found[0];
    std::println!("as a member: package `{package}` test `{target}`: {why}");
    assert_eq!((package.as_str(), target.as_str()), ("cookie-factory", "http"));
    assert!(matches!(why, super::TargetRefused::NoFile { .. }));
    let (_, _, found) = refused(&[("pkg/Cargo.toml", "[package]\nname = \"pkg\"\n[[test]]\nname = \"out\"\npath = \"../x.rs\"\n"), ("pkg/src/lib.rs", ""), ("x.rs", "")]);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].0.as_str(), "out");
    assert!(matches!(found[0].1, super::TargetRefused::OutsidePackage { .. }));
    // Two tables of one kind and name: the first stays, the second is the finding.
    let (_, good, found) = refused(&[("pkg/Cargo.toml", "[package]\nname = \"pkg\"\nedition = \"2021\"\n[[test]]\nname = \"d\"\npath = \"src/lib.rs\"\n[[test]]\nname = \"d\"\npath = \"src/lib.rs\"\n"), ("pkg/src/lib.rs", "")]);
    assert_eq!(good, ["d"]);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].0.as_str(), "d");
    assert!(matches!(found[0].1, super::TargetRefused::Duplicate { .. }));
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

#[test]
fn patch_and_transitive_path_packages_outside_members() {
    const WS: &str = "\
[workspace]
members = [\"crates/app\"]
exclude = [\"vendor/sub\", \"vendor/helper\", \"vendor/excluded\", \"vendor/unused\"]

[workspace.package]
edition = \"2024\"

[patch.crates-io]
patched = { path = \"vendor/sub/crates/patched\" }
unused-patch = { path = \"vendor/unused\" }
git-patch = { git = \"https://example.com/git-patch.git\" }
";
    const APP_TOML: &str = "\
[package]
name = \"app\"
edition.workspace = true

[dependencies]
patched = \"1.0\"
excluded = { path = \"../../vendor/excluded\" }
";
    const SUB_WS: &str = "\
[workspace]
members = [\"crates/patched\"]

[workspace.package]
version = \"1.2.3\"
edition = \"2021\"
";
    const PATCHED_TOML: &str = "\
[package]
name = \"patched\"
version.workspace = true
edition.workspace = true

[dependencies]
helper = { path = \"../../../helper\" }

[dev-dependencies]
missing-dev = { path = \"../../../does-not-exist\" }
";
    const HELPER_TOML: &str = "[package]\nname = \"helper\"\nversion = \"0.1.0\"\nedition = \"2021\"\n";
    const EXCLUDED_TOML: &str = "[package]\nname = \"excluded\"\nversion = \"0.2.0\"\nedition = \"2021\"\n";
    const UNUSED_TOML: &str = "[package]\nname = \"unused-patch\"\nversion = \"0.3.0\"\nedition = \"2021\"\n";
    let tree = Tree {
        files: &[
            ("Cargo.toml", WS),
            ("crates/app/Cargo.toml", APP_TOML),
            ("crates/app/src/lib.rs", ""),
            ("vendor/sub/Cargo.toml", SUB_WS),
            ("vendor/sub/crates/patched/Cargo.toml", PATCHED_TOML),
            ("vendor/sub/crates/patched/src/lib.rs", ""),
            ("vendor/sub/crates/patched/tests/ignored.rs", ""),
            ("vendor/helper/Cargo.toml", HELPER_TOML),
            ("vendor/helper/src/lib.rs", ""),
            ("vendor/excluded/Cargo.toml", EXCLUDED_TOML),
            ("vendor/excluded/src/lib.rs", ""),
            ("vendor/unused/Cargo.toml", UNUSED_TOML),
            ("vendor/unused/src/lib.rs", ""),
        ],
    };
    let (units, _) = tree.read_opts("", true, true).expect("layout with path packages");
    let alloc = blaze_own::testing::heap().alloc();
    let mut member_names = blaze_own::Vec::new_in(alloc);
    member_names.extend(units.members.iter().map(|m| m.name.as_str()));
    assert_eq!(member_names.as_slice(), ["app"]);
    let mut path_pkgs = blaze_own::Vec::new_in(alloc);
    path_pkgs.extend(units.path_packages.iter().map(|p| (p.dir.as_str(), p.name.as_str())));
    assert_eq!(
        path_pkgs.as_slice(),
        [
            ("vendor/excluded", "excluded"),
            ("vendor/helper", "helper"),
            ("vendor/sub/crates/patched", "patched"),
            ("vendor/unused", "unused-patch"),
        ]
    );
    let patched = units.path_packages.iter().find(|p| p.name.as_str() == "patched").expect("patched");
    assert_eq!(patched.version.as_ref().map(|v| v.as_str()), Some("1.2.3"), "inherited from vendor/sub workspace");
    assert_eq!(patched.edition.syntax, edition::Edition::Edition2021, "inherited edition from vendor/sub, not root");
    assert!(patched.extras.is_empty(), "non-member path package skips test targets");
    assert!(
        !patched.dependencies.iter().any(|d| d.kind == DepKind::Dev),
        "non-member path package skips dev-dependencies"
    );
    // CONTROL: without `with_path_packages`, no non-member path packages are discovered.
    let (without, _) = tree.read_opts("", true, false).expect("layout without path packages");
    assert!(without.path_packages.is_empty());
}

#[test]
fn unused_patch_with_missing_transitive_path_dep_does_not_fail_layout() {
    const ROOT_TOML: &str = r#"[workspace]
members = ["app"]

[patch.crates-io]
unused-patch = { path = "vendor/unused" }
"#;
    const APP_TOML: &str = "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n";
    const UNUSED_TOML: &str = r#"[package]
name = "unused-patch"
version = "0.3.0"
edition = "2021"

[dependencies]
absent-dep = { path = "../does-not-exist" }
"#;
    let tree = Tree {
        files: &[
            ("Cargo.toml", ROOT_TOML),
            ("app/Cargo.toml", APP_TOML),
            ("app/src/lib.rs", ""),
            ("vendor/unused/Cargo.toml", UNUSED_TOML),
            ("vendor/unused/src/lib.rs", ""),
        ],
    };
    let (units, _) = tree.read_opts("", true, true).expect("unused patch with missing transitive dep succeeds");
    assert_eq!(units.members.len(), 1);
    assert_eq!(units.path_packages.len(), 1);
    assert_eq!(units.path_packages[0].name.as_str(), "unused-patch");
    assert_eq!(units.path_packages[0].patched_from.len(), 1);
    assert_eq!(units.path_packages[0].patched_from[0].as_str(), "crates-io");
}
