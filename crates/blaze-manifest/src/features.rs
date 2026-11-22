//! Editions, resolver versions and `cargo-features`, as the cargo of the pinned nightly
//! (`pins/toolchain`) knows them (`src/workspace/features.rs` and `src/resolver/types.rs`
//! there). blaze builds with that nightly only, so its channel is `nightly`: a listed unstable
//! feature is activated, as nightly cargo activates it.

use std::alloc::Global;
use std::fmt;
use std::str::FromStr;

use blaze_semver::Version;

/// What an `edition` or a `cargo-features` entry refuses, in cargo's words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeatureError {
    /// `blaze-..` that names no blaze edition.
    NoBlazeEdition { written: String },
    /// A blaze edition this blaze does not know.
    UnknownBlazeEdition(edition::UnknownBlazeEdition),
    /// A year after 2024 that this cargo does not know yet.
    NewerEdition { written: String },
    /// Any other edition value.
    UnknownEdition { written: String },
    /// A `cargo-features` entry naming no feature; `cli` when a `-Z` flag of that name exists.
    UnknownFeature { name: String, cli: bool },
    /// The same `cargo-features` entry twice.
    AlreadyActivated { name: String },
    /// A feature cargo removed.
    Removed { name: String, version: &'static str, is_local: bool, docs: &'static str },
    /// An unstable feature the manifest did not list.
    Required { name: String, is_local: bool, docs: &'static str, hint: Option<String> },
}

impl fmt::Display for FeatureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FeatureError::NoBlazeEdition { written } => {
                write!(f, "`{written}` is no blaze edition")
            }
            FeatureError::UnknownBlazeEdition(unknown) => write!(f, "{unknown}"),
            FeatureError::NewerEdition { written } => write!(
                f,
                "this version of Cargo is older than the `{written}` edition, \
                 and only supports `2015`, `2018`, `2021`, and `2024` editions."
            ),
            FeatureError::UnknownEdition { written } => write!(
                f,
                "supported edition values are `2015`, `2018`, `2021`, or `2024`, \
                 but `{written}` is unknown"
            ),
            FeatureError::UnknownFeature { name, cli } => {
                write!(f, "unknown Cargo.toml feature `{name}`\n\n")?;
                if name.contains('_') {
                    f.write_str("Feature names must use '-' instead of '_'.\n")
                } else {
                    if *cli {
                        write!(
                            f,
                            "This feature can be enabled via -Z{name} or the `[unstable]` \
                             section in config.toml.\n"
                        )?;
                    }
                    f.write_str(
                        "See https://doc.rust-lang.org/nightly/cargo/reference/unstable.html for more information.\n",
                    )
                }
            }
            FeatureError::AlreadyActivated { name } => {
                write!(f, "the cargo feature `{name}` has already been activated")
            }
            FeatureError::Removed { name, version, is_local, docs } => {
                write!(f, "the cargo feature `{name}` has been removed in the {version} release\n\n")?;
                if *is_local {
                    f.write_str("Remove the feature from Cargo.toml to remove this error.\n")?;
                } else {
                    write!(
                        f,
                        "This package cannot be used with this version of Cargo, as the \
                         unstable feature `{name}` is no longer supported.\n"
                    )?;
                }
                writeln!(f, "{}", see_docs(docs))
            }
            FeatureError::Required { name, is_local, docs, hint } => {
                write!(
                    f,
                    "feature `{name}` is required\n\n\
                     The package requires the Cargo feature called `{name}`, but that feature is \
                     not stabilized in this version of Cargo ({}).\n",
                    blaze_pins::CARGO_VERSION
                )?;
                if *is_local {
                    write!(
                        f,
                        "Consider adding `cargo-features = [\"{name}\"]` to the top of Cargo.toml \
                         (above the [package] table) to tell Cargo you are opting in to use this \
                         unstable feature.\n"
                    )?;
                } else {
                    f.write_str("Consider trying a more recent nightly release.\n")?;
                }
                write!(
                    f,
                    "See https://doc.rust-lang.org/nightly/cargo/{docs} for more information about the status of this feature.\n"
                )?;
                if let Some(hint) = hint {
                    writeln!(f, "{hint}")?;
                }
                Ok(())
            }
        }
    }
}

