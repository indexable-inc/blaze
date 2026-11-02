//! Profiles as the pinned nightly's cargo computes them (`src/workspace/profiles.rs` and
//! `validate_profile` in its manifest parser): the root manifest's `[profile]` tables with the
//! configuration's merged over them, the built-in ones, `inherits` chains, and the `Profile` of
//! a unit from its package, whether it is built for the host and its target, serialized as
//! `cargo build --unit-graph` prints it.
//!
//! What a unit is built for (`UnitFor`) is the unit graph's to compute; this module takes it.

use std::collections::BTreeMap;

use anyhow::{Context as _, bail};
use blaze_hash::{HashMap, HashSet};
use cargo_util_schemas::core::PackageIdSpec;
use cargo_util_schemas::manifest::{
    ProfileName, ProfilePackageSpec, StringOrBool, TomlDebugInfo, TomlProfile, TomlProfiles,
    TomlTrimPaths,
};
use serde::Serialize;

use crate::config::{CliUnstable, CompileKind, Config};
use crate::features::{Feature, Features};
use crate::files::Files;
use crate::manifest::PackageId;

fn spec_matches(spec: &PackageIdSpec, pkg: &PackageId) -> bool {
    if spec.name() != pkg.name {
        return false;
    }
    if let Some(v) = spec.partial_version()
        && !v.matches(&pkg.version)
    {
        return false;
    }
    if let Some(u) = spec.url()
        && u != pkg.source_id.url()
    {
        return false;
    }
    if let Some(k) = spec.kind()
        && !crate::source::spec_kind_is(k, pkg.source_id.kind())
    {
        return false;
    }
    true
}

/// Every profile of a workspace, from which each unit's is made.
#[derive(Clone, Debug)]
pub struct Profiles {
    incremental: Option<bool>,
    dir_names: HashMap<String, String>,
    by_name: HashMap<String, ProfileMaker>,
    original_profiles: BTreeMap<String, TomlProfile>,
    requested_profile: String,
    rustc_host: String,
}

impl Profiles {
    /// The profiles of a workspace whose root manifest has `manifest_profiles` and the cargo
    /// features `features`, building with `requested_profile` on a rustc whose host is
    /// `rustc_host`. The warnings of config profiles go to `config`.
    pub fn new<F: Files>(
        manifest_profiles: Option<&TomlProfiles>,
        features: &Features,
        config: &Config<F>,
        requested_profile: &str,
        rustc_host: &str,
    ) -> anyhow::Result<Profiles> {
        let incremental = match config.env("CARGO_INCREMENTAL") {
            Some(v) => Some(v == "1"),
            None => config.build_config()?.incremental.or_else(|| is_ci(config).then_some(false)),
        };
        let mut profiles =
            merge_config_profiles(manifest_profiles, features, config, requested_profile)?;
        let mut makers = Profiles {
            incremental,
            dir_names: predefined_dir_names(),
            by_name: HashMap::default(),
            original_profiles: profiles.clone(),
            requested_profile: requested_profile.to_owned(),
            rustc_host: rustc_host.to_owned(),
        };
        makers.by_name.insert(
            "dev".to_owned(),
            ProfileMaker { default: Profile::default_dev(), toml: profiles.get("dev").cloned() },
        );
        makers.by_name.insert(
            "release".to_owned(),
            ProfileMaker {
                default: Profile::default_release(),
                toml: profiles.get("release").cloned(),
            },
        );
        for (predef_name, mut predef_prof) in predefined_profiles() {
            match profiles.get_mut(predef_name) {
                None => {
                    profiles.insert(predef_name.to_owned(), predef_prof);
                }
                Some(r) => {
                    predef_prof.merge(r);
                    *r = predef_prof;
                }
            }
        }
        for (name, profile) in &profiles {
            makers.add_maker(name, profile, &profiles)?;
        }
        makers.get_profile_maker(requested_profile)?;
        Ok(makers)
    }

