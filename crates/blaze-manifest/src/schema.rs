//! `Cargo.toml` as cargo's schema shapes it: the types of cargo-util-schemas 0.14.3's
//! `manifest` module (MIT or Apache-2.0), which cargo 1.99.0 (5f94df4789) reads a manifest into,
//! under the same names so that the code normalizing them reads side by side with cargo's; the
//! readers are blaze's, written by hand over a parsed document.
//!
//! A reader walks a [`Node`] (a `blaze_toml::Item`, or another tree of values that implements
//! it) and builds the owned types; nothing goes through serde. What does not fit is a
//! [`SchemaError`]: the key path, the byte range of the value and what is wrong, in the words
//! cargo's serde readers use (`invalid type: integer `1`, expected SemVer version`). Keys the
//! schema has no field for are passed over, as cargo passes over them; a dependency keeps its
//! own (`_unused_keys`) for the warning cargo gives.
//!
//! The spellings cargo accepts twice (`dev-dependencies` and `dev_dependencies`, `crate-type`
//! and `crate_type`, `proc-macro` and `proc_macro`, `default-features` and `default_features`)
//! are two fields here as there, read through the same accessors.

use std::alloc::Global;
use std::borrow::Borrow;
use std::collections::BTreeMap;
use std::fmt;
use std::ops::{Deref, Range};
use std::path::PathBuf;

use blaze_data::{Map, Value};
use blaze_resolve::{PackageIdSpec, ResolveError, RustVersion};
use blaze_semver::{Limits, Version};
use blaze_toml::{Item, Kind};

// ---------------------------------------------------------------------------------------------
// The tree a reader walks.

/// What a value is, as a reader sees it.
#[derive(Clone, Copy, Debug)]
pub enum Shape<'n> {
    Str(&'n str),
    Int(i64),
    Float(f64),
    Bool(bool),
    Datetime,
    Seq,
    Map,
}

/// A value of a tree of values a schema reader walks: a TOML document's item, or a value of
/// another source (the configuration's) that implements it.
pub trait Node<'n>: Copy {
    fn shape(self) -> Shape<'n>;
    /// A table's entries in text order, each with its key.
    fn entries(self) -> impl Iterator<Item = (&'n str, Self)>;
    /// An array's elements in order.
    fn elements(self) -> impl Iterator<Item = Self>;
    fn get(self, key: &str) -> Option<Self>;
    /// The byte range of the value in its file.
    fn span(self) -> Range<usize>;
    /// The byte range of the key it is under; the value's own range where it has none.
    fn key_span(self) -> Range<usize>;
    /// The dotted key path from the root (`package.version`, `bin[0].name`), for errors.
    fn path(self) -> String;
    /// The value as data, for what the schema keeps unread (`metadata`).
    fn to_value(self) -> Value<Global>;
    /// The text of a datetime, for an error naming it.
    fn datetime_text(self) -> String;
}

impl<'d, 't> Node<'d> for Item<'d, 't, Global> {
    fn shape(self) -> Shape<'d> {
        match self.kind() {
            Kind::Table => Shape::Map,
            Kind::Array => Shape::Seq,
            Kind::String => Shape::Str(self.as_str().unwrap_or_default()),
            Kind::Integer => Shape::Int(self.as_integer().unwrap_or_default()),
            Kind::Float => Shape::Float(self.as_float().unwrap_or_default()),
            Kind::Bool => Shape::Bool(self.as_bool().unwrap_or_default()),
            Kind::Datetime => Shape::Datetime,
        }
    }

    fn entries(self) -> impl Iterator<Item = (&'d str, Self)> {
        self.children().map(|it| (it.key(), it))
    }

    fn elements(self) -> impl Iterator<Item = Self> {
        self.children()
    }

    fn get(self, key: &str) -> Option<Self> {
        Item::get(&self, key)
    }

    fn span(self) -> Range<usize> {
        Item::span(&self)
    }

    fn key_span(self) -> Range<usize> {
        let span = Item::key_span(&self);
        if span.is_empty() { Item::span(&self) } else { span }
    }

    fn path(self) -> String {
        // The steps from the root, nearest first, then turned around.
        let mut steps: Vec<String> = Vec::new();
        let mut at = self;
        while let Some(parent) = at.parent() {
            if parent.is_array() {
                let start = Item::span(&at).start;
                let index = parent.children().position(|it| Item::span(&it).start == start);
                steps.push(format!("[{}]", index.unwrap_or_default()));
            } else {
                steps.push(at.key().to_owned());
            }
            at = parent;
        }
        let mut path = String::new();
        for step in steps.iter().rev() {
            if !path.is_empty() && !step.starts_with('[') {
                path.push('.');
            }
            path.push_str(step);
        }
        path
    }

    fn to_value(self) -> Value<Global> {
        self.to_value_in(Global)
    }

    fn datetime_text(self) -> String {
        self.as_datetime().map(|it| it.to_string()).unwrap_or_default()
    }
}

// ---------------------------------------------------------------------------------------------
// Errors.

/// A value as an error names what was found, in serde's words (`string "x"`, `integer `1``).
#[derive(Clone, Debug, PartialEq)]
pub enum Found {
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    Datetime(String),
    Seq,
    Map,
}

impl Found {
    fn of<'n, N: Node<'n>>(n: N) -> Found {
        match n.shape() {
            Shape::Str(s) => Found::Str(s.to_owned()),
            Shape::Int(i) => Found::Int(i),
            Shape::Float(f) => Found::Float(f),
            Shape::Bool(b) => Found::Bool(b),
            Shape::Datetime => Found::Datetime(n.datetime_text()),
            Shape::Seq => Found::Seq,
            Shape::Map => Found::Map,
        }
    }
}

impl fmt::Display for Found {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Found::Str(s) => write!(f, "string {s:?}"),
            Found::Int(i) => write!(f, "integer `{i}`"),
            Found::Float(x) if x.is_finite() && x.fract() == 0.0 => {
                write!(f, "floating point `{x:.1}`")
            }
            Found::Float(x) => write!(f, "floating point `{x}`"),
            Found::Bool(b) => write!(f, "boolean `{b}`"),
            Found::Datetime(d) => write!(f, "datetime `{d}`"),
            Found::Seq => f.write_str("sequence"),
            Found::Map => f.write_str("map"),
        }
    }
}

/// A profile name cargo refuses (cargo-util-schemas' `validate_profile_name`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProfileNameError {
    Char { ch: char, name: String },
    BuildOverride { name: String },
    Reserved { name: String },
}

impl fmt::Display for ProfileNameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let reserved = |f: &mut fmt::Formatter<'_>, name: &str, help: &str| {
            write!(
                f,
                "profile name `{name}` is reserved\n{help}\nSee \
                 https://doc.rust-lang.org/cargo/reference/profiles.html for more on configuring \
                 profiles."
            )
        };
        match self {
            ProfileNameError::Char { ch, name } => write!(
                f,
                "invalid character `{ch}` in profile name: `{name}`, allowed characters are \
                 letters, numbers, underscore, and hyphen"
            ),
            ProfileNameError::BuildOverride { name } => reserved(
                f,
                name,
                "To configure build dependency settings, use [profile.dev.build-override] and \
                 [profile.release.build-override]",
            ),
            ProfileNameError::Reserved { name } => {
                reserved(f, name, "Please choose a different name.")
            }
        }
    }
}

impl core::error::Error for ProfileNameError {}

/// What is wrong with a value, in the words of cargo's serde readers.
#[derive(Debug)]
pub enum SchemaWhat {
    /// `invalid type: {found}, expected {expected}`.
    InvalidType { found: Found, expected: &'static str },
    /// `invalid value: {found}, expected {expected}`.
    InvalidValue { found: Found, expected: &'static str },
    /// `unknown variant `{found}`, expected one of ...`.
    UnknownVariant { found: String, expected: &'static [&'static str] },
    /// `missing field `{0}``, at the table that lacks it.
    MissingField(&'static str),
    /// `workspace = false`.
    WorkspaceFalse,
    /// A dependency written as a bool.
    DependencyBool(bool),
    /// `cargo-features` under `[package]`.
    CargoFeaturesInPackage,
    /// An `opt-level` string other than `s` and `z`.
    OptLevel(String),
    /// A `trim-paths` string that is no option.
    TrimPaths,
    /// A version that does not parse.
    Version(blaze_semver::Error),
    /// A name, `rust-version` or package id spec blaze-resolve refuses.
    Resolve(ResolveError),
    ProfileName(ProfileNameError),
}

const DEPENDENCY: &str =
    "a version string like \"0.9.8\" or a detailed dependency like { version = \"0.9.8\" }";
const DEBUG: &str = "a boolean, 0, 1, 2, \"none\", \"limited\", \"full\", \"line-tables-only\", \
                     or \"line-directives-only\"";
const TRIM_PATHS: &str =
    r#"a boolean, "none", "diagnostics", "macro", "object", "all", or an array with these options"#;
const LINT_LEVELS: &[&str] = &["forbid", "deny", "warn", "allow"];
const TRIM_PATHS_VALUES: &[&str] = &["diagnostics", "macro", "object"];

impl fmt::Display for SchemaWhat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SchemaWhat::InvalidType { found, expected } => {
                write!(f, "invalid type: {found}, expected {expected}")
            }
            SchemaWhat::InvalidValue { found, expected } => {
                write!(f, "invalid value: {found}, expected {expected}")
            }
            SchemaWhat::UnknownVariant { found, expected } => {
                write!(f, "unknown variant `{found}`, expected ")?;
                match expected {
                    [] => f.write_str("no variants"),
                    [one] => write!(f, "`{one}`"),
                    [a, b] => write!(f, "`{a}` or `{b}`"),
                    many => {
                        f.write_str("one of ")?;
                        for (i, v) in many.iter().enumerate() {
                            if i > 0 {
                                f.write_str(", ")?;
                            }
                            write!(f, "`{v}`")?;
                        }
                        Ok(())
                    }
                }
            }
            SchemaWhat::MissingField(field) => write!(f, "missing field `{field}`"),
            SchemaWhat::WorkspaceFalse => f.write_str("`workspace` cannot be false"),
            SchemaWhat::DependencyBool(value) => {
                write!(f, "invalid type: boolean `{value}`, expected {DEPENDENCY}")?;
                if *value {
                    write!(
                        f,
                        "\nnote: if you meant to use a workspace member, you can write\n \
                         dep.workspace = {value}"
                    )?;
                }
                Ok(())
            }
            SchemaWhat::CargoFeaturesInPackage => f.write_str(
                "the field `cargo-features` should be set at the top of Cargo.toml before any \
                 tables",
            ),
            SchemaWhat::OptLevel(found) => write!(
                f,
                "must be `0`, `1`, `2`, `3`, `s` or `z`, but found the string: \"{found}\""
            ),
            SchemaWhat::TrimPaths => write!(f, "expected {TRIM_PATHS}"),
            SchemaWhat::Version(e) => write!(f, "{e}"),
            SchemaWhat::Resolve(e) => write!(f, "{e}"),
            SchemaWhat::ProfileName(e) => write!(f, "{e}"),
        }
    }
}

