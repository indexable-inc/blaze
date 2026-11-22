//! A script workspace's `[features]` lines (D112 N6): each feature keeps the entries of the
//! package's own `[features]` table that the script's build can satisfy.
//!
//! The script's member manifest has only the package's build-dependencies (as its
//! `[dependencies]`), so a feature's entries are cut to what names them:
//! - `dep/feat`, `dep?/feat` and `dep:dep` are kept when `dep` is a build-dependency (in
//!   `[build-dependencies]` or a `[target.*.build-dependencies]` form), dropped when it names a
//!   dependency the script does not build with (a normal or dev dependency only);
//! - a bare name is kept when it is a feature the `[features]` table declares or a
//!   build-dependency (an optional one's implicit feature), dropped otherwise (an optional
//!   normal dependency's implicit feature).
//!
//! Without this, librocksdb-sys's `bindgen-runtime = ["bindgen/runtime"]` was written as
//! `"bindgen-runtime" = []`, bindgen and clang-sys were built without `runtime`, and clang-sys's
//! build script looked for libclang to link at build time ("couldn't find any valid shared
//! libraries matching: ['libclang.dylib']").

use super::{build_scripts::build_dep_names, feature_names, table_value};

/// The line `"<feature>" = [<entries>]\n` for the script's manifest: `feature`'s entries in
/// `manifest`'s `[features]` table that name a build-dependency or a declared feature, in the
/// table's order. A name the table does not declare (an optional dependency's implicit feature)
/// is written with no entries.
pub(super) fn script_feature_line(manifest: &str, feature: &str) -> String {
    let build_deps = build_dep_names(manifest);
    let declared = feature_names(manifest);
    let optional = optional_build_deps(manifest);
    let entries = table_value(manifest, "features", feature).map(quoted_strings).unwrap_or_default();
    let kept: Vec<String> =
        entries.into_iter().filter_map(|entry| script_entry(&entry, &build_deps, &optional, &declared)).map(|entry| format!("{entry:?}")).collect();
    format!("{feature:?} = [{}]\n", kept.join(", "))
}

/// The feature entry `entry` as the script's manifest writes it, `None` when it is dropped.
///
/// A name can be an OPTIONAL normal dependency and a plain build-dependency at once
/// (tempo-precompiles: `serde = { workspace = true, optional = true }` under `[dependencies]`,
/// `serde.workspace = true` under `[build-dependencies]`, and `serde = ["dep:serde", ..]`). The
/// script builds with the build-dependency alone, which is always there, so `dep:serde` and the
/// implicit feature `serde` name nothing the script can turn on (Cargo: "feature `serde` includes
/// `dep:serde`, but `serde` is not an optional dependency") and are dropped, and `serde?/x` is
/// `serde/x`: the dependency it waits for is always present.
fn script_entry(entry: &str, build_deps: &[String], optional: &[String], declared: &[String]) -> Option<String> {
    let is_build_dep = |name: &str| build_deps.iter().any(|dep| dep == name);
    let is_optional = |name: &str| optional.iter().any(|dep| dep == name);
    if let Some(dep) = entry.strip_prefix("dep:") {
        return is_optional(dep).then(|| entry.to_owned());
    }
    if let Some((dep, feat)) = entry.split_once('/') {
        let name = dep.strip_suffix('?').unwrap_or(dep);
        if !is_build_dep(name) {
            return None;
        }
        return Some(if dep.ends_with('?') && !is_optional(name) { format!("{name}/{feat}") } else { entry.to_owned() });
    }
    (declared.iter().any(|name| name == entry) || is_optional(entry)).then(|| entry.to_owned())
}