    fn add_maker(
        &mut self,
        name: &str,
        profile: &TomlProfile,
        profiles: &BTreeMap<String, TomlProfile>,
    ) -> anyhow::Result<()> {
        if let Some(dir_name) = &profile.dir_name {
            self.dir_names.insert(name.to_owned(), dir_name.clone());
        }
        if name == "dev" || name == "release" {
            if profile.inherits.is_some() {
                bail!("`inherits` must not be specified in root profile `{name}`");
            }
            return Ok(());
        }
        let mut set = HashSet::default();
        set.insert(name.to_owned());
        let maker = self.process_chain(name, profile, &mut set, profiles)?;
        self.by_name.insert(name.to_owned(), maker);
        Ok(())
    }

    fn process_chain(
        &mut self,
        name: &str,
        profile: &TomlProfile,
        set: &mut HashSet<String>,
        profiles: &BTreeMap<String, TomlProfile>,
    ) -> anyhow::Result<ProfileMaker> {
        let mut maker = match &profile.inherits {
            Some(inherits_name) if inherits_name == "dev" || inherits_name == "release" => {
                self.get_profile_maker(inherits_name).unwrap().clone()
            }
            Some(inherits_name) => {
                if !set.insert(inherits_name.clone()) {
                    bail!(
                        "profile inheritance loop detected with profile `{name}` inheriting `{inherits_name}`"
                    );
                }
                match profiles.get(inherits_name) {
                    None => bail!(
                        "profile `{name}` inherits from `{inherits_name}`, but that profile is not defined"
                    ),
                    Some(parent) => self.process_chain(inherits_name, parent, set, profiles)?,
                }
            }
            None => bail!(
                "profile `{name}` is missing an `inherits` directive \
                 (`inherits` is required for all profiles except `dev` or `release`)"
            ),
        };
        match &mut maker.toml {
            Some(toml) => toml.merge(profile),
            None => maker.toml = Some(profile.clone()),
        }
        Ok(maker)
    }

    /// The profile of a unit of `pkg`, as cargo's `Profiles::get_profile`.
    pub fn get_profile(
        &self,
        pkg: &PackageId,
        is_member: bool,
        is_local: bool,
        unit_for: UnitFor,
        kind: &CompileKind,
    ) -> Profile {
        let maker = self.get_profile_maker(&self.requested_profile).unwrap();
        let mut profile = maker.get_profile(Some(pkg), is_member, unit_for.is_for_host());
        if unit_for.panic_setting == PanicSetting::AlwaysUnwind {
            profile.panic = PanicStrategy::Unwind;
        }
        if profile.debuginfo.is_turned_on() && profile.split_debuginfo.is_none() {
            let target = match kind {
                CompileKind::Host => self.rustc_host.as_str(),
                CompileKind::Target(target) => target.as_str(),
            };
            if target.contains("-apple-") {
                profile.split_debuginfo = Some("unpacked".to_owned());
            }
        }
        if let Some(v) = self.incremental {
            profile.incremental = v;
        }
        if !is_local {
            profile.incremental = false;
        }
        profile.name.clone_from(&self.requested_profile);
        profile
    }

    /// The profile of running a build script, which only sets a few of its variables.
    pub fn get_profile_run_custom_build(&self, for_unit_profile: &Profile) -> Profile {
        Profile {
            name: for_unit_profile.name.clone(),
            root: for_unit_profile.root,
            debuginfo: for_unit_profile.debuginfo,
            opt_level: for_unit_profile.opt_level.clone(),
            debug_assertions: for_unit_profile.debug_assertions,
            trim_paths: for_unit_profile.trim_paths.clone(),
            ..Profile::default()
        }
    }

    /// The profile of no package in particular, for the `Finished` line.
    pub fn base_profile(&self) -> Profile {
        let maker = self.get_profile_maker(&self.requested_profile).unwrap();
        maker.get_profile(None, true, false)
    }