impl core::error::Error for FeatureError {}

fn see_docs(docs: &str) -> String {
    format!("See {} for more information about using this feature.", cargo_docs_link(docs))
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Edition {
    #[default]
    Edition2015,
    Edition2018,
    Edition2021,
    Edition2024,
    /// A blaze edition (`edition = "blaze-2026.11"`; docs/v2/plan/design/std.md, "The blaze
    /// edition: opting in"): cargo's 2024 edition for everything a manifest's edition decides
    /// (the resolver, the keys refused from 2024 on, target discovery), which is why it
    /// orders after it, and [`Edition::blaze`] for what blaze adds. No cargo reads it: a
    /// package of it is built by blaze alone, and that is said at the key cargo stops at.
    Blaze2026_11,
    EditionFuture,
}

impl Edition {
    /// The blaze edition this is, if it is one: its one conversion to the front end's
    /// [`edition::BlazeEdition`], whose spellings it shares.
    pub fn blaze(&self) -> Option<edition::BlazeEdition> {
        match self {
            Edition::Blaze2026_11 => Some(edition::BlazeEdition::Blaze2026_11),
            Edition::Edition2015
            | Edition::Edition2018
            | Edition::Edition2021
            | Edition::Edition2024
            | Edition::EditionFuture => None,
        }
    }

    /// The Rust edition its sources are written in, as rustc's and rustfmt's `--edition`
    /// take it: itself, or for a blaze edition the Rust edition it fixes
    /// ([`edition::BlazeEdition::rust`]). What a formatter of Rust's syntax is handed.
    pub fn rust_syntax(&self) -> String {
        match self.blaze() {
            Some(blaze) => blaze.rust().to_string(),
            None => self.to_string(),
        }
    }

    /// The manifest's edition of a blaze edition.
    pub fn of_blaze(blaze: edition::BlazeEdition) -> Edition {
        match blaze {
            edition::BlazeEdition::Blaze2026_11 => Edition::Blaze2026_11,
        }
    }

    pub const LATEST_STABLE: Edition = Edition::Edition2024;
    pub const ALL: &'static [Edition] = &[
        Edition::Edition2015,
        Edition::Edition2018,
        Edition::Edition2021,
        Edition::Edition2024,
        Edition::EditionFuture,
    ];

    /// The release that stabilized it.
    pub fn first_version(&self) -> Option<Version<Global>> {
        match self {
            Edition::Edition2015 => None,
            Edition::Edition2018 => Some(Version::new(1, 31, 0)),
            Edition::Edition2021 => Some(Version::new(1, 56, 0)),
            Edition::Edition2024 => Some(Version::new(1, 85, 0)),
            // No release of Rust has it: `rust-version` says nothing about a blaze edition.
            Edition::Blaze2026_11 => None,
            Edition::EditionFuture => None,
        }
    }

    pub fn is_stable(&self) -> bool {
        *self != Edition::EditionFuture
    }

    pub fn default_resolve_behavior(&self) -> ResolveBehavior {
        if *self >= Edition::Edition2024 {
            ResolveBehavior::V3
        } else if *self >= Edition::Edition2021 {
            ResolveBehavior::V2
        } else {
            ResolveBehavior::V1
        }
    }
}

impl fmt::Display for Edition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Edition::Edition2015 => "2015",
            Edition::Edition2018 => "2018",
            Edition::Edition2021 => "2021",
            Edition::Edition2024 => "2024",
            Edition::Blaze2026_11 => edition::BlazeEdition::Blaze2026_11.manifest(),
            Edition::EditionFuture => "future",
        })
    }
}

