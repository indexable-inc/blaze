//! A package as `cargo metadata` prints it, read as blaze-resolve's `Summary`, and the root
//! manifest's `[patch]` and `[replace]` tables.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use blaze_resolve::workspace_resolve::Patch;
use blaze_resolve::{
    DepKind, Dependency, GitReference, InternedString, PackageIdSpec, Platform, SourceId, Summary,
};
use serde_json::Value;
use url::Url;

const LIB_KINDS: &[&str] = &["lib", "rlib", "dylib", "cdylib", "staticlib", "proc-macro"];

fn str_of<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value[key].as_str().ok_or_else(|| anyhow!("no `{key}` in a package"))
}

/// What the resolve and its graph read of one package.
pub struct Read {
    pub summary: Summary,
    /// The crate name of its library, `-` as `_`.
    pub lib: Option<InternedString>,
    pub proc_macro_lib: bool,
    pub any_proc_macro: bool,
    /// Whether it has a build script, without which its build dependencies are not built.
    pub build_script: bool,
    /// The `required-features` of each of its binaries.
    pub bins: Vec<Vec<String>>,
    /// `package.license` and `package.repository`.
    pub license: Option<String>,
    pub repository: Option<String>,
}

pub(crate) fn package_source(package: &Value) -> Result<SourceId> {
    match package["source"].as_str() {
        Some(source) => SourceId::from_url(source),
        None => SourceId::for_path(
            Path::new(str_of(package, "manifest_path")?).parent().context("a manifest's dir")?,
        ),
    }
}

fn dependency(dep: &Value) -> Result<Dependency> {
    let source = match (dep["source"].as_str(), dep["path"].as_str()) {
        (Some(source), _) => SourceId::from_url(source)?,
        (None, Some(path)) => SourceId::for_path(Path::new(path))?,
        (None, None) => SourceId::crates_io(),
    };
    // A path or git dependency without a `version` is written `*` but, unlike `version = "*"`,
    // matches prereleases.
    let req = match str_of(dep, "req")? {
        "*" if !source.is_registry() => None,
        req => Some(req),
    };
    let mut d = Dependency::parse(str_of(dep, "name")?, req, source)?;
    d.set_kind(match dep["kind"].as_str() {
        None => DepKind::Normal,
        Some("dev") => DepKind::Development,
        Some("build") => DepKind::Build,
        Some(kind) => return Err(anyhow!("dependency kind `{kind}`")),
    });
    if let Some(rename) = dep["rename"].as_str() {
        d.set_explicit_name_in_toml(rename);
    }
    d.set_optional(dep["optional"].as_bool().unwrap_or(false));
    d.set_default_features(dep["uses_default_features"].as_bool().unwrap_or(true));
    let features: Vec<&str> =
        dep["features"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
    d.set_features(features);
    if let Some(target) = dep["target"].as_str() {
        d.set_platform(Some(target.parse::<Platform>()?));
    }
    Ok(d)
}

/// `package`, the JSON `cargo metadata` prints of it, read.
pub(crate) fn read(package: &Value) -> Result<Read> {
    let id = blaze_resolve::PackageId::try_new(
        str_of(package, "name")?,
        str_of(package, "version")?,
        package_source(package)?,
    )?;
    let deps = package["dependencies"]
        .as_array()
        .into_iter()
        .flatten()
        .map(dependency)
        .collect::<Result<Vec<_>>>()?;
    let features: BTreeMap<InternedString, Vec<InternedString>> = package["features"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(name, values)| {
            let values = values.as_array().into_iter().flatten().filter_map(Value::as_str);
            (InternedString::new(name), values.map(InternedString::new).collect())
        })
        .collect();
    let summary = Summary::new(id, deps, &features, package["links"].as_str(), None)?;
    let kinds = |target: &Value| -> Vec<String> {
        let kinds = target["kind"].as_array().into_iter().flatten().filter_map(Value::as_str);
        kinds.map(str::to_owned).collect()
    };
    let targets: Vec<&Value> = package["targets"].as_array().into_iter().flatten().collect();
    let lib = targets.iter().find(|it| kinds(it).iter().any(|k| LIB_KINDS.contains(&k.as_str())));
    Ok(Read {
        summary,
        lib: match lib {
            Some(lib) => Some(InternedString::new(&str_of(lib, "name")?.replace('-', "_"))),
            None => None,
        },
        proc_macro_lib: lib.is_some_and(|it| kinds(it).iter().any(|k| k == "proc-macro")),
        any_proc_macro: targets.iter().any(|it| kinds(it).iter().any(|k| k == "proc-macro")),
        build_script: targets.iter().any(|it| kinds(it).iter().any(|k| k == "custom-build")),
        bins: targets
            .iter()
            .filter(|it| kinds(it).iter().any(|k| k == "bin"))
            .map(|it| {
                let required = it["required-features"].as_array().into_iter().flatten();
                required.filter_map(Value::as_str).map(str::to_owned).collect()
            })
            .collect(),
        license: package["license"].as_str().map(str::to_owned),
        repository: package["repository"].as_str().map(str::to_owned),
    })
}

/// `path` with `.` and `..` taken out, as cargo writes a path dependency's source.
pub(crate) fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            c => out.push(c),
        }
    }
    out
}