    /// The directory under the target directory, like `debug` or `release`.
    pub fn get_dir_name(&self) -> &str {
        self.dir_names.get(&self.requested_profile).unwrap_or(&self.requested_profile)
    }

    /// Checks the package overrides against the packages of the resolve: none matched by two
    /// specs, and a warning for each spec of the manifest's that matches none.
    pub fn validate_packages(
        &self,
        manifest_profiles: Option<&TomlProfiles>,
        packages: &[&PackageId],
        warnings: &mut Vec<String>,
    ) -> anyhow::Result<()> {
        let mut names: Vec<&String> = self.by_name.keys().collect();
        names.sort();
        for name in names {
            let profile = &self.by_name[name];
            if self.original_profiles.get(name).and_then(|orig| orig.package.as_ref()).is_none() {
                continue;
            }
            let found = validate_packages_unique(packages, name, profile.toml.as_ref())?;
            if let Some(toml_profile) = manifest_profiles.and_then(|p| p.get(name)) {
                validate_packages_unmatched(packages, name, toml_profile, &found, warnings);
            }
        }
        Ok(())
    }

    fn get_profile_maker(&self, name: &str) -> anyhow::Result<&ProfileMaker> {
        self.by_name.get(name).ok_or_else(|| anyhow::anyhow!("profile `{name}` is not defined"))
    }

    pub fn profile_names(&self) -> impl Iterator<Item = &str> {
        self.by_name.keys().map(String::as_str)
    }
}

/// The profile a command builds with, as cargo's `get_profile_name`: `default` (`dev`, or
/// `test` and `bench` for those commands) unless `--release` or `--profile` names another.
pub fn requested_profile(
    default: &str,
    release: bool,
    profile: Option<&str>,
) -> anyhow::Result<String> {
    match (release, profile) {
        (false, None) => Ok(default.to_owned()),
        (true, None) => Ok("release".to_owned()),
        (_, Some("doc")) => {
            bail!("profile `doc` is reserved and not allowed to be explicitly specified")
        }
        (_, Some(name)) => {
            ProfileName::new(name)?;
            Ok(name.to_owned())
        }
    }
}

fn is_ci<F: Files>(config: &Config<F>) -> bool {
    config.env("CI").is_some() || config.env("TF_BUILD").is_some()
}

fn predefined_dir_names() -> HashMap<String, String> {
    [("dev", "debug"), ("test", "debug"), ("bench", "release")]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect()
}