/// The build-dependencies `manifest` declares optional: `optional = true` in a dependency's
/// inline table or dotted key under a `[build-dependencies]` table, or under its own
/// `[build-dependencies.name]` table, and the `[target.*]` forms of each.
fn optional_build_deps(manifest: &str) -> Vec<String> {
    let mut names = Vec::new();
    // Inside a `[..build-dependencies]` table (each line a dependency), or inside one
    // dependency's own `[..build-dependencies.name]` table.
    let mut inside = false;
    let mut table: Option<String> = None;
    for line in manifest.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            let header = trimmed.trim_start_matches('[').trim_end_matches(']').trim();
            let rest = header.strip_prefix("build-dependencies").or_else(|| header.split_once(".build-dependencies").map(|(_, rest)| rest));
            inside = rest == Some("");
            table = rest.and_then(|rest| rest.strip_prefix('.')).map(|name| name.trim_matches('"').to_owned());
            continue;
        }
        let Some((key, value)) = trimmed.split_once('=') else { continue };
        let (key, value) = (key.trim(), value.trim());
        if inside {
            let name = key.split('.').next().unwrap_or("").trim_matches('"');
            let squeezed: String = value.chars().filter(|c| !c.is_whitespace()).collect();
            if squeezed.contains("optional=true") || (key.ends_with(".optional") && value.starts_with("true")) {
                names.push(name.to_owned());
            }
        } else if let Some(name) = &table
            && key == "optional"
            && value.starts_with("true")
        {
            names.push(name.clone());
        }
    }
    names.sort();
    names.dedup();
    names
}

