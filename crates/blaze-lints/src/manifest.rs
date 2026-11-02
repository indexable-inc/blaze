//! A package's `[lints]`, as cargo reads them from its manifest: its own, or the workspace's
//! `[workspace.lints]` when it has `workspace = true`.

use std::path::Path;

use crate::Level;

/// One lint of a `[lints]` table: its tool (`rust`, `clippy`), its name, level and priority.
pub type ManifestLint = (String, String, Level, i8);

/// The `[lints]` of the package whose manifest is `manifest`, in the workspace whose root
/// manifest is `workspace`.
pub fn lints(manifest: &Path, workspace: &Path) -> Result<Vec<ManifestLint>, String> {
    let table = read(manifest)?;
    let Some(lints) = table.get("lints").and_then(|it| it.as_table()) else {
        return Ok(Vec::new());
    };
    if lints.get("workspace").and_then(|it| it.as_bool()) == Some(true) {
        let root = read(workspace)?;
        let inherited =
            root.get("workspace").and_then(|it| it.get("lints")).and_then(|it| it.as_table());
        let Some(inherited) = inherited else {
            return Err(format!(
                "{}: error inheriting `lints` from workspace root manifest's `workspace.lints`: \
                 `workspace.lints` was not defined in {}",
                manifest.display(),
                workspace.display()
            ));
        };
        return of_table(inherited, workspace);
    }
    of_table(lints, manifest)
}

/// The flags cargo gives rustc for `lints`, in its order: `--warn=clippy::all`.
pub fn flags(lints: &[ManifestLint]) -> Vec<String> {
    let command_line = crate::level::of_manifest(lints);
    let flags = command_line.lints.opts.into_iter();
    flags.map(|(name, level)| format!("{}={name}", crate::level::flag_of(level))).collect()
}

fn read(path: &Path) -> Result<toml::Table, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("reading {}: {error}", path.display()))?;
    text.parse::<toml::Table>()
        .map_err(|error| format!("reading {}: {}", path.display(), error.message()))
}

/// The lints of `lints`, read from `path`, refused where cargo's `TomlLints` does not
/// deserialize: a level that is not one, a table without one, a priority that is not an `i8`.
fn of_table(lints: &toml::Table, path: &Path) -> Result<Vec<ManifestLint>, String> {
    let wrong = |at: &str, why: &str| format!("{}: `{at}`: {why}", path.display());
    let mut found = Vec::new();
    for (tool, of_tool) in lints {
        if tool == "workspace" {
            continue;
        }
        let Some(of_tool) = of_tool.as_table() else {
            return Err(wrong(&format!("lints.{tool}"), "expected a table of lints"));
        };
        for (name, value) in of_tool {
            let at = format!("lints.{tool}.{name}");
            let (level, priority) = match value {
                toml::Value::String(level) => (level.as_str(), 0),
                toml::Value::Table(it) => {
                    let level = match it.get("level") {
                        Some(toml::Value::String(level)) => level.as_str(),
                        Some(_) => return Err(wrong(&at, "`level` is not a string")),
                        None => return Err(wrong(&at, "missing field `level`")),
                    };
                    let priority = match it.get("priority") {
                        None => 0,
                        Some(toml::Value::Integer(it)) => i8::try_from(*it)
                            .map_err(|_| wrong(&at, &format!("invalid value: integer `{it}`, expected i8")))?,
                        Some(_) => return Err(wrong(&at, "`priority` is not an integer")),
                    };
                    (level, priority)
                }
                _ => return Err(wrong(&at, "expected a level or a table with `level`")),
            };
            let level = match level {
                "forbid" => Level::Forbid,
                "deny" => Level::Deny,
                "warn" => Level::Warn,
                "allow" => Level::Allow,
                other => {
                    return Err(wrong(
                        &at,
                        &format!("unknown variant `{other}`, expected one of `forbid`, `deny`, `warn`, `allow`"),
                    ));
                }
            };
            found.push((tool.clone(), name.clone(), level, priority));
        }
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inherited_from_the_workspace() {
        let dir = std::env::temp_dir().join(format!("blaze-lints-manifest-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("member")).unwrap();
        let root = dir.join("Cargo.toml");
        std::fs::write(
            &root,
            "[workspace]\n[workspace.lints.clippy]\nall = { level = \"warn\", priority = -1 }\nempty_enums = \"warn\"\n",
        )
        .unwrap();
        let member = dir.join("member/Cargo.toml");
        std::fs::write(&member, "[package]\nname = \"m\"\n[lints]\nworkspace = true\n").unwrap();
        let mut found = lints(&member, &root).unwrap();
        found.sort();
        assert_eq!(
            found,
            vec![
                ("clippy".to_owned(), "all".to_owned(), Level::Warn, -1),
                ("clippy".to_owned(), "empty_enums".to_owned(), Level::Warn, 0),
            ]
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// What cargo's `TomlLints` does not deserialize is the manifest's error, never a lint
    /// left out or a priority of 0.
    #[test]
    fn a_lint_cargo_does_not_read_is_an_error() {
        let dir = std::env::temp_dir().join(format!("blaze-lints-refused-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let manifest = dir.join("Cargo.toml");
        for (table, says) in [
            ("[lints.clippy]\nall = \"loud\"\n", "unknown variant `loud`"),
            ("[lints.clippy]\nall = { priority = 1 }\n", "missing field `level`"),
            ("[lints.clippy]\nall = { level = \"warn\", priority = 300 }\n", "integer `300`, expected i8"),
            ("[lints]\nclippy = 1\n", "`lints.clippy`: expected a table"),
            ("[lints]\nworkspace = true\n", "`workspace.lints` was not defined"),
        ] {
            std::fs::write(&manifest, format!("[package]\nname = \"m\"\n{table}")).unwrap();
            let error = lints(&manifest, &manifest).expect_err(table);
            assert!(error.contains(says), "{table}: {error}");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn blazes_table_becomes_blaze_flags() {
        let dir = std::env::temp_dir().join(format!("blaze-lints-blaze-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let manifest = dir.join("Cargo.toml");
        std::fs::write(
            &manifest,
            "[package]\nname = \"m\"\n[lints.blaze]\nhot = \"deny\"\nstd_hashmap = { level = \"warn\", priority = 1 }\n[lints.clippy]\nall = \"warn\"\n",
        )
        .unwrap();
        let found = lints(&manifest, &manifest).unwrap();
        assert_eq!(
            flags(&found),
            ["--deny=blaze::hot", "--warn=clippy::all", "--warn=blaze::std_hashmap"]
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
