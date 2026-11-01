//! What the resolve reads of a package as `cargo metadata` prints it, and of the root
//! manifest's `[patch]` and `[replace]` tables, given as text: one copy for the load,
//! blaze-metadata, blaze-tree and blaze-units' `metadata_resolve` example. [`Read`] is the one
//! type of a package read, so a resolve's packages are handed on as read, never copied into
//! another crate's form.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, anyhow, bail};
use blaze_hash::HashSet;
use blaze_manifest::config::schema::{
    CargoResolverConfig, FeatureUnification, IncompatibleRustVersions,
};
use blaze_manifest::dependency::SerializedDependency;
use blaze_manifest::files::Files;
use blaze_manifest::manifest::{EitherManifest, Manifest, read_manifest};
use blaze_manifest::metadata::{SerializedPackage, export_package};
use blaze_manifest::paths::normalize_path;
use blaze_manifest::targets::{SerializedTarget, TargetKind};
use blaze_manifest::{Config, Context, Workspace};
use blaze_resolve::resolver::ResolveBehavior;
use blaze_resolve::workspace_resolve::Patch;
use blaze_resolve::{
    DepKind, Dependency, GitReference, InternedString, PackageIdSpec, PartialVersion, RustVersion,
    SourceId, Summary,
};
use url::Url;

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

/// The crate types `cargo metadata` names a library by.
const LIB_KINDS: &[&str] = &["lib", "rlib", "dylib", "cdylib", "staticlib", "proc-macro"];

/// The kinds `cargo metadata` prints of a target (`TargetKind`'s `Serialize`) include `kind`.
fn has_kind(target: &SerializedTarget<'_>, kind: &str) -> bool {
    match target.kind {
        TargetKind::Lib(types) => types.iter().any(|it| it.as_str() == kind),
        TargetKind::Bin => kind == "bin",
        TargetKind::ExampleBin | TargetKind::ExampleLib(_) => kind == "example",
        TargetKind::Test => kind == "test",
        TargetKind::Bench => kind == "bench",
        TargetKind::CustomBuild => kind == "custom-build",
    }
}

/// Whether `target` is a library as `cargo metadata` prints it: a kind of [`LIB_KINDS`].
fn is_lib(target: &SerializedTarget<'_>) -> bool {
    matches!(target.kind, TargetKind::Lib(types) if types.iter().any(|it| LIB_KINDS.contains(&it.as_str())))
}

/// Where `package` is from: its `source`, else the directory of its manifest.
pub fn package_source(package: &SerializedPackage<'_>) -> Result<SourceId> {
    match &package.source {
        Some(source) => SourceId::from_url(source),
        None => SourceId::for_path(package.manifest_path.parent().context("a manifest's dir")?),
    }
}

/// `dep` as the resolve takes it.
pub fn dependency(dep: &SerializedDependency<'_>) -> Result<Dependency> {
    let source = match (&dep.source, &dep.path) {
        (Some(source), _) => SourceId::from_url(source)?,
        (None, Some(path)) => SourceId::for_path(path)?,
        (None, None) => SourceId::crates_io(),
    };
    // A path or git dependency without a `version` is written `*` but, unlike `version = "*"`,
    // matches prereleases.
    let req = match dep.req.as_str() {
        "*" if !source.is_registry() => None,
        req => Some(req),
    };
    let mut d = Dependency::parse(dep.name, req, source)?;
    d.set_kind(dep.kind);
    if let Some(rename) = dep.rename {
        d.set_explicit_name_in_toml(rename);
    }
    d.set_optional(dep.optional);
    d.set_default_features(dep.uses_default_features);
    d.set_features(dep.features.iter().map(String::as_str));
    if let Some(target) = dep.target {
        // clone: the dependency owns its platform, once per targeted dependency per load
        d.set_platform(Some(target.clone()));
    }
    Ok(d)
}

