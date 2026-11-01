//! A package's targets: cargo's layout conventions (`src/lib.rs`, `src/main.rs`, `src/bin/*.rs`,
//! `examples/`, `tests/`, `benches/`, `build.rs`) merged with what the manifest declares, as
//! cargo's `targets.rs` does it.

use std::fmt::{self, Write as _};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail};
use blaze_hash::{HashMap, HashSet};
use cargo_util_schemas::manifest::{
    PathValue, StringOrVec, TomlManifest, TomlPackageBuild, TomlTarget,
};
use serde::Serialize;

use crate::edit_distance::closest_msg;
use crate::features::{Edition, Feature, Features};
use crate::files::Files;
use crate::manifest::deprecated_underscore;
use crate::paths::normalize_path;

const DEFAULT_TEST_DIR_NAME: &str = "tests";
const DEFAULT_BENCH_DIR_NAME: &str = "benches";
const DEFAULT_EXAMPLE_DIR_NAME: &str = "examples";

const TARGET_KIND_HUMAN_LIB: &str = "library";
const TARGET_KIND_HUMAN_BIN: &str = "binary";
const TARGET_KIND_HUMAN_EXAMPLE: &str = "example";
const TARGET_KIND_HUMAN_TEST: &str = "test";
const TARGET_KIND_HUMAN_BENCH: &str = "benchmark";

const TARGET_KIND_LIB: &str = "lib";
const TARGET_KIND_BIN: &str = "bin";
const TARGET_KIND_EXAMPLE: &str = "example";
const TARGET_KIND_TEST: &str = "test";
const TARGET_KIND_BENCH: &str = "bench";

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum CrateType {
    Bin,
    Lib,
    Rlib,
    Dylib,
    Cdylib,
    Staticlib,
    ProcMacro,
    Other(String),
}

impl CrateType {
    pub fn as_str(&self) -> &str {
        match self {
            CrateType::Bin => "bin",
            CrateType::Lib => "lib",
            CrateType::Rlib => "rlib",
            CrateType::Dylib => "dylib",
            CrateType::Cdylib => "cdylib",
            CrateType::Staticlib => "staticlib",
            CrateType::ProcMacro => "proc-macro",
            CrateType::Other(s) => s,
        }
    }

    pub fn can_lto(&self) -> bool {
        matches!(self, CrateType::Bin | CrateType::Staticlib | CrateType::Cdylib)
    }

    pub fn is_linkable(&self) -> bool {
        matches!(self, CrateType::Lib | CrateType::Rlib | CrateType::Dylib | CrateType::ProcMacro)
    }

    pub fn is_dynamic(&self) -> bool {
        matches!(self, CrateType::Dylib | CrateType::Cdylib | CrateType::ProcMacro)
    }

    pub fn requires_upstream_objects(&self) -> bool {
        !matches!(self, CrateType::Lib | CrateType::Rlib)
    }
}

impl CrateType {
    /// A manifest's `crate-type` as cargo's `From<&String>` reads it: `procmacro`, which is
    /// not what `as_str` writes.
    pub fn parse(name: &str) -> CrateType {
        match name {
            "bin" => CrateType::Bin,
            "lib" => CrateType::Lib,
            "rlib" => CrateType::Rlib,
            "dylib" => CrateType::Dylib,
            "cdylib" => CrateType::Cdylib,
            "staticlib" => CrateType::Staticlib,
            "procmacro" => CrateType::ProcMacro,
            _ => CrateType::Other(name.to_owned()),
        }
    }
}

impl From<&String> for CrateType {
    fn from(s: &String) -> CrateType {
        CrateType::parse(s)
    }
}

impl fmt::Display for CrateType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for CrateType {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum TargetKind {
    Lib(Vec<CrateType>),
    Bin,
    Test,
    Bench,
    ExampleLib(Vec<CrateType>),
    ExampleBin,
    CustomBuild,
}

impl TargetKind {
    pub fn description(&self) -> &'static str {
        match self {
            TargetKind::Lib(..) => "lib",
            TargetKind::Bin => "bin",
            TargetKind::Test => "integration-test",
            TargetKind::ExampleBin | TargetKind::ExampleLib(..) => "example",
            TargetKind::Bench => "bench",
            TargetKind::CustomBuild => "build-script",
        }
    }

    pub fn rustc_crate_types(&self) -> &[CrateType] {
        match self {
            TargetKind::Lib(kinds) | TargetKind::ExampleLib(kinds) => kinds,
            TargetKind::CustomBuild
            | TargetKind::Bench
            | TargetKind::Test
            | TargetKind::ExampleBin
            | TargetKind::Bin => &[CrateType::Bin],
        }
    }

    pub fn is_bin(&self) -> bool {
        matches!(self, TargetKind::Bin)
    }

    pub fn is_example(&self) -> bool {
        matches!(self, TargetKind::ExampleLib(_) | TargetKind::ExampleBin)
    }

    pub fn is_executable(&self) -> bool {
        matches!(self, TargetKind::Bin | TargetKind::ExampleBin)
    }

    pub fn is_custom_build(&self) -> bool {
        matches!(self, TargetKind::CustomBuild)
    }

    pub fn is_test_or_bench(&self) -> bool {
        matches!(self, TargetKind::Test | TargetKind::Bench)
    }

    fn lib_has(&self, crate_type: &CrateType) -> bool {
        matches!(self, TargetKind::Lib(types) if types.contains(crate_type))
    }

    pub fn is_dylib(&self) -> bool {
        self.lib_has(&CrateType::Dylib)
    }

    pub fn is_cdylib(&self) -> bool {
        self.lib_has(&CrateType::Cdylib)
    }

    /// cargo's `Target::for_host`: a build script, or a library that is a proc macro.
    pub fn for_host(&self) -> bool {
        self.is_custom_build() || self.lib_has(&CrateType::ProcMacro)
    }
}