impl FromStr for Edition {
    type Err = FeatureError;
    fn from_str(s: &str) -> Result<Edition, FeatureError> {
        let written = || s.to_owned();
        match s {
            "2015" => Ok(Edition::Edition2015),
            "2018" => Ok(Edition::Edition2018),
            "2021" => Ok(Edition::Edition2021),
            "2024" => Ok(Edition::Edition2024),
            "future" => Ok(Edition::EditionFuture),
            s if s.starts_with(edition::BlazeEdition::MANIFEST_PREFIX) => {
                match edition::BlazeEdition::from_manifest(s) {
                    Ok(Some(blaze)) => Ok(Edition::of_blaze(blaze)),
                    Ok(None) => Err(FeatureError::NoBlazeEdition { written: written() }),
                    Err(unknown) => Err(FeatureError::UnknownBlazeEdition(unknown)),
                }
            }
            s if s.parse().is_ok_and(|y: u16| y > 2024 && y < 2050) => {
                Err(FeatureError::NewerEdition { written: written() })
            }
            _ => Err(FeatureError::UnknownEdition { written: written() }),
        }
    }
}

pub use blaze_resolve::resolver::ResolveBehavior;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Status {
    Stable,
    Unstable,
    Removed,
}

/// A feature of `cargo-features`: its name in the manifest, status, the release that
/// stabilized or removed it, and the page of cargo's book about it.
#[derive(Debug)]
pub struct Feature {
    name: &'static str,
    status: Status,
    version: &'static str,
    docs: &'static str,
}

macro_rules! features {
    ($(($status:ident, $name:ident, $version:expr, $docs:expr),)*) => {
        const FEATURES: &[Feature] = &[
            $(Feature {
                name: stringify!($name),
                status: features!(@status $status),
                version: $version,
                docs: $docs,
            },)*
        ];

        #[allow(non_upper_case_globals)]
        impl Feature {
            $(pub const $name: &'static Feature = &Feature {
                name: stringify!($name),
                status: features!(@status $status),
                version: $version,
                docs: $docs,
            };)*
        }
    };
    (@status stable) => { Status::Stable };
    (@status unstable) => { Status::Unstable };
    (@status removed) => { Status::Removed };
}

features! {
    (stable, test_dummy_stable, "1.0", ""),
    (unstable, test_dummy_unstable, "", "reference/unstable.html"),
    (stable, alternative_registries, "1.34", "reference/registries.html"),
    (stable, edition, "1.31", "reference/manifest.html#the-edition-field"),
    (stable, rename_dependency, "1.31", "reference/specifying-dependencies.html#renaming-dependencies-in-cargotoml"),
    (removed, publish_lockfile, "1.37", "reference/unstable.html#publish-lockfile"),
    (stable, profile_overrides, "1.41", "reference/profiles.html#overrides"),
    (stable, default_run, "1.37", "reference/manifest.html#the-default-run-field"),
    (unstable, metabuild, "", "reference/unstable.html#metabuild"),
    (unstable, public_dependency, "", "reference/unstable.html#public-dependency"),
    (stable, named_profiles, "1.57", "reference/profiles.html#custom-profiles"),
    (stable, resolver, "1.51", "reference/resolver.html#resolver-versions"),
    (stable, strip, "1.58", "reference/profiles.html#strip-option"),
    (stable, rust_version, "1.56", "reference/manifest.html#the-rust-version-field"),
    (stable, edition2021, "1.56", "reference/manifest.html#the-edition-field"),
    (unstable, per_package_target, "", "reference/unstable.html#per-package-target"),
    (unstable, codegen_backend, "", "reference/unstable.html#codegen-backend"),
    (unstable, different_binary_name, "", "reference/unstable.html#different-binary-name"),
    (unstable, profile_rustflags, "", "reference/unstable.html#profile-rustflags-option"),
    (stable, workspace_inheritance, "1.64", "reference/unstable.html#workspace-inheritance"),
    (stable, edition2024, "1.85", "reference/manifest.html#the-edition-field"),
    (stable, trim_paths, "1.101", "reference/profiles.html#trim-paths"),
    (unstable, open_namespaces, "", "reference/unstable.html#open-namespaces"),
    (unstable, path_bases, "", "reference/unstable.html#path-bases"),
    (unstable, unstable_editions, "", "reference/unstable.html#unstable-editions"),
    (unstable, multiple_build_scripts, "", "reference/unstable.html#multiple-build-scripts"),
    (unstable, panic_immediate_abort, "", "reference/unstable.html#panic-immediate-abort"),
    (unstable, feature_metadata, "", "reference/unstable.html#feature_metadata"),
    (unstable, builtin_dependencies, "", "reference/unstable.html#builtin-dependencies"),
}

