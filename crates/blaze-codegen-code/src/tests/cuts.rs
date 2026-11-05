//! An identity's cuts held to the files of its roots that name them (C1 of the overkey review),
//! on blaze's own tree: a root may call a cut package only from a file [`Cut::named_in`] lists
//! with why no keyed value depends on it, and never from the code that makes a record. Rust read
//! as tokens, and only the code a crate is compiled from (tests aside, inline `#[cfg(test)]`
//! modules too), as the hooks test does.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

use super::hooks::{Token, compiled, outside_tests, roots, tokens};
use crate::{IDENTITIES, Identity};

/// Files of blaze-cargo that make a record: no cut may be named in them, whatever the reason.
const MAKES_RECORDS: &[(&str, &str)] =
    &[("blaze-cargo", "src/serve/unit/"), ("blaze-cargo", "src/serve/unit.rs")];

/// The crate name a package's code names it by.
fn crate_name(package: &str) -> String {
    package.replace('-', "_")
}

/// The dependency of `manifest` (and its workspace's, for `workspace = true`) renamed from a cut
/// package: a rename hides every use from the scan below.
fn renamed(manifest: &toml::Table, workspace: &toml::Table, cut: &BTreeSet<&str>) -> Vec<String> {
    let inherited = workspace
        .get("workspace")
        .and_then(|it| it.get("dependencies"))
        .and_then(toml::Value::as_table);
    let mut tables: Vec<&toml::Table> = Vec::new();
    for key in ["dependencies", "build-dependencies"] {
        tables.extend(manifest.get(key).and_then(toml::Value::as_table));
    }
    for target in manifest
        .get("target")
        .and_then(toml::Value::as_table)
        .into_iter()
        .flat_map(|it| it.values())
    {
        for key in ["dependencies", "build-dependencies"] {
            tables.extend(target.get(key).and_then(toml::Value::as_table));
        }
    }
    let mut out = Vec::new();
    for table in tables {
        for (key, value) in table {
            let package = match value.get("package").and_then(toml::Value::as_str) {
                Some(package) => Some(package),
                None if value.get("workspace").and_then(toml::Value::as_bool) == Some(true) => {
                    inherited
                        .and_then(|it| it.get(key))
                        .and_then(|it| it.get("package"))
                        .and_then(toml::Value::as_str)
                }
                None => None,
            };
            if let Some(package) = package
                && cut.contains(package)
                && package != key
            {
                out.push(format!(
                    "depends on the cut `{package}` as `{key}`: name it by its own name"
                ));
            }
        }
    }
    out
}

/// Every problem of `identities`' cuts against their roots' files in the checkout.
fn uncovered(identities: &[&Identity]) -> Vec<String> {
    let engine = Path::new(env!("CARGO_MANIFEST_DIR")).join("../blaze-engine");
    let workspace = crate::Workspace::read(&engine).unwrap();
    let checkout: BTreeMap<&str, &Path> = workspace.checkout().into_iter().collect();
    let top: toml::Table =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Cargo.toml"))
            .unwrap()
            .parse()
            .unwrap();
    let mut problems = Vec::new();
    for identity in identities.iter().filter(|it| !it.cut.is_empty()) {
        let cut_packages: BTreeSet<&str> = identity.cut.iter().map(|it| it.package).collect();
        let by_crate: BTreeMap<String, &str> =
            cut_packages.iter().map(|it| (crate_name(it), *it)).collect();
        // (root, file from its directory, cut package) for each file that names a cut.
        let mut found: BTreeSet<(String, String, String)> = BTreeSet::new();
        for root in identity.roots {
            let Some(&dir) = checkout.get(root.package) else { continue };
            let manifest: toml::Table =
                std::fs::read_to_string(dir.join("Cargo.toml")).unwrap().parse().unwrap();
            for problem in renamed(&manifest, &top, &cut_packages) {
                problems.push(format!("{}: {} {problem}", identity.what, root.package));
            }
            let mut files: Vec<_> = roots(dir, false).iter().flat_map(|it| compiled(it)).collect();
            files.sort();
            files.dedup();
            for file in files {
                let relative = file.strip_prefix(dir).unwrap().to_string_lossy().replace('\\', "/");
                let tokens = outside_tests(&tokens(&std::fs::read_to_string(&file).unwrap()));
                for (at, token) in tokens.iter().enumerate() {
                    let Token::Ident(word) = token else { continue };
                    let Some(&package) = by_crate.get(word) else { continue };
                    found.insert((root.package.to_owned(), relative.clone(), package.to_owned()));
                    if matches!(tokens.get(at + 1), Some(Token::Ident(it)) if it == "as") {
                        problems.push(format!(
                            "{relative}: renames `{word}`, which hides its uses from this test"
                        ));
                    }
                    let start = tokens[..at]
                        .iter()
                        .rposition(|it| matches!(it, Token::Punct(';' | '{' | '}')))
                        .map_or(0, |it| it + 1);
                    let statement = &tokens[start..at];
                    let word_at = |w: &str| {
                        statement.iter().position(|it| matches!(it, Token::Ident(x) if x == w))
                    };
                    if let (Some(public), Some(used)) = (word_at("pub"), word_at("use"))
                        && public < used
                    {
                        problems.push(format!(
                            "{relative}: re-exports `{word}`, which hides its uses from this test"
                        ));
                    }
                }
            }
        }
        let mut listed = BTreeSet::new();
        for cut in identity.cut {
            for it in cut.named_in {
                listed.insert((it.package.to_owned(), it.file.to_owned(), cut.package.to_owned()));
            }
        }
        for (root, file, cut) in found.difference(&listed) {
            problems.push(format!(
                "{root}/{file} names `{cut}`, which {} cuts: list the file in the cut's `named_in` with why no \
                 keyed value depends on it, or drop the cut",
                identity.what
            ));
        }
        for (root, file, cut) in listed.difference(&found) {
            problems.push(format!(
                "{}: `{cut}` is listed as named in {root}/{file}, which no longer names it",
                identity.what
            ));
        }
        for (root, file, cut) in found.union(&listed) {
            if MAKES_RECORDS
                .iter()
                .any(|(package, under)| package == root && file.starts_with(under))
            {
                problems.push(format!(
                    "{root}/{file} makes a record and names `{cut}`, which {} cuts: drop the cut",
                    identity.what
                ));
            }
        }
    }
    problems
}

#[test]
fn every_file_of_a_root_that_names_a_cut_is_listed_and_none_makes_a_record() {
    let problems = uncovered(IDENTITIES);
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}