fn predefined_profiles() -> [(&'static str, TomlProfile); 4] {
    let inherits =
        |name: &str| TomlProfile { inherits: Some(name.to_owned()), ..TomlProfile::default() };
    [
        ("debug", inherits("dev")),
        ("bench", inherits("release")),
        ("test", inherits("dev")),
        ("doc", inherits("dev")),
    ]
}

/// The hard-coded defaults of a profile and its TOML, `inherits` merged in. The precedence,
/// first winning: config profiles (merged into the TOML), `[profile.x.package.<name>]`,
/// `[profile.x.package."*"]` (not for members), `[profile.x.build-override]` (for the host),
/// `[profile.x]`, the defaults.
#[derive(Debug, Clone)]
struct ProfileMaker {
    default: Profile,
    toml: Option<TomlProfile>,
}

impl ProfileMaker {
    fn get_profile(
        &self,
        pkg: Option<&PackageId>,
        is_member: bool,
        is_for_host: bool,
    ) -> Profile {
        let mut profile = self.default.clone();
        if let Some(toml) = &self.toml {
            merge_profile(&mut profile, toml);
        }
        if is_for_host {
            profile.opt_level = "0".to_owned();
            profile.codegen_units = None;
            profile.debuginfo = DebugInfo::Deferred(profile.debuginfo.into_inner());
        }
        if let Some(toml) = &self.toml {
            merge_toml_overrides(pkg, is_member, is_for_host, &mut profile, toml);
        }
        profile
    }
}

fn merge_toml_overrides(
    pkg: Option<&PackageId>,
    is_member: bool,
    is_for_host: bool,
    profile: &mut Profile,
    toml: &TomlProfile,
) {
    if is_for_host && let Some(build_override) = &toml.build_override {
        merge_profile(profile, build_override);
    }
    if let Some(overrides) = toml.package.as_ref() {
        if !is_member && let Some(all) = overrides.get(&ProfilePackageSpec::All) {
            merge_profile(profile, all);
        }
        if let Some(pkg) = pkg {
            let mut matches = overrides.iter().filter_map(|(key, spec_profile)| match key {
                ProfilePackageSpec::All => None,
                ProfilePackageSpec::Spec(s) => spec_matches(s, pkg).then_some(spec_profile),
            });
            if let Some(spec_profile) = matches.next() {
                merge_profile(profile, spec_profile);
                assert!(
                    matches.next().is_none(),
                    "package `{pkg}` matched multiple package profile overrides"
                );
            }
        }
    }
}

fn merge_profile(profile: &mut Profile, toml: &TomlProfile) {
    if let Some(opt_level) = &toml.opt_level {
        profile.opt_level.clone_from(&opt_level.0);
    }
    match &toml.lto {
        Some(StringOrBool::Bool(b)) => profile.lto = Lto::Bool(*b),
        Some(StringOrBool::String(n)) if is_off(n) => profile.lto = Lto::Off,
        Some(StringOrBool::String(n)) => profile.lto = Lto::Named(n.clone()),
        None => {}
    }
    if toml.codegen_backend.is_some() {
        profile.codegen_backend.clone_from(&toml.codegen_backend);
    }
    if toml.codegen_units.is_some() {
        profile.codegen_units = toml.codegen_units;
    }
    if let Some(debuginfo) = toml.debug {
        profile.debuginfo = DebugInfo::Resolved(debuginfo);
    }
    if let Some(debug_assertions) = toml.debug_assertions {
        profile.debug_assertions = debug_assertions;
    }
    if let Some(split_debuginfo) = &toml.split_debuginfo {
        profile.split_debuginfo = Some(split_debuginfo.clone());
    }
    if let Some(rpath) = toml.rpath {
        profile.rpath = rpath;
    }
    if let Some(panic) = &toml.panic {
        profile.panic = match panic.as_str() {
            "unwind" => PanicStrategy::Unwind,
            "abort" => PanicStrategy::Abort,
            "immediate-abort" => PanicStrategy::ImmediateAbort,
            _ => panic!("Unexpected panic setting `{panic}`"),
        };
    }
    if let Some(overflow_checks) = toml.overflow_checks {
        profile.overflow_checks = overflow_checks;
    }
    if let Some(incremental) = toml.incremental {
        profile.incremental = incremental;
    }
    if let Some(flags) = &toml.rustflags {
        profile.rustflags.clone_from(flags);
    }
    if let Some(trim_paths) = &toml.trim_paths {
        profile.trim_paths = Some(trim_paths.clone());
    }
    if let Some(hint_mostly_unused) = toml.hint_mostly_unused {
        profile.hint_mostly_unused = Some(hint_mostly_unused);
    }
    profile.strip = match &toml.strip {
        Some(StringOrBool::Bool(true)) => Strip::Resolved(StripInner::Named("symbols".to_owned())),
        Some(StringOrBool::Bool(false)) => Strip::Resolved(StripInner::None),
        Some(StringOrBool::String(n)) if n == "none" => Strip::Resolved(StripInner::None),
        Some(StringOrBool::String(n)) => Strip::Resolved(StripInner::Named(n.clone())),
        None => Strip::Deferred(StripInner::None),
    };
}

fn is_off(s: &str) -> bool {
    matches!(s, "off" | "n" | "no" | "none")
}

/// `dev` or `release`, for the `PROFILE` of build scripts.
#[derive(Clone, Copy, Eq, PartialOrd, Ord, PartialEq, Debug)]
pub enum ProfileRoot {
    Release,
    Debug,
}

/// The compiler settings of a unit. Two are equal as cargo's are, whatever their name.
#[derive(Clone, Debug, Eq, Serialize)]
pub struct Profile {
    pub name: String,
    pub opt_level: String,
    #[serde(skip)]
    pub root: ProfileRoot,
    pub lto: Lto,
    pub codegen_backend: Option<String>,
    pub codegen_units: Option<u32>,
    pub debuginfo: DebugInfo,
    pub split_debuginfo: Option<String>,
    pub debug_assertions: bool,
    pub overflow_checks: bool,
    pub rpath: bool,
    pub incremental: bool,
    pub panic: PanicStrategy,
    pub strip: Strip,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub rustflags: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trim_paths: Option<TomlTrimPaths>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint_mostly_unused: Option<bool>,
}

impl Default for Profile {
    fn default() -> Profile {
        Profile {
            name: String::new(),
            opt_level: "0".to_owned(),
            root: ProfileRoot::Debug,
            lto: Lto::Bool(false),
            codegen_backend: None,
            codegen_units: None,
            debuginfo: DebugInfo::Resolved(TomlDebugInfo::None),
            split_debuginfo: None,
            debug_assertions: false,
            overflow_checks: false,
            rpath: false,
            incremental: false,
            panic: PanicStrategy::Unwind,
            strip: Strip::Deferred(StripInner::None),
            rustflags: Vec::new(),
            trim_paths: None,
            hint_mostly_unused: None,
        }
    }
}

impl PartialEq for Profile {
    fn eq(&self, other: &Profile) -> bool {
        self.comparable() == other.comparable()
    }
}

impl Profile {
    fn comparable(&self) -> impl Eq + '_ {
        (
            &self.opt_level,
            &self.lto,
            &self.codegen_backend,
            self.codegen_units,
            self.debuginfo,
            &self.split_debuginfo,
            self.debug_assertions,
            self.overflow_checks,
            self.rpath,
            (self.incremental, self.panic, &self.strip),
            &self.rustflags,
            &self.trim_paths,
        )
    }

    fn default_dev() -> Profile {
        Profile {
            name: "dev".to_owned(),
            root: ProfileRoot::Debug,
            debuginfo: DebugInfo::Resolved(TomlDebugInfo::Full),
            debug_assertions: true,
            overflow_checks: true,
            incremental: true,
            ..Profile::default()
        }
    }

    fn default_release() -> Profile {
        Profile {
            name: "release".to_owned(),
            root: ProfileRoot::Release,
            opt_level: "3".to_owned(),
            ..Profile::default()
        }
    }
}

