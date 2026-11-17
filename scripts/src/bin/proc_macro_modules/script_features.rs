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
    let entries = table_value(manifest, "features", feature).map(quoted_strings).unwrap_or_default();
    let kept: Vec<String> =
        entries.into_iter().filter(|entry| script_keeps(entry, &build_deps, &declared)).map(|entry| format!("{entry:?}")).collect();
    format!("{feature:?} = [{}]\n", kept.join(", "))
}

/// Whether the script's manifest keeps the feature entry `entry`.
fn script_keeps(entry: &str, build_deps: &[String], declared: &[String]) -> bool {
    let is_build_dep = |name: &str| build_deps.iter().any(|dep| dep == name);
    if let Some(dep) = entry.strip_prefix("dep:") {
        return is_build_dep(dep);
    }
    if let Some((dep, _)) = entry.split_once('/') {
        return is_build_dep(dep.strip_suffix('?').unwrap_or(dep));
    }
    declared.iter().any(|name| name == entry) || is_build_dep(entry)
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

    fn written(features: &[&str]) -> String {
        let features: Vec<String> = features.iter().map(|it| (*it).to_owned()).collect();
        script_manifest(PACKAGE, Path::new("/build/vendor/sys-0.1.0/build.rs"), "sys", "0.1.0", &features)
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
}