/// A value that does not fit the schema: where (the dotted key path and the byte range of the
/// value, or of the key for a refused key) and what.
#[derive(Debug)]
pub struct SchemaError {
    pub key: String,
    pub span: Range<usize>,
    pub what: SchemaWhat,
}

impl SchemaError {
    fn at<'n, N: Node<'n>>(n: N, what: SchemaWhat) -> SchemaError {
        SchemaError { key: n.path(), span: n.span(), what }
    }

    fn at_key<'n, N: Node<'n>>(n: N, what: SchemaWhat) -> SchemaError {
        SchemaError { key: n.path(), span: n.key_span(), what }
    }
}

impl fmt::Display for SchemaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.what)
    }
}

impl core::error::Error for SchemaError {}

type Read<T> = Result<T, SchemaError>;

fn invalid<'n, N: Node<'n>>(n: N, expected: &'static str) -> SchemaError {
    SchemaError::at(n, SchemaWhat::InvalidType { found: Found::of(n), expected })
}

// ---------------------------------------------------------------------------------------------
// Readers of the plain shapes.

fn string<'n, N: Node<'n>>(n: N) -> Read<String> {
    match n.shape() {
        Shape::Str(s) => Ok(s.to_owned()),
        _ => Err(invalid(n, "a string")),
    }
}

fn boolean<'n, N: Node<'n>>(n: N) -> Read<bool> {
    match n.shape() {
        Shape::Bool(b) => Ok(b),
        _ => Err(invalid(n, "a boolean")),
    }
}

fn int_in<'n, N: Node<'n>, T: TryFrom<i64>>(n: N, expected: &'static str) -> Read<T> {
    match n.shape() {
        Shape::Int(i) => T::try_from(i).map_err(|_| {
            SchemaError::at(n, SchemaWhat::InvalidValue { found: Found::Int(i), expected })
        }),
        _ => Err(invalid(n, expected)),
    }
}

fn table<'n, N: Node<'n>>(n: N, expected: &'static str) -> Read<N> {
    match n.shape() {
        Shape::Map => Ok(n),
        _ => Err(invalid(n, expected)),
    }
}

fn seq<'n, N: Node<'n>>(n: N) -> Read<N> {
    match n.shape() {
        Shape::Seq => Ok(n),
        _ => Err(invalid(n, "a sequence")),
    }
}

fn strings<'n, N: Node<'n>>(n: N) -> Read<Vec<String>> {
    seq(n)?.elements().map(string).collect()
}

