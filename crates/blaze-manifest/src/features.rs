//! Editions, resolver versions and `cargo-features`, as the cargo of the pinned nightly
//! (`harness/toolchain`) knows them (`src/workspace/features.rs` and `src/resolver/types.rs`
//! there). blaze builds with that nightly only, so its channel is `nightly`: a listed unstable
//! feature is activated, as nightly cargo activates it.

use std::fmt;
use std::str::FromStr;

use anyhow::bail;
use semver::Version;

/// The cargo whose behaviour this crate has, the pinned nightly's, in the form cargo prints
/// itself (`cargo -vV`; commit cad07cf8e597397fff5d0f6cb79c9f870c0b1dd2).
pub const CARGO_VERSION: &str = "1.101.0-nightly (cad07cf8e 2026-10-23)";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Edition {
    #[default]
    Edition2015,
    Edition2018,
    Edition2021,
    Edition2024,
    EditionFuture,
}

impl Edition {
    pub const LATEST_STABLE: Edition = Edition::Edition2024;
    pub const ALL: &'static [Edition] = &[
        Edition::Edition2015,
        Edition::Edition2018,
        Edition::Edition2021,
        Edition::Edition2024,
        Edition::EditionFuture,
    ];

    /// The release that stabilized it.
    pub fn first_version(&self) -> Option<Version> {
        match self {
            Edition::Edition2015 => None,
            Edition::Edition2018 => Some(Version::new(1, 31, 0)),
            Edition::Edition2021 => Some(Version::new(1, 56, 0)),
            Edition::Edition2024 => Some(Version::new(1, 85, 0)),
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
            Edition::EditionFuture => "future",
        })
    }
}

impl FromStr for Edition {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> anyhow::Result<Edition> {
        match s {
            "2015" => Ok(Edition::Edition2015),
            "2018" => Ok(Edition::Edition2018),
            "2021" => Ok(Edition::Edition2021),
            "2024" => Ok(Edition::Edition2024),
            "future" => Ok(Edition::EditionFuture),
            s if s.parse().is_ok_and(|y: u16| y > 2024 && y < 2050) => bail!(
                "this version of Cargo is older than the `{s}` edition, \
                 and only supports `2015`, `2018`, `2021`, and `2024` editions."
            ),
            s => bail!(
                "supported edition values are `2015`, `2018`, `2021`, or `2024`, \
                 but `{s}` is unknown"
            ),
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
    ) -> anyhow::Result<Features> {
        let mut ret = Features { activated: Vec::new(), enabled: Vec::new(), is_local };
        for name in features {
            ret.add(name, warnings)?;
            ret.activated.push(name.clone());
        }
        Ok(ret)
    }

    fn add(&mut self, name: &str, warnings: &mut Vec<String>) -> anyhow::Result<()> {
        let Some(feature) = status(name) else {
            let mut msg = format!("unknown Cargo.toml feature `{name}`\n\n");
            if name.contains('_') {
                msg.push_str("Feature names must use '-' instead of '_'.\n");
            } else {
                if CLI_UNSTABLE.contains(&name.replace('-', "_").as_str()) {
                    msg.push_str(&format!(
                        "This feature can be enabled via -Z{name} or the `[unstable]` section \
                         in config.toml.\n"
                    ));
                }
                msg.push_str(
                    "See https://doc.rust-lang.org/nightly/cargo/reference/unstable.html for more information.\n",
                );
            }
            bail!(msg)
        };
        if self.enabled.contains(&feature.name) {
            bail!("the cargo feature `{name}` has already been activated");
        }
        let see_docs = format!(
            "See {} for more information about using this feature.",
            cargo_docs_link(feature.docs)
        );
        match feature.status {
            Status::Stable => warnings.push(format!(
                "the cargo feature `{name}` has been stabilized in the {} release and is no \
                 longer necessary to be listed in the manifest\n  {see_docs}",
                feature.version
            )),
            Status::Unstable => {}
            Status::Removed => {
                let mut msg = format!(
                    "the cargo feature `{name}` has been removed in the {} release\n\n",
                    feature.version
                );
                if self.is_local {
                    msg.push_str("Remove the feature from Cargo.toml to remove this error.\n");
                } else {
                    msg.push_str(&format!(
                        "This package cannot be used with this version of Cargo, as the \
                         unstable feature `{name}` is no longer supported.\n"
                    ));
                }
                msg.push_str(&see_docs);
                msg.push('\n');
                bail!(msg)
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
    pub fn require(&self, feature: &Feature) -> anyhow::Result<()> {
        self.require_with_hint(feature, None)
    }

    pub fn require_with_hint(&self, feature: &Feature, hint: Option<&str>) -> anyhow::Result<()> {
        if self.is_enabled(feature) {
            return Ok(());
        }
        let name = feature.name.replace('_', "-");
        let mut msg = format!(
            "feature `{name}` is required\n\n\
             The package requires the Cargo feature called `{name}`, but that feature is not \
             stabilized in this version of Cargo ({CARGO_VERSION}).\n"
        );
        if self.is_local {
            msg.push_str(&format!(
                "Consider adding `cargo-features = [\"{name}\"]` to the top of Cargo.toml \
                 (above the [package] table) to tell Cargo you are opting in to use this \
                 unstable feature.\n"
            ));
        } else {
            msg.push_str("Consider trying a more recent nightly release.\n");
        }
        msg.push_str(&format!(
            "See https://doc.rust-lang.org/nightly/cargo/{} for more information about the status of this feature.\n",
            feature.docs
        ));
        if let Some(hint) = hint {
            msg.push_str(hint);
            msg.push('\n');
        }
        bail!("{msg}")
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