/// `package`, as `cargo metadata` prints it, read.
pub fn read(package: &SerializedPackage<'_>) -> Result<Read> {
    // clone: the id owns its version, once per package per load
    let id = blaze_resolve::PackageId::new(InternedString::new(package.name), package.version.clone(), package_source(package)?);
    // alloc: the summary's dependencies, once per package per load
    let deps = package.dependencies.iter().map(dependency).collect::<Result<Vec<_>>>()?;
    let features: BTreeMap<InternedString, Vec<InternedString>> = package
        .features
        .iter()
        .map(|(name, values)| {
            (InternedString::new(name), values.iter().map(|it| InternedString::new(it)).collect()) // alloc: the summary's feature map, once per package per load
        })
        .collect(); // alloc: as above
    let summary = Summary::new(id, deps, &features, package.links, None)?;
    let targets = &package.targets;
    let lib = targets.iter().find(|it| is_lib(it));
    Ok(Read {
        summary,
        lib: lib.map(|lib| InternedString::new(&lib.name.replace('-', "_"))), // alloc: the crate name, once per library per load
        proc_macro_lib: lib.is_some_and(|it| has_kind(it, "proc-macro")),
        any_proc_macro: targets.iter().any(|it| has_kind(it, "proc-macro")),
        build_script: targets.iter().any(|it| has_kind(it, "custom-build")),
        bins: targets
            .iter()
            .filter(|it| has_kind(it, "bin"))
            .map(|it| it.required_features.map(<[String]>::to_vec).unwrap_or_default()) // alloc: `Read::bins`, once per binary per load
            .collect(), // alloc: as above
        license: package.license.map(str::to_owned), // alloc: `Read::license`, once per package per load
        repository: package.repository.map(str::to_owned), // alloc: `Read::repository`, once per package per load
    })
}

/// The root manifest, `text` being what it reads, as the tables below read it.
pub struct Root<'a> {
    pub dir: &'a Path,
    pub table: toml::Table,
}