fn map_of<'n, N: Node<'n>, K: Ord, V>(
    n: N,
    key: impl Fn(&'n str, N) -> Read<K>,
    value: impl Fn(N) -> Read<V>,
) -> Read<BTreeMap<K, V>> {
    let mut out = BTreeMap::new();
    for (k, v) in table(n, "a map")?.entries() {
        out.insert(key(k, v)?, value(v)?);
    }
    Ok(out)
}

fn plain_key<'n, N: Node<'n>>(key: &'n str, _: N) -> Read<String> {
    Ok(key.to_owned())
}

fn string_map<'n, N: Node<'n>>(n: N) -> Read<BTreeMap<String, String>> {
    map_of(n, plain_key, string)
}

fn badges<'n, N: Node<'n>>(n: N) -> Read<BTreeMap<String, BTreeMap<String, String>>> {
    map_of(n, plain_key, string_map)
}

fn version<'n, N: Node<'n>>(n: N, text: &str) -> Read<Version<Global>> {
    Version::parse_in(text, Limits::CARGO, Global)
        .map_err(|e| SchemaError::at(n, SchemaWhat::Version(e)))
}

fn rust_version_of<'n, N: Node<'n>>(n: N, text: &str) -> Read<RustVersion> {
    text.parse::<RustVersion>().map_err(|e| SchemaError::at(n, SchemaWhat::Resolve(e)))
}

// ---------------------------------------------------------------------------------------------
// Names, validated when read.

macro_rules! name_type {
    ($(#[$doc:meta])* $name:ident, $error:ty, $validate:path) => {
        $(#[$doc])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(String);

        impl $name {
            pub fn new(name: impl Into<String>) -> Result<$name, $error> {
                let name = name.into();
                $validate(&name)?;
                Ok($name(name))
            }

            pub fn into_inner(self) -> String {
                self.0
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl Deref for $name {
            type Target = String;

            fn deref(&self) -> &String {
                &self.0
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }

        impl Borrow<str> for $name {
            fn borrow(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl serde::Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(&self.0)
            }
        }
    };
}

name_type!(
    /// A package name cargo accepts.
    PackageName, ResolveError, blaze_resolve::validate_package_name
);
name_type!(
    /// A registry name cargo accepts.
    RegistryName, ResolveError, blaze_resolve::validate_registry_name
);
name_type!(
    /// A feature name cargo accepts.
    FeatureName, ResolveError, blaze_resolve::validate_feature_name
);
name_type!(
    /// A `[path-bases]` name cargo accepts.
    PathBaseName, ResolveError, blaze_resolve::validate_path_base_name
);
name_type!(
    /// A profile name cargo accepts.
    ProfileName, ProfileNameError, validate_profile_name
);

impl PackageName {
    /// A name known valid: one read from a record cargo or blaze already checked.
    pub(crate) fn trusted(name: String) -> PackageName {
        PackageName(name)
    }
}

/// cargo-util-schemas' `validate_profile_name`: letters, numbers, `_` and `-`, and none of the
/// names cargo keeps for itself.
pub fn validate_profile_name(name: &str) -> Result<(), ProfileNameError> {
    if let Some(ch) = name.chars().find(|ch| !ch.is_alphanumeric() && *ch != '_' && *ch != '-') {
        return Err(ProfileNameError::Char { ch, name: name.to_owned() });
    }
    let lower = name.to_lowercase();
    if lower == "build-override" {
        return Err(ProfileNameError::BuildOverride { name: name.to_owned() });
    }
    const RESERVED: &[&str] = &[
        "build", "check", "clean", "config", "fetch", "fix", "install", "metadata", "package",
        "publish", "report", "root", "run", "rust", "rustc", "rustdoc", "target", "tmp",
        "uninstall",
    ];
    if RESERVED.contains(&lower.as_str()) || lower.starts_with("cargo") {
        return Err(ProfileNameError::Reserved { name: name.to_owned() });
    }
    Ok(())
}

fn package_name<'n, N: Node<'n>>(key: &'n str, n: N) -> Read<PackageName> {
    PackageName::new(key).map_err(|e| SchemaError::at_key(n, SchemaWhat::Resolve(e)))
}

fn package_name_value<'n, N: Node<'n>>(n: N) -> Read<PackageName> {
    PackageName::new(string(n)?).map_err(|e| SchemaError::at(n, SchemaWhat::Resolve(e)))
}

fn feature_name<'n, N: Node<'n>>(key: &'n str, n: N) -> Read<FeatureName> {
    FeatureName::new(key).map_err(|e| SchemaError::at_key(n, SchemaWhat::Resolve(e)))
}

fn profile_name<'n, N: Node<'n>>(key: &'n str, n: N) -> Read<ProfileName> {
    ProfileName::new(key).map_err(|e| SchemaError::at_key(n, SchemaWhat::ProfileName(e)))
}

// ---------------------------------------------------------------------------------------------
// The manifest.

/// A whole `Cargo.toml`.
#[derive(Debug, Default, Clone)]
pub struct TomlManifest {
    pub cargo_features: Option<Vec<String>>,
    pub package: Option<Box<TomlPackage>>,
    pub project: Option<Box<TomlPackage>>,
    pub badges: Option<BTreeMap<String, BTreeMap<String, String>>>,
    pub features: Option<BTreeMap<FeatureName, Vec<String>>>,
    pub lib: Option<TomlLibTarget>,
    pub bin: Option<Vec<TomlBinTarget>>,
    pub example: Option<Vec<TomlExampleTarget>>,
    pub test: Option<Vec<TomlTestTarget>>,
    pub bench: Option<Vec<TomlTestTarget>>,
    pub dependencies: Option<BTreeMap<PackageName, InheritableDependency>>,
    pub dev_dependencies: Option<BTreeMap<PackageName, InheritableDependency>>,
    pub dev_dependencies2: Option<BTreeMap<PackageName, InheritableDependency>>,
    pub build_dependencies: Option<BTreeMap<PackageName, InheritableDependency>>,
    pub build_dependencies2: Option<BTreeMap<PackageName, InheritableDependency>>,
    pub target: Option<BTreeMap<String, TomlPlatform>>,
    pub lints: Option<InheritableLints>,
    pub hints: Option<Hints>,
    pub workspace: Option<TomlWorkspace>,
    pub profile: Option<TomlProfiles>,
    pub patch: Option<BTreeMap<String, BTreeMap<PackageName, TomlDependency>>>,
    pub replace: Option<BTreeMap<String, TomlDependency>>,
}

impl TomlManifest {
    /// Reads a manifest from its root table.
    pub fn read<'n, N: Node<'n>>(n: N) -> Read<TomlManifest> {
        let mut m = TomlManifest::default();
        for (key, v) in table(n, "struct TomlManifest")?.entries() {
            match key {
                "cargo-features" => m.cargo_features = Some(strings(v)?),
                "package" => m.package = Some(Box::new(TomlPackage::read(v)?)),
                "project" => m.project = Some(Box::new(TomlPackage::read(v)?)),
                "badges" => m.badges = Some(badges(v)?),
                "features" => m.features = Some(map_of(v, feature_name, strings)?),
                "lib" => m.lib = Some(TomlTarget::read(v)?),
                "bin" => m.bin = Some(targets(v)?),
                "example" => m.example = Some(targets(v)?),
                "test" => m.test = Some(targets(v)?),
                "bench" => m.bench = Some(targets(v)?),
                "dependencies" => m.dependencies = Some(inheritable_dependencies(v)?),
                "dev-dependencies" => m.dev_dependencies = Some(inheritable_dependencies(v)?),
                "dev_dependencies" => m.dev_dependencies2 = Some(inheritable_dependencies(v)?),
                "build-dependencies" => {
                    m.build_dependencies = Some(inheritable_dependencies(v)?)
                }
                "build_dependencies" => {
                    m.build_dependencies2 = Some(inheritable_dependencies(v)?)
                }
                "target" => m.target = Some(map_of(v, plain_key, TomlPlatform::read)?),
                "lints" => m.lints = Some(InheritableLints::read(v)?),
                "hints" => m.hints = Some(Hints::read(v)?),
                "workspace" => m.workspace = Some(TomlWorkspace::read(v)?),
                "profile" => m.profile = Some(TomlProfiles(map_of(v, profile_name, TomlProfile::read)?)),
                "patch" => {
                    m.patch = Some(map_of(v, plain_key, |t| {
                        map_of(t, package_name, TomlDependency::read)
                    })?)
                }
                "replace" => m.replace = Some(map_of(v, plain_key, TomlDependency::read)?),
                _ => {}
            }
        }
        Ok(m)
    }

    pub fn requires_package(&self) -> impl Iterator<Item = &'static str> {
        [
            self.badges.as_ref().map(|_| "badges"),
            self.features.as_ref().map(|_| "features"),
            self.lib.as_ref().map(|_| "lib"),
            self.bin.as_ref().map(|_| "bin"),
            self.example.as_ref().map(|_| "example"),
            self.test.as_ref().map(|_| "test"),
            self.bench.as_ref().map(|_| "bench"),
            self.dependencies.as_ref().map(|_| "dependencies"),
            self.dev_dependencies().map(|_| "dev-dependencies"),
            self.build_dependencies().map(|_| "build-dependencies"),
            self.target.as_ref().map(|_| "target"),
            self.lints.as_ref().map(|_| "lints"),
            self.hints.as_ref().map(|_| "hints"),
        ]
        .into_iter()
        .flatten()
    }

    pub fn has_profiles(&self) -> bool {
        self.profile.is_some()
    }

    pub fn package(&self) -> Option<&Box<TomlPackage>> {
        self.package.as_ref().or(self.project.as_ref())
    }

    pub fn dev_dependencies(&self) -> Option<&BTreeMap<PackageName, InheritableDependency>> {
        self.dev_dependencies.as_ref().or(self.dev_dependencies2.as_ref())
    }

    pub fn build_dependencies(&self) -> Option<&BTreeMap<PackageName, InheritableDependency>> {
        self.build_dependencies.as_ref().or(self.build_dependencies2.as_ref())
    }

    pub fn features(&self) -> Option<&BTreeMap<FeatureName, Vec<String>>> {
        self.features.as_ref()
    }

    pub fn normalized_lints(&self) -> Result<Option<&TomlLints>, UnresolvedError> {
        self.lints.as_ref().map(|l| l.normalized()).transpose()
    }
}

fn targets<'n, N: Node<'n>>(n: N) -> Read<Vec<TomlTarget>> {
    seq(n)?.elements().map(TomlTarget::read).collect()
}

fn inheritable_dependencies<'n, N: Node<'n>>(
    n: N,
) -> Read<BTreeMap<PackageName, InheritableDependency>> {
    map_of(n, package_name, InheritableDependency::read)
}

/// A field of the manifest that is still `workspace = true`, asked for as if normalized.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnresolvedError;

impl fmt::Display for UnresolvedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("manifest field was not resolved")
    }
}

impl core::error::Error for UnresolvedError {}

/// `[workspace]`.
#[derive(Debug, Default, Clone)]
pub struct TomlWorkspace {
    pub members: Option<Vec<String>>,
    pub exclude: Option<Vec<String>>,
    pub default_members: Option<Vec<String>>,
    pub resolver: Option<String>,
    pub metadata: Option<Value<Global>>,
    pub package: Option<InheritablePackage>,
    pub dependencies: Option<BTreeMap<PackageName, TomlDependency>>,
    pub lints: Option<TomlLints>,
}

impl TomlWorkspace {
    fn read<'n, N: Node<'n>>(n: N) -> Read<TomlWorkspace> {
        let mut w = TomlWorkspace::default();
        for (key, v) in table(n, "struct TomlWorkspace")?.entries() {
            match key {
                "members" => w.members = Some(strings(v)?),
                "exclude" => w.exclude = Some(strings(v)?),
                "default-members" => w.default_members = Some(strings(v)?),
                "resolver" => w.resolver = Some(string(v)?),
                "metadata" => w.metadata = Some(v.to_value()),
                "package" => w.package = Some(InheritablePackage::read(v)?),
                "dependencies" => w.dependencies = Some(map_of(v, package_name, TomlDependency::read)?),
                "lints" => w.lints = Some(lints(v)?),
                _ => {}
            }
        }
        Ok(w)
    }
}

/// `[workspace.package]`: what members may inherit.
#[derive(Debug, Default, Clone)]
pub struct InheritablePackage {
    pub version: Option<Version<Global>>,
    pub authors: Option<Vec<String>>,
    pub description: Option<String>,
    pub homepage: Option<String>,
    pub documentation: Option<String>,
    pub readme: Option<StringOrBool>,
    pub keywords: Option<Vec<String>>,
    pub categories: Option<Vec<String>>,
    pub license: Option<String>,
    pub license_file: Option<String>,
    pub repository: Option<String>,
    pub publish: Option<VecStringOrBool>,
    pub edition: Option<String>,
    pub badges: Option<BTreeMap<String, BTreeMap<String, String>>>,
    pub exclude: Option<Vec<String>>,
    pub include: Option<Vec<String>>,
    pub rust_version: Option<RustVersion>,
}

impl InheritablePackage {
    fn read<'n, N: Node<'n>>(n: N) -> Read<InheritablePackage> {
        let mut p = InheritablePackage::default();
        for (key, v) in table(n, "struct InheritablePackage")?.entries() {
            match key {
                "version" => match v.shape() {
                    Shape::Str(s) => p.version = Some(version(v, s)?),
                    _ => return Err(invalid(v, "semver version")),
                },
                "authors" => p.authors = Some(strings(v)?),
                "description" => p.description = Some(string(v)?),
                "homepage" => p.homepage = Some(string(v)?),
                "documentation" => p.documentation = Some(string(v)?),
                "readme" => p.readme = Some(StringOrBool::read(v)?),
                "keywords" => p.keywords = Some(strings(v)?),
                "categories" => p.categories = Some(strings(v)?),
                "license" => p.license = Some(string(v)?),
                "license-file" => p.license_file = Some(string(v)?),
                "repository" => p.repository = Some(string(v)?),
                "publish" => p.publish = Some(VecStringOrBool::read(v)?),
                "edition" => p.edition = Some(string(v)?),
                "badges" => p.badges = Some(badges(v)?),
                "exclude" => p.exclude = Some(strings(v)?),
                "include" => p.include = Some(strings(v)?),
                "rust-version" => match v.shape() {
                    Shape::Str(s) => p.rust_version = Some(rust_version_of(v, s)?),
                    _ => return Err(invalid(v, "SemVer version")),
                },
                _ => {}
            }
        }
        Ok(p)
    }
}

/// `[package]` (or the old `[project]`).
#[derive(Debug, Default, Clone)]
pub struct TomlPackage {
    pub edition: Option<InheritableString>,
    pub rust_version: Option<InheritableRustVersion>,
    pub name: Option<PackageName>,
    pub version: Option<InheritableSemverVersion>,
    pub authors: Option<InheritableVecString>,
    pub build: Option<TomlPackageBuild>,
    pub metabuild: Option<StringOrVec>,
    pub default_target: Option<String>,
    pub forced_target: Option<String>,
    pub links: Option<String>,
    pub exclude: Option<InheritableVecString>,
    pub include: Option<InheritableVecString>,
    pub publish: Option<InheritableVecStringOrBool>,
    pub workspace: Option<String>,
    pub im_a_teapot: Option<bool>,
    pub autolib: Option<bool>,
    pub autobins: Option<bool>,
    pub autoexamples: Option<bool>,
    pub autotests: Option<bool>,
    pub autobenches: Option<bool>,
    pub default_run: Option<String>,
    pub description: Option<InheritableString>,
    pub homepage: Option<InheritableString>,
    pub documentation: Option<InheritableString>,
    pub readme: Option<InheritableStringOrBool>,
    pub keywords: Option<InheritableVecString>,
    pub categories: Option<InheritableVecString>,
    pub license: Option<InheritableString>,
    pub license_file: Option<InheritableString>,
    pub repository: Option<InheritableString>,
    pub resolver: Option<String>,
    pub metadata: Option<Value<Global>>,
}

impl TomlPackage {
    pub fn new(name: PackageName) -> TomlPackage {
        TomlPackage { name: Some(name), ..TomlPackage::default() }
    }

    fn read<'n, N: Node<'n>>(n: N) -> Read<TomlPackage> {
        let mut p = TomlPackage::default();
        let s = inheritable_string;
        let vs = inheritable_strings;
        for (key, v) in table(n, "struct TomlPackage")?.entries() {
            match key {
                "edition" => p.edition = Some(s(v)?),
                "rust-version" => p.rust_version = Some(inheritable_rust_version(v)?),
                "name" => p.name = Some(package_name_value(v)?),
                "version" => p.version = Some(inheritable_version(v)?),
                "authors" => p.authors = Some(vs(v)?),
                "build" => p.build = Some(TomlPackageBuild::read(v)?),
                "metabuild" => p.metabuild = Some(StringOrVec::read(v)?),
                "default-target" => p.default_target = Some(string(v)?),
                "forced-target" => p.forced_target = Some(string(v)?),
                "links" => p.links = Some(string(v)?),
                "exclude" => p.exclude = Some(vs(v)?),
                "include" => p.include = Some(vs(v)?),
                "publish" => p.publish = Some(inheritable_vec_string_or_bool(v)?),
                "workspace" => p.workspace = Some(string(v)?),
                "im-a-teapot" => p.im_a_teapot = Some(boolean(v)?),
                "autolib" => p.autolib = Some(boolean(v)?),
                "autobins" => p.autobins = Some(boolean(v)?),
                "autoexamples" => p.autoexamples = Some(boolean(v)?),
                "autotests" => p.autotests = Some(boolean(v)?),
                "autobenches" => p.autobenches = Some(boolean(v)?),
                "default-run" => p.default_run = Some(string(v)?),
                "description" => p.description = Some(s(v)?),
                "homepage" => p.homepage = Some(s(v)?),
                "documentation" => p.documentation = Some(s(v)?),
                "readme" => p.readme = Some(inheritable_string_or_bool(v)?),
                "keywords" => p.keywords = Some(vs(v)?),
                "categories" => p.categories = Some(vs(v)?),
                "license" => p.license = Some(s(v)?),
                "license-file" => p.license_file = Some(s(v)?),
                "repository" => p.repository = Some(s(v)?),
                "resolver" => p.resolver = Some(string(v)?),
                "metadata" => p.metadata = Some(v.to_value()),
                "cargo-features" => {
                    return Err(SchemaError::at(v, SchemaWhat::CargoFeaturesInPackage));
                }
                _ => {}
            }
        }
        Ok(p)
    }

    pub fn normalized_name(&self) -> Result<&PackageName, UnresolvedError> {
        self.name.as_ref().ok_or(UnresolvedError)
    }

    pub fn normalized_edition(&self) -> Result<Option<&String>, UnresolvedError> {
        self.edition.as_ref().map(|v| v.normalized()).transpose()
    }

    pub fn normalized_rust_version(&self) -> Result<Option<&RustVersion>, UnresolvedError> {
        self.rust_version.as_ref().map(|v| v.normalized()).transpose()
    }

    pub fn normalized_version(&self) -> Result<Option<&Version<Global>>, UnresolvedError> {
        self.version.as_ref().map(|v| v.normalized()).transpose()
    }

    pub fn normalized_authors(&self) -> Result<Option<&Vec<String>>, UnresolvedError> {
        self.authors.as_ref().map(|v| v.normalized()).transpose()
    }

    pub fn normalized_build(&self) -> Result<Option<&[String]>, UnresolvedError> {
        match self.build.as_ref().ok_or(UnresolvedError)? {
            TomlPackageBuild::Auto(false) => Ok(None),
            TomlPackageBuild::Auto(true) => Err(UnresolvedError),
            TomlPackageBuild::SingleScript(value) => Ok(Some(std::slice::from_ref(value))),
            TomlPackageBuild::MultipleScript(scripts) => Ok(Some(scripts)),
        }
    }

    pub fn normalized_exclude(&self) -> Result<Option<&Vec<String>>, UnresolvedError> {
        self.exclude.as_ref().map(|v| v.normalized()).transpose()
    }

    pub fn normalized_include(&self) -> Result<Option<&Vec<String>>, UnresolvedError> {
        self.include.as_ref().map(|v| v.normalized()).transpose()
    }

    pub fn normalized_publish(&self) -> Result<Option<&VecStringOrBool>, UnresolvedError> {
        self.publish.as_ref().map(|v| v.normalized()).transpose()
    }

    pub fn normalized_description(&self) -> Result<Option<&String>, UnresolvedError> {
        self.description.as_ref().map(|v| v.normalized()).transpose()
    }

    pub fn normalized_homepage(&self) -> Result<Option<&String>, UnresolvedError> {
        self.homepage.as_ref().map(|v| v.normalized()).transpose()
    }

    pub fn normalized_documentation(&self) -> Result<Option<&String>, UnresolvedError> {
        self.documentation.as_ref().map(|v| v.normalized()).transpose()
    }

    pub fn normalized_readme(&self) -> Result<Option<&String>, UnresolvedError> {
        let readme = self.readme.as_ref().ok_or(UnresolvedError)?;
        match readme.normalized()? {
            StringOrBool::Bool(false) => Ok(None),
            StringOrBool::Bool(true) => Err(UnresolvedError),
            StringOrBool::String(value) => Ok(Some(value)),
        }
    }

    pub fn normalized_keywords(&self) -> Result<Option<&Vec<String>>, UnresolvedError> {
        self.keywords.as_ref().map(|v| v.normalized()).transpose()
    }

    pub fn normalized_categories(&self) -> Result<Option<&Vec<String>>, UnresolvedError> {
        self.categories.as_ref().map(|v| v.normalized()).transpose()
    }

    pub fn normalized_license(&self) -> Result<Option<&String>, UnresolvedError> {
        self.license.as_ref().map(|v| v.normalized()).transpose()
    }

    pub fn normalized_license_file(&self) -> Result<Option<&String>, UnresolvedError> {
        self.license_file.as_ref().map(|v| v.normalized()).transpose()
    }

    pub fn normalized_repository(&self) -> Result<Option<&String>, UnresolvedError> {
        self.repository.as_ref().map(|v| v.normalized()).transpose()
    }
}

// ---------------------------------------------------------------------------------------------
// `workspace = true` fields.

/// A package field given, or `{ workspace = true }`.
#[derive(Debug, Clone, Copy)]
pub enum InheritableField<T> {
    Value(T),
    Inherit(TomlInheritedField),
}

pub type InheritableSemverVersion = InheritableField<Version<Global>>;
pub type InheritableString = InheritableField<String>;
pub type InheritableRustVersion = InheritableField<RustVersion>;
pub type InheritableVecString = InheritableField<Vec<String>>;
pub type InheritableStringOrBool = InheritableField<StringOrBool>;
pub type InheritableVecStringOrBool = InheritableField<VecStringOrBool>;
pub type InheritableBtreeMap = InheritableField<BTreeMap<String, BTreeMap<String, String>>>;

impl<T> InheritableField<T> {
    pub fn normalized(&self) -> Result<&T, UnresolvedError> {
        self.as_value().ok_or(UnresolvedError)
    }

    pub fn as_value(&self) -> Option<&T> {
        match self {
            InheritableField::Inherit(_) => None,
            InheritableField::Value(defined) => Some(defined),
        }
    }

    pub fn into_value(self) -> Option<T> {
        match self {
            InheritableField::Inherit(_) => None,
            InheritableField::Value(defined) => Some(defined),
        }
    }

    pub fn is_inherited(&self) -> bool {
        matches!(self, InheritableField::Inherit(_))
    }
}

/// `{ workspace = true }`.
#[derive(Debug, Clone, Copy, Default)]
pub struct TomlInheritedField {
    _workspace: (),
}

impl TomlInheritedField {
    pub fn new() -> TomlInheritedField {
        TomlInheritedField { _workspace: () }
    }

    fn read<'n, N: Node<'n>>(n: N) -> Read<TomlInheritedField> {
        match n.get("workspace") {
            None => Err(SchemaError::at(n, SchemaWhat::MissingField("workspace"))),
            Some(w) => match boolean(w)? {
                true => Ok(TomlInheritedField::new()),
                false => Err(SchemaError::at(w, SchemaWhat::WorkspaceFalse)),
            },
        }
    }
}

fn inherit_or<'n, N: Node<'n>, T>(
    n: N,
    expected: &'static str,
    value: impl FnOnce(N, Shape<'n>) -> Option<Read<T>>,
) -> Read<InheritableField<T>> {
    match n.shape() {
        Shape::Map => TomlInheritedField::read(n).map(InheritableField::Inherit),
        shape => match value(n, shape) {
            Some(read) => read.map(InheritableField::Value),
            None => Err(invalid(n, expected)),
        },
    }
}

fn inheritable_string<'n, N: Node<'n>>(n: N) -> Read<InheritableString> {
    inherit_or(n, "a string or workspace", |_, shape| match shape {
        Shape::Str(s) => Some(Ok(s.to_owned())),
        _ => None,
    })
}

fn inheritable_version<'n, N: Node<'n>>(n: N) -> Read<InheritableSemverVersion> {
    inherit_or(n, "SemVer version", |n, shape| match shape {
        Shape::Str(s) => Some(version(n, s.trim())),
        _ => None,
    })
}

fn inheritable_rust_version<'n, N: Node<'n>>(n: N) -> Read<InheritableRustVersion> {
    inherit_or(n, "a semver or workspace", |n, shape| match shape {
        Shape::Str(s) => Some(rust_version_of(n, s)),
        _ => None,
    })
}

fn inheritable_strings<'n, N: Node<'n>>(n: N) -> Read<InheritableVecString> {
    inherit_or(n, "a vector of strings or workspace", |n, shape| match shape {
        Shape::Seq => Some(strings(n)),
        _ => None,
    })
}

fn inheritable_string_or_bool<'n, N: Node<'n>>(n: N) -> Read<InheritableStringOrBool> {
    inherit_or(n, "a string, a bool, or workspace", |_, shape| match shape {
        Shape::Str(s) => Some(Ok(StringOrBool::String(s.to_owned()))),
        Shape::Bool(b) => Some(Ok(StringOrBool::Bool(b))),
        _ => None,
    })
}

fn inheritable_vec_string_or_bool<'n, N: Node<'n>>(n: N) -> Read<InheritableVecStringOrBool> {
    inherit_or(n, "a boolean, a vector of strings, or workspace", |n, shape| match shape {
        Shape::Seq => Some(strings(n).map(VecStringOrBool::VecString)),
        Shape::Bool(b) => Some(Ok(VecStringOrBool::Bool(b))),
        _ => None,
    })
}

// ---------------------------------------------------------------------------------------------
// Dependencies.

/// A dependency given, or `{ workspace = true, .. }`.
#[derive(Debug, Clone)]
pub enum InheritableDependency {
    Value(TomlDependency),
    Inherit(TomlInheritedDependency),
}

impl InheritableDependency {
    fn read<'n, N: Node<'n>>(n: N) -> Read<InheritableDependency> {
        if let Shape::Map = n.shape()
            && let Some(w) = n.get("workspace")
            && let Shape::Bool(workspace) = w.shape()
        {
            if !workspace {
                return Err(SchemaError::at(w, SchemaWhat::WorkspaceFalse));
            }
            return TomlInheritedDependency::read(n).map(InheritableDependency::Inherit);
        }
        TomlDependency::read(n).map(InheritableDependency::Value)
    }

    pub fn unused_keys(&self) -> Vec<String> {
        match self {
            InheritableDependency::Value(d) => d.unused_keys(),
            InheritableDependency::Inherit(w) => w._unused_keys.keys().cloned().collect(),
        }
    }

    pub fn normalized(&self) -> Result<&TomlDependency, UnresolvedError> {
        match self {
            InheritableDependency::Value(d) => Ok(d),
            InheritableDependency::Inherit(_) => Err(UnresolvedError),
        }
    }

    pub fn is_inherited(&self) -> bool {
        matches!(self, InheritableDependency::Inherit(_))
    }
}

/// `dep = { workspace = true, .. }`: what a member adds to the workspace's dependency.
#[derive(Debug, Clone, Default)]
pub struct TomlInheritedDependency {
    pub workspace: bool,
    pub features: Option<Vec<String>>,
    pub default_features: Option<bool>,
    pub default_features2: Option<bool>,
    pub optional: Option<bool>,
    pub public: Option<bool>,
    pub _unused_keys: BTreeMap<String, Value<Global>>,
}

impl TomlInheritedDependency {
    fn read<'n, N: Node<'n>>(n: N) -> Read<TomlInheritedDependency> {
        let mut d = TomlInheritedDependency::default();
        for (key, v) in n.entries() {
            match key {
                "workspace" => d.workspace = boolean(v)?,
                "features" => d.features = Some(strings(v)?),
                "default-features" => d.default_features = Some(boolean(v)?),
                "default_features" => d.default_features2 = Some(boolean(v)?),
                "optional" => d.optional = Some(boolean(v)?),
                "public" => d.public = Some(boolean(v)?),
                _ => {
                    d._unused_keys.insert(key.to_owned(), v.to_value());
                }
            }
        }
        Ok(d)
    }

    pub fn default_features(&self) -> Option<bool> {
        self.default_features.or(self.default_features2)
    }
}

/// A dependency: a version requirement alone, or a table.
#[derive(Debug, Clone)]
pub enum TomlDependency<P: Clone = String> {
    Simple(String),
    Detailed(TomlDetailedDependency<P>),
}

impl TomlDependency {
    fn read<'n, N: Node<'n>>(n: N) -> Read<TomlDependency> {
        match n.shape() {
            Shape::Str(s) => Ok(TomlDependency::Simple(s.to_owned())),
            Shape::Bool(b) => Err(SchemaError::at(n, SchemaWhat::DependencyBool(b))),
            Shape::Map => TomlDetailedDependency::read(n).map(TomlDependency::Detailed),
            _ => Err(invalid(n, DEPENDENCY)),
        }
    }
}

impl<P: Clone> TomlDependency<P> {
    pub fn is_version_specified(&self) -> bool {
        match self {
            TomlDependency::Detailed(d) => d.version.is_some(),
            TomlDependency::Simple(..) => true,
        }
    }

    pub fn is_optional(&self) -> bool {
        match self {
            TomlDependency::Detailed(d) => d.optional.unwrap_or(false),
            TomlDependency::Simple(..) => false,
        }
    }

    pub fn is_public(&self) -> bool {
        match self {
            TomlDependency::Detailed(d) => d.public.unwrap_or(false),
            TomlDependency::Simple(..) => false,
        }
    }

    pub fn default_features(&self) -> Option<bool> {
        match self {
            TomlDependency::Detailed(d) => d.default_features(),
            TomlDependency::Simple(..) => None,
        }
    }

    pub fn unused_keys(&self) -> Vec<String> {
        match self {
            TomlDependency::Simple(_) => vec![],
            TomlDependency::Detailed(d) => d._unused_keys.keys().cloned().collect(),
        }
    }
}

/// `dep = { version = "..", path = "..", .. }`.
#[derive(Debug, Clone)]
pub struct TomlDetailedDependency<P: Clone = String> {
    pub version: Option<String>,
    pub registry: Option<RegistryName>,
    pub registry_index: Option<String>,
    pub path: Option<P>,
    pub base: Option<PathBaseName>,
    pub git: Option<String>,
    pub branch: Option<String>,
    pub tag: Option<String>,
    pub rev: Option<String>,
    pub features: Option<Vec<String>>,
    pub optional: Option<bool>,
    pub default_features: Option<bool>,
    pub default_features2: Option<bool>,
    pub package: Option<PackageName>,
    pub public: Option<bool>,
    pub artifact: Option<StringOrVec>,
    pub lib: Option<bool>,
    pub target: Option<String>,
    pub _unused_keys: BTreeMap<String, Value<Global>>,
}

impl<P: Clone> Default for TomlDetailedDependency<P> {
    fn default() -> Self {
        TomlDetailedDependency {
            version: None,
            registry: None,
            registry_index: None,
            path: None,
            base: None,
            git: None,
            branch: None,
            tag: None,
            rev: None,
            features: None,
            optional: None,
            default_features: None,
            default_features2: None,
            package: None,
            public: None,
            artifact: None,
            lib: None,
            target: None,
            _unused_keys: BTreeMap::new(),
        }
    }
}

impl TomlDetailedDependency {
    fn read<'n, N: Node<'n>>(n: N) -> Read<TomlDetailedDependency> {
        let mut d = TomlDetailedDependency::default();
        for (key, v) in n.entries() {
            match key {
                "version" => d.version = Some(string(v)?),
                "registry" => {
                    d.registry = Some(
                        RegistryName::new(string(v)?)
                            .map_err(|e| SchemaError::at(v, SchemaWhat::Resolve(e)))?,
                    )
                }
                "registry-index" => d.registry_index = Some(string(v)?),
                "path" => d.path = Some(string(v)?),
                "base" => {
                    d.base = Some(
                        PathBaseName::new(string(v)?)
                            .map_err(|e| SchemaError::at(v, SchemaWhat::Resolve(e)))?,
                    )
                }
                "git" => d.git = Some(string(v)?),
                "branch" => d.branch = Some(string(v)?),
                "tag" => d.tag = Some(string(v)?),
                "rev" => d.rev = Some(string(v)?),
                "features" => d.features = Some(strings(v)?),
                "optional" => d.optional = Some(boolean(v)?),
                "default-features" => d.default_features = Some(boolean(v)?),
                "default_features" => d.default_features2 = Some(boolean(v)?),
                "package" => d.package = Some(package_name_value(v)?),
                "public" => d.public = Some(boolean(v)?),
                "artifact" => d.artifact = Some(StringOrVec::read(v)?),
                "lib" => d.lib = Some(boolean(v)?),
                "target" => d.target = Some(string(v)?),
                _ => {
                    d._unused_keys.insert(key.to_owned(), v.to_value());
                }
            }
        }
        Ok(d)
    }
}

impl<P: Clone> TomlDetailedDependency<P> {
    pub fn default_features(&self) -> Option<bool> {
        self.default_features.or(self.default_features2)
    }
}

/// `[target.<platform>]`.
#[derive(Debug, Default, Clone)]
pub struct TomlPlatform {
    pub dependencies: Option<BTreeMap<PackageName, InheritableDependency>>,
    pub build_dependencies: Option<BTreeMap<PackageName, InheritableDependency>>,
    pub build_dependencies2: Option<BTreeMap<PackageName, InheritableDependency>>,
    pub dev_dependencies: Option<BTreeMap<PackageName, InheritableDependency>>,
    pub dev_dependencies2: Option<BTreeMap<PackageName, InheritableDependency>>,
}

impl TomlPlatform {
    fn read<'n, N: Node<'n>>(n: N) -> Read<TomlPlatform> {
        let mut p = TomlPlatform::default();
        for (key, v) in table(n, "struct TomlPlatform")?.entries() {
            match key {
                "dependencies" => p.dependencies = Some(inheritable_dependencies(v)?),
                "build-dependencies" => p.build_dependencies = Some(inheritable_dependencies(v)?),
                "build_dependencies" => {
                    p.build_dependencies2 = Some(inheritable_dependencies(v)?)
                }
                "dev-dependencies" => p.dev_dependencies = Some(inheritable_dependencies(v)?),
                "dev_dependencies" => p.dev_dependencies2 = Some(inheritable_dependencies(v)?),
                _ => {}
            }
        }
        Ok(p)
    }

    pub fn dev_dependencies(&self) -> Option<&BTreeMap<PackageName, InheritableDependency>> {
        self.dev_dependencies.as_ref().or(self.dev_dependencies2.as_ref())
    }

    pub fn build_dependencies(&self) -> Option<&BTreeMap<PackageName, InheritableDependency>> {
        self.build_dependencies.as_ref().or(self.build_dependencies2.as_ref())
    }
}

// ---------------------------------------------------------------------------------------------
// Profiles.

/// `[profile]`: each profile by its name.
#[derive(Debug, Default, Clone)]
pub struct TomlProfiles(pub BTreeMap<ProfileName, TomlProfile>);

impl TomlProfiles {
    pub fn get_all(&self) -> &BTreeMap<ProfileName, TomlProfile> {
        &self.0
    }

    pub fn get(&self, name: &str) -> Option<&TomlProfile> {
        self.0.get(name)
    }
}

/// `[profile.<name>]`, also as the configuration's `profile.<name>` gives it.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TomlProfile {
    pub opt_level: Option<TomlOptLevel>,
    pub lto: Option<StringOrBool>,
    pub codegen_backend: Option<String>,
    pub codegen_units: Option<u32>,
    pub debug: Option<TomlDebugInfo>,
    pub split_debuginfo: Option<String>,
    pub debug_assertions: Option<bool>,
    pub rpath: Option<bool>,
    pub panic: Option<String>,
    pub overflow_checks: Option<bool>,
    pub incremental: Option<bool>,
    pub dir_name: Option<String>,
    pub inherits: Option<String>,
    pub strip: Option<StringOrBool>,
    pub rustflags: Option<Vec<String>>,
    pub package: Option<BTreeMap<ProfilePackageSpec, TomlProfile>>,
    pub build_override: Option<Box<TomlProfile>>,
    pub trim_paths: Option<TomlTrimPaths>,
    pub hint_mostly_unused: Option<bool>,
}

impl TomlProfile {
    /// Reads a profile's table; public so that the configuration reads `profile.<name>` with
    /// the same reader.
    pub fn read<'n, N: Node<'n>>(n: N) -> Read<TomlProfile> {
        let mut p = TomlProfile::default();
        for (key, v) in table(n, "struct TomlProfile")?.entries() {
            match key {
                "opt-level" => p.opt_level = Some(TomlOptLevel::read(v)?),
                "lto" => p.lto = Some(StringOrBool::read(v)?),
                "codegen-backend" => p.codegen_backend = Some(string(v)?),
                "codegen-units" => p.codegen_units = Some(int_in(v, "u32")?),
                "debug" => p.debug = Some(TomlDebugInfo::read(v)?),
                "split-debuginfo" => p.split_debuginfo = Some(string(v)?),
                "debug-assertions" => p.debug_assertions = Some(boolean(v)?),
                "rpath" => p.rpath = Some(boolean(v)?),
                "panic" => p.panic = Some(string(v)?),
                "overflow-checks" => p.overflow_checks = Some(boolean(v)?),
                "incremental" => p.incremental = Some(boolean(v)?),
                "dir-name" => p.dir_name = Some(string(v)?),
                "inherits" => p.inherits = Some(string(v)?),
                "strip" => p.strip = Some(StringOrBool::read(v)?),
                "rustflags" => p.rustflags = Some(strings(v)?),
                "package" => {
                    p.package = Some(map_of(v, ProfilePackageSpec::read_key, TomlProfile::read)?)
                }
                "build-override" => p.build_override = Some(Box::new(TomlProfile::read(v)?)),
                "trim-paths" => p.trim_paths = Some(TomlTrimPaths::read(v)?),
                "hint-mostly-unused" => p.hint_mostly_unused = Some(boolean(v)?),
                _ => {}
            }
        }
        Ok(p)
    }

    /// Overwrites this profile's values with `profile`'s, where it has them.
    pub fn merge(&mut self, profile: &TomlProfile) {
        fn take<T: Clone>(to: &mut Option<T>, from: &Option<T>) {
            if let Some(v) = from {
                *to = Some(v.clone());
            }
        }
        take(&mut self.opt_level, &profile.opt_level);
        take(&mut self.lto, &profile.lto);
        take(&mut self.codegen_backend, &profile.codegen_backend);
        take(&mut self.codegen_units, &profile.codegen_units);
        take(&mut self.debug, &profile.debug);
        take(&mut self.debug_assertions, &profile.debug_assertions);
        take(&mut self.split_debuginfo, &profile.split_debuginfo);
        take(&mut self.rpath, &profile.rpath);
        take(&mut self.panic, &profile.panic);
        take(&mut self.overflow_checks, &profile.overflow_checks);
        take(&mut self.incremental, &profile.incremental);
        take(&mut self.rustflags, &profile.rustflags);
        if let Some(other_package) = &profile.package {
            match &mut self.package {
                Some(self_package) => {
                    for (spec, other) in other_package {
                        match self_package.get_mut(spec) {
                            Some(p) => p.merge(other),
                            None => {
                                self_package.insert(spec.clone(), other.clone());
                            }
                        }
                    }
                }
                None => self.package = Some(other_package.clone()),
            }
        }
        if let Some(other) = &profile.build_override {
            match &mut self.build_override {
                Some(own) => own.merge(other),
                None => self.build_override = Some(other.clone()),
            }
        }
        take(&mut self.inherits, &profile.inherits);
        take(&mut self.dir_name, &profile.dir_name);
        take(&mut self.strip, &profile.strip);
        take(&mut self.trim_paths, &profile.trim_paths);
        take(&mut self.hint_mostly_unused, &profile.hint_mostly_unused);
    }
}

/// A key of `[profile.<name>.package]`: a package id spec, or `*`.
#[derive(Clone, Debug, PartialEq, Eq, Ord, PartialOrd, Hash)]
pub enum ProfilePackageSpec {
    Spec(PackageIdSpec),
    All,
}

impl ProfilePackageSpec {
    fn read_key<'n, N: Node<'n>>(key: &'n str, n: N) -> Read<ProfilePackageSpec> {
        if key == "*" {
            return Ok(ProfilePackageSpec::All);
        }
        PackageIdSpec::parse(key)
            .map(ProfilePackageSpec::Spec)
            .map_err(|e| SchemaError::at_key(n, SchemaWhat::Resolve(e)))
    }
}

impl fmt::Display for ProfilePackageSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProfilePackageSpec::Spec(spec) => spec.fmt(f),
            ProfilePackageSpec::All => f.write_str("*"),
        }
    }
}