/// A debuginfo level, which for a host unit is `Deferred`: the unit graph lowers it to none
/// unless the unit is shared with the target's.
#[derive(Debug, Copy, Clone, Serialize)]
#[serde(untagged)]
pub enum DebugInfo {
    Resolved(TomlDebugInfo),
    Deferred(TomlDebugInfo),
}

impl DebugInfo {
    pub fn into_inner(self) -> TomlDebugInfo {
        match self {
            DebugInfo::Resolved(v) | DebugInfo::Deferred(v) => v,
        }
    }

    pub fn is_turned_on(&self) -> bool {
        !matches!(self.into_inner(), TomlDebugInfo::None)
    }

    pub fn is_deferred(&self) -> bool {
        matches!(self, DebugInfo::Deferred(_))
    }

    pub fn finalize(self) -> Self {
        DebugInfo::Resolved(self.into_inner())
    }

    pub fn weaken(self) -> Self {
        DebugInfo::Resolved(TomlDebugInfo::None)
    }
}

impl PartialEq for DebugInfo {
    fn eq(&self, other: &DebugInfo) -> bool {
        self.into_inner() == other.into_inner()
    }
}

impl Eq for DebugInfo {}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Lto {
    /// No LTO, thin-local LTO off too.
    Off,
    /// `true` is fat LTO, `false` rustc's default.
    Bool(bool),
    Named(String),
}