impl Serialize for TargetKind {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            TargetKind::Lib(kinds) => s.collect_seq(kinds.iter().map(|t| t.to_string())),
            TargetKind::Bin => ["bin"].serialize(s),
            TargetKind::ExampleBin | TargetKind::ExampleLib(_) => ["example"].serialize(s),
            TargetKind::Test => ["test"].serialize(s),
            TargetKind::CustomBuild => ["custom-build"].serialize(s),
            TargetKind::Bench => ["bench"].serialize(s),
        }
    }
}

/// A target of a package. `src_path` is `None` for a metabuild script, which has no file.
#[derive(Clone, Debug)]
pub struct Target {
    pub kind: TargetKind,
    pub name: String,
    pub name_inferred: bool,
    pub bin_name: Option<String>,
    pub src_path: Option<PathBuf>,
    pub required_features: Option<Vec<String>>,
    pub tested: bool,
    pub benched: bool,
    pub doc: bool,
    pub doctest: bool,
    pub harness: bool,
    pub for_host: bool,
    pub proc_macro: bool,
    pub doc_scrape_examples: Option<bool>,
    pub edition: Edition,
}

impl Target {
    fn new(src_path: Option<PathBuf>, kind: TargetKind, name: &str, edition: Edition) -> Target {
        Target {
            kind,
            name: name.to_owned(),
            name_inferred: false,
            bin_name: None,
            src_path,
            required_features: None,
            tested: true,
            benched: true,
            doc: false,
            doctest: false,
            harness: true,
            for_host: false,
            proc_macro: false,
            doc_scrape_examples: None,
            edition,
        }
    }

    fn lib_target(
        name: &str,
        crate_types: Vec<CrateType>,
        path: PathBuf,
        edition: Edition,
    ) -> Target {
        let mut t = Target::new(Some(path), TargetKind::Lib(crate_types), name, edition);
        t.doctest = true;
        t.doc = true;
        t
    }

    fn bin_target(
        name: &str,
        bin_name: Option<String>,
        path: PathBuf,
        required_features: Option<Vec<String>>,
        edition: Edition,
    ) -> Target {
        let mut t = Target::new(Some(path), TargetKind::Bin, name, edition);
        t.bin_name = bin_name;
        t.required_features = required_features;
        t.doc = true;
        t
    }

    fn custom_build_target(name: &str, path: PathBuf, edition: Edition) -> Target {
        let mut t = Target::new(Some(path), TargetKind::CustomBuild, name, edition);
        t.for_host = true;
        t.benched = false;
        t.tested = false;
        t.doc_scrape_examples = Some(false);
        t
    }

    fn metabuild_target(name: &str) -> Target {
        let mut t = Target::new(None, TargetKind::CustomBuild, name, Edition::Edition2018);
        t.for_host = true;
        t.benched = false;
        t.tested = false;
        t.doc_scrape_examples = Some(false);
        t
    }

    fn example_target(
        name: &str,
        crate_types: Vec<CrateType>,
        path: PathBuf,
        required_features: Option<Vec<String>>,
        edition: Edition,
    ) -> Target {
        let kind = if crate_types.is_empty() || crate_types.iter().all(|t| *t == CrateType::Bin) {
            TargetKind::ExampleBin
        } else {
            TargetKind::ExampleLib(crate_types)
        };
        let mut t = Target::new(Some(path), kind, name, edition);
        t.required_features = required_features;
        t.tested = false;
        t.benched = false;
        t
    }

    fn test_target(
        name: &str,
        path: PathBuf,
        required_features: Option<Vec<String>>,
        edition: Edition,
    ) -> Target {
        let mut t = Target::new(Some(path), TargetKind::Test, name, edition);
        t.required_features = required_features;
        t.benched = false;
        t
    }

    fn bench_target(
        name: &str,
        path: PathBuf,
        required_features: Option<Vec<String>>,
        edition: Edition,
    ) -> Target {
        let mut t = Target::new(Some(path), TargetKind::Bench, name, edition);
        t.required_features = required_features;
        t.tested = false;
        t
    }

    pub fn is_custom_build(&self) -> bool {
        self.kind == TargetKind::CustomBuild
    }

    pub fn is_bin(&self) -> bool {
        self.kind == TargetKind::Bin
    }

    pub fn is_lib(&self) -> bool {
        matches!(self.kind, TargetKind::Lib(_))
    }

    pub fn doctestable(&self) -> bool {
        match &self.kind {
            TargetKind::Lib(kinds) => kinds.iter().any(|k| {
                *k == CrateType::Rlib || *k == CrateType::Lib || *k == CrateType::ProcMacro
            }),
            _ => false,
        }
    }

    pub fn serialized(&self) -> Option<SerializedTarget<'_>> {
        Some(SerializedTarget {
            kind: &self.kind,
            crate_types: self.kind.rustc_crate_types().to_vec(),
            name: &self.name,
            src_path: self.src_path.as_ref()?,
            edition: self.edition.to_string(),
            required_features: self.required_features.as_deref(),
            doc: self.doc,
            doctest: self.doctest && self.doctestable(),
            test: self.tested,
        })
    }
}

/// A target as `cargo metadata` prints it, fields in cargo's order.
#[derive(Serialize)]
pub struct SerializedTarget<'a> {
    pub kind: &'a TargetKind,
    pub crate_types: Vec<CrateType>,
    pub name: &'a str,
    pub src_path: &'a PathBuf,
    pub edition: String,
    #[serde(rename = "required-features", skip_serializing_if = "Option::is_none")]
    pub required_features: Option<&'a [String]>,
    pub doc: bool,
    pub doctest: bool,
    pub test: bool,
}