impl serde::Serialize for ProfilePackageSpec {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

/// `opt-level`: `0` to `3`, `"s"` or `"z"`, kept as written.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TomlOptLevel(pub String);

impl TomlOptLevel {
    fn read<'n, N: Node<'n>>(n: N) -> Read<TomlOptLevel> {
        match n.shape() {
            Shape::Int(i) => Ok(TomlOptLevel(i.to_string())),
            Shape::Str(s) if s == "s" || s == "z" => Ok(TomlOptLevel(s.to_owned())),
            Shape::Str(s) => Err(SchemaError::at(n, SchemaWhat::OptLevel(s.to_owned()))),
            _ => Err(invalid(n, "an optimization level")),
        }
    }
}

impl serde::Serialize for TomlOptLevel {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self.0.parse::<u32>() {
            Ok(n) => s.serialize_u32(n),
            Err(_) => s.serialize_str(&self.0),
        }
    }
}

/// `debug`.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, PartialOrd, Ord)]
pub enum TomlDebugInfo {
    None,
    LineDirectivesOnly,
    LineTablesOnly,
    Limited,
    Full,
}

impl TomlDebugInfo {
    fn read<'n, N: Node<'n>>(n: N) -> Read<TomlDebugInfo> {
        let bad = |found| SchemaError::at(n, SchemaWhat::InvalidValue { found, expected: DEBUG });
        match n.shape() {
            Shape::Bool(true) => Ok(TomlDebugInfo::Full),
            Shape::Bool(false) => Ok(TomlDebugInfo::None),
            Shape::Int(0) => Ok(TomlDebugInfo::None),
            Shape::Int(1) => Ok(TomlDebugInfo::Limited),
            Shape::Int(2) => Ok(TomlDebugInfo::Full),
            Shape::Int(i) => Err(bad(Found::Int(i))),
            Shape::Str("none") => Ok(TomlDebugInfo::None),
            Shape::Str("limited") => Ok(TomlDebugInfo::Limited),
            Shape::Str("full") => Ok(TomlDebugInfo::Full),
            Shape::Str("line-directives-only") => Ok(TomlDebugInfo::LineDirectivesOnly),
            Shape::Str("line-tables-only") => Ok(TomlDebugInfo::LineTablesOnly),
            Shape::Str(s) => Err(bad(Found::Str(s.to_owned()))),
            _ => Err(invalid(n, DEBUG)),
        }
    }
}