impl Serialize for Lto {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Lto::Off => "off".serialize(s),
            Lto::Bool(b) => b.to_string().serialize(s),
            Lto::Named(n) => n.serialize(s),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PanicStrategy {
    Unwind,
    Abort,
    ImmediateAbort,
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize)]
pub enum StripInner {
    None,
    Named(String),
}

/// What to strip; `Deferred` lets the unit graph strip debuginfo when nothing wants it.
#[derive(Clone, Debug, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Strip {
    Resolved(StripInner),
    Deferred(StripInner),
}

impl Strip {
    pub fn into_inner(&self) -> &StripInner {
        match self {
            Strip::Resolved(v) | Strip::Deferred(v) => v,
        }
    }

    pub fn is_deferred(&self) -> bool {
        matches!(self, Strip::Deferred(_))
    }

    pub fn strip_debuginfo(self) -> Self {
        Strip::Resolved(StripInner::Named("debuginfo".to_owned()))
    }
}

impl PartialEq for Strip {
    fn eq(&self, other: &Self) -> bool {
        self.into_inner() == other.into_inner()
    }
}

/// What a unit is built for, as far as its profile goes: cargo's `UnitFor` less its features.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, PartialOrd, Ord)]
pub struct UnitFor {
    host: bool,
    host_features: bool,
    panic_setting: PanicSetting,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, PartialOrd, Ord)]
enum PanicSetting {
    AlwaysUnwind,
    ReadProfile,
}

impl UnitFor {
    /// A unit the user asked for, built for the target.
    pub fn new_normal() -> UnitFor {
        UnitFor { host: false, host_features: false, panic_setting: PanicSetting::ReadProfile }
    }

    /// A build script or proc macro the user asked for, and their dependencies.
    pub fn new_host(host_features: bool) -> UnitFor {
        UnitFor { host: true, host_features, panic_setting: PanicSetting::AlwaysUnwind }
    }

    /// A unit of the compiler's, as `new_normal` but always unwinding.
    pub fn new_compiler() -> UnitFor {
        UnitFor { host: false, host_features: false, panic_setting: PanicSetting::AlwaysUnwind }
    }

    /// A test or bench; `-Zpanic-abort-tests` being unstable, always unwinding.
    pub fn new_test() -> UnitFor {
        UnitFor::new_compiler()
    }

    /// A test of a proc macro or build script.
    pub fn new_host_test() -> UnitFor {
        UnitFor { host: true, host_features: true, panic_setting: PanicSetting::AlwaysUnwind }
    }

    /// The dependency of a unit of `parent_is_custom_build` (its target a build script) on a
    /// target that is `dep_for_host` (a build script or proc macro) and `dep_proc_macro`.
    pub fn with_dependency(
        self,
        parent_is_custom_build: bool,
        dep_for_host: bool,
        dep_proc_macro: bool,
    ) -> UnitFor {
        UnitFor {
            host: self.host || dep_for_host,
            host_features: self.host_features || parent_is_custom_build || dep_proc_macro,
            panic_setting: if dep_for_host {
                PanicSetting::AlwaysUnwind
            } else {
                self.panic_setting
            },
        }
    }

    /// The unit of compiling a build script.
    pub fn for_custom_build(self) -> UnitFor {
        UnitFor {
            host: true,
            host_features: self.host_features,
            panic_setting: PanicSetting::AlwaysUnwind,
        }
    }

    pub fn is_for_host(&self) -> bool {
        self.host
    }

    pub fn is_for_host_features(&self) -> bool {
        self.host_features
    }
}