impl<'a> Root<'a> {
    pub fn new(dir: &'a Path, text: &str) -> Result<Root<'a>> {
        // alloc: the root manifest's table, once per load; the path only on the error path
        Ok(Root { dir, table: toml::from_str(text).with_context(|| dir.join("Cargo.toml").display().to_string())? })
    }
}

/// One entry of a `[patch]` table of the root manifest.
pub fn patch_dependency(root: &Path, name: &str, entry: &toml::Value) -> Result<Option<Dependency>> {
    let get = |key: &str| entry.get(key).and_then(|v| v.as_str());
    let (version, source) = match entry {
        toml::Value::String(version) => (Some(version.as_str()), SourceId::crates_io()),
        _ if get("registry").is_some() => return Ok(None),
        _ => {
            let source = if let Some(path) = get("path") {
                SourceId::for_path(&normalize_path(&root.join(path)))?
            } else if let Some(git) = get("git") {
                // alloc: a `[patch]` entry's git reference, once per entry per load
                let reference = match (get("branch"), get("tag"), get("rev")) {
                    (Some(b), _, _) => GitReference::Branch(b.to_owned()), // alloc: as above
                    (_, Some(t), _) => GitReference::Tag(t.to_owned()), // alloc: as above
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
pub fn patch_paths(root: &Root<'_>) -> Vec<PathBuf> {
    let tables = root.table.get("patch").and_then(|p| p.as_table()).into_iter().flatten();
    let entries = tables.flat_map(|(_, table)| table.as_table().into_iter().flatten());
    let paths = entries.filter_map(|(_, entry)| entry.get("path")?.as_str());
    paths.map(|path| normalize_path(&root.dir.join(path))).collect() // alloc: the queue of path packages, once per load
}

/// The root manifest's `[patch]`, without entries that name an alternative registry.
pub fn patch_tables(root: &Root<'_>) -> Result<Vec<(Url, Vec<Patch>)>> {
    let manifest_path = root.dir.join("Cargo.toml");
    let mut tables = Vec::new(); // alloc: the resolve's `[patch]`, once per load
    for (key, table) in root.table.get("patch").and_then(|p| p.as_table()).into_iter().flatten() {
        let url = if key == "crates-io" {
            SourceId::crates_io().url().clone() // clone: each table owns its URL, once per table
        } else {
            match key.parse::<Url>() {
                Ok(url) => url,
                Err(_) => bail!(
                    "`[patch.{key}]` in `{}` names a registry by its name, which is not looked up in \
                     `[registries]` here: name it by its index URL",
                    manifest_path.display()
                ),
            }
        };
        let mut patches = Vec::new(); // alloc: a table's patches, once per table per load
        for (name, entry) in table.as_table().into_iter().flatten() {
            let Some(dep) = patch_dependency(root.dir, name, entry)? else { continue };
            // alloc: `Patch::loc`, once per patch per load
            patches.push(Patch { dep, loc: manifest_path.display().to_string() });
        }
        tables.push((url, patches));
    }
    Ok(tables)
}

/// The root manifest's `[replace]`, as cargo 1.99's `toml::replace` reads it: each key a spec
/// with a version (its source crates.io unless it names one), each value the source it is
/// replaced with, required at exactly that version.
pub fn replace_table(root: &Root<'_>) -> Result<Vec<(PackageIdSpec, Dependency)>> {
    let Some(table) = root.table.get("replace").and_then(|r| r.as_table()) else {
        return Ok(Vec::new()); // alloc: empty, allocates nothing
    };
    if root.table.contains_key("patch") {
        bail!("cannot specify both [replace] and [patch]");
    }
    let mut replace = Vec::new(); // alloc: the resolve's `[replace]`, once per load
    for (key, entry) in table {
        let mut spec = PackageIdSpec::parse(key).with_context(|| {
            // alloc: the error path
            format!(
                "replacements must specify a valid semver version to replace, but `{key}` does not"
            )
        })?;
        if spec.url().is_none() {
            spec = spec.with_url(SourceId::crates_io().url().clone()); // clone: the spec owns its URL, once per entry
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
        let mut entry = entry.clone(); // clone: the entry with its version pinned, once per `[replace]` entry
        if let Some(it) = entry.as_table_mut() {
            // alloc: as above
            it.insert("version".to_owned(), toml::Value::String(format!("={version}")));
        }
        let dep = patch_dependency(root.dir, spec.name(), &entry)?.context("a `[replace]` entry")?;
        replace.push((spec, dep));
    }
    Ok(replace)
}

/// The directories of the path packages the root manifest's `[replace]` names.
pub fn replace_paths(replace: &[(PackageIdSpec, Dependency)]) -> Vec<PathBuf> {
    replace.iter().filter_map(|(_, dep)| dep.source_id().local_path()).collect() // alloc: the queue of path packages, once per load
}

/// The directories of a package's path dependencies, as `cargo metadata` prints it.
pub fn path_dependencies(package: &SerializedPackage<'_>) -> impl Iterator<Item = PathBuf> {
    let deps = package.dependencies.iter().filter(|it| it.source.is_none());
    deps.filter_map(|it| it.path.as_deref()).map(normalize_path) // alloc: the queue of path packages, once per dependency per load
}

/// A package read for the resolve: its manifest, and the source the lock file pins it to when
/// it is not a path package.
pub struct Package {
    pub manifest_path: PathBuf,
    pub manifest: Box<Manifest>,
    pub locked_source: Option<String>,
}

/// The path packages that are not members: those the members and the root's `[patch]` and
/// `[replace]` name by path, and theirs in turn.
pub fn path_packages<F: Files>(
    ws: &Workspace<'_, F>,
    root: &Root<'_>,
    replace: &[(PackageIdSpec, Dependency)],
    ctx: &Context<F>,
) -> Result<Vec<Package>> {
    // alloc: the directories seen, once per load
    let mut seen: HashSet<PathBuf> = ws.members().filter_map(|(path, _)| path.parent().map(Path::to_owned)).collect();
    let mut queue = patch_paths(root);
    queue.extend(replace_paths(replace));
    for (path, m) in ws.members() {
        queue.extend(path_dependencies(&export_package(path, m)));
    }
    let mut read = Vec::new(); // alloc: the path packages, once per load
    while let Some(dir) = queue.pop() {
        if !seen.insert(dir.clone()) { // clone: `seen` and the read each keep the directory, once per package
            continue;
        }
        let manifest_path = dir.join("Cargo.toml");
        let source = SourceId::for_path(&dir)?;
        let EitherManifest::Real(manifest) = read_manifest(&manifest_path, source, ctx)? else {
            bail!("{} is a path dependency with no package", manifest_path.display());
        };
        queue.extend(path_dependencies(&export_package(&manifest_path, &manifest)));
        read.push(Package { manifest_path, manifest, locked_source: None });
    }
    Ok(read)
}

/// The source of a package a lock file pins to `source`, as blaze-manifest names it.
pub fn manifest_source<F: Files>(source: &str, config: &Config<F>) -> Result<SourceId> {
    let (kind, rest) = source.split_once('+').context("a source with no kind")?;
    match kind {
        "registry" if rest == blaze_manifest::source::CRATES_IO_INDEX => blaze_manifest::source::crates_io(config),
        "registry" => SourceId::for_registry(&rest.parse()?),
        "sparse" => SourceId::for_registry(&source.parse()?),
        "git" => {
            let mut url: Url = rest.parse()?;
            let reference = GitReference::from_query(url.query_pairs());
            url.set_fragment(None);
            url.set_query(None);
            SourceId::for_git(&url, reference)
        }
        _ => bail!("the source {source} is not one blaze reads yet"),
    }
}

/// The Rust versions the resolve prefers packages to support, as cargo's `resolve_with_previous`
/// gives them: when `resolver.incompatible-rust-versions` is `fallback`, or unset with resolver
/// 3, the members' `rust-version`s, or rustc's release if none names one. `rustc_version` is
/// what `rustc -vV` says (`1.101.0-nightly (...)`) or the release alone.
pub fn rust_versions<F: Files>(
    ctx: &Context<F>,
    behavior: ResolveBehavior,
    members: &[RustVersion],
    rustc_version: Option<&str>,
) -> Result<Option<Vec<PartialVersion>>> {
    let config: CargoResolverConfig = ctx.config.get("resolver")?;
    let honours = match config.incompatible_rust_versions {
        Some(IncompatibleRustVersions::Allow) => false,
        Some(IncompatibleRustVersions::Fallback) => true,
        None => behavior == ResolveBehavior::V3,
    };
    if !honours {
        return Ok(None);
    }
    if members.is_empty() {
        let rustc_version = rustc_version.context(
            "the resolve prefers packages that support the workspace's Rust version, no member \
             names a `rust-version`, and the release of the rustc that builds is not known",
        )?;
        let release = rustc_version.split(' ').next().unwrap_or(rustc_version);
        return Ok(Some(vec![release.parse()?])); // alloc: the resolve's Rust versions, once per load
    }
    Ok(Some(members.iter().map(RustVersion::to_partial).collect())) // alloc: as above
}

/// `resolver.feature-unification`, which nightly cargo reads only with `-Zfeature-unification`
/// (`unstable.feature-unification`); `selected` otherwise.
pub fn feature_unification<F: Files>(config: &Config<F>) -> Result<FeatureUnification> {
    let resolver = config.get::<CargoResolverConfig>("resolver")?;
    let unstable = config.get::<Option<bool>>("unstable.feature-unification")?;
    Ok(match (unstable, resolver.feature_unification) {
        (Some(true), Some(it)) => it,
        _ => FeatureUnification::Selected,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn replace_of(table: &str) -> Result<Vec<(PackageIdSpec, Dependency)>> {
        replace_table(&Root::new(Path::new("/w"), &format!("[workspace]\n\n[replace]\n{table}"))?)
    }

    #[test]
    fn a_name_and_version_is_a_crates_io_package_replaced_at_exactly_that_version() {
        let replace = replace_of("\"foo:0.1.0\" = { path = \"vendor/../foo\" }").unwrap();
        let [(spec, dep)] = &replace[..] else { panic!("{replace:?}") };
        assert_eq!(spec.name(), "foo");
        assert_eq!(spec.version(), Some(semver::Version::new(0, 1, 0)));
        assert_eq!(spec.url(), Some(SourceId::crates_io().url()));
        assert_eq!(dep.package_name().as_str(), "foo");
        assert_eq!(dep.version_req().to_string(), "=0.1.0");
        assert_eq!(dep.source_id().local_path(), Some(PathBuf::from("/w/foo")));
        assert_eq!(replace_paths(&replace), vec![PathBuf::from("/w/foo")]);
    }

    #[test]
    fn a_spec_names_its_own_source() {
        let replace = replace_of(
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
        let err = |table| replace_of(table).unwrap_err().to_string();
        assert_eq!(
            err("foo = { path = \"foo\" }"),
            "replacements must specify a version to replace, but \
             `https://github.com/rust-lang/crates.io-index#foo` does not"
        );
        assert!(
            err("\"foo:0.1.0\" = { path = \"foo\", version = \"0.1\" }")
                .starts_with("replacements cannot specify a version requirement")
        );
        assert!(err("\"foo:0.1.0\" = \"0.1\"").starts_with("replacements cannot"));
        let both = "[workspace]\n[patch.crates-io]\nfoo = { path = \"foo\" }\n[replace]\n\"foo:0.1.0\" = { path = \"foo\" }";
        let root = Root::new(Path::new("/w"), both).unwrap();
        assert_eq!(replace_table(&root).unwrap_err().to_string(), "cannot specify both [replace] and [patch]");
    }

    #[test]
    fn no_replace_table_is_no_replacement() {
        let root = Root::new(Path::new("/w"), "[workspace]\n").unwrap();
        assert!(replace_table(&root).unwrap().is_empty());
    }

    fn target<'a>(kind: &'a TargetKind, name: &'a str, src: &'a PathBuf, required: Option<&'a [String]>) -> SerializedTarget<'a> {
        SerializedTarget {
            kind,
            crate_types: Vec::new(),
            name,
            src_path: src,
            edition: "2024".to_owned(),
            required_features: required,
            doc: true,
            doctest: true,
            test: true,
        }
    }

    fn dep<'a>(name: &'a str, req: &str, kind: DepKind) -> SerializedDependency<'a> {
        SerializedDependency {
            name,
            source: None,
            req: req.to_owned(),
            kind,
            rename: None,
            optional: false,
            uses_default_features: true,
            features: &[],
            artifact: None,
            target: None,
            registry: None,
            path: None,
        }
    }

    // The typed read is what the JSON one was: the id of the manifest's directory, `*` of a
    // path dependency as no requirement, the library by its crate types, `-` as `_`.
    #[test]
    fn a_package_is_read_from_its_typed_form() {
        let version = semver::Version::new(0, 1, 0);
        let src = PathBuf::from("/w/m/src/lib.rs");
        let (lib, bin, build, example) = (
            TargetKind::Lib(vec![blaze_manifest::targets::CrateType::ProcMacro]),
            TargetKind::Bin,
            TargetKind::CustomBuild,
            TargetKind::ExampleLib(vec![blaze_manifest::targets::CrateType::Lib]),
        );
        let required = ["cli".to_owned()];
        let feature_list = ["serde".to_owned()];
        let platform: blaze_resolve::Platform = "cfg(unix)".parse().unwrap();
        let mut path_dep = dep("helper", "*", DepKind::Normal);
        path_dep.path = Some(PathBuf::from("/w/m/../helper"));
        // Optional, so a normal dependency: cargo refuses an optional dev-dependency.
        let mut registry_dep = dep("serde_json", "^1.0", DepKind::Normal);
        registry_dep.rename = Some("json");
        registry_dep.optional = true;
        registry_dep.uses_default_features = false;
        registry_dep.features = &feature_list;
        registry_dep.target = Some(&platform);
        let package = SerializedPackage {
            name: "my-macros",
            version: &version,
            id: cargo_util_schemas::core::PackageIdSpec::parse("my-macros@0.1.0").unwrap(),
            license: Some("MIT"),
            license_file: None,
            description: None,
            source: None,
            dependencies: vec![path_dep, registry_dep, dep("cc", "^1", DepKind::Build)],
            targets: vec![
                target(&example, "ex", &src, None),
                target(&lib, "my-macros", &src, None),
                target(&bin, "tool", &src, Some(&required)),
                target(&bin, "plain", &src, None),
                target(&build, "build-script-build", &src, None),
            ],
            features: BTreeMap::from([("cli", Vec::new()), ("default", vec!["cli".to_owned()])]),
            manifest_path: Path::new("/w/m/Cargo.toml"),
            metadata: None,
            publish: None,
            authors: &[],
            categories: &[],
            keywords: &[],
            readme: None,
            repository: Some("https://example.com/m"),
            homepage: None,
            documentation: None,
            edition: "2024".to_owned(),
            links: Some("m"),
            metabuild: None,
            default_run: None,
            rust_version: None,
            hints: None,
        };
        let read = read(&package).unwrap();
        let id = read.summary.package_id();
        assert_eq!((id.name().as_str(), id.version().to_string()), ("my-macros", "0.1.0".to_owned()));
        assert_eq!(id.source_id().local_path(), Some(PathBuf::from("/w/m")));
        assert_eq!(read.lib.as_deref(), Some("my_macros"));
        assert!(read.proc_macro_lib && read.any_proc_macro && read.build_script);
        assert_eq!(read.bins, [vec!["cli".to_owned()], Vec::new()]);
        assert_eq!((read.license.as_deref(), read.repository.as_deref()), (Some("MIT"), Some("https://example.com/m")));
        assert_eq!(read.summary.links().map(|it| it.as_str()), Some("m"));
        assert!(read.summary.features().contains_key(&InternedString::new("default")));
        let deps = read.summary.dependencies();
        let [helper, json, cc] = deps else { panic!("{} dependencies", deps.len()) };
        assert!(helper.source_id().is_path());
        assert_eq!((json.name_in_toml().as_str(), json.kind()), ("json", DepKind::Normal));
        assert!(json.is_optional() && !json.uses_default_features() && json.platform() == Some(&platform));
        assert_eq!(json.features().iter().map(|it| it.as_str()).collect::<Vec<_>>(), ["serde"]);
        assert!(json.source_id().is_registry());
        assert_eq!(cc.kind(), DepKind::Build);
        assert_eq!(path_dependencies(&package).collect::<Vec<_>>(), [PathBuf::from("/w/helper")]);
    }

    #[test]
    fn patch_paths_are_the_roots() {
        let root = Root::new(Path::new("/w"), "[patch.crates-io]\na = { path = \"vendor/a\" }\nb = \"1\"\n").unwrap();
        assert_eq!(patch_paths(&root), vec![PathBuf::from("/w/vendor/a")]);
        assert_eq!(patch_tables(&root).unwrap()[0].1.len(), 2);
    }

    /// A `[patch]` of a registry named, not given by URL, is refused naming it, never left out.
    #[test]
    fn a_patch_of_a_named_registry_is_an_error() {
        let root = Root::new(Path::new("/w"), "[patch.my-registry]\na = { path = \"vendor/a\" }\n").unwrap();
        let Err(error) = patch_tables(&root) else { panic!("a named registry's patch is not left out") };
        let error = error.to_string();
        assert!(error.contains("[patch.my-registry]"), "{error}");
    }

    #[test]
    fn feature_unification_is_read_only_when_unstable_allows_it() {
        let unification = |env: &[(&str, &str)]| {
            let mut env: Vec<(String, String)> = env.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect();
            env.push(("CARGO_HOME".to_owned(), "/blaze-no-such-home".to_owned()));
            let config = Config::load_with(Path::new("/blaze-no-such-dir"), env, blaze_manifest::files::Disk).unwrap();
            feature_unification(&config).unwrap()
        };
        let asked = ("CARGO_RESOLVER_FEATURE_UNIFICATION", "workspace");
        let allowed = ("CARGO_UNSTABLE_FEATURE_UNIFICATION", "true");
        assert!(matches!(unification(&[]), FeatureUnification::Selected));
        assert!(matches!(unification(&[asked]), FeatureUnification::Selected));
        assert!(matches!(unification(&[allowed]), FeatureUnification::Selected));
        assert!(matches!(unification(&[asked, allowed]), FeatureUnification::Workspace));
    }
}