/// One entry of a `[patch]` table of the root manifest.
fn patch_dependency(root: &Path, name: &str, entry: &toml::Value) -> Result<Option<Dependency>> {
    let get = |key: &str| entry.get(key).and_then(|v| v.as_str());
    let (version, source) = match entry {
        toml::Value::String(version) => (Some(version.as_str()), SourceId::crates_io()),
        _ if get("registry").is_some() => return Ok(None),
        _ => {
            let source = if let Some(path) = get("path") {
                SourceId::for_path(&normalize(&root.join(path)))?
            } else if let Some(git) = get("git") {
                let reference = match (get("branch"), get("tag"), get("rev")) {
                    (Some(b), _, _) => GitReference::Branch(b.to_owned()),
                    (_, Some(t), _) => GitReference::Tag(t.to_owned()),
                    (_, _, Some(r)) => GitReference::Rev(r.to_owned()),
                    _ => GitReference::DefaultBranch,
                };
                SourceId::for_git(&git.parse()?, reference)?
            } else {
                SourceId::crates_io()
            };
            (get("version"), source)
        }
    };
    let mut dep = Dependency::parse(get("package").unwrap_or(name), version, source)?;
    if get("package").is_some() {
        dep.set_explicit_name_in_toml(name);
    }
    Ok(Some(dep))
}

/// The directories of the path packages the root manifest's `[patch]` names.
pub(crate) fn patch_paths(root: &Path) -> Result<Vec<PathBuf>> {
    let manifest = root_table(root)?;
    let tables = manifest.get("patch").and_then(|p| p.as_table()).into_iter().flatten();
    let entries = tables.flat_map(|(_, table)| table.as_table().into_iter().flatten());
    let paths = entries.filter_map(|(_, entry)| entry.get("path")?.as_str());
    Ok(paths.map(|path| normalize(&root.join(path))).collect())
}

fn root_table(root: &Path) -> Result<toml::Table> {
    let path = root.join("Cargo.toml");
    let text = std::fs::read_to_string(&path).with_context(|| path.display().to_string())?;
    Ok(toml::from_str(&text)?)
}

/// The root manifest's `[patch]`, without entries that name an alternative registry.
pub(crate) fn patch_tables(root: &Path) -> Result<Vec<(Url, Vec<Patch>)>> {
    let manifest_path = root.join("Cargo.toml");
    let manifest = root_table(root)?;
    let mut tables = Vec::new();
    for (key, table) in manifest.get("patch").and_then(|p| p.as_table()).into_iter().flatten() {
        let url = if key == "crates-io" {
            SourceId::crates_io().url().clone()
        } else {
            match key.parse::<Url>() {
                Ok(url) => url,
                Err(_) => continue,
            }
        };
        let mut patches = Vec::new();
        for (name, entry) in table.as_table().into_iter().flatten() {
            let Some(dep) = patch_dependency(root, name, entry)? else { continue };
            patches.push(Patch { dep, loc: manifest_path.display().to_string() });
        }
        tables.push((url, patches));
    }
    Ok(tables)
}