/// The manifest's profiles with the configuration's merged over them, and the profiles only the
/// configuration defines that the requested one or the built-in ones reach.
fn merge_config_profiles<F: Files>(
    manifest_profiles: Option<&TomlProfiles>,
    features: &Features,
    config: &Config<F>,
    requested_profile: &str,
) -> anyhow::Result<BTreeMap<String, TomlProfile>> {
    let mut profiles: BTreeMap<String, TomlProfile> = match manifest_profiles {
        Some(profiles) => {
            profiles.get_all().iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
        }
        None => BTreeMap::new(),
    };
    let mut check_to_add = HashSet::default();
    check_to_add.insert(requested_profile.to_owned());
    for (name, profile) in &mut profiles {
        if let Some(config_profile) = get_config_profile(features, config, name)? {
            profile.merge(&config_profile);
        }
        if let Some(inherits) = &profile.inherits {
            check_to_add.insert(inherits.clone());
        }
    }
    for name in ["dev", "release", "debug", "test", "bench"] {
        check_to_add.insert(name.to_owned());
    }
    let mut current = HashSet::default();
    while !check_to_add.is_empty() {
        std::mem::swap(&mut current, &mut check_to_add);
        for name in current.drain() {
            if !profiles.contains_key(&name)
                && let Some(config_profile) = get_config_profile(features, config, &name)?
            {
                if let Some(inherits) = &config_profile.inherits {
                    check_to_add.insert(inherits.clone());
                }
                profiles.insert(name, config_profile);
            }
        }
    }
    Ok(profiles)
}

fn get_config_profile<F: Files>(
    features: &Features,
    config: &Config<F>,
    name: &str,
) -> anyhow::Result<Option<TomlProfile>> {
    let Some(profile) = config.config_profile(name)? else { return Ok(None) };
    let mut warnings = Vec::new();
    validate_profile(&profile.val, name, features, config.unstable(), &mut warnings).with_context(|| {
        format!("config profile `{name}` is not valid (defined in `{}`)", profile.definition)
    })?;
    for warning in warnings {
        config.warn(warning);
    }
    Ok(Some(profile.val))
}

fn validate_packages_unique(
    packages: &[&PackageId],
    name: &str,
    toml: Option<&TomlProfile>,
) -> anyhow::Result<Vec<PackageIdSpec>> {
    let Some(overrides) = toml.and_then(|t| t.package.as_ref()) else { return Ok(Vec::new()) };
    let mut found = Vec::new();
    for pkg in packages {
        let matches: Vec<&PackageIdSpec> = overrides
            .keys()
            .filter_map(|key| match key {
                ProfilePackageSpec::All => None,
                ProfilePackageSpec::Spec(spec) => spec_matches(spec, *pkg).then_some(spec),
            })
            .collect();
        match matches.as_slice() {
            [] => {}
            [one] => {
                if !found.contains(*one) {
                    found.push((*one).clone());
                }
            }
            _ => {
                let specs = matches.iter().map(|s| s.to_string()).collect::<Vec<_>>().join(", ");
                bail!(
                    "multiple package overrides in profile `{name}` match package `{pkg}`\n\
                     found package specs: {specs}"
                );
            }
        }
    }
    Ok(found)
}

fn validate_packages_unmatched(
    packages: &[&PackageId],
    name: &str,
    toml: &TomlProfile,
    found: &[PackageIdSpec],
    warnings: &mut Vec<String>,
) {
    let Some(overrides) = toml.package.as_ref() else { return };
    for key in overrides.keys() {
        let ProfilePackageSpec::Spec(spec) = key else { continue };
        if found.contains(spec) {
            continue;
        }
        let name_matches: Vec<String> =
            packages.iter().filter(|p| p.name == spec.name()).map(|p| p.to_string()).collect();
        if name_matches.is_empty() {
            let suggestion = crate::edit_distance::closest_msg(
                spec.name(),
                packages.iter(),
                |p| p.name.as_str(),
                "package",
            );
            warnings.push(format!(
                "profile package spec `{spec}` in profile `{name}` did not match any packages{suggestion}"
            ));
        } else {
            warnings.push(format!(
                "profile package spec `{spec}` in profile `{name}` has a version or URL that does \
                 not match any of the packages: {}",
                name_matches.join(", ")
            ));
        }
    }
}

