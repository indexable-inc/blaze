//! The feature resolver and the members `-p` and `--features` pick, on cases of cargo
//! 1.99.0's (5f94df4789) `tests/testsuite/{features2,weak_dep_features,package_features}.rs`.
//! cargo's tests build the project and read the features from what it prints or fails to
//! compile; here they are read from the feature resolver. cargo is MIT or Apache-2.0.

#[path = "resolver/helpers.rs"]
mod helpers;

use std::collections::BTreeMap;
use std::fmt::Write as _;

use anyhow::Result as CargoResult;
use blaze_resolve::resolver::{
    CliFeatures, CompileKind, FeatureOpts, FeatureResolver, FeaturesFor, ForceAllTargets,
    HasDevUnits, Members, PackageTargets, ResolveBehavior, ResolveOpts, VersionOrdering,
    features::RequestedFeatures, members_with_features,
};
use blaze_resolve::{Cfg, DepKind, Dependency, PackageId, PackageIdSpec, Platform, Summary};

use helpers::{ToDep, dep, dep_kind, dep_platform, pkg, pkg_dep, pkg_dep_with, resolve_summaries};

const HOST: &str = "x86_64-unknown-linux-gnu";

struct Targets {
    proc_macros: &'static [&'static str],
    host_cfg: Vec<Cfg>,
}

impl PackageTargets for Targets {
    fn has_any_proc_macro(&self, package_id: PackageId) -> bool {
        self.proc_macros.contains(&package_id.name().as_str())
    }

    fn has_proc_macro_lib(&self, package_id: PackageId) -> bool {
        self.proc_macros.contains(&package_id.name().as_str())
    }

    fn platform_activated(&self, platform: &Platform, kind: CompileKind) -> bool {
        match kind {
            CompileKind::Host => platform.matches(HOST, &self.host_cfg),
            CompileKind::Target(triple) => platform.matches(&triple, &[]),
        }
    }
}

struct Ws {
    members: Vec<Summary>,
    registry: Vec<Summary>,
    behavior: ResolveBehavior,
    /// The member in the current directory; none for a virtual workspace's root.
    current: Option<&'static str>,
    proc_macros: &'static [&'static str],
}

impl Ws {
    fn new(members: Vec<Summary>, registry: Vec<Summary>, behavior: ResolveBehavior) -> Ws {
        let current = Some(members[0].name().as_str());
        Ws { members, registry, behavior, current, proc_macros: &[] }
    }

    /// `cargo build` with `args` (`-p`, `--features`, `--all-features`,
    /// `--no-default-features`), as cargo's `resolve_ws_with_opts` does: the members and their
    /// features, the resolve, then the features of each package, one line each, `(host)` for a
    /// build dependency or proc macro's.
    fn build(&self, args: &str, has_dev_units: HasDevUnits) -> CargoResult<String> {
        let mut specs = Vec::new();
        let mut features = Vec::new();
        let (mut all_features, mut uses_default_features) = (false, true);
        let mut words = args.split_whitespace();
        while let Some(word) = words.next() {
            match word {
                "-p" => specs.push(PackageIdSpec::parse(words.next().unwrap())?),
                "--features" => features.push(words.next().unwrap().to_owned()),
                "--all-features" => all_features = true,
                "--no-default-features" => uses_default_features = false,
                _ => panic!("unknown argument {word}"),
            }
        }
        let current = self
            .current
            .map(|name| self.members.iter().find(|m| m.name() == name).unwrap().package_id());
        if specs.is_empty() {
            specs.push(PackageIdSpec::new(current.expect("a current package").name().to_string()));
        }
        let cli_features =
            CliFeatures::from_command_line(&features, all_features, uses_default_features)?;

        let members: Vec<&Summary> = self.members.iter().collect();
        let ws = Members {
            members: &members,
            current,
            new_cli_feature_behavior: current.is_none() || self.behavior != ResolveBehavior::V1,
        };
        let member_features = members_with_features(&ws, &specs, &cli_features)?;
        let roots: Vec<_> = member_features
            .iter()
            .map(|(member, features)| {
                let opts = ResolveOpts {
                    dev_deps: true,
                    features: RequestedFeatures::CliFeatures(features.clone()),
                };
                ((*member).clone(), opts)
            })
            .collect();
        let registry: Vec<Summary> = self.registry.iter().chain(&self.members).cloned().collect();
        let resolve = resolve_summaries(&roots, &registry, VersionOrdering::MaximumVersionsFirst)?;

        let targets = Targets {
            proc_macros: self.proc_macros,
            host_cfg: vec!["unix".parse().unwrap(), r#"target_os = "linux""#.parse().unwrap()],
        };
        let opts = FeatureOpts::new(self.behavior, has_dev_units, ForceAllTargets::No);
        let resolved = FeatureResolver::resolve(
            &targets,
            &resolve,
            &member_features,
            &[CompileKind::Host],
            opts,
        )?;
        let mut lines = BTreeMap::new();
        for ((pkg, fk), features) in &resolved.activated_features {
            let host = if *fk == FeaturesFor::HostDep { " (host)" } else { "" };
            let features: Vec<_> = features.iter().map(|f| f.as_str()).collect();
            lines.insert(format!("{}{host}", pkg.name()), features.join(" "));
        }
        Ok(lines.into_iter().fold(String::new(), |mut output, (pkg, features)| {
            writeln!(output, "{pkg}: {features}").unwrap();
            output
        }))
    }
}

fn features(name: &'static str, features: &[&'static str]) -> Summary {
    let map: Vec<(&'static str, &[&'static str])> =
        features.iter().map(|f| (*f, &[][..])).collect();
    pkg_dep_with(&name, vec![], &map)
}