/// The strings of a TOML array's text (`["a", 'b',\n # note\n "c"]`), in order: basic strings
/// (with `\` escapes kept as written) and literal strings; comments skipped.
fn quoted_strings(array: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = array.chars();
    while let Some(c) = chars.next() {
        match c {
            '#' => {
                for c in chars.by_ref() {
                    if c == '\n' {
                        break;
                    }
                }
            }
            '"' | '\'' => {
                let mut text = String::new();
                while let Some(inner) = chars.next() {
                    if inner == c {
                        break;
                    }
                    if inner == '\\' && c == '"' {
                        if let Some(escaped) = chars.next() {
                            text.push(escaped);
                        }
                        continue;
                    }
                    text.push(inner);
                }
                out.push(text);
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::super::build_scripts::script_manifest;

    /// A package whose `rt` enables a build-dependency's feature and a normal dependency's.
    const PACKAGE: &str = "[package]\nname = \"sys\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
[dependencies.libc]\nversion = \"0.2\"\n\n\
[dependencies.extra]\nversion = \"1\"\noptional = true\n\n\
[build-dependencies.bindgen]\nversion = \"0.72\"\n\n\
[target.'cfg(unix)'.build-dependencies]\ncc = { version = \"1\", optional = true }\n\n\
[features]\n\
rt = [\"bindgen/runtime\", \"libc/std\"]\n\
default = [\n    \"static\",\n    # the runtime-linked libclang\n    \"bindgen/runtime\",\n    \"extra\",\n]\n\
static = []\n\
native = [\"cc?/parallel\", \"dep:cc\", \"extra?/x\", \"dep:extra\"]\n";

    /// The script's manifest made from `package`, which the test requires to be written.
    fn made(package: &str, name: &str, version: &str, features: &[&str]) -> String {
        let features: Vec<String> = features.iter().map(|it| (*it).to_owned()).collect();
        let alloc = blaze_own::testing::heap().alloc();
        let dir = Path::new("/build/vendor").join(format!("{name}-{version}"));
        script_manifest(package, &dir.join("Cargo.toml"), &dir.join("build.rs"), name, version, &features, alloc).unwrap_or_else(|failure| panic!("{failure}"))
    }

    fn written(features: &[&str]) -> String {
        made(PACKAGE, "sys", "0.1.0", features)
    }

    /// libsqlite3-sys 0.35.0 as its record has it: `cc`, `pkg-config` and `vcpkg` optional
    /// build-dependencies that no `dep:` entry names, so each is an implicit feature.
    const LIBSQLITE3_SYS: &str = "[package]\nname = \"libsqlite3-sys\"\nversion = \"0.35.0\"\nedition = \"2021\"\nbuild = \"build.rs\"\n\n\
[build-dependencies.cc]\nversion = \"1.1.6\"\noptional = true\n\n\
[build-dependencies.pkg-config]\nversion = \"0.3.19\"\noptional = true\n\n\
[build-dependencies.vcpkg]\nversion = \"0.2.15\"\noptional = true\n\n\
[dependencies.openssl-sys]\nversion = \"0.9.103\"\noptional = true\n\n\
[features]\n\
\"bundled\" = [\"cc\", \"bundled_bindings\"]\n\
\"bundled_bindings\" = []\n\
\"default\" = [\"min_sqlite_version_3_14_0\"]\n\
\"min_sqlite_version_3_14_0\" = [\"pkg-config\", \"vcpkg\"]\n";

    /// turso's libsqlite3-sys 0.35.0 (2026-11-21): the unit's features name the implicit features
    /// `cc`, `pkg-config` and `vcpkg`, and the script's manifest declared each (`"cc" = []`),
    /// which hides the optional dependency's own feature: cargo refused it ("optional dependency
    /// `cc` is not included in any feature"). Written as the package has them, they stay
    /// implicit, and `bundled` keeps its `cc` entry. Twin: the unit without `bundled` keeps the
    /// same rule for `pkg-config` and `vcpkg`. Control: `openssl-sys`, an optional NORMAL
    /// dependency the script does not build with, is not in the script's manifest, so its
    /// feature is declared empty. Prints the feature table.
    #[test]
    fn an_optional_build_dependencys_implicit_feature_stays_implicit() {
        let unit = ["bundled", "bundled_bindings", "cc", "default", "min_sqlite_version_3_14_0", "pkg-config", "vcpkg"];
        let manifest = made(LIBSQLITE3_SYS, "libsqlite3-sys", "0.35.0", &unit);
        let table = manifest.split_once("[features]\n").expect("a features table").1.split_once("\n\n").expect("its end").0;
        println!("libsqlite3-sys script features:\n{table}");
        for name in ["cc", "pkg-config", "vcpkg"] {
            assert!(!manifest.lines().any(|line| line.starts_with(&format!("\"{name}\" ="))), "{name} declared:\n{manifest}");
        }
        assert!(manifest.lines().any(|line| line == "\"bundled\" = [\"cc\", \"bundled_bindings\"]"), "{manifest}");
        assert!(manifest.contains("[dependencies.cc]\nversion = \"1.1.6\"\noptional = true"), "{manifest}");
        let alloc = blaze_own::testing::heap().alloc();
        let doc = super::super::member_dependencies::parse(&manifest, Path::new("script/Cargo.toml"), alloc).unwrap_or_else(|failure| panic!("{failure}"));
        let implicit: Vec<&str> = super::super::member_dependencies::implicit_features(&doc, &super::super::member_dependencies::ALL_KINDS, alloc).iter().copied().collect();
        assert_eq!(implicit, ["cc", "pkg-config", "vcpkg"]);

        let twin = made(LIBSQLITE3_SYS, "libsqlite3-sys", "0.35.0", &["default", "min_sqlite_version_3_14_0", "pkg-config", "vcpkg"]);
        assert!(!twin.lines().any(|line| line.starts_with("\"pkg-config\" =") || line.starts_with("\"vcpkg\" =")), "{twin}");

        let control = made(LIBSQLITE3_SYS, "libsqlite3-sys", "0.35.0", &["default", "min_sqlite_version_3_14_0", "openssl-sys", "pkg-config", "vcpkg"]);
        assert!(control.lines().any(|line| line == "\"openssl-sys\" = []"), "{control}");
        assert!(!control.contains("[dependencies.openssl-sys]"), "{control}");
    }

    /// The Cargo reference ("Features", "Dependency features"): `"dep/feat"` enables `feat` of
    /// dependency `dep`, so `rt = ["bindgen/runtime"]` is what turns on bindgen's runtime
    /// libclang loading; the script builds with bindgen (a build-dependency) and never with
    /// libc (a normal one). Prints "script manifest rt: \"rt\" = [\"bindgen/runtime\"]".
    #[test]
    fn a_feature_keeps_its_build_dependency_enables() {
        let manifest = written(&["rt"]);
        let line = manifest.lines().find(|line| line.starts_with("\"rt\"")).expect("the rt line");
        println!("script manifest rt: {line}");
        assert_eq!(line, "\"rt\" = [\"bindgen/runtime\"]");
    }

    /// `dep?/feat` and `dep:dep` on a target-form optional build-dependency are kept; on an
    /// optional normal dependency dropped; a declared feature's name kept, a normal optional
    /// dependency's implicit feature dropped; a multi-line array with a comment read whole.
    /// Prints "script manifest default: .. | native: ..".
    #[test]
    fn every_entry_form_is_cut_to_the_build_dependencies() {
        let manifest = written(&["default", "native", "static"]);
        let line = |name: &str| manifest.lines().find(|line| line.starts_with(&format!("\"{name}\" ="))).expect("a feature line").to_owned();
        println!("script manifest default: {} | native: {}", line("default"), line("native"));
        assert_eq!(line("default"), "\"default\" = [\"static\", \"bindgen/runtime\"]");
        assert_eq!(line("native"), "\"native\" = [\"cc?/parallel\", \"dep:cc\"]");
        assert_eq!(line("static"), "\"static\" = []");
    }

    /// tempo-precompiles 1.18.3's shape (the 1,100-crate workspace, 2026-11-17: "feature `serde`
    /// includes `dep:serde`, but `serde` is not an optional dependency", and no module was
    /// built): a name that is an optional normal dependency AND a plain build-dependency. The
    /// script's manifest has the build-dependency alone, so `dep:serde` and the implicit
    /// feature are dropped and the weak `serde?/derive` is `serde/derive`. Prints the lines.
    #[test]
    fn a_plain_build_dependency_that_is_also_an_optional_dependency_keeps_no_dep_entry() {
        const BOTH: &str = "[package]\nname = \"pre\"\nversion = \"1.0.0\"\nedition = \"2024\"\n\n\
[dependencies]\nserde = { version = \"1\", optional = true }\nserde_json = { version = \"1\", optional = true }\n\n\
[build-dependencies]\nserde = { version = \"1\" }\nserde_json = { version = \"1\", features = [\"std\"] }\n\n\
[features]\n\
default = [\"rpc\"]\n\
rpc = []\n\
serde = [\"dep:serde\", \"serde?/derive\", \"other/serde\"]\n\
test-utils = [\"dep:serde\", \"dep:serde_json\", \"serde_json\", \"rpc\"]\n";
        let features: Vec<String> = ["default", "rpc", "serde", "test-utils"].iter().map(|it| (*it).to_owned()).collect();
        let alloc = blaze_own::testing::heap().alloc();
        let manifest = script_manifest(BOTH, Path::new("/build/pre/Cargo.toml"), Path::new("/build/pre/build.rs"), "pre", "1.0.0", &features, alloc)
            .unwrap_or_else(|failure| panic!("{failure}"));
        let line = |name: &str| manifest.lines().find(|line| line.starts_with(&format!("\"{name}\" ="))).expect("a feature line").to_owned();
        println!("script manifest serde: {} | test-utils: {}", line("serde"), line("test-utils"));
        assert_eq!(line("serde"), "\"serde\" = [\"serde/derive\"]");
        assert_eq!(line("test-utils"), "\"test-utils\" = [\"rpc\"]");
        assert_eq!(super::optional_build_deps(BOTH), Vec::<String>::new());
        assert_eq!(super::optional_build_deps(PACKAGE), vec!["cc".to_owned()]);
    }
}