/// The names of nightly cargo's `-Z` flags (`CliUnstable::help`), for the hint an unknown
/// `cargo-features` entry gets when it names one of them.
const CLI_UNSTABLE: &[&str] = &[
    "allow_features",
    "embed_metadata",
    "print_im_a_teapot",
    "advanced_env",
    "any_build_script_metadata",
    "avoid_dev_deps",
    "binary_dep_depinfo",
    "bindeps",
    "build_analysis",
    "build_dir_new_layout",
    "build_std",
    "build_std_features",
    "checksum_freshness",
    "codegen_backend",
    "direct_minimal_versions",
    "dual_proc_macros",
    "feature_unification",
    "features",
    "fine_grain_locking",
    "fix_edition",
    "gc",
    "git",
    "gitoxide",
    "hint_min_opt_level",
    "hint_msrv",
    "host_config",
    "json_target_spec",
    "mem_stats",
    "minimal_versions",
    "msrv_policy",
    "mtime_on_use",
    "next_lockfile_bump",
    "no_index_update",
    "panic_abort_tests",
    "panic_immediate_abort",
    "prerelease",
    "profile_hint_mostly_unused",
    "profile_rustflags",
    "public_dependency",
    "publish_timeout",
    "root_dir",
    "rustc_unicode",
    "rustdoc_depinfo",
    "rustdoc_map",
    "rustdoc_mergeable_info",
    "rustdoc_scrape_examples",
    "sbom",
    "script",
    "section_timings",
    "separate_nightlies",
    "skip_rustdoc_fingerprint",
    "target_applies_to_host",
    "unstable_options",
];

/// The `cargo-features` of a manifest, as nightly cargo keeps them: each listed feature is
/// activated, unstable ones included, since blaze's channel is `nightly`. cargo's
/// `-Zallow-features`, which narrows that, is not read.
#[derive(Clone, Debug, Default)]
pub struct Features {
    /// The entries of `cargo-features`, as written.
    activated: Vec<String>,
    /// The features they name.
    enabled: Vec<&'static str>,
    /// Whether the manifest is a local package's.
    is_local: bool,
}

impl Features {
    pub fn new(
        features: &[String],
        warnings: &mut Vec<String>,
        is_local: bool,
    ) -> Result<Features, FeatureError> {
        let mut ret = Features { activated: Vec::new(), enabled: Vec::new(), is_local };
        for name in features {
            ret.add(name, warnings)?;
            ret.activated.push(name.clone());
        }
        Ok(ret)
    }

    fn add(&mut self, name: &str, warnings: &mut Vec<String>) -> Result<(), FeatureError> {
        let Some(feature) = status(name) else {
            let cli = CLI_UNSTABLE.contains(&name.replace('-', "_").as_str());
            return Err(FeatureError::UnknownFeature { name: name.to_owned(), cli });
        };
        if self.enabled.contains(&feature.name) {
            return Err(FeatureError::AlreadyActivated { name: name.to_owned() });
        }
        match feature.status {
            Status::Stable => warnings.push(format!(
                "the cargo feature `{name}` has been stabilized in the {} release and is no \
                 longer necessary to be listed in the manifest\n  {}",
                feature.version,
                see_docs(feature.docs)
            )),
            Status::Unstable => {}
            Status::Removed => {
                return Err(FeatureError::Removed {
                    name: name.to_owned(),
                    version: feature.version,
                    is_local: self.is_local,
                    docs: feature.docs,
                });
            }
        }
        self.enabled.push(feature.name);
        Ok(())
    }

    /// The entries of `cargo-features`, in their order.
    pub fn activated(&self) -> &[String] {
        &self.activated
    }

