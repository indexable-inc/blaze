//! A dependency as a manifest declares it (cargo's `Dependency`), and the feature map of a
//! package (cargo's `build_feature_map`), with cargo's checks and messages.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context as _, bail};
use blaze_resolve::{FeatureDep, InternedString};
use cargo_platform::Platform;
use semver::VersionReq;
use serde::Serialize;

use crate::source::SourceId;

pub use blaze_resolve::DepKind;

#[derive(Clone, Debug)]
pub struct Dependency {
    /// The name of the package depended on, which is what `name_in_toml` renames.
    pub name: String,
    pub source_id: SourceId,
    /// The registry named by `registry` or `registry-index`, when one is.
    pub registry_id: Option<SourceId>,
    /// `None` is any version, cargo's `OptVersionReq::Any`.
    pub req: Option<VersionReq>,
    pub kind: DepKind,
    pub optional: bool,
    pub public: bool,
    pub features: Vec<String>,
    pub default_features: bool,
    pub platform: Option<Platform>,
    pub explicit_name_in_toml: Option<String>,
    /// `artifact = ..` with `-Z bindeps`.
    pub artifact: Option<Artifact>,
}

/// An artifact dependency's `artifact`, `lib` and `target`, as cargo's `Artifact::parse` checks
/// them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Artifact {
    /// As written: `bin`, `bin:<name>`, `cdylib`, `staticlib`.
    pub kinds: Vec<String>,
    pub lib: bool,
    /// `target = "<triple>"`, or `"target"` for the build's own.
    pub target: Option<String>,
}

impl Artifact {
    pub fn parse(kinds: &[String], lib: bool, target: Option<&str>) -> anyhow::Result<Artifact> {
        for kind in kinds {
            if !matches!(kind.as_str(), "bin" | "cdylib" | "staticlib") && !kind.starts_with("bin:") {
                bail!("'{kind}' is not a valid artifact specifier");
            }
        }
        if kinds.iter().any(|it| it == "bin") && kinds.iter().any(|it| it.starts_with("bin:")) {
            bail!(
                "Cannot specify both 'bin' and 'bin:<name>' binary artifacts, as 'bin' selects all available binaries."
            );
        }
        let mut unique: Vec<&str> = kinds.iter().map(String::as_str).collect();
        unique.sort_unstable();
        unique.dedup();
        let dupes = kinds.len() - unique.len();
        if dupes != 0 {
            bail!("Found {dupes} duplicate binary artifact{}", if dupes > 1 { "s" } else { "" });
        }
        if let Some(target) = target
            && target != "target"
        {
            // cargo's `CompileTarget::new`; blaze does not read `-Zjson-target-spec`.
            let target = target.trim();
            if target.is_empty() {
                bail!("target was empty");
            }
            if target.ends_with(".json") {
                bail!("`.json` target specs require -Zjson-target-spec");
            }
        }
        Ok(Artifact { kinds: kinds.to_vec(), lib, target: target.map(str::to_owned) })
    }
}

impl Dependency {
    pub fn parse(name: &str, version: Option<&str>, source_id: SourceId) -> anyhow::Result<Self> {
        let req = version
            .map(|v| {
                VersionReq::parse(v).with_context(|| {
                    format!("failed to parse the version requirement `{v}` for dependency `{name}`")
                })
            })
            .transpose()?;
        Ok(Dependency {
            name: name.to_owned(),
            source_id,
            registry_id: None,
            req,
            kind: DepKind::Normal,
            optional: false,
            public: false,
            features: Vec::new(),
            default_features: true,
            platform: None,
            explicit_name_in_toml: None,
            artifact: None,
        })
    }

    /// What the manifest calls it: the key in its dependency table.
    pub fn name_in_toml(&self) -> &str {
        self.explicit_name_in_toml.as_deref().unwrap_or(&self.name)
    }

    /// Whether it is built for the package's own targets, which dev-dependencies are not.
    pub fn is_transitive(&self) -> bool {
        self.kind != DepKind::Development
    }