impl fmt::Display for TomlDebugInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            TomlDebugInfo::None => "0",
            TomlDebugInfo::Limited => "1",
            TomlDebugInfo::Full => "2",
            TomlDebugInfo::LineDirectivesOnly => "line-directives-only",
            TomlDebugInfo::LineTablesOnly => "line-tables-only",
        })
    }
}

impl serde::Serialize for TomlDebugInfo {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            TomlDebugInfo::None => s.serialize_i32(0),
            TomlDebugInfo::Limited => s.serialize_i32(1),
            TomlDebugInfo::Full => s.serialize_i32(2),
            TomlDebugInfo::LineDirectivesOnly => s.serialize_str("line-directives-only"),
            TomlDebugInfo::LineTablesOnly => s.serialize_str("line-tables-only"),
        }
    }
}

/// `trim-paths`.
#[derive(Clone, Debug, PartialEq, Eq, Ord, PartialOrd, Hash)]
pub enum TomlTrimPaths {
    Values(Vec<TomlTrimPathsValue>),
    All,
}

impl TomlTrimPaths {
    pub fn none() -> TomlTrimPaths {
        TomlTrimPaths::Values(Vec::new())
    }

    pub fn is_none(&self) -> bool {
        match self {
            TomlTrimPaths::Values(v) => v.is_empty(),
            TomlTrimPaths::All => false,
        }
    }