/// The root manifest's `[replace]`, as cargo 1.99's `toml::replace` reads it: each key a spec
/// with a version (its source crates.io unless it names one), each value the source it is
/// replaced with, required at exactly that version.
pub(crate) fn replace_table(root: &Path) -> Result<Vec<(PackageIdSpec, Dependency)>> {
    let manifest = root_table(root)?;
    let Some(table) = manifest.get("replace").and_then(|r| r.as_table()) else {
        return Ok(Vec::new());
    };
    if manifest.contains_key("patch") {
        bail!("cannot specify both [replace] and [patch]");
    }
    let mut replace = Vec::new();
    for (key, entry) in table {
        let mut spec = PackageIdSpec::parse(key).with_context(|| {
            format!(
                "replacements must specify a valid semver version to replace, but `{key}` does not"
            )
        })?;
        if spec.url().is_none() {
            spec = spec.with_url(SourceId::crates_io().url().clone());
        }
        if entry.is_str() || entry.get("version").is_some() {
            bail!("replacements cannot specify a version requirement, but found one for `{spec}`");
        }
        if entry.get("registry").is_some() {
            bail!("blaze reads no `registry` in `[replace]` (for `{spec}`)");
        }
        let version = spec.version().ok_or_else(|| {
            anyhow!("replacements must specify a version to replace, but `{spec}` does not")
        })?;
        let mut entry = entry.clone();
        if let Some(it) = entry.as_table_mut() {
            it.insert("version".to_owned(), toml::Value::String(format!("={version}")));
        }
        let dep = patch_dependency(root, spec.name(), &entry)?.context("a `[replace]` entry")?;
        replace.push((spec, dep));
    }
    Ok(replace)
}

/// The directories of the path packages the root manifest's `[replace]` names.
pub(crate) fn replace_paths(replace: &[(PackageIdSpec, Dependency)]) -> Vec<PathBuf> {
    replace.iter().filter_map(|(_, dep)| dep.source_id().local_path()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root_with(name: &str, manifest: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("blaze-replace-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("Cargo.toml"), manifest).unwrap();
        root
    }

    fn replace_of(name: &str, table: &str) -> Result<Vec<(PackageIdSpec, Dependency)>> {
        replace_table(&root_with(name, &format!("[workspace]\n\n[replace]\n{table}")))
    }

    #[test]
    fn a_name_and_version_is_a_crates_io_package_replaced_at_exactly_that_version() {
        let replace =
            replace_of("crates-io", "\"foo:0.1.0\" = { path = \"vendor/../foo\" }").unwrap();
        let [(spec, dep)] = &replace[..] else { panic!("{replace:?}") };
        assert_eq!(spec.name(), "foo");
        assert_eq!(spec.version(), Some(semver::Version::new(0, 1, 0)));
        assert_eq!(spec.url(), Some(SourceId::crates_io().url()));
        assert_eq!(dep.package_name().as_str(), "foo");
        assert_eq!(dep.version_req().to_string(), "=0.1.0");
        let root = root_with("crates-io", "");
        assert_eq!(dep.source_id().local_path(), Some(root.join("foo")));
        assert_eq!(replace_paths(&replace), vec![root.join("foo")]);
    }

    #[test]
    fn a_spec_names_its_own_source() {
        let replace = replace_of(
            "source",
            "\"registry+file:///r/index#dep@0.1.0\" = { git = \"https://example.com/dep\", tag = \"v1\" }",
        )
        .unwrap();
        let [(spec, dep)] = &replace[..] else { panic!("{replace:?}") };
        assert_eq!(spec.url().map(Url::as_str), Some("file:///r/index"));
        assert!(dep.source_id().is_git());
        assert!(replace_paths(&replace).is_empty());
    }

    #[test]
    fn what_cargo_refuses_is_refused() {
        let err = |name, table| replace_of(name, table).unwrap_err().to_string();
        assert_eq!(
            err("noversion", "foo = { path = \"foo\" }"),
            "replacements must specify a version to replace, but \
             `https://github.com/rust-lang/crates.io-index#foo` does not"
        );
        assert!(
            err("req", "\"foo:0.1.0\" = { path = \"foo\", version = \"0.1\" }")
                .starts_with("replacements cannot specify a version requirement")
        );
        assert!(err("string", "\"foo:0.1.0\" = \"0.1\"").starts_with("replacements cannot"));
        let both = "[workspace]\n[patch.crates-io]\nfoo = { path = \"foo\" }\n[replace]\n\"foo:0.1.0\" = { path = \"foo\" }";
        assert_eq!(
            replace_table(&root_with("both", both)).unwrap_err().to_string(),
            "cannot specify both [replace] and [patch]"
        );
    }

    #[test]
    fn no_replace_table_is_no_replacement() {
        assert!(replace_table(&root_with("none", "[workspace]\n")).unwrap().is_empty());
    }
}
