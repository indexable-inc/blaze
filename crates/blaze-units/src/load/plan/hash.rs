//! cargo's `compute_metadata` (`compiler/build_runner/compilation_files.rs`) and its inputs: the
//! types cargo hashes, mirrored variant for variant so that their derived `Hash` writes what
//! cargo's derived `Hash` writes (`CrateType` and `TargetKind` are blaze-manifest's, which are
//! cargo's variant for variant), and `lto.rs`'s `generate`.

use std::hash::{Hash, Hasher};
use std::path::Path;

use blaze_ids::radix;
use blaze_manifest::targets::{CrateType, TargetKind};
use blaze_resolve::{PackageId, SourceId, SourceKind};

use crate::query::LoadError;
use crate::schema::{self, Mode, Unit};

/// cargo's `util::StableHasher`.
pub(super) type StableHasher = rustc_stable_hash::StableSipHasher128;

/// cargo's `METADATA_VERSION`.
pub(super) const METADATA_VERSION: u8 = 2;

fn error(what: String) -> LoadError {
    LoadError { what, at: None }
}

/// A crate type as the unit graph prints it (`proc-macro` with its `-`, which a manifest's
/// `crate-type` spells `procmacro`).
pub(super) fn crate_type(name: &str) -> CrateType {
    match name {
        "proc-macro" => CrateType::ProcMacro,
        other => CrateType::parse(other),
    }
}

/// The kind a target of the unit graph prints as its `kind` and `crate_types` (cargo's
/// `Serialize for TargetKind`, and `example_target`'s choice of `ExampleBin`).
pub(super) fn target_kind(target: &schema::Target) -> TargetKind {
    let types = |names: &[String]| names.iter().map(|it| crate_type(it)).collect::<Vec<_>>();
    match target.kind.first().map(String::as_str) {
        Some("custom-build") => TargetKind::CustomBuild,
        Some("bin") => TargetKind::Bin,
        Some("test") => TargetKind::Test,
        Some("bench") => TargetKind::Bench,
        Some("example") => {
            if target.crate_types.iter().all(|it| it == "bin") {
                TargetKind::ExampleBin
            } else {
                TargetKind::ExampleLib(types(&target.crate_types))
            }
        }
        _ => TargetKind::Lib(types(&target.kind)),
    }
}

/// cargo's `CompileMode`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum CompileMode {
    Test,
    Build,
    Check { test: bool },
    Doc,
    Doctest,
    Docscrape,
    RunCustomBuild,
}

impl CompileMode {
    /// The unit's mode; the modes that run rustdoc are refused, blaze never running it.
    pub fn of(unit: &Unit) -> Result<CompileMode, LoadError> {
        match unit.mode {
            Mode::Test => Ok(CompileMode::Test),
            Mode::Build => Ok(CompileMode::Build),
            Mode::Check => Ok(CompileMode::Check { test: unit.checked_as_test }),
            Mode::RunCustomBuild => Ok(CompileMode::RunCustomBuild),
            Mode::Doc | Mode::Doctest | Mode::Docscrape => Err(error(format!(
                "blaze does not run rustdoc: the `{:?}` unit of `{}`'s target `{}` has no plan",
                unit.mode, unit.pkg_id, unit.target.name
            ))),
        }
    }

    pub fn is_any_test(self) -> bool {
        matches!(self, CompileMode::Test | CompileMode::Check { test: true } | CompileMode::Doctest)
    }

    pub fn is_check(self) -> bool {
        matches!(self, CompileMode::Check { .. })
    }
}

/// cargo's `profiles::Lto`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) enum ProfileLto {
    Off,
    Bool(bool),
    Named(String),
}

impl ProfileLto {
    /// From the unit graph's text of it (`Serialize for Lto`). A profile's `lto = "true"` as a
    /// string is `Named("true")` in cargo and prints as `true` too; the graph keeps no difference.
    pub fn parse(text: &str) -> ProfileLto {
        match text {
            "off" => ProfileLto::Off,
            "true" => ProfileLto::Bool(true),
            "false" => ProfileLto::Bool(false),
            other => ProfileLto::Named(other.to_owned()),
        }
    }
}

/// cargo's `PanicStrategy`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum PanicStrategy {
    Unwind,
    Abort,
    ImmediateAbort,
}

/// cargo's `StripInner`, which `Strip`'s `Hash` writes.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum StripInner {
    None,
    Named(String),
}