    fn read<'n, N: Node<'n>>(n: N) -> Read<TomlTrimPaths> {
        match n.shape() {
            Shape::Bool(true) => Ok(TomlTrimPaths::All),
            Shape::Bool(false) => Ok(TomlTrimPaths::none()),
            Shape::Str("none") => Ok(TomlTrimPaths::none()),
            Shape::Str("all") => Ok(TomlTrimPaths::All),
            Shape::Str(s) => match TomlTrimPathsValue::of(s) {
                Some(v) => Ok(TomlTrimPaths::Values(vec![v])),
                None => Err(SchemaError::at(n, SchemaWhat::TrimPaths)),
            },
            Shape::Seq => {
                let mut values = Vec::new();
                for element in n.elements() {
                    let s = string(element)?;
                    match TomlTrimPathsValue::of(&s) {
                        Some(v) => values.push(v),
                        None => {
                            return Err(SchemaError::at(
                                element,
                                SchemaWhat::UnknownVariant {
                                    found: s,
                                    expected: TRIM_PATHS_VALUES,
                                },
                            ));
                        }
                    }
                }
                Ok(TomlTrimPaths::Values(values))
            }
            _ => Err(invalid(n, TRIM_PATHS)),
        }
    }
}

impl fmt::Display for TomlTrimPaths {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TomlTrimPaths::All => f.write_str("all"),
            TomlTrimPaths::Values(v) if v.is_empty() => f.write_str("none"),
            TomlTrimPaths::Values(v) => {
                for (i, value) in v.iter().enumerate() {
                    if i > 0 {
                        f.write_str(",")?;
                    }
                    f.write_str(value.as_str())?;
                }
                Ok(())
            }
        }
    }
}