pub(crate) fn to_targets(
    features: &Features,
    original_toml: &TomlManifest,
    normalized_toml: &TomlManifest,
    package_root: &Path,
    edition: Edition,
    metabuild: &Option<StringOrVec>,
    warnings: &mut Vec<String>,
) -> anyhow::Result<Vec<Target>> {
    let mut targets = Vec::new();

    if let Some(target) = to_lib_target(
        original_toml.lib.as_ref(),
        normalized_toml.lib.as_ref(),
        package_root,
        edition,
        warnings,
    )? {
        targets.push(target);
    }

    let package = normalized_toml
        .package
        .as_ref()
        .ok_or_else(|| anyhow::format_err!("manifest has no `package` (or `project`)"))?;

    targets.extend(to_bin_targets(
        features,
        normalized_toml.bin.as_deref().unwrap_or_default(),
        package_root,
        edition,
        warnings,
    )?);
    targets.extend(to_example_targets(
        normalized_toml.example.as_deref().unwrap_or_default(),
        package_root,
        edition,
        warnings,
    )?);
    targets.extend(to_test_targets(
        normalized_toml.test.as_deref().unwrap_or_default(),
        package_root,
        edition,
        warnings,
    )?);
    targets.extend(to_bench_targets(
        normalized_toml.bench.as_deref().unwrap_or_default(),
        package_root,
        edition,
        warnings,
    )?);

    if let Some(custom_build) = package.normalized_build().expect("previously normalized") {
        if metabuild.is_some() {
            bail!("cannot specify both `metabuild` and `build`");
        }
        validate_unique_build_scripts(custom_build)?;
        for script in custom_build {
            let script_path = Path::new(script);
            let name = format!(
                "build-script-{}",
                script_path.file_stem().and_then(|s| s.to_str()).expect("previously normalized")
            );
            targets.push(Target::custom_build_target(
                &name,
                package_root.join(script_path),
                edition,
            ));
        }
    }
    if let Some(metabuild) = metabuild {
        let bdeps = normalized_toml.build_dependencies.as_ref();
        for name in &metabuild.0 {
            if !bdeps.is_some_and(|bd| bd.contains_key(name.as_str())) {
                bail!("metabuild package `{name}` must be specified in `build-dependencies`");
            }
        }
        targets.push(Target::metabuild_target(&format!(
            "metabuild-{}",
            package.normalized_name().expect("previously normalized")
        )));
    }

    Ok(targets)
}

pub(crate) fn normalize_lib<F: Files>(
    files: &F,
    original_lib: Option<&TomlTarget>,
    package_root: &Path,
    package_name: &str,
    edition: Edition,
    autodiscover: Option<bool>,
    warnings: &mut Vec<String>,
) -> anyhow::Result<Option<TomlTarget>> {
    if is_normalized(original_lib, autodiscover) {
        let Some(mut lib) = original_lib.cloned() else {
            return Ok(None);
        };
        validate_lib_name(&lib, warnings)?;
        validate_proc_macro(&lib, TARGET_KIND_HUMAN_LIB, edition, warnings)?;
        validate_crate_types(&lib, TARGET_KIND_HUMAN_LIB, edition, warnings)?;
        if let Some(PathValue(path)) = &lib.path {
            lib.path = Some(PathValue(normalize_path(path)));
        }
        Ok(Some(lib))
    } else {
        let inferred = inferred_lib(files, package_root);
        let lib = original_lib.cloned().or_else(|| {
            inferred
                .as_ref()
                .map(|lib| TomlTarget { path: Some(PathValue(lib.clone())), ..TomlTarget::new() })
        });
        let Some(mut lib) = lib else { return Ok(None) };
        lib.name.get_or_insert_with(|| package_name.replace('-', "_"));

        validate_lib_name(&lib, warnings)?;
        validate_proc_macro(&lib, TARGET_KIND_HUMAN_LIB, edition, warnings)?;
        validate_crate_types(&lib, TARGET_KIND_HUMAN_LIB, edition, warnings)?;

        if lib.path.is_none() {
            if let Some(inferred) = inferred {
                lib.path = Some(PathValue(inferred));
            } else {
                let name = name_or_panic(&lib);
                let legacy_path = Path::new("src").join(format!("{name}.rs"));
                if edition == Edition::Edition2015 && files.exists(&package_root.join(&legacy_path)) {
                    warnings.push(format!(
                        "path `{}` was erroneously implicitly accepted for library `{name}`,\n\
                         please rename the file to `src/lib.rs` or set lib.path in Cargo.toml",
                        legacy_path.display(),
                    ));
                    lib.path = Some(PathValue(legacy_path));
                } else {
                    bail!(
                        "can't find library `{name}`, \
                         rename file to `src/lib.rs` or specify lib.path",
                    )
                }
            }
        }

        if let Some(PathValue(path)) = lib.path.as_ref() {
            lib.path = Some(PathValue(normalize_path(path)));
        }
        Ok(Some(lib))
    }
}