    /// Whether `feature` may be used: it is stable or the manifest activated it.
    pub fn is_enabled(&self, feature: &Feature) -> bool {
        feature.status == Status::Stable || self.enabled.contains(&feature.name)
    }

    /// Fails as nightly cargo does for a manifest that uses an unstable feature it did not
    /// list in `cargo-features`.
    pub fn require(&self, feature: &Feature) -> Result<(), FeatureError> {
        self.require_with_hint(feature, None)
    }

    pub fn require_with_hint(
        &self,
        feature: &Feature,
        hint: Option<&str>,
    ) -> Result<(), FeatureError> {
        if self.is_enabled(feature) {
            return Ok(());
        }
        Err(FeatureError::Required {
            name: feature.name.replace('_', "-"),
            is_local: self.is_local,
            docs: feature.docs,
            hint: hint.map(str::to_owned),
        })
    }
}

/// The feature a `cargo-features` entry names: spelled with `-`, never `_`.
fn status(name: &str) -> Option<&'static Feature> {
    if name.contains('_') {
        return None;
    }
    let name = name.replace('-', "_");
    FEATURES.iter().find(|f| f.name == name)
}

fn cargo_docs_link(path: &str) -> String {
    format!("https://doc.rust-lang.org/cargo/{path}")
}

#[cfg(test)]
mod blaze_edition_tests {
    use super::Edition;

    /// The opt-in is the manifest's `edition`: `blaze-2026.11` parses to the blaze edition,
    /// prints as it was written, and is cargo's 2024 for every `>=` a manifest rule asks.
    #[test]
    fn a_blaze_edition_parses_prints_and_orders_as_2024_or_later() {
        let edition: Edition = "blaze-2026.11".parse().unwrap();
        assert_eq!(edition, Edition::Blaze2026_11);
        assert_eq!(edition.to_string(), "blaze-2026.11");
        assert_eq!(edition.blaze(), Some(edition::BlazeEdition::Blaze2026_11));
        assert_eq!(Edition::of_blaze(edition::BlazeEdition::LATEST), Edition::Blaze2026_11);
        assert!(edition >= Edition::Edition2024, "the rules of 2024 hold in it");
        assert!(edition.is_stable(), "it needs no `cargo-features`");
        assert_eq!(edition.first_version(), None);
        assert_eq!(edition.rust_syntax(), "2024", "a formatter is handed the Rust edition");
        assert_eq!(Edition::Edition2021.rust_syntax(), "2021");
        assert_eq!(edition.default_resolve_behavior(), Edition::Edition2024.default_resolve_behavior());
    }

    /// A Rust edition is no blaze edition (the control: were `blaze` to say yes for one,
    /// every old package would be compiled as the blaze edition), and its text is unchanged.
    #[test]
    fn a_rust_edition_is_no_blaze_edition() {
        for (written, edition) in [
            ("2015", Edition::Edition2015),
            ("2018", Edition::Edition2018),
            ("2021", Edition::Edition2021),
            ("2024", Edition::Edition2024),
            ("future", Edition::EditionFuture),
        ] {
            assert_eq!(written.parse::<Edition>().unwrap(), edition);
            assert_eq!(edition.blaze(), None, "{written}");
            assert_eq!(edition.to_string(), written);
        }
        assert!(!Edition::ALL.contains(&Edition::Blaze2026_11), "cargo's editions are cargo's");
    }

    /// A `blaze-` value this blaze does not know is refused naming it and what is known,
    /// never read as some Rust edition.
    #[test]
    fn an_unknown_blaze_edition_is_refused_by_name() {
        for written in ["blaze-2026.12", "blaze-", "blaze-2024"] {
            let refused = written.parse::<Edition>().unwrap_err().to_string();
            assert!(refused.contains(written) && refused.contains("blaze-2026.11"), "{refused}");
        }
        let cargo = "2019".parse::<Edition>().unwrap_err().to_string();
        assert!(cargo.contains("supported edition values"), "cargo's message for cargo's values: {cargo}");
    }
}