    pub fn serialized(&self) -> SerializedDependency<'_> {
        SerializedDependency {
            name: &self.name,
            source: (!self.source_id.is_path()).then(|| self.source_id.as_url().to_string()),
            req: match &self.req {
                Some(req) => req.to_string(),
                None => "*".to_owned(),
            },
            kind: self.kind,
            rename: self.explicit_name_in_toml.as_deref(),
            optional: self.optional,
            uses_default_features: self.default_features,
            features: &self.features,
            artifact: self.artifact.as_ref(),
            target: self.platform.as_ref(),
            registry: self.registry_id.map(|id| id.url().to_string()),
            path: self.source_id.local_path(),
        }
    }
}

/// A dependency as `cargo metadata` prints it, fields in cargo's order.
#[derive(Serialize)]
pub struct SerializedDependency<'a> {
    pub name: &'a str,
    pub source: Option<String>,
    pub req: String,
    pub kind: DepKind,
    pub rename: Option<&'a str>,
    pub optional: bool,
    pub uses_default_features: bool,
    pub features: &'a [String],
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifact: Option<&'a Artifact>,
    pub target: Option<&'a Platform>,
    pub registry: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
}

pub use blaze_resolve::{FeatureMap, FeatureValue};

/// The checks of cargo's `Summary::new`, then its feature map (blaze-resolve's
/// `build_feature_map`): the declared features, plus one implicit feature per optional
/// dependency that no `dep:` names.
pub fn build_summary_features(
    features: &BTreeMap<String, Vec<String>>,
    dependencies: &[Dependency],
) -> anyhow::Result<FeatureMap> {
    for dep in dependencies {
        if dep.optional && !dep.is_transitive() {
            bail!("dev-dependencies are not allowed to be optional: `{}`", dep.name_in_toml());
        }
    }
    let features: BTreeMap<InternedString, Vec<InternedString>> = features
        .iter()
        .map(|(name, list)| {
            (InternedString::new(name), list.iter().map(|it| InternedString::new(it)).collect())
        })
        .collect();
    let deps = dependencies.iter().map(|dep| FeatureDep {
        name_in_toml: InternedString::new(dep.name_in_toml()),
        optional: dep.optional,
    });
    blaze_resolve::build_feature_map(&features, deps)
}