/// cargo-util-schemas' `TomlDebugInfo`, which `DebugInfo`'s `Hash` writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum TomlDebugInfo {
    None,
    LineDirectivesOnly,
    LineTablesOnly,
    Limited,
    Full,
}

impl TomlDebugInfo {
    pub fn of(debuginfo: &schema::DebugInfo) -> Result<TomlDebugInfo, LoadError> {
        match debuginfo {
            schema::DebugInfo::Level(0) => Ok(TomlDebugInfo::None),
            schema::DebugInfo::Level(1) => Ok(TomlDebugInfo::Limited),
            schema::DebugInfo::Level(2) => Ok(TomlDebugInfo::Full),
            schema::DebugInfo::Named(name) if name == "line-directives-only" => Ok(TomlDebugInfo::LineDirectivesOnly),
            schema::DebugInfo::Named(name) if name == "line-tables-only" => Ok(TomlDebugInfo::LineTablesOnly),
            other => Err(error(format!("the debuginfo level `{other:?}` is none cargo has"))),
        }
    }

    pub fn is_turned_on(self) -> bool {
        self != TomlDebugInfo::None
    }
}

/// cargo-util-schemas' `TomlTrimPaths` of the pinned cargo.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum TrimPaths {
    None,
    Object,
    All,
}

impl TrimPaths {
    pub fn of(value: Option<&serde_json::Value>) -> Result<TrimPaths, LoadError> {
        match value.map(|it| (it, it.as_str())) {
            None | Some((_, Some("none"))) => Ok(TrimPaths::None),
            Some((_, Some("object"))) => Ok(TrimPaths::Object),
            Some((_, Some("all"))) => Ok(TrimPaths::All),
            Some((other, _)) => Err(error(format!("the profile's `trim-paths` `{other}` is none the pinned cargo has"))),
        }
    }
}

/// A unit's profile as cargo's `Profile::comparable` hashes it, field by field.
pub(super) struct HashedProfile<'a> {
    opt_level: &'a str,
    pub lto: ProfileLto,
    codegen_backend: Option<&'a str>,
    codegen_units: Option<u32>,
    pub debuginfo: TomlDebugInfo,
    split_debuginfo: Option<&'a str>,
    debug_assertions: bool,
    overflow_checks: bool,
    rpath: bool,
    incremental: bool,
    panic: PanicStrategy,
    strip: StripInner,
    rustflags: &'a [String],
    pub trim_paths: TrimPaths,
}

impl<'a> HashedProfile<'a> {
    pub fn of(profile: &'a schema::Profile) -> Result<HashedProfile<'a>, LoadError> {
        let panic = match profile.panic.as_str() {
            "unwind" => PanicStrategy::Unwind,
            "abort" => PanicStrategy::Abort,
            "immediate-abort" => PanicStrategy::ImmediateAbort,
            other => return Err(error(format!("the panic strategy `{other}` is none cargo has"))),
        };
        let strip = match &profile.strip {
            schema::Strip::Resolved(it) | schema::Strip::Deferred(it) => match it {
                schema::Stripped::None => StripInner::None,
                schema::Stripped::Named(name) => StripInner::Named(name.clone()),
            },
        };
        Ok(HashedProfile {
            opt_level: &profile.opt_level,
            lto: ProfileLto::parse(&profile.lto),
            codegen_backend: profile.codegen_backend.as_deref(),
            codegen_units: profile.codegen_units,
            debuginfo: TomlDebugInfo::of(&profile.debuginfo)?,
            split_debuginfo: profile.split_debuginfo.as_deref(),
            debug_assertions: profile.debug_assertions,
            overflow_checks: profile.overflow_checks,
            rpath: profile.rpath,
            incremental: profile.incremental,
            panic,
            strip,
            rustflags: &profile.rustflags,
            trim_paths: TrimPaths::of(profile.trim_paths.as_ref())?,
        })
    }
}

impl Hash for HashedProfile<'_> {
    fn hash<H: Hasher>(&self, into: &mut H) {
        self.opt_level.hash(into);
        self.lto.hash(into);
        self.codegen_backend.hash(into);
        self.codegen_units.hash(into);
        self.debuginfo.hash(into);
        self.split_debuginfo.hash(into);
        self.debug_assertions.hash(into);
        self.overflow_checks.hash(into);
        self.rpath.hash(into);
        (self.incremental, self.panic, &self.strip).hash(into);
        self.rustflags.hash(into);
        self.trim_paths.hash(into);
    }
}