impl serde::Serialize for TomlTrimPaths {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeSeq;
        match self {
            TomlTrimPaths::All => s.serialize_unit_variant("TomlTrimPaths", 1, "all"),
            TomlTrimPaths::Values(values) => {
                let mut seq = s.serialize_seq(Some(values.len()))?;
                for v in values {
                    seq.serialize_element(v.as_str())?;
                }
                seq.end()
            }
        }
    }
}

impl From<TomlTrimPathsValue> for TomlTrimPaths {
    fn from(value: TomlTrimPathsValue) -> TomlTrimPaths {
        TomlTrimPaths::Values(vec![value])
    }
}

impl From<Vec<TomlTrimPathsValue>> for TomlTrimPaths {
    fn from(value: Vec<TomlTrimPathsValue>) -> TomlTrimPaths {
        TomlTrimPaths::Values(value)
    }
}

/// One `trim-paths` option.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Ord, PartialOrd, Hash)]
pub enum TomlTrimPathsValue {
    Diagnostics,
    Macro,
    Object,
}

impl TomlTrimPathsValue {
    fn of(s: &str) -> Option<TomlTrimPathsValue> {
        match s {
            "diagnostics" => Some(TomlTrimPathsValue::Diagnostics),
            "macro" => Some(TomlTrimPathsValue::Macro),
            "object" => Some(TomlTrimPathsValue::Object),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            TomlTrimPathsValue::Diagnostics => "diagnostics",
            TomlTrimPathsValue::Macro => "macro",
            TomlTrimPathsValue::Object => "object",
        }
    }
}

impl fmt::Display for TomlTrimPathsValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl serde::Serialize for TomlTrimPathsValue {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------------------------
// Targets.

/// `[lib]`, `[[bin]]`, `[[example]]`, `[[test]]` or `[[bench]]`.
#[derive(Debug, Default, Clone)]
pub struct TomlTarget {
    pub name: Option<String>,
    pub crate_type: Option<Vec<String>>,
    pub crate_type2: Option<Vec<String>>,
    pub path: Option<PathValue>,
    pub filename: Option<String>,
    pub test: Option<bool>,
    pub doctest: Option<bool>,
    pub bench: Option<bool>,
    pub doc: Option<bool>,
    pub doc_scrape_examples: Option<bool>,
    pub proc_macro: Option<bool>,
    pub proc_macro2: Option<bool>,
    pub harness: Option<bool>,
    pub required_features: Option<Vec<String>>,
    pub edition: Option<String>,
}

pub type TomlLibTarget = TomlTarget;
pub type TomlBinTarget = TomlTarget;
pub type TomlExampleTarget = TomlTarget;
pub type TomlTestTarget = TomlTarget;
pub type TomlBenchTarget = TomlTarget;

impl TomlTarget {
    pub fn new() -> TomlTarget {
        TomlTarget::default()
    }

    fn read<'n, N: Node<'n>>(n: N) -> Read<TomlTarget> {
        let mut t = TomlTarget::default();
        for (key, v) in table(n, "struct TomlTarget")?.entries() {
            match key {
                "name" => t.name = Some(string(v)?),
                "crate-type" => t.crate_type = Some(strings(v)?),
                "crate_type" => t.crate_type2 = Some(strings(v)?),
                "path" => t.path = Some(PathValue(PathBuf::from(string(v)?))),
                "filename" => t.filename = Some(string(v)?),
                "test" => t.test = Some(boolean(v)?),
                "doctest" => t.doctest = Some(boolean(v)?),
                "bench" => t.bench = Some(boolean(v)?),
                "doc" => t.doc = Some(boolean(v)?),
                "doc-scrape-examples" => t.doc_scrape_examples = Some(boolean(v)?),
                "proc-macro" => t.proc_macro = Some(boolean(v)?),
                "proc_macro" => t.proc_macro2 = Some(boolean(v)?),
                "harness" => t.harness = Some(boolean(v)?),
                "required-features" => t.required_features = Some(strings(v)?),
                "edition" => t.edition = Some(string(v)?),
                _ => {}
            }
        }
        Ok(t)
    }

    pub fn proc_macro(&self) -> Option<bool> {
        self.proc_macro.or(self.proc_macro2).or_else(|| {
            self.crate_types()
                .is_some_and(|types| types.iter().any(|t| t == "proc-macro"))
                .then_some(true)
        })
    }