fn to_lib_target(
    original_lib: Option<&TomlTarget>,
    normalized_lib: Option<&TomlTarget>,
    package_root: &Path,
    edition: Edition,
    warnings: &mut Vec<String>,
) -> anyhow::Result<Option<Target>> {
    let Some(lib) = normalized_lib else {
        return Ok(None);
    };
    let path = package_root.join(&lib.path.as_ref().expect("previously normalized").0);

    let crate_types = match (lib.crate_types(), lib.proc_macro()) {
        (Some(kinds), _)
            if kinds.iter().any(|k| k == "dylib") && kinds.iter().any(|k| k == "cdylib") =>
        {
            bail!(
                "library `{}` cannot set the crate type of both `dylib` and `cdylib`",
                name_or_panic(lib)
            );
        }
        (Some(kinds), _) if kinds.iter().any(|k| k == "proc-macro") => {
            warnings.push(format!(
                "library `{}` should only specify `proc-macro = true` instead of setting `crate-type`",
                name_or_panic(lib)
            ));
            if kinds.len() > 1 {
                bail!("cannot mix `proc-macro` crate type with others");
            }
            vec![CrateType::ProcMacro]
        }
        (Some(kinds), _) => kinds.iter().map(CrateType::from).collect(),
        (None, Some(true)) => vec![CrateType::ProcMacro],
        (None, _) => vec![CrateType::Lib],
    };

    let mut target = Target::lib_target(name_or_panic(lib), crate_types, path, edition);
    configure(lib, &mut target, TARGET_KIND_HUMAN_LIB, warnings)?;
    target.name_inferred = original_lib.is_none_or(|v| v.name.is_none());
    Ok(Some(target))
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn normalize_bins<F: Files>(
    files: &F,
    toml_bins: Option<&Vec<TomlTarget>>,
    package_root: &Path,
    package_name: &str,
    edition: Edition,
    autodiscover: Option<bool>,
    warnings: &mut Vec<String>,
    errors: &mut Vec<String>,
    has_lib: bool,
) -> anyhow::Result<Vec<TomlTarget>> {
    if are_normalized(toml_bins, autodiscover) {
        let mut toml_bins = toml_bins.cloned().unwrap_or_default();
        for bin in toml_bins.iter_mut() {
            validate_bin_name(bin, warnings)?;
            validate_bin_crate_types(bin, edition, warnings, errors)?;
            validate_bin_proc_macro(bin, edition, warnings, errors)?;
            if let Some(PathValue(path)) = &bin.path {
                bin.path = Some(PathValue(normalize_path(path)));
            }
        }
        Ok(toml_bins)
    } else {
        let inferred = inferred_bins(files, package_root, package_name);
        let mut bins = toml_targets_and_inferred(
            toml_bins,
            &inferred,
            package_root,
            autodiscover,
            edition,
            warnings,
            TARGET_KIND_HUMAN_BIN,
            TARGET_KIND_BIN,
            "autobins",
        );

        for bin in &mut bins {
            validate_bin_name(bin, warnings)?;
            validate_bin_crate_types(bin, edition, warnings, errors)?;
            validate_bin_proc_macro(bin, edition, warnings, errors)?;

            let path = target_path(
                files,
                bin,
                &inferred,
                TARGET_KIND_BIN,
                package_root,
                edition,
                Legacy::Bin { has_lib },
                warnings,
            );
            let path = match path {
                Ok(path) => normalize_path(&path),
                Err(e) => bail!("{e}"),
            };
            bin.path = Some(PathValue(path));
        }
        Ok(bins)
    }
}

fn to_bin_targets(
    features: &Features,
    bins: &[TomlTarget],
    package_root: &Path,
    edition: Edition,
    warnings: &mut Vec<String>,
) -> anyhow::Result<Vec<Target>> {
    for bin in bins {
        if bin.filename.is_some() {
            features.require(Feature::different_binary_name)?;
        }
    }
    validate_unique_names(bins, TARGET_KIND_HUMAN_BIN)?;

    let mut result = Vec::with_capacity(bins.len());
    for bin in bins {
        let path = package_root.join(&bin.path.as_ref().expect("previously normalized").0);
        let mut target = Target::bin_target(
            name_or_panic(bin),
            bin.filename.clone(),
            path,
            bin.required_features.clone(),
            edition,
        );
        configure(bin, &mut target, TARGET_KIND_HUMAN_BIN, warnings)?;
        result.push(target);
    }
    Ok(result)
}

fn legacy_bin_path<F: Files>(
    files: &F,
    package_root: &Path,
    name: &str,
    has_lib: bool,
) -> Option<PathBuf> {
    if !has_lib {
        let rel_path = Path::new("src").join(format!("{name}.rs"));
        if files.exists(&package_root.join(&rel_path)) {
            return Some(rel_path);
        }
    }
    let rel_path = Path::new("src").join("main.rs");
    if files.exists(&package_root.join(&rel_path)) {
        return Some(rel_path);
    }
    let rel_path = Path::new("src").join("bin").join("main.rs");
    if files.exists(&package_root.join(&rel_path)) {
        return Some(rel_path);
    }
    None
}

pub(crate) fn normalize_examples<F: Files>(
    files: &F,
    toml_examples: Option<&Vec<TomlTarget>>,
    package_root: &Path,
    edition: Edition,
    autodiscover: Option<bool>,
    warnings: &mut Vec<String>,
    errors: &mut Vec<String>,
) -> anyhow::Result<Vec<TomlTarget>> {
    normalize_targets(
        files,
        TARGET_KIND_HUMAN_EXAMPLE,
        TARGET_KIND_EXAMPLE,
        toml_examples,
        DEFAULT_EXAMPLE_DIR_NAME,
        package_root,
        edition,
        autodiscover,
        warnings,
        errors,
        Legacy::None,
        "autoexamples",
    )
}

fn to_example_targets(
    targets: &[TomlTarget],
    package_root: &Path,
    edition: Edition,
    warnings: &mut Vec<String>,
) -> anyhow::Result<Vec<Target>> {
    validate_unique_names(targets, TARGET_KIND_EXAMPLE)?;
    let mut result = Vec::with_capacity(targets.len());
    for toml in targets {
        let path = package_root.join(&toml.path.as_ref().expect("previously normalized").0);
        let crate_types = match toml.crate_types() {
            Some(kinds) => kinds.iter().map(CrateType::from).collect(),
            None => Vec::new(),
        };
        let mut target = Target::example_target(
            name_or_panic(toml),
            crate_types,
            path,
            toml.required_features.clone(),
            edition,
        );
        configure(toml, &mut target, TARGET_KIND_HUMAN_EXAMPLE, warnings)?;
        result.push(target);
    }
    Ok(result)
}

pub(crate) fn normalize_tests<F: Files>(
    files: &F,
    toml_tests: Option<&Vec<TomlTarget>>,
    package_root: &Path,
    edition: Edition,
    autodiscover: Option<bool>,
    warnings: &mut Vec<String>,
    errors: &mut Vec<String>,
) -> anyhow::Result<Vec<TomlTarget>> {
    normalize_targets(
        files,
        TARGET_KIND_HUMAN_TEST,
        TARGET_KIND_TEST,
        toml_tests,
        DEFAULT_TEST_DIR_NAME,
        package_root,
        edition,
        autodiscover,
        warnings,
        errors,
        Legacy::None,
        "autotests",
    )
}

fn to_test_targets(
    targets: &[TomlTarget],
    package_root: &Path,
    edition: Edition,
    warnings: &mut Vec<String>,
) -> anyhow::Result<Vec<Target>> {
    validate_unique_names(targets, TARGET_KIND_TEST)?;
    let mut result = Vec::with_capacity(targets.len());
    for toml in targets {
        let path = package_root.join(&toml.path.as_ref().expect("previously normalized").0);
        let mut target =
            Target::test_target(name_or_panic(toml), path, toml.required_features.clone(), edition);
        configure(toml, &mut target, TARGET_KIND_HUMAN_TEST, warnings)?;
        result.push(target);
    }
    Ok(result)
}

pub(crate) fn normalize_benches<F: Files>(
    files: &F,
    toml_benches: Option<&Vec<TomlTarget>>,
    package_root: &Path,
    edition: Edition,
    autodiscover: Option<bool>,
    warnings: &mut Vec<String>,
    errors: &mut Vec<String>,
) -> anyhow::Result<Vec<TomlTarget>> {
    normalize_targets(
        files,
        TARGET_KIND_HUMAN_BENCH,
        TARGET_KIND_BENCH,
        toml_benches,
        DEFAULT_BENCH_DIR_NAME,
        package_root,
        edition,
        autodiscover,
        warnings,
        errors,
        Legacy::Bench,
        "autobenches",
    )
}

/// Where an edition 2015 target without a `path` that cargo once found elsewhere may still be.
#[derive(Clone, Copy)]
enum Legacy {
    None,
    /// `src/<name>.rs` (with no library), `src/main.rs` or `src/bin/main.rs`.
    Bin { has_lib: bool },
    /// `src/bench.rs`, for a benchmark named `bench`.
    Bench,
}

impl Legacy {
    /// The legacy path of `target` if it is there, with cargo's warning about it in `warnings`.
    fn path<F: Files>(
        self,
        files: &F,
        package_root: &Path,
        target: &TomlTarget,
        warnings: &mut Vec<String>,
    ) -> Option<PathBuf> {
        let name = name_or_panic(target);
        let (legacy_path, kind, key) = match self {
            Legacy::None => return None,
            Legacy::Bin { has_lib } => {
                (legacy_bin_path(files, package_root, name, has_lib)?, "binary", "bin")
            }
            Legacy::Bench => {
                let legacy_path = Path::new("src").join("bench.rs");
                if !(name == "bench" && files.exists(&package_root.join(&legacy_path))) {
                    return None;
                }
                (legacy_path, "benchmark", "bench")
            }
        };
        warnings.push(format!(
            "path `{}` was erroneously implicitly accepted for {kind} `{name}`,\n\
             please set {key}.path in Cargo.toml",
            legacy_path.display(),
        ));
        Some(legacy_path)
    }
}

fn to_bench_targets(
    targets: &[TomlTarget],
    package_root: &Path,
    edition: Edition,
    warnings: &mut Vec<String>,
) -> anyhow::Result<Vec<Target>> {
    validate_unique_names(targets, TARGET_KIND_BENCH)?;
    let mut result = Vec::with_capacity(targets.len());
    for toml in targets {
        let path = package_root.join(&toml.path.as_ref().expect("previously normalized").0);
        let mut target = Target::bench_target(
            name_or_panic(toml),
            path,
            toml.required_features.clone(),
            edition,
        );
        configure(toml, &mut target, TARGET_KIND_HUMAN_BENCH, warnings)?;
        result.push(target);
    }
    Ok(result)
}

fn is_normalized(toml_target: Option<&TomlTarget>, autodiscover: Option<bool>) -> bool {
    are_normalized_(toml_target.map(std::slice::from_ref), autodiscover)
}

fn are_normalized(toml_targets: Option<&Vec<TomlTarget>>, autodiscover: Option<bool>) -> bool {
    are_normalized_(toml_targets.map(|v| v.as_slice()), autodiscover)
}

fn are_normalized_(toml_targets: Option<&[TomlTarget]>, autodiscover: Option<bool>) -> bool {
    if autodiscover != Some(false) {
        return false;
    }
    let Some(toml_targets) = toml_targets else {
        return true;
    };
    toml_targets.iter().all(|t| t.name.is_some() && t.path.is_some())
}

#[allow(clippy::too_many_arguments)]
/// `inferred_dir`'s targets merged with the manifest's; a legacy path's warnings go after the rest.
fn normalize_targets<F: Files>(
    files: &F,
    target_kind_human: &str,
    target_kind: &str,
    toml_targets: Option<&Vec<TomlTarget>>,
    inferred_dir: &str,
    package_root: &Path,
    edition: Edition,
    autodiscover: Option<bool>,
    warnings: &mut Vec<String>,
    errors: &mut Vec<String>,
    legacy: Legacy,
    autodiscover_flag_name: &str,
) -> anyhow::Result<Vec<TomlTarget>> {
    if are_normalized(toml_targets, autodiscover) {
        let mut toml_targets = toml_targets.cloned().unwrap_or_default();
        for target in toml_targets.iter_mut() {
            validate_target_name(target, target_kind_human, target_kind, warnings)?;
            validate_proc_macro(target, target_kind_human, edition, warnings)?;
            validate_crate_types(target, target_kind_human, edition, warnings)?;
            if let Some(PathValue(path)) = &target.path {
                target.path = Some(PathValue(normalize_path(path)));
            }
        }
        Ok(toml_targets)
    } else {
        let inferred = infer_from_directory(files, package_root, Path::new(inferred_dir));
        let toml_targets = toml_targets_and_inferred(
            toml_targets,
            &inferred,
            package_root,
            autodiscover,
            edition,
            warnings,
            target_kind_human,
            target_kind,
            autodiscover_flag_name,
        );
        for target in &toml_targets {
            validate_target_name(target, target_kind_human, target_kind, warnings)?;
            validate_proc_macro(target, target_kind_human, edition, warnings)?;
            validate_crate_types(target, target_kind_human, edition, warnings)?;
        }
        let mut result = Vec::new();
        let mut legacy_warnings = Vec::new();
        for mut target in toml_targets {
            let path = target_path(
                files,
                &target,
                &inferred,
                target_kind,
                package_root,
                edition,
                legacy,
                &mut legacy_warnings,
            );
            let path = match path {
                Ok(path) => path,
                Err(e) => {
                    errors.push(e);
                    continue;
                }
            };
            target.path = Some(PathValue(normalize_path(&path)));
            result.push(target);
        }
        warnings.append(&mut legacy_warnings);
        Ok(result)
    }
}

fn inferred_lib<F: Files>(files: &F, package_root: &Path) -> Option<PathBuf> {
    let lib = Path::new("src").join("lib.rs");
    files.exists(&package_root.join(&lib)).then_some(lib)
}

fn inferred_bins<F: Files>(
    files: &F,
    package_root: &Path,
    package_name: &str,
) -> Vec<(String, PathBuf)> {
    let main = "src/main.rs";
    let mut result = Vec::new();
    if files.exists(&package_root.join(main)) {
        result.push((package_name.to_owned(), PathBuf::from(main)));
    }
    result.extend(infer_from_directory(files, package_root, &Path::new("src").join("bin")));
    result
}

/// The targets in a directory by name order: each `*.rs` file, and each subdirectory with a
/// `main.rs`, dotfiles left out.
fn infer_from_directory<F: Files>(
    files: &F,
    package_root: &Path,
    relpath: &Path,
) -> Vec<(String, PathBuf)> {
    let dir = package_root.join(relpath);
    let Ok(entries) = files.list(&dir) else {
        return Vec::new();
    };
    entries
        .iter()
        .filter(|(name, _)| !name.starts_with('.'))
        .filter_map(|(name, is_dir)| infer_any(files, package_root, &dir.join(name), *is_dir))
        .collect()
}

fn infer_any<F: Files>(
    files: &F,
    package_root: &Path,
    path: &Path,
    is_dir: bool,
) -> Option<(String, PathBuf)> {
    if is_dir {
        infer_subdirectory(files, package_root, path)
    } else if path.extension().and_then(|p| p.to_str()) == Some("rs") {
        infer_file(package_root, path)
    } else {
        None
    }
}

fn infer_file(package_root: &Path, path: &Path) -> Option<(String, PathBuf)> {
    let stem = path.file_stem()?.to_str()?.to_owned();
    let path = path.strip_prefix(package_root).unwrap_or(path).to_owned();
    Some((stem, path))
}

fn infer_subdirectory<F: Files>(
    files: &F,
    package_root: &Path,
    path: &Path,
) -> Option<(String, PathBuf)> {
    let main = path.join("main.rs");
    let name = path.file_name()?.to_str()?.to_owned();
    if files.exists(&main) {
        let main = main.strip_prefix(package_root).map(|p| p.to_owned()).unwrap_or(main);
        Some((name, main))
    } else {
        None
    }
}

#[allow(clippy::too_many_arguments)]
fn toml_targets_and_inferred(
    toml_targets: Option<&Vec<TomlTarget>>,
    inferred: &[(String, PathBuf)],
    package_root: &Path,
    autodiscover: Option<bool>,
    edition: Edition,
    warnings: &mut Vec<String>,
    target_kind_human: &str,
    target_kind: &str,
    autodiscover_flag_name: &str,
) -> Vec<TomlTarget> {
    let inferred_targets = inferred_to_toml_targets(inferred);
    let mut toml_targets = match toml_targets {
        None => {
            if let Some(false) = autodiscover {
                vec![]
            } else {
                inferred_targets
            }
        }
        Some(targets) => {
            let mut targets = targets.clone();
            let target_path =
                |target: &TomlTarget| target.path.clone().map(|p| package_root.join(p.0));

            let mut seen_names = HashSet::default();
            let mut seen_paths = HashSet::default();
            for target in targets.iter() {
                seen_names.insert(target.name.clone());
                seen_paths.insert(target_path(target));
            }

            let mut rem_targets = vec![];
            for target in inferred_targets {
                if !seen_names.contains(&target.name) && !seen_paths.contains(&target_path(&target))
                {
                    rem_targets.push(target);
                }
            }

            let autodiscover = match autodiscover {
                Some(autodiscover) => autodiscover,
                None => {
                    if edition == Edition::Edition2015 {
                        if !rem_targets.is_empty() {
                            let mut rem_targets_str = String::new();
                            for t in rem_targets.iter() {
                                if let Some(p) = t.path.clone() {
                                    rem_targets_str.push_str(&format!("* {}\n", p.0.display()))
                                }
                            }
                            warnings.push(format!(
                                "\
An explicit [[{target_kind}]] section is specified in Cargo.toml which currently
disables Cargo from automatically inferring other {target_kind_human} targets.
This inference behavior will change in the Rust 2018 edition and the following
files will be included as a {target_kind_human} target:

{rem_targets_str}
This is likely to break cargo build or cargo test as these files may not be
ready to be compiled as a {target_kind_human} target today. You can future-proof yourself
and disable this warning by adding `{autodiscover_flag_name} = false` to your [package]
section. You may also move the files to a location where Cargo would not
automatically infer them to be a target, such as in subfolders.

For more information on this warning you can consult
https://github.com/rust-lang/cargo/issues/5330",
                            ));
                        }
                        false
                    } else {
                        true
                    }
                }
            };

            if autodiscover {
                targets.append(&mut rem_targets);
            }
            targets
        }
    };
    toml_targets.sort_unstable_by(|a, b| a.name.cmp(&b.name));
    toml_targets
}

fn inferred_to_toml_targets(inferred: &[(String, PathBuf)]) -> Vec<TomlTarget> {
    inferred
        .iter()
        .map(|(name, path)| TomlTarget {
            name: Some(name.clone()),
            path: Some(PathValue(path.clone())),
            ..TomlTarget::new()
        })
        .collect()
}

fn validate_unique_names(targets: &[TomlTarget], target_kind: &str) -> anyhow::Result<()> {
    let mut seen = HashSet::default();
    for name in targets.iter().map(name_or_panic) {
        if !seen.insert(name) {
            bail!(
                "found duplicate {target_kind} name {name}, \
                 but all {target_kind} targets must have a unique name",
            );
        }
    }
    Ok(())
}

fn validate_unique_build_scripts(scripts: &[String]) -> anyhow::Result<()> {
    let mut seen: HashMap<&str, Vec<&str>> = HashMap::default();
    for script in scripts {
        let stem =
            Path::new(script).file_stem().and_then(|s| s.to_str()).expect("previously normalized");
        seen.entry(stem).or_default().push(script.as_str());
    }
    let mut conflict_file_stem = false;
    let mut err_msg = String::from(
        "found build scripts with duplicate file stems, but all build scripts must have a unique file stem",
    );
    for (stem, paths) in seen {
        if paths.len() > 1 {
            conflict_file_stem = true;
            write!(&mut err_msg, "\n  for stem `{stem}`: {}", paths.join(", "))?;
        }
    }
    if conflict_file_stem {
        bail!(err_msg);
    }
    Ok(())
}

fn configure(
    toml: &TomlTarget,
    target: &mut Target,
    target_kind_human: &str,
    warnings: &mut Vec<String>,
) -> anyhow::Result<()> {
    let t2 = target.clone();
    target.tested = toml.test.unwrap_or(t2.tested);
    target.doc = toml.doc.unwrap_or(t2.doc);
    target.doctest = toml.doctest.unwrap_or(t2.doctest);
    target.benched = toml.bench.unwrap_or(t2.benched);
    target.harness = toml.harness.unwrap_or(t2.harness);
    target.proc_macro = toml.proc_macro().unwrap_or(t2.proc_macro);
    target.doc_scrape_examples = toml.doc_scrape_examples;
    target.for_host = toml.proc_macro().unwrap_or(t2.for_host);

    if let Some(edition) = toml.edition.clone() {
        let name = &target.name;
        warnings
            .push(format!("`edition` is set on {target_kind_human} `{name}` which is deprecated"));
        target.edition = edition.parse().context("failed to parse the `edition` key")?;
    }
    Ok(())
}

fn target_path_not_found_error_message<F: Files>(
    files: &F,
    package_root: &Path,
    target: &TomlTarget,
    target_kind: &str,
    inferred: &[(String, PathBuf)],
) -> String {
    fn possible_target_paths(name: &str, kind: &str, commonly_wrong: bool) -> [PathBuf; 2] {
        let mut target_path = PathBuf::new();
        match (kind, commonly_wrong) {
            ("test" | "bench" | "example", true) => target_path.push(kind),
            ("bin", true) => target_path.extend(["src", "bins"]),
            ("test", false) => target_path.push(DEFAULT_TEST_DIR_NAME),
            ("bench", false) => target_path.push(DEFAULT_BENCH_DIR_NAME),
            ("example", false) => target_path.push(DEFAULT_EXAMPLE_DIR_NAME),
            ("bin", false) => target_path.extend(["src", "bin"]),
            _ => unreachable!("invalid target kind: {}", kind),
        }
        let mut target_path_file = target_path.clone();
        target_path_file.push(format!("{name}.rs"));
        target_path.extend([name, "main.rs"]);
        [target_path_file, target_path]
    }

    let target_name = name_or_panic(target);
    let commonly_wrong_paths = possible_target_paths(target_name, target_kind, true);
    let possible_paths = possible_target_paths(target_name, target_kind, false);

    let msg = closest_msg(target_name, inferred.iter(), |(n, _p)| n, target_kind);
    if let Some((wrong_path, possible_path)) = commonly_wrong_paths
        .iter()
        .zip(possible_paths.iter())
        .find(|(wp, _)| files.exists(&package_root.join(wp)))
    {
        let [wrong_path, possible_path] = [wrong_path, possible_path].map(|p| p.display());
        format!(
            "can't find `{target_name}` {target_kind} at default paths, but found a file at `{wrong_path}`.\n\
             Perhaps rename the file to `{possible_path}` for target auto-discovery, \
             or specify {target_kind}.path if you want to use a non-default path.{msg}",
        )
    } else {
        let [path_file, path_dir] = possible_paths.each_ref().map(|p| p.display());
        format!(
            "can't find `{target_name}` {target_kind} at `{path_file}` or `{path_dir}`. \
             Please specify {target_kind}.path if you want to use a non-default path.{msg}"
        )
    }
}

#[allow(clippy::too_many_arguments)]
fn target_path<F: Files>(
    files: &F,
    target: &TomlTarget,
    inferred: &[(String, PathBuf)],
    target_kind: &str,
    package_root: &Path,
    edition: Edition,
    legacy: Legacy,
    legacy_warnings: &mut Vec<String>,
) -> Result<PathBuf, String> {
    if let Some(ref path) = target.path {
        return Ok(path.0.clone());
    }
    let name = name_or_panic(target).to_owned();
    let mut matching = inferred.iter().filter(|(n, _)| n == &name).map(|(_, p)| p.clone());
    let first = matching.next();
    let second = matching.next();
    match (first, second) {
        (Some(path), None) => Ok(path),
        (None, None) => {
            if edition == Edition::Edition2015
                && let Some(path) = legacy.path(files, package_root, target, legacy_warnings)
            {
                return Ok(path);
            }
            Err(target_path_not_found_error_message(
                files,
                package_root,
                target,
                target_kind,
                inferred,
            ))
        }
        (Some(p0), Some(p1)) => {
            if edition == Edition::Edition2015
                && let Some(path) = legacy.path(files, package_root, target, legacy_warnings)
            {
                return Ok(path);
            }
            Err(format!(
                "\
cannot infer path for `{}` {}
Cargo doesn't know which to use because multiple target files found at `{}` and `{}`.",
                name_or_panic(target),
                target_kind,
                p0.strip_prefix(package_root).unwrap_or(&p0).display(),
                p1.strip_prefix(package_root).unwrap_or(&p1).display(),
            ))
        }
        (None, Some(_)) => unreachable!(),
    }
}

/// The build scripts of a package: `build.rs` next to the manifest unless `build` says otherwise.
pub(crate) fn normalize_build<F: Files>(
    files: &F,
    build: Option<&TomlPackageBuild>,
    package_root: &Path,
) -> anyhow::Result<Option<TomlPackageBuild>> {
    const BUILD_RS: &str = "build.rs";
    match build {
        None => {
            if files.is_file(&package_root.join(BUILD_RS)) {
                Ok(Some(TomlPackageBuild::SingleScript(BUILD_RS.to_owned())))
            } else {
                Ok(Some(TomlPackageBuild::Auto(false)))
            }
        }
        Some(TomlPackageBuild::Auto(false)) => Ok(build.cloned()),
        Some(TomlPackageBuild::SingleScript(build_file)) => {
            validate_build_name(build_file)?;
            let build = normalize_path(Path::new(build_file))
                .into_os_string()
                .into_string()
                .expect("came from a string");
            Ok(Some(TomlPackageBuild::SingleScript(build)))
        }
        Some(TomlPackageBuild::Auto(true)) => {
            Ok(Some(TomlPackageBuild::SingleScript(BUILD_RS.to_owned())))
        }
        Some(TomlPackageBuild::MultipleScript(scripts)) => {
            for script in scripts {
                validate_build_name(script)?;
            }
            Ok(build.cloned())
        }
    }
}

fn validate_build_name(build: &str) -> anyhow::Result<()> {
    if Path::new(build).file_stem().is_none() {
        bail!("invalid `package.build` file name");
    }
    Ok(())
}

fn name_or_panic(target: &TomlTarget) -> &str {
    target.name.as_deref().unwrap_or_else(|| panic!("target name is required"))
}

fn validate_lib_name(target: &TomlTarget, warnings: &mut Vec<String>) -> anyhow::Result<()> {
    validate_target_name(target, TARGET_KIND_HUMAN_LIB, TARGET_KIND_LIB, warnings)?;
    let name = name_or_panic(target);
    if name.contains('-') {
        bail!("library target names cannot contain hyphens: {name}")
    }
    Ok(())
}

fn validate_bin_name(bin: &TomlTarget, warnings: &mut Vec<String>) -> anyhow::Result<()> {
    validate_target_name(bin, TARGET_KIND_HUMAN_BIN, TARGET_KIND_BIN, warnings)?;
    let name = name_or_panic(bin);
    if ["deps", "examples", "build", "incremental"].contains(&name) {
        bail!(
            "the binary target name `{name}` is forbidden, \
             it conflicts with cargo's build directory names",
        )
    }
    Ok(())
}

fn validate_target_name(
    target: &TomlTarget,
    target_kind_human: &str,
    target_kind: &str,
    _warnings: &mut Vec<String>,
) -> anyhow::Result<()> {
    match target.name {
        Some(ref name) => {
            if name.trim().is_empty() {
                bail!("{target_kind_human} target names cannot be empty")
            }
        }
        None => bail!("{target_kind_human} target {target_kind}.name is required"),
    }
    Ok(())
}

fn validate_bin_proc_macro(
    target: &TomlTarget,
    edition: Edition,
    warnings: &mut Vec<String>,
    errors: &mut Vec<String>,
) -> anyhow::Result<()> {
    if target.proc_macro() == Some(true) {
        let name = name_or_panic(target);
        errors.push(format!(
            "the target `{name}` is a binary and can't have `proc-macro` set `true`"
        ));
    } else {
        validate_proc_macro(target, TARGET_KIND_HUMAN_BIN, edition, warnings)?;
    }
    Ok(())
}

fn validate_proc_macro(
    target: &TomlTarget,
    kind: &str,
    edition: Edition,
    warnings: &mut Vec<String>,
) -> anyhow::Result<()> {
    deprecated_underscore(
        &target.proc_macro2,
        &target.proc_macro,
        "proc-macro",
        name_or_panic(target),
        &format!("{kind} target"),
        edition,
        warnings,
    )
}

fn validate_bin_crate_types(
    target: &TomlTarget,
    edition: Edition,
    warnings: &mut Vec<String>,
    errors: &mut Vec<String>,
) -> anyhow::Result<()> {
    if let Some(crate_types) = target.crate_types() {
        if !crate_types.is_empty() {
            let name = name_or_panic(target);
            errors.push(format!(
                "the target `{name}` is a binary and can't have any \
                 crate-types set (currently \"{}\")",
                crate_types.join(", ")
            ));
        } else {
            validate_crate_types(target, TARGET_KIND_HUMAN_BIN, edition, warnings)?;
        }
    }
    Ok(())
}

fn validate_crate_types(
    target: &TomlTarget,
    kind: &str,
    edition: Edition,
    warnings: &mut Vec<String>,
) -> anyhow::Result<()> {
    deprecated_underscore(
        &target.crate_type2,
        &target.crate_type,
        "crate-type",
        name_or_panic(target),
        &format!("{kind} target"),
        edition,
        warnings,
    )
}