/// cargo's `compiler::Lto`, what `lto.rs` decides for a unit.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) enum Lto {
    Run(Option<String>),
    Off,
    OnlyBitcode,
    ObjectAndBitcode,
    OnlyObject,
}

fn needs_object(crate_types: &[CrateType]) -> bool {
    crate_types.iter().any(|it| it.can_lto() || it.is_dynamic())
}

fn lto_when_needs_object(crate_types: &[CrateType]) -> Lto {
    if crate_types.iter().all(|it| *it == CrateType::Dylib) { Lto::OnlyObject } else { Lto::ObjectAndBitcode }
}

/// What `lto::generate` is given of each unit.
pub(super) struct LtoUnit<'a> {
    pub kind: &'a TargetKind,
    pub mode: CompileMode,
    pub lto: &'a ProfileLto,
    pub deps: &'a [u32],
}

/// cargo's `lto::generate`: every unit reachable from `roots`, from the roots down. A unit
/// planned elsewhere (`None`) is no root and no unit's dependency here.
pub(super) fn generate(units: &[Option<LtoUnit<'_>>], roots: &[usize]) -> Vec<Option<Lto>> {
    let mut map: Vec<Option<Lto>> = vec![None; units.len()];
    for &root in roots {
        let Some(unit) = &units[root] else { continue };
        let root_lto = match unit.lto {
            ProfileLto::Bool(false) => Lto::OnlyObject,
            ProfileLto::Off => Lto::Off,
            _ => {
                let crate_types = unit.kind.rustc_crate_types();
                if unit.kind.for_host() {
                    Lto::OnlyObject
                } else if needs_object(crate_types) {
                    lto_when_needs_object(crate_types)
                } else {
                    Lto::OnlyBitcode
                }
            }
        };
        calculate(units, &mut map, root, &root_lto);
    }
    map
}

fn calculate(units: &[Option<LtoUnit<'_>>], map: &mut [Option<Lto>], at: usize, parent: &Lto) {
    let Some(unit) = &units[at] else { return };
    let crate_types: &[CrateType] = match unit.mode {
        CompileMode::Test | CompileMode::Doctest => &[CrateType::Bin],
        _ => unit.kind.rustc_crate_types(),
    };
    let lto = if unit.kind.for_host() {
        Lto::OnlyObject
    } else if crate_types.iter().all(CrateType::can_lto) {
        match unit.lto {
            ProfileLto::Named(name) => Lto::Run(Some(name.clone())),
            ProfileLto::Off => Lto::Off,
            ProfileLto::Bool(true) => Lto::Run(None),
            ProfileLto::Bool(false) => Lto::OnlyObject,
        }
    } else {
        match (parent, needs_object(crate_types)) {
            (Lto::Run(_), false) => Lto::OnlyBitcode,
            (Lto::Run(_), true) | (Lto::OnlyBitcode, true) => lto_when_needs_object(crate_types),
            (Lto::Off, _) => Lto::Off,
            (_, false) | (Lto::OnlyObject, true) | (Lto::ObjectAndBitcode, true) => parent.clone(),
        }
    };
    let merged = match &map[at] {
        None => lto,
        Some(before) => {
            let result = match (&lto, before) {
                (Lto::OnlyBitcode, Lto::OnlyBitcode) => Lto::OnlyBitcode,
                (Lto::OnlyObject, Lto::OnlyObject) => Lto::OnlyObject,
                (Lto::Run(name), _) | (_, Lto::Run(name)) => Lto::Run(name.clone()),
                (Lto::Off, _) | (_, Lto::Off) => Lto::Off,
                (Lto::ObjectAndBitcode, _) | (_, Lto::ObjectAndBitcode) => Lto::ObjectAndBitcode,
                (Lto::OnlyObject, Lto::OnlyBitcode) | (Lto::OnlyBitcode, Lto::OnlyObject) => Lto::ObjectAndBitcode,
            };
            if result == *before {
                return;
            }
            result
        }
    };
    map[at] = Some(merged.clone());
    for &dep in unit.deps {
        calculate(units, map, dep as usize, &merged);
    }
}

/// cargo's `SourceId::stable_hash`: a path source under `workspace` by its relative path, any
/// other by its URL (a git one by its canonical URL).
pub(super) fn source_stable_hash<H: Hasher>(source: SourceId, workspace: &Path, into: &mut H) -> Result<(), LoadError> {
    if source.is_path() {
        let path = source
            .url()
            .to_file_path()
            .map_err(|()| error(format!("the path source `{}` is no file path", source.url())))?;
        if let Ok(relative) = path.strip_prefix(workspace) {
            let relative = relative
                .to_str()
                .ok_or_else(|| error(format!("the path `{}` is not UTF-8", relative.display())))?;
            source.kind().hash(into);
            relative.hash(into);
            return Ok(());
        }
    }
    source.kind().hash(into);
    match source.kind() {
        SourceKind::Git(_) => source.canonical_url().hash(into),
        _ => source.url().as_str().hash(into),
    }
    Ok(())
}

/// cargo's `PackageIdStableHash`: the name, the version, the source's stable hash.
pub(super) fn package_stable_hash<H: Hasher>(id: PackageId, workspace: &Path, into: &mut H) -> Result<(), LoadError> {
    id.name().as_str().hash(into);
    id.version().hash(into);
    source_stable_hash(id.source_id(), workspace, into)
}

/// cargo's `util::short_hash`: the little-endian bytes of the hash, in hex.
pub(super) fn short_hex(hash: u64) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(16);
    for byte in hash.to_le_bytes() {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// cargo's `target_short_hash`: `short_hash(&(METADATA_VERSION, pkg.stable_hash(ws_root)))`.
pub(super) fn target_short_hash(id: PackageId, workspace: &Path) -> Result<String, LoadError> {
    let mut hasher = StableHasher::new();
    METADATA_VERSION.hash(&mut hasher);
    package_stable_hash(id, workspace, &mut hasher)?;
    Ok(short_hex(Hasher::finish(&hasher)))
}

/// cargo's `CompileKind::fingerprint_hash`: 0 for the host, a target by its name. A target
/// spec file's would be its contents, which the plan never reads.
pub(super) fn kind_fingerprint_hash(triple: Option<&str>) -> Result<u64, LoadError> {
    match triple {
        None => Ok(0),
        Some(triple) if triple.ends_with(".json") => {
            Err(error(format!("blaze does not build for the target spec file `{triple}`")))
        }
        Some(triple) => {
            let mut hasher = StableHasher::new();
            triple.hash(&mut hasher);
            Ok(Hasher::finish(&hasher))
        }
    }
}

/// What `hash_rustc_version` hashes of blaze's rustc.
pub(super) enum RustcVersion {
    /// A pre-release's first segment (`nightly`, `beta`, `dev`).
    Channel(String),
    /// A release's `rustc -vV`, on the host.
    Verbose(String),
}

impl RustcVersion {
    /// Of `rustc -V`'s text after `rustc ` (`1.101.0-nightly (16d1ffac1 2026-10-26)`).
    pub fn of(version: &str, verbose: impl FnOnce() -> String) -> Result<RustcVersion, LoadError> {
        let release = version.split_whitespace().next().unwrap_or_default();
        let parsed = semver::Version::parse(release)
            .map_err(|e| error(format!("blaze's rustc version `{version}` is not a version: {e}")))?;
        Ok(if parsed.pre.is_empty() {
            RustcVersion::Verbose(verbose())
        } else {
            RustcVersion::Channel(parsed.pre.as_str().split('.').next().unwrap_or_default().to_owned())
        })
    }

    /// cargo's `features::is_nightly_toolchain`, cargo being the same release as rustc.
    pub fn is_nightly(&self) -> bool {
        matches!(self, RustcVersion::Channel(channel) if channel == "nightly" || channel == "dev")
    }

    /// cargo's `hash_rustc_version`.
    fn hash_into<H: Hasher>(&self, host_unit: bool, host: &str, into: &mut H) {
        match self {
            RustcVersion::Verbose(verbose) => {
                for line in verbose.lines() {
                    if host_unit || !line.starts_with("host: ") {
                        line.hash(into);
                    }
                }
            }
            RustcVersion::Channel(channel) => {
                Some(channel.as_str()).hash(into);
                if host_unit {
                    host.hash(into);
                }
            }
        }
    }
}

/// cargo's `UnitHash`, hashed as its derive hashes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct UnitHash(u64);

/// A unit's hashes: cargo's `Metadata`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Meta {
    pub unit_id: u64,
    pub c_metadata: u64,
    pub c_extra_filename: bool,
    pub pkg_dir: bool,
}

impl Meta {
    /// A plan made elsewhere (the standard library's), read back. Only its hashes are read:
    /// `pkg_dir` names a directory of the plan's own, which nothing here makes.
    pub fn read(metadata: &str, unit_id: &str, extra_filename: &str, what: &str) -> Result<Meta, LoadError> {
        let hex = |text: &str| {
            u64::from_str_radix(text, 16)
                .ok()
                .filter(|_| text.len() == 16)
                .ok_or_else(|| error(format!("{what}'s hash `{text}` is not 16 hex digits")))
        };
        let meta = Meta { unit_id: hex(unit_id)?, c_metadata: hex(metadata)?, c_extra_filename: !extra_filename.is_empty(), pkg_dir: true };
        if meta.extra_filename() != extra_filename {
            return Err(error(format!("{what}'s `-C extra-filename` `{extra_filename}` is not of its unit id `{unit_id}`")));
        }
        Ok(meta)
    }

    pub fn unit_id(&self) -> String {
        format!("{:016x}", self.unit_id)
    }

    pub fn metadata(&self) -> String {
        format!("{:016x}", self.c_metadata)
    }

    pub fn extra_filename(&self) -> String {
        if self.c_extra_filename { format!("-{:016x}", self.unit_id) } else { String::new() }
    }

    /// cargo's `Metadata::c_extra_filename`, as `output_filename` takes it.
    pub fn extra_hex(&self) -> Option<String> {
        self.c_extra_filename.then(|| format!("{:016x}", self.unit_id))
    }
}

/// Everything of a unit `compute_metadata` hashes but its dependencies'.
pub(super) struct MetaInput<'a> {
    pub package: PackageId,
    /// The workspace root, or for a standard library unit the sysroot.
    pub root: &'a Path,
    pub features: &'a [String],
    pub profile: &'a HashedProfile<'a>,
    pub mode: CompileMode,
    pub lto: &'a Lto,
    pub kind_hash: u64,
    pub host_unit: bool,
    pub target_name: &'a str,
    pub target_kind: &'a TargetKind,
    pub rustc: &'a RustcVersion,
    pub host: &'a str,
    pub default_lib_metadata: Option<&'a str>,
    pub is_std: bool,
    /// The unit's rustflags (its kind's: cargo's `TargetInfo::rustflags`).
    pub rustflags: &'a [String],
    pub c_extra_filename: bool,
    pub pkg_dir: bool,
}

/// cargo's `compute_metadata`, given its dependencies' `deps`.
pub(super) fn compute_metadata(input: &MetaInput<'_>, deps: &[Meta]) -> Result<Meta, LoadError> {
    let mut shared = StableHasher::new();
    METADATA_VERSION.hash(&mut shared);
    package_stable_hash(input.package, input.root, &mut shared)?;
    input.features.hash(&mut shared);
    input.profile.hash(&mut shared);
    input.mode.hash(&mut shared);
    input.lto.hash(&mut shared);
    input.kind_hash.hash(&mut shared);
    input.target_name.hash(&mut shared);
    input.target_kind.hash(&mut shared);
    input.rustc.hash_into(input.host_unit, input.host, &mut shared);
    if let Some(channel) = input.default_lib_metadata {
        channel.hash(&mut shared);
    }
    input.is_std.hash(&mut shared);

    let mut c_metadata = shared.clone();
    let mut dep_metadata: Vec<UnitHash> = deps.iter().map(|it| UnitHash(it.c_metadata)).collect();
    radix::sort_copy_by_key(&mut dep_metadata, |it| it.0);
    dep_metadata.hash(&mut c_metadata);

    let mut unit_id = shared;
    let mut dep_ids: Vec<UnitHash> = deps.iter().map(|it| UnitHash(it.unit_id)).collect();
    radix::sort_copy_by_key(&mut dep_ids, |it| it.0);
    dep_ids.hash(&mut unit_id);
    // `cargo rustc`'s extra arguments, which blaze never has.
    Vec::<String>::new().hash(&mut unit_id);
    if !has_remap_path_prefix(input.rustflags) {
        input.rustflags.hash(&mut unit_id);
    }
    Ok(Meta {
        unit_id: Hasher::finish(&unit_id),
        c_metadata: Hasher::finish(&c_metadata),
        c_extra_filename: input.c_extra_filename,
        pkg_dir: input.pkg_dir,
    })
}

/// cargo's `has_remap_path_prefix`.
pub(super) fn has_remap_path_prefix(args: &[String]) -> bool {
    args.iter().any(|it| it.starts_with("--remap-path-prefix=") || it == "--remap-path-prefix")
}