    pub fn crate_types(&self) -> Option<&Vec<String>> {
        self.crate_type.as_ref().or(self.crate_type2.as_ref())
    }
}

// ---------------------------------------------------------------------------------------------
// Lints and hints.

pub type TomlLints = BTreeMap<String, TomlToolLints>;
pub type TomlToolLints = BTreeMap<String, TomlLint>;

fn lints<'n, N: Node<'n>>(n: N) -> Read<TomlLints> {
    map_of(n, plain_key, |tool| map_of(tool, plain_key, TomlLint::read))
}

/// `[lints]`: the lints, or `workspace = true`.
#[derive(Debug, Default, Clone)]
pub struct InheritableLints {
    pub workspace: bool,
    pub lints: TomlLints,
}

impl InheritableLints {
    fn read<'n, N: Node<'n>>(n: N) -> Read<InheritableLints> {
        let mut out = InheritableLints::default();
        for (key, v) in table(n, "a lints table")?.entries() {
            if key == "workspace" {
                out.workspace = match boolean(v)? {
                    true => true,
                    false => return Err(SchemaError::at(v, SchemaWhat::WorkspaceFalse)),
                };
            } else {
                out.lints.insert(key.to_owned(), map_of(v, plain_key, TomlLint::read)?);
            }
        }
        Ok(out)
    }

    pub fn normalized(&self) -> Result<&TomlLints, UnresolvedError> {
        if self.workspace { Err(UnresolvedError) } else { Ok(&self.lints) }
    }
}

/// A lint's level, or a table with its level, priority and configuration.
#[derive(Debug, Clone)]
pub enum TomlLint {
    Level(TomlLintLevel),
    Config(TomlLintConfig),
}

impl TomlLint {
    fn read<'n, N: Node<'n>>(n: N) -> Read<TomlLint> {
        match n.shape() {
            Shape::Str(_) => TomlLintLevel::read(n).map(TomlLint::Level),
            Shape::Map => {
                let mut level = None;
                let mut priority = 0;
                let mut config = Map::new_in(Global);
                for (key, v) in n.entries() {
                    match key {
                        "level" => level = Some(TomlLintLevel::read(v)?),
                        "priority" => priority = int_in(v, "i8")?,
                        _ => {
                            config.insert(blaze_own::String::from_str_in(key, Global), v.to_value());
                        }
                    }
                }
                let level =
                    level.ok_or_else(|| SchemaError::at(n, SchemaWhat::MissingField("level")))?;
                Ok(TomlLint::Config(TomlLintConfig { level, priority, config }))
            }
            _ => Err(invalid(n, "a string or map")),
        }
    }

    pub fn level(&self) -> TomlLintLevel {
        match self {
            TomlLint::Level(level) => *level,
            TomlLint::Config(config) => config.level,
        }
    }

    pub fn priority(&self) -> i8 {
        match self {
            TomlLint::Level(_) => 0,
            TomlLint::Config(config) => config.priority,
        }
    }

    pub fn config(&self) -> Option<&Map<Global>> {
        match self {
            TomlLint::Level(_) => None,
            TomlLint::Config(config) => Some(&config.config),
        }
    }
}

#[derive(Debug, Clone)]
pub struct TomlLintConfig {
    pub level: TomlLintLevel,
    pub priority: i8,
    pub config: Map<Global>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TomlLintLevel {
    Forbid,
    Deny,
    Warn,
    Allow,
}

impl TomlLintLevel {
    fn read<'n, N: Node<'n>>(n: N) -> Read<TomlLintLevel> {
        match n.shape() {
            Shape::Str("forbid") => Ok(TomlLintLevel::Forbid),
            Shape::Str("deny") => Ok(TomlLintLevel::Deny),
            Shape::Str("warn") => Ok(TomlLintLevel::Warn),
            Shape::Str("allow") => Ok(TomlLintLevel::Allow),
            Shape::Str(s) => Err(SchemaError::at(
                n,
                SchemaWhat::UnknownVariant { found: s.to_owned(), expected: LINT_LEVELS },
            )),
            _ => Err(invalid(n, "enum TomlLintLevel")),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            TomlLintLevel::Forbid => "forbid",
            TomlLintLevel::Deny => "deny",
            TomlLintLevel::Warn => "warn",
            TomlLintLevel::Allow => "allow",
        }
    }
}

impl fmt::Display for TomlLintLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// `[hints]`.
#[derive(Debug, Default, Clone)]
pub struct Hints {
    pub mostly_unused: Option<Value<Global>>,
}

impl Hints {
    fn read<'n, N: Node<'n>>(n: N) -> Read<Hints> {
        let mut h = Hints::default();
        for (key, v) in table(n, "struct Hints")?.entries() {
            if key == "mostly-unused" {
                h.mostly_unused = Some(v.to_value());
            }
        }
        Ok(h)
    }
}

// ---------------------------------------------------------------------------------------------
// Small unions.

/// A string or a list of strings.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StringOrVec(pub Vec<String>);

impl StringOrVec {
    fn read<'n, N: Node<'n>>(n: N) -> Read<StringOrVec> {
        match n.shape() {
            Shape::Str(s) => Ok(StringOrVec(vec![s.to_owned()])),
            Shape::Seq => strings(n).map(StringOrVec),
            _ => Err(invalid(n, "string or list of strings")),
        }
    }

    pub fn iter(&self) -> std::slice::Iter<'_, String> {
        self.0.iter()
    }
}

impl<'a> IntoIterator for &'a StringOrVec {
    type Item = &'a String;
    type IntoIter = std::slice::Iter<'a, String>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

/// A string or a bool.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum StringOrBool {
    String(String),
    Bool(bool),
}

impl StringOrBool {
    fn read<'n, N: Node<'n>>(n: N) -> Read<StringOrBool> {
        match n.shape() {
            Shape::Bool(b) => Ok(StringOrBool::Bool(b)),
            Shape::Str(s) => Ok(StringOrBool::String(s.to_owned())),
            _ => Err(invalid(n, "a boolean or string")),
        }
    }
}

impl serde::Serialize for StringOrBool {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            StringOrBool::String(v) => s.serialize_str(v),
            StringOrBool::Bool(v) => s.serialize_bool(*v),
        }
    }
}

/// `package.build`: `false`, `true`, a script or several.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TomlPackageBuild {
    Auto(bool),
    SingleScript(String),
    MultipleScript(Vec<String>),
}

impl TomlPackageBuild {
    fn read<'n, N: Node<'n>>(n: N) -> Read<TomlPackageBuild> {
        match n.shape() {
            Shape::Bool(b) => Ok(TomlPackageBuild::Auto(b)),
            Shape::Str(s) => Ok(TomlPackageBuild::SingleScript(s.to_owned())),
            Shape::Seq => strings(n).map(TomlPackageBuild::MultipleScript),
            _ => Err(invalid(n, "a boolean, string or array")),
        }
    }
}

/// `publish`: a bool or the registries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VecStringOrBool {
    VecString(Vec<String>),
    Bool(bool),
}

impl VecStringOrBool {
    fn read<'n, N: Node<'n>>(n: N) -> Read<VecStringOrBool> {
        match n.shape() {
            Shape::Bool(b) => Ok(VecStringOrBool::Bool(b)),
            Shape::Seq => strings(n).map(VecStringOrBool::VecString),
            _ => Err(invalid(n, "a boolean or vector of strings")),
        }
    }
}

/// A target's `path`, as written.
#[derive(Clone, PartialEq, Eq)]
pub struct PathValue(pub PathBuf);

impl fmt::Debug for PathValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl serde::Serialize for PathValue {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use blaze_toml::Document;

    fn read(text: &str) -> Read<TomlManifest> {
        let doc = Document::parse_in(text, blaze_toml::Limits::CARGO, Global).unwrap();
        TomlManifest::read(doc.root())
    }

    #[test]
    fn an_integer_version_is_cargos_error_at_the_value() {
        let text = "[package]\nname = \"a\"\nversion = 1\n";
        let error = read(text).unwrap_err();
        assert_eq!(error.to_string(), "invalid type: integer `1`, expected SemVer version");
        assert_eq!(error.key, "package.version");
        assert_eq!(&text[error.span.clone()], "1");
    }

    #[test]
    fn both_spellings_of_dev_dependencies_are_read() {
        let m = read("[dev-dependencies]\na = \"1\"\n[dev_dependencies]\nb = \"1\"\n").unwrap();
        assert!(m.dev_dependencies.unwrap().contains_key("a"));
        assert!(m.dev_dependencies2.unwrap().contains_key("b"));
    }

    #[test]
    fn a_dependency_inherits_with_workspace_true_and_refuses_false() {
        let m = read("[dependencies]\na = { workspace = true, features = [\"x\"] }\n").unwrap();
        assert!(m.dependencies.unwrap()["a"].is_inherited());
        let error = read("[dependencies]\na = { workspace = false }\n").unwrap_err();
        assert_eq!(error.to_string(), "`workspace` cannot be false");
    }

    #[test]
    fn metadata_keeps_manifest_order() {
        let m = read("[package]\nname = \"a\"\n[package.metadata]\nz = 1\na = 2\n").unwrap();
        let metadata = m.package.unwrap().metadata.unwrap();
        let keys: Vec<&str> = metadata.as_map().unwrap().iter().map(|(k, _)| k).collect();
        assert_eq!(keys, ["z", "a"]);
    }
}