fn with(dep: Dependency, features: &[&'static str]) -> Dependency {
    dep.with(features)
}

#[test]
fn inactivate_targets() {
    let registry = vec![
        features("common", &["f1"]),
        pkg_dep(&"bar", vec![with(dep_platform("common", "cfg(whatever)"), &["f1"])]),
    ];
    let foo = pkg_dep(&"foo", vec![dep("common"), dep("bar")]);

    let ws = Ws::new(vec![foo.clone()], registry.clone(), ResolveBehavior::V1);
    assert_eq!(ws.build("", HasDevUnits::No).unwrap(), "bar: \ncommon: f1\nfoo: \n");

    let ws = Ws::new(vec![foo], registry, ResolveBehavior::V2);
    assert_eq!(ws.build("", HasDevUnits::No).unwrap(), "bar: \ncommon: \nfoo: \n");
}

#[test]
fn inactive_target_optional() {
    let registry = vec![
        features("common", &["f1", "f2", "f3", "f4"]),
        pkg_dep_with(&"dep1", vec![dep("common").with(&["f1"])], &[("f2", &["common/f2"])]),
        pkg_dep_with(&"dep2", vec![dep("common")], &[("f3", &["common/f3"])]),
    ];
    let foo = pkg_dep_with(
        &"foo",
        vec![
            dep("common"),
            dep_platform("dep1", "cfg(whatever)").opt(),
            dep_platform("dep2", "cfg(whatever)").opt().with(&["f3"]),
            dep_platform("common", "cfg(whatever)").opt().with(&["f4"]),
        ],
        &[("foo1", &["dep1/f2"]), ("foo2", &["dep2"])],
    );

    let v1 = Ws::new(vec![foo.clone()], registry.clone(), ResolveBehavior::V1);
    let run = |args| v1.build(args, HasDevUnits::No).unwrap();
    assert_eq!(
        run("--all-features"),
        "common: f1 f2 f3 f4\ndep1: f2\ndep2: f3\nfoo: common dep1 dep2 foo1 foo2\n"
    );
    assert_eq!(run("--features dep1"), "common: f1\ndep1: \nfoo: dep1\n");
    assert_eq!(run("--features foo1"), "common: f1 f2\ndep1: f2\nfoo: dep1 foo1\n");
    assert_eq!(run("--features dep2"), "common: f3\ndep2: f3\nfoo: dep2\n");
    assert_eq!(run("--features common"), "common: f4\nfoo: common\n");

    let v2 = Ws::new(vec![foo], registry, ResolveBehavior::V2);
    let run = |args| v2.build(args, HasDevUnits::No).unwrap();
    assert_eq!(run("--all-features"), "common: \nfoo: common dep1 dep2 foo1 foo2\n");
    assert_eq!(run("--features dep1"), "common: \nfoo: dep1\n");
    assert_eq!(run("--features foo1"), "common: \nfoo: foo1\n");
    assert_eq!(run("--features dep2"), "common: \nfoo: dep2\n");
    assert_eq!(run("--features common"), "common: \nfoo: common\n");
}

#[test]
fn decouple_host_deps() {
    let registry = vec![features("common", &["f1"])];
    let foo =
        pkg_dep(&"foo", vec![with(dep_kind("common", DepKind::Build), &["f1"]), dep("common")]);

    let ws = Ws::new(vec![foo.clone()], registry.clone(), ResolveBehavior::V1);
    assert_eq!(ws.build("", HasDevUnits::No).unwrap(), "common: f1\nfoo: \n");

    let ws = Ws::new(vec![foo], registry, ResolveBehavior::V2);
    assert_eq!(ws.build("", HasDevUnits::No).unwrap(), "common: \ncommon (host): f1\nfoo: \n");
}

#[test]
fn decouple_host_deps_nested() {
    let registry =
        vec![features("common", &["f1"]), pkg_dep(&"bdep", vec![dep("common").with(&["f1"])])];
    let foo = pkg_dep(&"foo", vec![dep_kind("bdep", DepKind::Build), dep("common")]);

    let ws = Ws::new(vec![foo.clone()], registry.clone(), ResolveBehavior::V1);
    assert_eq!(ws.build("", HasDevUnits::No).unwrap(), "bdep: \ncommon: f1\nfoo: \n");

    let ws = Ws::new(vec![foo], registry, ResolveBehavior::V2);
    assert_eq!(
        ws.build("", HasDevUnits::No).unwrap(),
        "bdep (host): \ncommon: \ncommon (host): f1\nfoo: \n"
    );
}

#[test]
fn decouple_dev_deps() {
    let registry = vec![features("common", &["f1", "f2"])];
    let foo = pkg_dep(
        &"foo",
        vec![dep("common").with(&["f1"]), with(dep_kind("common", DepKind::Development), &["f2"])],
    );

    // `cargo run` and `cargo test`.
    let ws = Ws::new(vec![foo.clone()], registry.clone(), ResolveBehavior::V1);
    assert_eq!(ws.build("", HasDevUnits::No).unwrap(), "common: f1 f2\nfoo: \n");
    assert_eq!(ws.build("", HasDevUnits::Yes).unwrap(), "common: f1 f2\nfoo: \n");

    let ws = Ws::new(vec![foo], registry, ResolveBehavior::V2);
    assert_eq!(ws.build("", HasDevUnits::No).unwrap(), "common: f1\nfoo: \n");
    assert_eq!(ws.build("", HasDevUnits::Yes).unwrap(), "common: f1 f2\nfoo: \n");
}

#[test]
fn decouple_proc_macro() {
    let registry = vec![
        features("common", &["somefeat"]),
        pkg_dep(&"pm", vec![dep("common").with(&["somefeat"])]),
    ];
    let foo = pkg_dep(&"foo", vec![dep("pm"), dep("common")]);

    let mut ws = Ws::new(vec![foo.clone()], registry.clone(), ResolveBehavior::V1);
    ws.proc_macros = &["pm"];
    assert_eq!(ws.build("", HasDevUnits::No).unwrap(), "common: somefeat\nfoo: \npm: \n");

    let mut ws = Ws::new(vec![foo], registry, ResolveBehavior::V2);
    ws.proc_macros = &["pm"];
    assert_eq!(
        ws.build("", HasDevUnits::No).unwrap(),
        "common: \ncommon (host): somefeat\nfoo: \npm (host): \n"
    );
}

#[test]
fn weak_simple() {
    let registry = vec![features("bar", &["feat"])];
    let foo = pkg_dep_with(&"foo", vec!["bar".opt()], &[("f1", &["bar?/feat"])]);
    let ws = Ws::new(vec![foo], registry, ResolveBehavior::V1);
    assert_eq!(ws.build("--features f1", HasDevUnits::No).unwrap(), "foo: f1\n");
    assert_eq!(ws.build("--features f1,bar", HasDevUnits::No).unwrap(), "bar: feat\nfoo: bar f1\n");
}

#[test]
fn weak_deferred() {
    // A complex chain that requires deferring enabling the feature due to
    // another dependency getting enabled.
    let registry = vec![
        features("bar", &["feat"]),
        pkg_dep_with(&"dep", vec!["bar".opt()], &[("feat", &["bar?/feat"])]),
        pkg_dep(&"bar_activator", vec![dep("dep").with(&["bar"])]),
    ];
    let foo = pkg_dep(&"foo", vec![dep("dep").with(&["feat"]), dep("bar_activator")]);
    let ws = Ws::new(vec![foo], registry, ResolveBehavior::V1);
    assert_eq!(
        ws.build("", HasDevUnits::No).unwrap(),
        "bar: feat\nbar_activator: \ndep: bar feat\nfoo: \n"
    );
}

#[test]
fn other_member_from_current() {
    // -p for another member while in the current directory.
    let foo = pkg_dep_with(&"foo", vec![dep("bar").with(&["f3"])], &[("f1", &["bar/f4"])]);
    let bar = features("bar", &["f1", "f2", "f3", "f4"]);
    let bar_features = |ws: &Ws, args| {
        let out = ws.build(args, HasDevUnits::No)?;
        Ok::<_, anyhow::Error>(out.lines().find(|l| l.starts_with("bar:")).unwrap().to_owned())
    };

    let ws = Ws::new(vec![foo.clone(), bar.clone()], vec![], ResolveBehavior::V1);
    assert_eq!(bar_features(&ws, "-p bar --features f1").unwrap(), "bar: f3 f4");
    assert_eq!(
        bar_features(&ws, "-p bar --features f1,f2").unwrap_err().to_string(),
        "package `foo v1.0.0 (registry `https://example.com/`)` does not have the feature `f2`\n\n\
         help: a feature with a similar name exists: `f1`"
    );
    assert_eq!(bar_features(&ws, "-p bar --features bar/f1").unwrap(), "bar: f1 f3");

    let ws = Ws::new(vec![foo, bar], vec![], ResolveBehavior::V2);
    assert_eq!(bar_features(&ws, "-p bar --features f1").unwrap(), "bar: f1");
    assert_eq!(bar_features(&ws, "-p bar --features f1,f2").unwrap(), "bar: f1 f2");
    assert_eq!(bar_features(&ws, "-p bar --features bar/f1").unwrap(), "bar: f1");
}

#[test]
fn virtual_features() {
    // --features in root of virtual workspace.
    let mut ws = Ws::new(vec![features("a", &["f1"]), pkg("b")], vec![], ResolveBehavior::V1);
    ws.current = None;
    assert_eq!(ws.build("-p a -p b --features f1", HasDevUnits::No).unwrap(), "a: f1\nb: \n");
}

#[test]
fn virtual_with_specific() {
    // -p flags with --features in root of virtual.
    let mut ws = Ws::new(
        vec![features("a", &["f1", "f2"]), features("b", &["f2", "f3"])],
        vec![],
        ResolveBehavior::V1,
    );
    ws.current = None;
    assert_eq!(
        ws.build("-p a -p b --features f1,f2,f3", HasDevUnits::No).unwrap(),
        "a: f1 f2\nb: f2 f3\n"
    );
}

#[test]
fn virtual_typo_member_feature() {
    let ws = Ws::new(vec![features("a", &["deny-warnings"])], vec![], ResolveBehavior::V2);
    assert_eq!(
        ws.build("--features a/deny-warning", HasDevUnits::No).unwrap_err().to_string(),
        "the package 'a' does not contain this feature: a/deny-warning\n\
         help: there is a similarly named feature: a/deny-warnings"
    );
}

#[test]
fn non_member() {
    // -p for a non-member
    let foo = pkg_dep_with(&"foo", vec![dep("dep")], &[("f1", &[])]);
    let ws = Ws::new(vec![foo], vec![pkg("dep")], ResolveBehavior::V2);
    for args in ["-p dep --features f1", "-p dep --all-features", "-p dep --no-default-features"] {
        assert_eq!(
            ws.build(args, HasDevUnits::No).unwrap_err().to_string(),
            "cannot specify features for packages outside of workspace\n\n\
             help: a workspace member with a similar name exists: `foo`"
        );
    }
    assert_eq!(ws.build("-p dep", HasDevUnits::No).unwrap(), "dep: \nfoo: \n");
}

#[test]
fn resolver1_member_features() {
    // --features member-name/feature-name with resolver="1"
    let mut ws = Ws::new(
        vec![features("member1", &["m1-feature"]), pkg("member2")],
        vec![],
        ResolveBehavior::V1,
    );
    ws.current = Some("member2");
    assert_eq!(
        ws.build("-p member1 --features member1/m1-feature", HasDevUnits::No).unwrap(),
        "member1: m1-feature\nmember2: \n"
    );
    assert_eq!(
        ws.build("-p member1 --features member1/m2-feature", HasDevUnits::No)
            .unwrap_err()
            .to_string(),
        "package `member1 v1.0.0 (registry `https://example.com/`)` does not have the feature \
         `m2-feature`\n\nhelp: a feature with a similar name exists: `m1-feature`"
    );
}