/// Every profile of a manifest checked, as cargo checks them reading it.
pub fn validate_profiles(
    profiles: &TomlProfiles,
    features: &Features,
    unstable: &CliUnstable,
    warnings: &mut Vec<String>,
) -> anyhow::Result<()> {
    for (name, profile) in profiles.get_all() {
        validate_profile(profile, name, features, unstable, warnings)?;
    }
    Ok(())
}

/// A profile checked as nightly cargo's `validate_profile` does, the unstable keys
/// (`codegen-backend`, `rustflags`, `panic = "immediate-abort"`) refused unless the manifest's
/// `cargo-features` list them. For `codegen-backend` cargo also takes `-Zcodegen-backend` (or
/// `codegen-backend = true` under `[unstable]`) in place of the manifest's, and so does blaze; the
/// other two `-Z` flags (`-Zprofile-rustflags`, `-Zpanic-immediate-abort`) it does not read yet.
pub fn validate_profile(
    root: &TomlProfile,
    name: &str,
    features: &Features,
    unstable: &CliUnstable,
    warnings: &mut Vec<String>,
) -> anyhow::Result<()> {
    validate_profile_layer(root, features, unstable)?;
    if let Some(profile) = &root.build_override {
        validate_profile_override(profile, "build-override")?;
        validate_profile_layer(profile, features, unstable)?;
    }
    if let Some(packages) = &root.package {
        for profile in packages.values() {
            validate_profile_override(profile, "package")?;
            validate_profile_layer(profile, features, unstable)?;
        }
    }
    if let Some(dir_name) = &root.dir_name {
        bail!(
            "dir-name=\"{dir_name}\" in profile `{name}` is not currently allowed, \
             directory names are tied to the profile name for custom profiles"
        );
    }
    match name {
        "doc" => warnings.push("profile `doc` is deprecated and has no effect".to_owned()),
        "test" | "bench" if root.panic.is_some() => {
            warnings.push(format!("`panic` setting is ignored for `{name}` profile"));
        }
        _ => {}
    }
    if let Some(panic) = &root.panic
        && panic != "unwind"
        && panic != "abort"
        && panic != "immediate-abort"
    {
        bail!(
            "`panic` setting of `{panic}` is not a valid setting, \
             must be `unwind`, `abort`, or `immediate-abort`."
        );
    }
    if let Some(StringOrBool::String(arg)) = &root.lto
        && (arg == "true" || arg == "false")
    {
        bail!(
            "`lto` setting of string `\"{arg}\"` for `{name}` profile is not \
             a valid setting, must be a boolean (`true`/`false`) or a string \
             (`\"thin\"`/`\"fat\"`/`\"off\"`) or omitted."
        );
    }
    Ok(())
}

fn validate_profile_layer(
    profile: &TomlProfile,
    features: &Features,
    unstable: &CliUnstable,
) -> anyhow::Result<()> {
    if profile.codegen_backend.is_some() && !unstable.codegen_backend {
        features.require(Feature::codegen_backend)?;
    }
    if profile.rustflags.is_some() {
        features.require(Feature::profile_rustflags)?;
    }
    if profile.panic.as_deref() == Some("immediate-abort") {
        features.require(Feature::panic_immediate_abort)?;
    }
    Ok(())
}

fn validate_profile_override(profile: &TomlProfile, which: &str) -> anyhow::Result<()> {
    if profile.package.is_some() {
        bail!("package-specific profiles cannot be nested");
    }
    if profile.build_override.is_some() {
        bail!("build-override profiles cannot be nested");
    }
    if profile.panic.is_some() {
        bail!("`panic` may not be specified in a `{which}` profile")
    }
    if profile.lto.is_some() {
        bail!("`lto` may not be specified in a `{which}` profile")
    }
    if profile.rpath.is_some() {
        bail!("`rpath` may not be specified in a `{which}` profile")
    }
    Ok(())
}