#[cfg(test)]
mod reference;

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::files::tests::Mem;

    fn kinds(kinds: &[&str]) -> Vec<String> {
        kinds.iter().map(|it| (*it).to_owned()).collect()
    }

    #[test]
    fn an_artifact_takes_cargo_s_kinds() {
        let artifact = Artifact::parse(&kinds(&["bin:tool", "cdylib", "staticlib"]), true, Some("target"))
            .unwrap();
        assert_eq!(artifact.kinds, ["bin:tool", "cdylib", "staticlib"]);
        assert!(artifact.lib);
        assert_eq!(artifact.target.as_deref(), Some("target"));
    }

    #[test]
    fn an_artifact_refuses_what_cargo_refuses() {
        let error = |list: &[&str], target: Option<&str>| {
            Artifact::parse(&kinds(list), false, target).unwrap_err().to_string()
        };
        assert_eq!(error(&["rlib"], None), "'rlib' is not a valid artifact specifier");
        assert_eq!(error(&[""], None), "'' is not a valid artifact specifier");
        assert_eq!(
            error(&["bin", "bin:a"], None),
            "Cannot specify both 'bin' and 'bin:<name>' binary artifacts, as 'bin' selects all available binaries."
        );
        assert_eq!(error(&["cdylib", "cdylib"], None), "Found 1 duplicate binary artifact");
        assert_eq!(error(&["bin", "bin", "bin"], None), "Found 2 duplicate binary artifacts");
        assert_eq!(error(&["bin"], Some("")), "target was empty");
        assert_eq!(error(&["bin"], Some(" ")), "target was empty");
        assert_eq!(error(&["bin"], Some(" a.json ")), "`.json` target specs require -Zjson-target-spec");
        Artifact::parse(&kinds(&["bin"]), false, Some("x86_64-unknown-linux-gnu")).unwrap();
    }

    const ROOT: &str = "/blaze-nowhere/bindeps";

    fn files(config: &str, dependency: &str) -> Mem {
        Mem::default()
            .dir(&format!("{ROOT}/home"))
            .file(&format!("{ROOT}/.cargo/config.toml"), config)
            .file(
                &format!("{ROOT}/Cargo.toml"),
                &format!(
                    "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\
                     [build-dependencies]\nb = {{ path = \"b\", {dependency} }}\n"
                ),
            )
            .file(&format!("{ROOT}/src/lib.rs"), "")
            .file(
                &format!("{ROOT}/b/Cargo.toml"),
                "[package]\nname = \"b\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
            )
            .file(&format!("{ROOT}/b/src/main.rs"), "")
    }

    fn config(files: Mem, flags: &[&str]) -> anyhow::Result<crate::Config<Mem>> {
        let env = [("CARGO_HOME".to_owned(), format!("{ROOT}/home"))];
        let mut config = crate::Config::load_with(Path::new(ROOT), env, files)?;
        config.cli_unstable(&kinds(flags))?;
        Ok(config)
    }

    fn metadata(files: Mem, flags: &[&str]) -> anyhow::Result<String> {
        let ctx = crate::Context::new(config(files, flags)?);
        let manifest = crate::metadata::root_manifest(None, &ctx)?;
        let ws = crate::Workspace::new(&manifest, &ctx)?;
        Ok(serde_json::to_string(&crate::metadata::export_no_deps(&ws))?)
    }

    #[test]
    fn the_unstable_table_comes_first_and_each_flag_after_it() {
        let table = "[unstable]\nbindeps = true\nbuild-std = [\"std,core\", \"alloc\"]\n";
        let read = config(files(table, "artifact = \"bin\""), &[]).unwrap();
        assert!(read.bindeps());
        assert_eq!(read.unstable().build_std.as_deref(), Some(&kinds(&["std", "core", "alloc"])[..]));
        let read = config(files(table, "artifact = \"bin\""), &["bindeps=no", "build_std=core"]).unwrap();
        assert!(!read.bindeps());
        assert_eq!(read.unstable().build_std.as_deref(), Some(&kinds(&["core"])[..]));
        let read = config(files("", "artifact = \"bin\""), &["unstable-options"]).unwrap();
        assert!(read.unstable().unstable_options && !read.bindeps());
    }

    #[test]
    fn a_flag_blaze_does_not_read_is_an_error_naming_it() {
        let error = |flags: &[&str]| {
            config(files("", "artifact = \"bin\""), flags).map(drop).unwrap_err().to_string()
        };
        assert_eq!(error(&["minimal-versions"]), "blaze does not read `-Z minimal-versions` yet");
        assert_eq!(error(&["bindeps=maybe"]), "flag -Zbindeps expected `no` or `yes`, found: `maybe`");
    }

    #[test]
    fn an_artifact_dependency_needs_bindeps() {
        let error = metadata(files("", "artifact = \"bin\""), &[]).unwrap_err();
        assert!(format!("{error:#}").contains("`artifact = …` requires `-Z bindeps` (b)"), "{error:#}");
        let error = metadata(files("", "lib = true"), &[]).unwrap_err();
        assert!(
            format!("{error:#}").contains("'lib' specifier cannot be used without an 'artifact = …' value (b)"),
            "{error:#}"
        );
    }

    #[test]
    fn with_bindeps_cargo_metadata_says_the_artifact() {
        let json = metadata(files("", "artifact = [\"bin\"], target = \"target\""), &["bindeps"]).unwrap();
        assert!(
            json.contains(r#""features":[],"artifact":{"kinds":["bin"],"lib":false,"target":"target"},"target":null"#),
            "{json}"
        );
        let plain = metadata(files("", "version = \"0.1\""), &[]).unwrap();
        assert!(!plain.contains("\"artifact\""), "{plain}");
    }

    #[test]
    fn a_codegen_backend_profile_needs_its_cargo_feature_or_the_flag() {
        let ws = |config: &str| {
            Mem::default()
                .dir(&format!("{ROOT}/home"))
                .file(&format!("{ROOT}/.cargo/config.toml"), config)
                .file(
                    &format!("{ROOT}/Cargo.toml"),
                    "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\
                     [profile.dev]\ncodegen-backend = \"cranelift\"\n",
                )
                .file(&format!("{ROOT}/src/lib.rs"), "")
        };
        let error = metadata(ws(""), &[]).unwrap_err();
        assert!(format!("{error:#}").contains("feature `codegen-backend` is required"), "{error:#}");
        metadata(ws(""), &["codegen-backend"]).unwrap();
        metadata(ws("[unstable]\ncodegen-backend = true\n"), &[]).unwrap();
        let error = metadata(ws("[unstable]\ncodegen-backend = true\n"), &["codegen-backend=no"]).unwrap_err();
        assert!(format!("{error:#}").contains("feature `codegen-backend` is required"), "{error:#}");
    }

    fn feature_dep(name: &str, optional: bool, kind: DepKind) -> Dependency {
        let mut dep = Dependency::parse(name, Some("1"), SourceId::crates_io()).unwrap();
        dep.optional = optional;
        dep.kind = kind;
        dep
    }

    fn table(list: &[(&str, &[&str])]) -> BTreeMap<String, Vec<String>> {
        list.iter().map(|(k, v)| ((*k).to_owned(), v.iter().map(|it| (*it).to_owned()).collect())).collect()
    }

    /// The unused optional dependency an "is not included in any feature" error names.
    fn unused_named(message: &str) -> Option<&str> {
        message.strip_prefix("optional dependency `")?.split('`').next()
    }

    /// The shared feature map and the manifest's old one build the same map or fail with the
    /// same message; where several optional dependencies are unused, the old one named any of
    /// them and the shared one names the first in `[dependencies]` order.
    fn agree(features: &BTreeMap<String, Vec<String>>, deps: &[Dependency]) {
        let new = build_summary_features(features, deps);
        let old = reference::build_summary_features(features, deps);
        match (new, old) {
            (Ok(new), Ok(old)) => {
                let new: Vec<(String, Vec<String>)> = new
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.iter().map(ToString::to_string).collect()))
                    .collect();
                let old: Vec<(String, Vec<String>)> = old
                    .iter()
                    .map(|(k, v)| (k.clone(), v.iter().map(ToString::to_string).collect()))
                    .collect();
                assert_eq!(new, old, "{features:?}");
            }
            (Err(new), Err(old)) => {
                let (new, old) = (new.to_string(), old.to_string());
                if new == old {
                    return;
                }
                let (Some(named), Some(old_named)) = (unused_named(&new), unused_named(&old)) else {
                    panic!("{features:?}: `{new}` against the old `{old}`");
                };
                let at = |name: &str| deps.iter().position(|it| it.name_in_toml() == name).unwrap();
                assert!(at(named) < at(old_named), "{features:?}: `{new}` against the old `{old}`");
            }
            (new, old) => panic!("{features:?}: {:?} against the old {:?}", new.map(drop), old.map(drop)),
        }
    }

    #[test]
    fn the_feature_map_is_the_old_one_on_its_cases() {
        use DepKind::{Development, Normal};
        let deps = [
            feature_dep("a", true, Normal),
            feature_dep("b", true, Normal),
            feature_dep("c", true, Normal),
            feature_dep("d", false, Normal),
        ];
        agree(&table(&[]), &[]);
        agree(&table(&[("default", &["a", "x"]), ("x", &["dep:b", "d/std"]), ("c", &["dep:c", "a?/foo"])]), &deps);
        agree(&table(&[("a", &["stdd"]), ("std", &[])]), &[]);
        agree(&table(&[("f", &["dep:a"]), ("g", &["a"])]), &deps[..1]);
        agree(&table(&[("f", &["d"])]), &deps[3..]);
        agree(&table(&[("f", &["dep:x"])]), &[]);
        agree(&table(&[("f", &["dep:d"])]), &deps[3..]);
        agree(&table(&[("f", &["a/x/y"])]), &deps[..1]);
        agree(&table(&[("f", &["dep:a/x"])]), &deps[..1]);
        agree(&table(&[("f", &["x/a"])]), &[]);
        agree(&table(&[("f", &["d?/a"])]), &deps[3..]);
        agree(&table(&[("f", &["dep:a"]), ("a", &[])]), &deps[..1]);
        agree(&table(&[("dep:f", &[])]), &deps[..1]);
        agree(&table(&[("", &[])]), &[]);
        agree(&table(&[("a-b+c.d", &[]), ("1x", &[]), ("-x", &[])]), &[]);
        agree(&table(&[("a", &[])]), &deps[..1]);
        agree(&table(&[]), &[feature_dep("t", true, Development)]);
        let mut renamed = feature_dep("serde", true, Normal);
        renamed.explicit_name_in_toml = Some("serde1".to_owned());
        agree(&table(&[]), &[renamed]);
    }

    #[test]
    fn the_unused_optional_dependency_named_is_the_first_in_dependencies_order() {
        let deps = [
            feature_dep("z", true, DepKind::Normal),
            feature_dep("y", true, DepKind::Normal),
            feature_dep("x", true, DepKind::Normal),
        ];
        let features = table(&[("x", &[]), ("y", &[]), ("z", &[])]);
        let error = build_summary_features(&features, &deps).unwrap_err().to_string();
        assert_eq!(unused_named(&error), Some("z"));
        agree(&features, &deps);
    }

    #[hegel::test]
    fn the_feature_map_is_the_old_one_on_any_table(tc: hegel::TestCase) {
        use hegel::generators as gs;
        let deps: Vec<(usize, bool, u8)> = tc.draw(
            gs::vecs(gs::tuples!(gs::integers().max_value(4), gs::booleans(), gs::integers().max_value(3)))
                .max_size(5),
        );
        let features: BTreeMap<usize, Vec<(usize, u8, usize)>> = tc.draw(
            gs::btree_maps(
                gs::integers().max_value(8),
                gs::vecs(gs::tuples!(
                    gs::integers().max_value(4),
                    gs::integers().max_value(6),
                    gs::integers().max_value(2),
                ))
                .max_size(3),
            )
            .max_size(5),
        );
        const DEPS: [&str; 5] = ["a", "b", "c", "std", "serde"];
        const FEATURES: [&str; 9] = ["default", "std", "a", "b", "x", "dep:f", "1x", "-x", "s/t"];
        const SUBS: [&str; 3] = ["std", "x", "y/z"];
        let deps: Vec<Dependency> = deps
            .into_iter()
            .map(|(name, optional, kind)| {
                let kind = match kind {
                    0 => DepKind::Development,
                    1 => DepKind::Build,
                    _ => DepKind::Normal,
                };
                feature_dep(DEPS[name], optional, kind)
            })
            .collect();
        let features: BTreeMap<String, Vec<String>> = features
            .into_iter()
            .map(|(name, values)| {
                let values = values
                    .into_iter()
                    .map(|(dep, shape, at)| {
                        let (dep, sub) = (DEPS[dep], SUBS[at]);
                        match shape {
                            0 => dep.to_owned(),
                            1 => format!("dep:{dep}"),
                            2 => format!("{dep}/{sub}"),
                            3 => format!("{dep}?/{sub}"),
                            4 => format!("dep:{dep}/{sub}"),
                            5 => FEATURES[at].to_owned(),
                            _ => sub.to_owned(),
                        }
                    })
                    .collect();
                (FEATURES[name].to_owned(), values)
            })
            .collect();
        agree(&features, &deps);
    }
}
