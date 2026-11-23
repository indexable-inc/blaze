//! The rules on small fixtures, the diamond, resolver 1 against 2 on a build-dependency,
//! target-specific and dev-dependency cases, the planted bugs, and two properties on drawn
//! workspaces: enabling a feature removes nothing, and the order the work is taken in changes
//! nothing.

use std::borrow::ToOwned;
use std::format;
use std::string::{String as StdString, ToString};
use std::vec;
use std::vec::Vec as StdVec;

use blaze_manifest::layout::{DepKind, ExtraKind};
use blaze_manifest::lock::{Lock, LockId};
use blaze_own::{Alloc, String, Vec};
use blaze_sim::{Broke, Config, Tape};
use edition::{Edition, PackageEdition};

use crate::input::{Ask, Bin, CfgAtom, Dep, DepSource, FeatureDef, FeatureValue, Lib, Package, Platform, Resolver, TargetDesc};
use crate::{Extra, LeftOutKind, Pass, Plant, Resolution, UnitError, UnitGraph, UnitKind, record};

/// tempo-primitives and reth-codecs in miniature: dev-dependency unification (src/tests/dev_unify.rs).
mod dev_unify;

fn a() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

fn t(s: &str) -> String<Alloc<'static>> {
    String::from_str_in(s, a())
}

/// A dependency entry of a fixture.
#[derive(Clone)]
struct D {
    key: StdString,
    package: StdString,
    kind: DepKind,
    platform: Option<StdString>,
    optional: bool,
    default_features: bool,
    features: StdVec<StdString>,
    /// A version requirement, which also picks among two locked versions.
    req: Option<StdString>,
}

fn dep(key: &str) -> D {
    D {
        key: key.to_owned(),
        package: key.to_owned(),
        kind: DepKind::Normal,
        platform: None,
        optional: false,
        default_features: true,
        features: StdVec::new(),
        req: None,
    }
}

impl D {
    fn optional(mut self) -> D {
        self.optional = true;
        self
    }
    fn no_default(mut self) -> D {
        self.default_features = false;
        self
    }
    fn features(mut self, fs: &[&str]) -> D {
        self.features = fs.iter().map(|f| (*f).to_owned()).collect();
        self
    }
    fn build(mut self) -> D {
        self.kind = DepKind::Build;
        self
    }
    fn dev(mut self) -> D {
        self.kind = DepKind::Dev;
        self
    }
    fn on(mut self, platform: &str) -> D {
        self.platform = Some(platform.to_owned());
        self
    }
    fn package(mut self, package: &str) -> D {
        self.package = package.to_owned();
        self
    }
    fn req(mut self, req: &str) -> D {
        self.req = Some(req.to_owned());
        self
    }
}

/// A package of a fixture.
#[derive(Clone)]
struct P {
    name: StdString,
    version: StdString,
    member: bool,
    lib: bool,
    proc_macro: bool,
    /// Each binary's name and its `required-features`.
    bins: StdVec<(StdString, StdVec<StdString>)>,
    /// Each integration test, example or bench: its kind, name and `required-features`.
    extras: StdVec<(ExtraKind, StdString, StdVec<StdString>)>,
    build: bool,
    edition: Edition,
    features: StdVec<(StdString, StdVec<StdString>)>,
    deps: StdVec<D>,
    /// Its manifest is not on disk.
    missing: bool,
}

fn pkg(name: &str) -> P {
    P {
        name: name.to_owned(),
        version: "1.0.0".to_owned(),
        member: false,
        lib: true,
        proc_macro: false,
        bins: StdVec::new(),
        extras: StdVec::new(),
        build: false,
        edition: Edition::Edition2021,
        features: StdVec::new(),
        deps: StdVec::new(),
        missing: false,
    }
}

impl P {
    fn member(mut self) -> P {
        self.member = true;
        self
    }
    fn version(mut self, v: &str) -> P {
        self.version = v.to_owned();
        self
    }
    fn proc_macro(mut self) -> P {
        self.proc_macro = true;
        self
    }
    fn bin(mut self, name: &str) -> P {
        self.bins.push((name.to_owned(), StdVec::new()));
        self
    }
    /// A binary with `required-features = [..]`.
    fn bin_requiring(mut self, name: &str, required: &[&str]) -> P {
        self.bins.push((name.to_owned(), required.iter().map(|f| (*f).to_owned()).collect()));
        self
    }
    /// An integration test, example or bench with `required-features = [..]`.
    fn extra_requiring(mut self, kind: ExtraKind, name: &str, required: &[&str]) -> P {
        self.extras.push((kind, name.to_owned(), required.iter().map(|f| (*f).to_owned()).collect()));
        self
    }
    fn build(mut self) -> P {
        self.build = true;
        self
    }
    fn missing(mut self) -> P {
        self.missing = true;
        self
    }
    fn feature(mut self, name: &str, values: &[&str]) -> P {
        self.features.push((name.to_owned(), values.iter().map(|v| (*v).to_owned()).collect()));
        self
    }
    fn dep(mut self, d: D) -> P {
        self.deps.push(d);
        self
    }

    /// The facts, with the implicit features of optional dependencies as the layout makes them:
    /// one per optional dependency no feature names as `dep:x`, in every edition.
    fn facts(&self) -> Package<'static> {
        let mut features = Vec::new_in(a());
        for (name, values) in self.features.iter() {
            let mut vs = Vec::new_in(a());
            for v in values {
                vs.push(FeatureValue::parse(v, a()));
            }
            features.push(FeatureDef { name: t(name), values: vs });
        }
        for d in self.deps.iter().filter(|d| d.optional) {
            let named = self.features.iter().any(|(_, vs)| vs.iter().any(|v| *v == format!("dep:{}", d.key)));
            let exists = features.iter().any(|f: &FeatureDef<'_>| f.name.as_str() == d.key);
            if !named && !exists {
                let mut vs = Vec::new_in(a());
                vs.push(FeatureValue::Dep(t(&d.key)));
                features.push(FeatureDef { name: t(&d.key), values: vs });
            }
        }
        features.sort_by(|x, y| x.name.as_str().cmp(y.name.as_str()));
        let mut deps = Vec::new_in(a());
        for d in self.deps.iter() {
            let mut fs = Vec::new_in(a());
            for f in d.features.iter() {
                fs.push(t(f));
            }
            deps.push(Dep {
                key: t(&d.key),
                package: t(&d.package),
                kind: d.kind,
                platform: d.platform.as_ref().map(|p| Platform::parse(p, a())),
                optional: d.optional,
                default_features: d.default_features,
                features: fs,
                source: DepSource::Registry { req: d.req.as_ref().map(|r| t(r)) },
            });
        }
        let mut bins = Vec::new_in(a());
        for (b, required) in self.bins.iter() {
            let mut required_features = Vec::new_in(a());
            for f in required.iter() {
                required_features.push(t(f));
            }
            bins.push(Bin { name: t(b), root: t(&format!("src/bin/{b}.rs")), required_features });
        }
        let mut extras = Vec::new_in(a());
        for (kind, name, required) in self.extras.iter() {
            let mut required_features = Vec::new_in(a());
            for f in required.iter() {
                required_features.push(t(f));
            }
            required_features.sort_by(|x: &String<Alloc<'static>>, y: &String<Alloc<'static>>| x.as_str().cmp(y.as_str()));
            let dir = match kind {
                ExtraKind::Test => "tests",
                ExtraKind::Example => "examples",
                ExtraKind::Bench => "benches",
            };
            extras.push(Extra { kind: *kind, name: t(name), root: t(&format!("{dir}/{name}.rs")), harness: true, test: *kind == ExtraKind::Test, required_features });
        }
        Package {
            name: t(&self.name),
            edition: PackageEdition { syntax: self.edition, blaze: None },
            lib: self.lib.then(|| Lib { crate_name: t(&self.name.replace('-', "_")), root: t("src/lib.rs"), proc_macro: self.proc_macro }),
            bins,
            extras,
            build: self.build.then(|| t("build.rs")),
            links: None,
            features,
            deps,
            meta: crate::PackageMeta::unstated(a()),
            patched_from: Vec::new_in(a()),
        }
    }
}

/// The lock of a world: one package per fixture package, every entry naming the one its
/// requirement fits.
fn lock_text(world: &[P]) -> StdString {
    let mut out = StdString::from("version = 4\n");
    for p in world {
        out.push_str(&format!("\n[[package]]\nname = \"{}\"\nversion = \"{}\"\n", p.name, p.version));
        if !p.member {
            out.push_str("source = \"registry+https://github.com/rust-lang/crates.io-index\"\n");
            out.push_str("checksum = \"0000000000000000000000000000000000000000000000000000000000000000\"\n");
        }
        let mut entries: StdVec<StdString> = StdVec::new();
        for d in p.deps.iter() {
            if d.kind == DepKind::Dev && !p.member {
                continue;
            }
            let same: StdVec<&P> = world.iter().filter(|q| q.name == d.package).collect();
            let fits: StdVec<&&P> = same.iter().filter(|q| d.req.as_ref().is_none_or(|r| q.version.starts_with(r.as_str()))).collect();
            let Some(q) = fits.first() else { continue };
            entries.push(match same.len() {
                1 => q.name.clone(),
                _ => format!("{} {}", q.name, q.version),
            });
        }
        entries.sort();
        entries.dedup();
        if !entries.is_empty() {
            out.push_str("dependencies = [\n");
            for e in entries {
                out.push_str(&format!(" \"{e}\",\n"));
            }
            out.push_str("]\n");
        }
    }
    out
}

fn mac() -> TargetDesc<'static> {
    let mut cfg = Vec::new_in(a());
    cfg.push(CfgAtom::flag("unix", a()));
    cfg.push(CfgAtom::pair("target_os", "macos", a()));
    cfg.push(CfgAtom::pair("target_arch", "aarch64", a()));
    TargetDesc::new("aarch64-apple-darwin", cfg, a())
}

fn windows() -> TargetDesc<'static> {
    let mut cfg = Vec::new_in(a());
    cfg.push(CfgAtom::flag("windows", a()));
    cfg.push(CfgAtom::pair("target_os", "windows", a()));
    cfg.push(CfgAtom::pair("target_arch", "x86_64", a()));
    TargetDesc::new("x86_64-pc-windows-msvc", cfg, a())
}

/// How a fixture is solved.
#[derive(Clone)]
struct Opts {
    resolver: Resolver,
    features: StdVec<StdString>,
    all_features: bool,
    no_default_features: bool,
    tests: bool,
    target: TargetDesc<'static>,
    plant: Plant,
}

fn opts(resolver: Resolver) -> Opts {
    Opts { resolver, features: StdVec::new(), all_features: false, no_default_features: false, tests: false, target: mac(), plant: Plant::None }
}

fn solve_by(world: &[P], o: &Opts, pick: &mut dyn FnMut(usize) -> usize) -> Result<UnitGraph<'static>, UnitError<'static>> {
    match settle_by(world, o, pick)? {
        Pass::Done(graph) => Ok(graph),
        Pass::Partial(partial) => panic!("the fixture's manifests of {:?} are missing: no graph", partial.missing()),
        Pass::Need(needs) => panic!("settle_by ended on a need of {needs:?}"),
    }
}

/// The world resolved to its end: `Done` or `Partial`, never `Need`.
fn settle_by(world: &[P], o: &Opts, pick: &mut dyn FnMut(usize) -> usize) -> Result<Pass<'static>, UnitError<'static>> {
    let text = lock_text(world);
    let lock = match Lock::read(text.as_bytes(), a()) {
        Ok(lock) => lock,
        Err(e) => panic!("fixture lock: {e}\n{text}"),
    };
    let lock: &'static Lock<'static> = std::boxed::Box::leak(std::boxed::Box::new(lock));
    let id_of = |p: &P| -> LockId {
        lock.named(&p.name).find(|id| lock.package(*id).version.to_string() == p.version).expect("every fixture package is locked")
    };
    let mut roots = Vec::new_in(a());
    for p in world.iter().filter(|p| p.member) {
        roots.push(id_of(p));
    }
    let mut features = Vec::new_in(a());
    for f in o.features.iter() {
        features.push(t(f));
    }
    let ask = Ask { roots, features, all_features: o.all_features, no_default_features: o.no_default_features, tests: o.tests };
    let host = mac();
    let mut res = Resolution::new(lock, &o.target, &host, &ask, o.resolver, a());
    res.plant(o.plant);
    for p in world.iter().filter(|p| p.member) {
        res.member(id_of(p), p.facts())?;
    }
    for _ in 0..64 {
        match res.run(pick)? {
            Pass::Need(needs) => {
                for id in needs.iter() {
                    let locked = lock.package(*id);
                    let p = world.iter().find(|p| p.name == locked.name.as_str() && p.version == locked.version.to_string()).expect("locked from the world");
                    res.package(*id, (!p.missing).then(|| p.facts()))?;
                }
            }
            settled => return Ok(settled),
        }
    }
    panic!("no fixpoint in 64 passes");
}

fn solve(world: &[P], o: &Opts) -> Result<UnitGraph<'static>, UnitError<'static>> {
    solve_by(world, o, &mut |n| n - 1)
}

/// Each unit as one line: `name version kind side [features] -> deps`.
fn lines(graph: &UnitGraph<'_>) -> StdVec<StdString> {
    graph
        .units
        .iter()
        .map(|u| {
            let kind = match &u.kind {
                UnitKind::Lib => "lib".to_owned(),
                UnitKind::ProcMacro => "proc-macro".to_owned(),
                UnitKind::Bin(b) => format!("bin:{b}"),
                UnitKind::BuildScript => "build".to_owned(),
                UnitKind::Test { bin: None } => "test".to_owned(),
                UnitKind::Test { bin: Some(b) } => format!("test:{b}"),
                UnitKind::IntegrationTest(x) => format!("integration-test:{}", x.name),
                UnitKind::Example(x) => format!("example:{}", x.name),
                UnitKind::Bench(x) => format!("bench:{}", x.name),
            };
            let side = if u.for_host { "host" } else { "target" };
            let fs: StdVec<&str> = u.features.iter().map(|f| f.as_str()).collect();
            let ds: StdVec<StdString> = u.deps.iter().map(|d| format!("{}={}", d.name, graph.units[d.unit.index()].name)).collect();
            format!("{} {} {kind} {side} [{}] -> {}", u.name, u.version, fs.join(","), ds.join(","))
        })
        .collect()
}

fn ok(world: &[P], o: &Opts) -> StdVec<StdString> {
    match solve(world, o) {
        Ok(graph) => lines(&graph),
        Err(e) => panic!("{e}"),
    }
}

fn features_of(lines: &[StdString], prefix: &str) -> StdVec<StdString> {
    lines.iter().filter(|l| l.starts_with(prefix)).cloned().collect()
}

/// Rule 1: `x/f` turns on optional `x`, its feature `f`, and the implicit feature `x`.
#[test]
fn slash_turns_on_the_dependency_and_its_implicit_feature() {
    let world = [
        pkg("app").member().feature("default", &["fast"]).feature("fast", &["x/simd"]).dep(dep("x").optional()),
        pkg("x").feature("simd", &[]),
    ];
    let got = ok(&world, &opts(Resolver::V2));
    assert_eq!(
        got,
        vec![
            "app 1.0.0 lib target [default,fast,x] -> x=x".to_string(),
            "x 1.0.0 lib target [simd] -> ".to_string(),
        ]
    );
}

/// Rule 1 and 2: `dep:x` turns `x` on with no feature `x`; asking for `x` is an error; the
/// planted bug (an implicit feature made anyway) is caught.
#[test]
fn dep_colon_makes_no_implicit_feature() {
    let world = [
        pkg("app").member().dep(dep("lib").features(&["x"])),
        pkg("lib").feature("tls", &["dep:x"]).dep(dep("x").optional()),
        pkg("x"),
    ];
    match solve(&world, &opts(Resolver::V2)) {
        Err(UnitError::NoSuchFeature { package, feature, .. }) => assert_eq!((package.as_str(), feature.as_str()), ("lib", "x")),
        Err(e) => panic!("another error: {e}"),
        Ok(g) => panic!("`x` was a feature of `lib`: {:?}", lines(&g)),
    }
    let planted = Opts { plant: Plant::DepMakesImplicit, ..opts(Resolver::V2) };
    assert!(solve(&world, &planted).is_ok(), "the planted bug went unseen");
    let on = [
        pkg("app").member().dep(dep("lib").features(&["tls"])),
        pkg("lib").feature("tls", &["dep:x"]).dep(dep("x").optional()),
        pkg("x"),
    ];
    assert_eq!(features_of(&ok(&on, &opts(Resolver::V2)), "lib "), vec!["lib 1.0.0 lib target [tls] -> x=x".to_string()]);
}

/// A binary whose `required-features` are not all on is no unit (the Cargo Book, "Cargo
/// Targets", "The `required-features` field": "If any of the required features are not
/// enabled, the target will be skipped"). tempo's bin/tempo in miniature: `tempo-localnet`
/// requires `localnet = ["dep:nix"]`, nix a `cfg(unix)` optional dependency; made with default
/// features it was checked without `nix`: three false "cannot find `nix`". Controls: with the
/// feature on, the binary is a unit and has `nix`; a required name the package lacks is the
/// package's error, never a skip; `dep/feature` holds only when the dependency has it.
#[test]
fn required_features_skip_a_binary() {
    let world = |required: &'static [&'static str]| {
        [
            pkg("app")
                .member()
                .bin("app")
                .bin_requiring("app-localnet", required)
                .feature("localnet", &["dep:nix"])
                .dep(dep("nix").optional().on("cfg(unix)")),
            pkg("nix").feature("signal", &[]),
        ]
    };
    let bins = |o: &Opts, required: &'static [&'static str]| -> StdVec<StdString> {
        ok(&world(required), o).into_iter().filter(|l| l.contains(" bin:")).collect()
    };
    let off = opts(Resolver::V2);
    assert_eq!(bins(&off, &["localnet"]), vec!["app 1.0.0 bin:app target [] -> app=app".to_string()]);
    let on = Opts { features: vec!["localnet".to_owned()], ..opts(Resolver::V2) };
    assert_eq!(
        bins(&on, &["localnet"]),
        vec![
            "app 1.0.0 bin:app target [localnet] -> app=app,nix=nix".to_string(),
            "app 1.0.0 bin:app-localnet target [localnet] -> app=app,nix=nix".to_string(),
        ]
    );
    let all = Opts { all_features: true, ..opts(Resolver::V2) };
    assert_eq!(bins(&all, &["localnet"]).len(), 2, "--all-features turns the required feature on");
    match solve(&world(&["no-such"]), &off) {
        Err(UnitError::NoSuchFeature { package, feature, .. }) => assert_eq!((package.as_str(), feature.as_str()), ("app", "no-such")),
        Err(e) => panic!("another error: {e}"),
        Ok(g) => panic!("an unknown required feature was skipped quietly: {:?}", lines(&g)),
    }
    // `nix/signal`: nix on (by `localnet`) but without `signal` skips; with it, a unit.
    assert_eq!(bins(&on, &["nix/signal"]).len(), 1, "nix lacks `signal`: the binary is skipped");
    let signal = Opts { features: vec!["localnet".to_owned(), "nix/signal".to_owned()], ..opts(Resolver::V2) };
    assert_eq!(bins(&signal, &["nix/signal"]).len(), 2, "nix has `signal`: the binary is a unit");
}

/// An integration test, example or bench whose `required-features` are not all on is no unit
/// and is said (the Cargo Book, "Cargo Targets", "The `required-features` field": "If any of
/// the required features are not enabled, the target will be skipped."). turso in miniature
/// (c1814): `core`'s bench `logical_log_serialization` requires `bench`, bindings/rust's
/// example `sync_example` requires `sync`; both were checked without them. TWIN: with
/// `--features bench,sync` both are units and nothing is left out. CONTROL: a target with no
/// `required-features` beside them is a unit either way, and the binary rule says its own
/// skip too. The record holds the left-out list: decode gives it back equal.
#[test]
fn required_features_skip_a_test_example_or_bench_and_say_so() {
    let world = [pkg("turso")
        .member()
        .feature("bench", &[])
        .feature("sync", &[])
        .bin_requiring("turso-sync", &["sync"])
        .extra_requiring(ExtraKind::Bench, "logical_log_serialization", &["bench"])
        .extra_requiring(ExtraKind::Example, "sync_example", &["sync"])
        .extra_requiring(ExtraKind::Example, "both", &["sync", "bench"])
        .extra_requiring(ExtraKind::Test, "plain", &[])];
    let tests = Opts { tests: true, ..opts(Resolver::V2) };
    let graph = solve(&world, &tests).unwrap_or_else(|e| panic!("{e}"));
    let extras: StdVec<StdString> = lines(&graph).into_iter().filter(|l| l.contains("integration-test:") || l.contains("example:") || l.contains("bench:")).collect();
    assert_eq!(extras, vec!["turso 1.0.0 integration-test:plain target [] -> turso=turso".to_string()], "only the target with nothing required");
    let said: StdVec<(LeftOutKind, &str, StdVec<&str>)> =
        graph.left_out.iter().map(|l| (l.kind, l.name.as_str(), l.off.iter().map(|f| f.as_str()).collect())).collect();
    assert_eq!(
        said,
        vec![
            (LeftOutKind::Bin, "turso-sync", vec!["sync"]),
            (LeftOutKind::Example, "both", vec!["bench", "sync"]),
            (LeftOutKind::Example, "sync_example", vec!["sync"]),
            (LeftOutKind::Bench, "logical_log_serialization", vec!["bench"]),
        ],
        "each skip is said, with what is off"
    );
    let note = graph.left_out[3].to_string();
    assert!(note.contains("bench target `logical_log_serialization` of package `turso`") && note.contains("`bench` is not enabled"), "{note}");
    let two = graph.left_out[1].to_string();
    assert!(two.contains("required features `bench`, `sync` are not enabled"), "{two}");
    let bytes = record::encode(&graph, &tests.target, &mac(), a()).expect("its own cfg");
    let back = record::decode(&bytes, a()).expect("the record decodes");
    assert_eq!(back.graph.left_out, graph.left_out, "the record holds the left-out targets");

    let on = Opts { features: vec!["bench".to_owned(), "sync".to_owned()], ..tests.clone() };
    let graph = solve(&world, &on).unwrap_or_else(|e| panic!("{e}"));
    let made: StdVec<StdString> = lines(&graph).into_iter().filter(|l| l.contains("example:") || l.contains("bench:") || l.contains(" bin:")).collect();
    assert_eq!(made.len(), 4, "TWIN: with the features on every target is a unit: {made:?}");
    assert!(graph.left_out.is_empty(), "TWIN: nothing is left out: {:?}", graph.left_out);
    // A required name the package lacks is the package's error, never a skip.
    let wrong = [pkg("turso").member().extra_requiring(ExtraKind::Bench, "b", &["no-such"])];
    match solve(&wrong, &tests) {
        Err(UnitError::NoSuchFeature { feature, asked_by, .. }) => assert_eq!((feature.as_str(), asked_by.as_str()), ("no-such", "the required-features of bench b")),
        Err(e) => panic!("another error: {e}"),
        Ok(g) => panic!("an unknown required feature was skipped quietly: {:?}", lines(&g)),
    }
}

/// Rule 1: `x?/f` does not turn `x` on; when `x` is on for another reason, `f` follows.
#[test]
fn weak_feature_waits_for_its_dependency() {
    let off = [
        pkg("app").member().feature("default", &["x?/simd"]).dep(dep("x").optional()),
        pkg("x").feature("simd", &[]),
    ];
    assert_eq!(ok(&off, &opts(Resolver::V2)), vec!["app 1.0.0 lib target [default] -> ".to_string()]);
    let planted = Opts { plant: Plant::WeakEnables, ..opts(Resolver::V2) };
    let got = match solve(&off, &planted) {
        Ok(g) => lines(&g),
        Err(e) => panic!("{e}"),
    };
    assert_ne!(got, vec!["app 1.0.0 lib target [default] -> ".to_string()], "the planted bug went unseen");
    // On: the weak value comes before or after the dependency is turned on.
    for order in [["x?/simd", "x"], ["x", "x?/simd"]] {
        let on = [pkg("app").member().feature("default", &order).dep(dep("x").optional()), pkg("x").feature("simd", &[])];
        assert_eq!(features_of(&ok(&on, &opts(Resolver::V2)), "x "), vec!["x 1.0.0 lib target [simd] -> ".to_string()]);
    }
}

/// Rule 2 through the real reader: an edition-2024 manifest's optional dependency makes an
/// implicit feature, and one named by `dep:` does not.
#[test]
fn implicit_features_in_every_edition() {
    let text = "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\nx = { version = \"1\", optional = true }\ny = { version = \"1\", optional = true }\n\n[features]\ntls = [\"dep:y\"]\n";
    let mut reader = match blaze_manifest::layout::Layout::new("", a()) {
        Ok(r) => r,
        Err(e) => panic!("{e}"),
    };
    let units = loop {
        match reader.pass() {
            Ok(blaze_manifest::layout::Pass::Done(units)) => break units,
            Ok(blaze_manifest::layout::Pass::Need(needs)) => {
                for need in needs.iter() {
                    match need {
                        blaze_manifest::layout::Need::Manifest(path) => {
                            let answer = (path.as_str() == "Cargo.toml").then_some(text);
                            if let Err(e) = reader.manifest(path, answer) {
                                panic!("{e}");
                            }
                        }
                        blaze_manifest::layout::Need::Listing(dir) => {
                            let src = [blaze_manifest::layout::Entry { name: "lib.rs", is_dir: false }];
                            let top = [blaze_manifest::layout::Entry { name: "src", is_dir: true }];
                            let listed: &[blaze_manifest::layout::Entry<'_>] = if dir.as_str() == "src" { &src } else { &top };
                            reader.listing(dir, Some(listed));
                        }
                    }
                }
            }
            Err(e) => panic!("{e}"),
        }
    };
    let p = Package::from_member(&units.members[0], a());
    assert_eq!(p.edition.syntax, Edition::Edition2024);
    assert!(p.feature("x").is_some(), "no implicit feature `x` in edition 2024");
    assert!(p.feature("y").is_none(), "`dep:y` left an implicit feature `y`");
}

/// Integration tests, examples and benches are units (test.md, "The other targets": "each file
/// its own unit, kind test, depending on the library's PLAIN unit and the dev-dependencies"),
/// through the real reader with `with_test_targets`: a package with `src/lib.rs`, `tests/a.rs`,
/// `tests/b/main.rs`, `examples/e.rs`, `benches/x.rs` and `[[test]] name = "c" path =
/// "other/c.rs" harness = false`. For `test` (`Ask::tests`) the graph holds one unit per target,
/// each depending on the plain library unit (and the dev-dependency `rand`); for `run`/`build`
/// it holds none of them. Prints:
///
/// ```text
/// test: pkg 1.0.0 lib target [] ->  | pkg 1.0.0 test target [] -> rand=rand | pkg 1.0.0 integration-test:a target [] -> pkg=pkg,rand=rand | pkg 1.0.0 integration-test:b target [] -> pkg=pkg,rand=rand | pkg 1.0.0 integration-test:c target [] -> pkg=pkg,rand=rand | pkg 1.0.0 example:e target [] -> pkg=pkg,rand=rand | pkg 1.0.0 bench:x target [] -> pkg=pkg,rand=rand
/// run: pkg 1.0.0 lib target [] ->
/// ```
///
/// (`rand`'s own units sort before `pkg`'s and are left out of the lines.)
#[test]
fn integration_tests_examples_and_benches_are_units() {
    let manifest = "[package]\nname = \"pkg\"\nversion = \"1.0.0\"\nedition = \"2021\"\n\n[dev-dependencies]\nrand = \"1\"\n\n[[test]]\nname = \"c\"\npath = \"other/c.rs\"\nharness = false\n";
    let files: [&'static str; 6] = ["src/lib.rs", "tests/a.rs", "tests/b/main.rs", "examples/e.rs", "benches/x.rs", "other/c.rs"];
    let mut reader = match blaze_manifest::layout::Layout::new("", a()) {
        Ok(r) => r.with_test_targets(),
        Err(e) => panic!("{e}"),
    };
    let units = loop {
        match reader.pass() {
            Ok(blaze_manifest::layout::Pass::Done(units)) => break units,
            Ok(blaze_manifest::layout::Pass::Need(needs)) => {
                for need in needs.iter() {
                    match need {
                        blaze_manifest::layout::Need::Manifest(path) => {
                            let answer = (path.as_str() == "Cargo.toml").then_some(manifest);
                            if let Err(e) = reader.manifest(path, answer) {
                                panic!("{e}");
                            }
                        }
                        blaze_manifest::layout::Need::Listing(dir) => {
                            // Every first component under `dir`, from the file list.
                            let mut listed: StdVec<blaze_manifest::layout::Entry<'static>> = StdVec::new();
                            for path in files {
                                let rest = match dir.is_empty() {
                                    true => Some(path),
                                    false => path.strip_prefix(dir.as_str()).and_then(|rest| rest.strip_prefix('/')),
                                };
                                let Some(rest) = rest else { continue };
                                let (name, is_dir) = match rest.find('/') {
                                    Some(cut) => (&rest[..cut], true),
                                    None => (rest, false),
                                };
                                if !listed.iter().any(|entry| entry.name == name) {
                                    listed.push(blaze_manifest::layout::Entry { name, is_dir });
                                }
                            }
                            reader.listing(dir, (!listed.is_empty()).then_some(listed.as_slice()));
                        }
                    }
                }
            }
            Err(e) => panic!("{e}"),
        }
    };
    let facts = Package::from_member(&units.members[0], a());
    let world = [pkg("pkg").member().dep(dep("rand").dev()), pkg("rand")];
    let text = lock_text(&world);
    let lock = match Lock::read(text.as_bytes(), a()) {
        Ok(lock) => lock,
        Err(e) => panic!("fixture lock: {e}\n{text}"),
    };
    let lock: &'static Lock<'static> = std::boxed::Box::leak(std::boxed::Box::new(lock));
    let root = lock.named("pkg").next().expect("pkg is locked");
    let rand = lock.named("rand").next().expect("rand is locked");
    let graph_for = |tests: bool| -> StdString {
        let mut roots = Vec::new_in(a());
        roots.push(root);
        let ask = Ask { roots, features: Vec::new_in(a()), all_features: false, no_default_features: false, tests };
        let target = mac();
        let host = mac();
        let mut res = Resolution::new(lock, &target, &host, &ask, Resolver::V2, a());
        if let Err(e) = res.member(root, facts.clone()) {
            panic!("{e}");
        }
        for _ in 0..8 {
            match res.pass() {
                Ok(Pass::Done(graph)) => {
                    let lines: StdVec<StdString> = lines(&graph).into_iter().filter(|line| line.starts_with("pkg ")).collect();
                    return lines.join(" | ");
                }
                Ok(Pass::Partial(partial)) => panic!("every manifest is answered, yet {:?} are missing", partial.missing()),
                Ok(Pass::Need(needs)) => {
                    for id in needs.iter() {
                        assert_eq!(*id, rand, "only rand is asked for");
                        if let Err(e) = res.package(*id, Some(pkg("rand").facts())) {
                            panic!("{e}");
                        }
                    }
                }
                Err(e) => panic!("{e}"),
            }
        }
        panic!("no fixpoint in 8 passes");
    };
    let test = graph_for(true);
    std::println!("test: {test}");
    assert_eq!(
        test,
        "pkg 1.0.0 lib target [] ->  | pkg 1.0.0 test target [] -> rand=rand | pkg 1.0.0 integration-test:a target [] -> pkg=pkg,rand=rand | pkg 1.0.0 integration-test:b target [] -> pkg=pkg,rand=rand | pkg 1.0.0 integration-test:c target [] -> pkg=pkg,rand=rand | pkg 1.0.0 example:e target [] -> pkg=pkg,rand=rand | pkg 1.0.0 bench:x target [] -> pkg=pkg,rand=rand"
    );
    let run = graph_for(false);
    std::println!("run: {run}");
    assert_eq!(run, "pkg 1.0.0 lib target [] -> ");
}

/// Rule 3: `default-features = false` drops `default`; another entry with defaults unifies it
/// back.
#[test]
fn defaults_and_their_unification() {
    let s = pkg("s").feature("default", &["std"]).feature("std", &[]);
    let alone = [pkg("app").member().dep(dep("s").no_default()), s.clone()];
    assert_eq!(features_of(&ok(&alone, &opts(Resolver::V2)), "s "), vec!["s 1.0.0 lib target [] -> ".to_string()]);
    let both = [pkg("app").member().dep(dep("s").no_default()).dep(dep("b")), pkg("b").dep(dep("s")), s];
    assert_eq!(features_of(&ok(&both, &opts(Resolver::V2)), "s "), vec!["s 1.0.0 lib target [default,std] -> ".to_string()]);
}

/// Rule 4: the command line's flags.
#[test]
fn command_line_flags() {
    let world = [
        pkg("app").member().feature("default", &["a"]).feature("a", &[]).feature("b", &["s/x"]).dep(dep("s")),
        pkg("s").feature("x", &[]).feature("y", &[]),
    ];
    let mut o = opts(Resolver::V2);
    o.no_default_features = true;
    assert_eq!(features_of(&ok(&world, &o), "app "), vec!["app 1.0.0 lib target [] -> s=s".to_string()]);
    o.features = vec!["b".to_owned(), "s/y".to_owned()];
    let got = ok(&world, &o);
    assert_eq!(features_of(&got, "app "), vec!["app 1.0.0 lib target [b] -> s=s".to_string()]);
    assert_eq!(features_of(&got, "s "), vec!["s 1.0.0 lib target [x,y] -> ".to_string()]);
    let mut all = opts(Resolver::V2);
    all.all_features = true;
    assert_eq!(features_of(&ok(&world, &all), "app "), vec!["app 1.0.0 lib target [a,b,default] -> s=s".to_string()]);
    let mut none = opts(Resolver::V2);
    none.features = vec!["nope".to_owned()];
    assert!(matches!(solve(&world, &none), Err(UnitError::NoRootHasFeature { .. })));
}

/// Rule 5: a diamond unifies into one unit with both features.
#[test]
fn diamond_unifies() {
    let world = [
        pkg("app").member().dep(dep("b")).dep(dep("c")),
        pkg("b").dep(dep("d").features(&["one"])),
        pkg("c").dep(dep("d").features(&["two"])),
        pkg("d").feature("one", &[]).feature("two", &[]).feature("three", &[]),
    ];
    assert_eq!(features_of(&ok(&world, &opts(Resolver::V2)), "d "), vec!["d 1.0.0 lib target [one,two] -> ".to_string()]);
}

/// Rule 6 and 12: under resolver 1 a build-dependency's features reach the normal dependency
/// of the same package; under 2 the host side keeps its own.
#[test]
fn resolver_1_against_2_on_a_build_dependency() {
    let world = [
        pkg("app").member().build().dep(dep("s")).dep(dep("s").build().features(&["std"])),
        pkg("s").feature("std", &[]),
    ];
    assert_eq!(
        ok(&world, &opts(Resolver::V1)),
        vec![
            "app 1.0.0 lib target [] -> s=s".to_string(),
            "app 1.0.0 build host [] -> s=s".to_string(),
            "s 1.0.0 lib target [std] -> ".to_string(),
            "s 1.0.0 lib host [std] -> ".to_string(),
        ]
    );
    assert_eq!(
        ok(&world, &opts(Resolver::V2)),
        vec![
            "app 1.0.0 lib target [] -> s=s".to_string(),
            "app 1.0.0 build host [] -> s=s".to_string(),
            "s 1.0.0 lib target [] -> ".to_string(),
            "s 1.0.0 lib host [std] -> ".to_string(),
        ]
    );
    let graph = match solve(&world, &opts(Resolver::V2)) {
        Ok(g) => g,
        Err(e) => panic!("{e}"),
    };
    let lib = &graph.units[0];
    assert_eq!(lib.build_script.map(|b| graph.units[b.index()].kind.clone()), Some(UnitKind::BuildScript));
}

/// Rule 6: a proc macro's dependencies are on the host, with their own features.
#[test]
fn proc_macro_dependencies_are_decoupled() {
    let world = [
        pkg("app").member().dep(dep("m")).dep(dep("s")),
        pkg("m").proc_macro().dep(dep("s").features(&["full"])),
        pkg("s").feature("full", &[]),
    ];
    let got = ok(&world, &opts(Resolver::V2));
    assert_eq!(
        got,
        vec![
            "app 1.0.0 lib target [] -> m=m,s=s".to_string(),
            "m 1.0.0 proc-macro host [] -> s=s".to_string(),
            "s 1.0.0 lib target [] -> ".to_string(),
            "s 1.0.0 lib host [full] -> ".to_string(),
        ]
    );
    let graph = match solve(&world, &opts(Resolver::V2)) {
        Ok(g) => g,
        Err(e) => panic!("{e}"),
    };
    assert!(graph.units[1].cfg.iter().any(|c| c.name.as_str() == "proc_macro"));
}

/// A member proc macro's library test harness is built for the host with the host side's
/// features, as the proc macro is (cargo compiles a host target, a proc macro's, for the host
/// whatever `--target` says): `app` turns on `s/asm` for the target, `m` (a member proc macro)
/// depends on `s` and has `v` as a dev-dependency with `dev`; `m`'s harness and `m` itself
/// share the host's `s`, never with the target's `asm`, and only the harness has `v` (with
/// `dev`); the build-dependency `b` of `app` is the host's too, on the host's `s`, without
/// `asm`. Before: the harness was a target unit on the target's `s` with `asm` (tempo's
/// precompiles-macros harness reached alloy-primitives with the node's `asm-keccak`,
/// check-g044908). CONTROL: without tests, no harness and no `v`.
#[test]
fn a_proc_macro_test_harness_is_on_the_host_side() {
    let world = [
        pkg("app").member().build().dep(dep("m")).dep(dep("s").features(&["asm"])).dep(dep("b").build()),
        pkg("m").member().proc_macro().dep(dep("s")).dep(dep("v").dev().features(&["dev"])),
        pkg("s").feature("asm", &[]),
        pkg("v").feature("dev", &[]),
        pkg("b").dep(dep("s")),
    ];
    let got = ok(&world, &Opts { tests: true, ..opts(Resolver::V2) });
    for line in ["m 1.0.0 proc-macro host [] -> s=s", "s 1.0.0 lib host [] -> ", "s 1.0.0 lib target [asm] -> ", "v 1.0.0 lib host [dev] -> ", "b 1.0.0 lib host [] -> s=s"] {
        assert!(got.iter().any(|l| l == line), "no `{line}` in {got:#?}");
    }
    let harness: StdVec<&StdString> = got.iter().filter(|l| l.starts_with("m 1.0.0 test ")).collect();
    assert_eq!(harness.len(), 1, "one harness: {got:#?}");
    assert!(harness[0].starts_with("m 1.0.0 test host [] -> ") && harness[0].contains("s=s") && harness[0].contains("v=v"), "the harness is a host unit on `s` and `v`: {got:#?}");
    assert_eq!(got.iter().filter(|l| l.starts_with("s 1.0.0 lib host")).count(), 1, "one host `s`, shared by m, its harness and b: {got:#?}");
    assert!(!got.iter().any(|l| l.contains(" host ") && l.contains("asm")), "no host unit has `asm`: {got:#?}");
    let without = ok(&world, &opts(Resolver::V2));
    assert!(!without.iter().any(|l| l.starts_with("m 1.0.0 test") || l.starts_with("v ")), "CONTROL: no harness, no `v`: {without:#?}");
}

/// Rule 7 under resolver 2: a dependency for another platform takes no part, its features
/// included; resolver 1 counts its features though it builds nothing for it.
#[test]
fn target_specific_dependencies() {
    let world = [
        pkg("app").member().dep(dep("s")).dep(dep("s").on("cfg(windows)").features(&["win"])).dep(dep("w").on("cfg(windows)")),
        pkg("s").feature("win", &[]),
        pkg("w"),
    ];
    assert_eq!(ok(&world, &opts(Resolver::V2)), vec!["app 1.0.0 lib target [] -> s=s".to_string(), "s 1.0.0 lib target [] -> ".to_string()]);
    assert_eq!(features_of(&ok(&world, &opts(Resolver::V1)), "s "), vec!["s 1.0.0 lib target [win] -> ".to_string()]);
    let on_windows = Opts { target: windows(), ..opts(Resolver::V2) };
    let got = ok(&world, &on_windows);
    assert_eq!(features_of(&got, "s "), vec!["s 1.0.0 lib target [win] -> ".to_string()]);
    assert_eq!(features_of(&got, "w "), vec!["w 1.0.0 lib target [] -> ".to_string()]);
    // A target name.
    let named = [pkg("app").member().dep(dep("w").on("x86_64-pc-windows-msvc")), pkg("w")];
    assert_eq!(ok(&named, &opts(Resolver::V2)).len(), 1);
    assert_eq!(ok(&named, &on_windows).len(), 2);
}

/// Rule 6 and 8: dev-dependencies count only with tests; the planted leak is caught.
#[test]
fn dev_dependencies_only_with_tests() {
    let world = [
        pkg("app").member().dep(dep("s")).dep(dep("s").dev().features(&["dev"])).dep(dep("t").dev()),
        pkg("s").feature("dev", &[]),
        pkg("t"),
    ];
    let calm = ok(&world, &opts(Resolver::V2));
    assert_eq!(calm, vec!["app 1.0.0 lib target [] -> s=s".to_string(), "s 1.0.0 lib target [] -> ".to_string()]);
    let leaked = ok(&world, &Opts { plant: Plant::DevLeaks, ..opts(Resolver::V2) });
    assert_ne!(features_of(&leaked, "s "), features_of(&calm, "s "), "the planted leak went unseen");
    let tests = ok(&world, &Opts { tests: true, ..opts(Resolver::V2) });
    assert_eq!(
        tests,
        vec![
            "app 1.0.0 lib target [] -> s=s".to_string(),
            "app 1.0.0 test target [] -> s=s,t=t".to_string(),
            "s 1.0.0 lib target [dev] -> ".to_string(),
            "t 1.0.0 lib target [] -> ".to_string(),
        ]
    );
}

/// Rule 8, Cargo's "Development dependencies": "Dev-dependencies are not used when compiling a
/// package for building, but are used for compiling tests, examples, and benchmarks." A
/// dev-dependency that itself depends on the package under test (a test helper) is no cycle:
/// the library's test unit depends on the helper, and the helper on the library's normal unit:
/// two units of one package.
#[test]
fn a_dev_dependency_on_the_tested_library_is_two_units_not_a_cycle() {
    let world = [pkg("app").member().dep(dep("h").dev()), pkg("h").dep(dep("app"))];
    assert_eq!(
        ok(&world, &Opts { tests: true, ..opts(Resolver::V2) }),
        vec![
            "app 1.0.0 lib target [] -> ".to_string(),
            "app 1.0.0 test target [] -> h=h".to_string(),
            "h 1.0.0 lib target [] -> app=app".to_string(),
        ]
    );
}

/// Rule 9 and 10: a renamed entry is called by its key; two locked versions are told apart by
/// the requirement.
#[test]
fn renamed_and_two_versions() {
    let world = [
        pkg("app").member().dep(dep("s").req("1")).dep(dep("s-two").package("s").req("2").features(&["new"])),
        pkg("s"),
        pkg("s").version("2.0.0").feature("new", &[]),
    ];
    assert_eq!(
        ok(&world, &opts(Resolver::V2)),
        vec![
            "app 1.0.0 lib target [] -> s=s,s_two=s".to_string(),
            "s 1.0.0 lib target [] -> ".to_string(),
            "s 2.0.0 lib target [new] -> ".to_string(),
        ]
    );
}

/// Binaries depend on their package's library.
#[test]
fn bins_depend_on_their_library() {
    let world = [pkg("app").member().bin("cli").dep(dep("here")), pkg("here")];
    let graph = match solve(&world, &opts(Resolver::V2)) {
        Ok(g) => g,
        Err(e) => panic!("{e}"),
    };
    assert_eq!(
        lines(&graph),
        vec!["app 1.0.0 lib target [] -> here=here".to_string(), "app 1.0.0 bin:cli target [] -> app=app,here=here".to_string(), "here 1.0.0 lib target [] -> ".to_string()]
    );
}

/// A missing manifest is never guessed and never a graph: the pass is `Partial`, naming the
/// package, and has no units to compile (states.md 35). Its control: the same world with the
/// manifest answered is `Done`.
#[test]
fn a_missing_manifest_is_partial_not_done() {
    let world = [pkg("app").member().bin("cli").dep(dep("gone")), pkg("gone").missing()];
    let partial = match settle_by(&world, &opts(Resolver::V2), &mut |n| n - 1) {
        Ok(Pass::Partial(partial)) => partial,
        Ok(Pass::Done(_)) => panic!("a graph missing a manifest answered Done"),
        Ok(Pass::Need(needs)) => panic!("settled on a need of {needs:?}"),
        Err(e) => panic!("{e}"),
    };
    assert_eq!(partial.missing().len(), 1);
    let control = [pkg("app").member().bin("cli").dep(dep("gone")), pkg("gone")];
    assert!(matches!(settle_by(&control, &opts(Resolver::V2), &mut |n| n - 1), Ok(Pass::Done(_))));
}

/// Rule 6's defaults.
#[test]
fn resolver_defaults() {
    let ed = |syntax| Some(PackageEdition { syntax, blaze: None });
    assert_eq!(Resolver::of(None, ed(Edition::Edition2018)), Ok(Resolver::V1));
    assert_eq!(Resolver::of(None, ed(Edition::Edition2021)), Ok(Resolver::V2));
    assert_eq!(Resolver::of(None, ed(Edition::Edition2024)), Ok(Resolver::V3));
    assert_eq!(Resolver::of(None, None), Ok(Resolver::V1));
    assert_eq!(Resolver::of(Some("2"), ed(Edition::Edition2015)), Ok(Resolver::V2));
    assert!(Resolver::of(Some("4"), None).is_err());
}

/// Rule 7's predicates on a platform's cfgs.
#[test]
fn cfg_predicates() {
    let m = mac();
    let holds = |p: &str| m.holds(&Platform::parse(p, a()), a());
    assert_eq!(holds("cfg(unix)"), Ok(true));
    assert_eq!(holds("cfg(all(unix, not(windows)))"), Ok(true));
    assert_eq!(holds("cfg(any(windows, target_os = \"linux\"))"), Ok(false));
    assert_eq!(holds("cfg(target_os = \"macos\")"), Ok(true));
    assert_eq!(holds("aarch64-apple-darwin"), Ok(true));
    assert!(holds("cfg(all(unix,").is_err());
}

/// Unit cfgs: platform, features, `debug_assertions`, `test`.
#[test]
fn unit_cfgs() {
    let world = [pkg("app").member().feature("default", &["x"]).feature("x", &[])];
    let graph = match solve(&world, &Opts { tests: true, ..opts(Resolver::V2) }) {
        Ok(g) => g,
        Err(e) => panic!("{e}"),
    };
    let text = |u: usize| -> StdVec<StdString> {
        graph.units[u]
            .cfg
            .iter()
            .map(|c| match &c.value {
                Some(v) => format!("{}=\"{}\"", c.name, v),
                None => c.name.to_string(),
            })
            .collect()
    };
    // The test unit unwinds whatever the profile says. This is Cargo's rule, quoted at resolve.rs
    // `unit_cfg`: "Tests, benchmarks, build scripts, and proc macros ignore the `panic`
    // setting"; `ignores_profile_panic` asks it. So it carries `panic = "unwind"` although
    // `mac()` lists no panic atom.
    assert_eq!(
        text(1),
        vec![
            "debug_assertions",
            "feature=\"default\"",
            "feature=\"x\"",
            "panic=\"unwind\"",
            "target_arch=\"aarch64\"",
            "target_os=\"macos\"",
            "test",
            "unix"
        ]
    );
    // The library unit is not a test. It keeps what the profile's desc says: no `test`, and no
    // panic atom, since `mac()` lists none.
    assert!(!text(0).contains(&"test".to_owned()));
    assert!(!text(0).iter().any(|atom| atom.starts_with("panic")), "a non-test unit's panic is the profile's: {:?}", text(0));
}

/// A drawn workspace: packages `p0..pn`, each depending only on later ones, with features whose
/// values name what exists; `p0` is the one member.
fn draw_world(tape: &Tape) -> StdVec<P> {
    let n = 2 + tape.index(5, "packages");
    let mut world: StdVec<P> = (0..n).map(|i| pkg(&format!("p{i}"))).collect();
    world[0].member = true;
    for i in (0..n).rev() {
        world[i].proc_macro = i > 0 && tape.index(5, "proc-macro") == 0;
        world[i].build = tape.index(4, "build script") == 0;
        for j in i + 1..n {
            if tape.index(3, "depends") != 0 {
                continue;
            }
            let child_features: StdVec<StdString> = world[j].features.iter().map(|f| f.0.clone()).collect();
            let mut d = dep(&format!("p{j}"));
            d.optional = tape.bool("optional");
            d.default_features = tape.bool("defaults");
            d.kind = match tape.index(4, "kind") {
                0 => DepKind::Build,
                1 if i == 0 => DepKind::Dev,
                _ => DepKind::Normal,
            };
            if tape.index(4, "platform") == 0 {
                d.platform = Some(tape.one_of(&["cfg(windows)", "cfg(unix)"], "which").to_string());
            }
            if !child_features.is_empty() && tape.bool("wants a feature") {
                d.features.push(tape.one_of(&child_features, "feature").clone());
            }
            world[i].deps.push(d);
        }
        let nf = tape.index(4, "features");
        for k in 0..nf {
            let mut values: StdVec<StdString> = StdVec::new();
            for _ in 0..tape.index(3, "values") {
                let deps = world[i].deps.clone();
                match tape.index(4, "value kind") {
                    0 if k > 0 => values.push(format!("f{}", tape.index(k, "earlier feature"))),
                    1 if deps.iter().any(|d| d.optional) => {
                        let opt: StdVec<&D> = deps.iter().filter(|d| d.optional).collect();
                        values.push(format!("dep:{}", tape.one_of(&opt, "dep").key));
                    }
                    2 | 3 if !deps.is_empty() => {
                        let d = tape.one_of(&deps, "dep");
                        let j: usize = d.key[1..].parse().expect("p<j>");
                        let fs: StdVec<StdString> = world[j].features.iter().map(|f| f.0.clone()).collect();
                        if fs.is_empty() {
                            continue;
                        }
                        let weak = if d.optional && tape.bool("weak") { "?" } else { "" };
                        values.push(format!("{}{weak}/{}", d.key, tape.one_of(&fs, "dep feature")));
                    }
                    _ => {}
                }
            }
            world[i].features.push((format!("f{k}"), values));
        }
        if !world[i].features.is_empty() && tape.bool("has default") {
            let f = tape.one_of(&world[i].features, "default").0.clone();
            world[i].features.push(("default".to_owned(), vec![f]));
        }
    }
    world
}

/// A unit's identity in the properties: everything but features and deps.
fn identity(line: &str) -> &str {
    line.split(" [").next().unwrap_or(line)
}

fn feature_list(line: &str) -> StdVec<&str> {
    let inner = line.split(" [").nth(1).and_then(|s| s.split(']').next()).unwrap_or("");
    inner.split(',').filter(|s| !s.is_empty()).collect()
}

/// Property: enabling a root feature (or tests) removes no unit and no feature.
#[test]
fn enabling_a_feature_removes_nothing() {
    blaze_sim::check(Config::cases(400), |tape| {
        let world = draw_world(tape);
        let resolver = *tape.one_of(&[Resolver::V1, Resolver::V2], "resolver");
        let root_features: StdVec<StdString> = world[0].features.iter().map(|f| f.0.clone()).collect();
        let mut small = opts(resolver);
        small.no_default_features = tape.bool("no defaults");
        small.tests = tape.bool("tests");
        for f in root_features.iter() {
            if tape.bool("asked") {
                small.features.push(f.clone());
            }
        }
        let mut big = small.clone();
        match tape.index(3, "what grows") {
            0 if !root_features.is_empty() => big.features.push(tape.one_of(&root_features, "added").clone()),
            1 => big.tests = true,
            _ => big.no_default_features = false,
        }
        let s = solve(&world, &small).map_err(|e| Broke::Harness(format!("{e}")))?;
        let b = solve(&world, &big).map_err(|e| Broke::Harness(format!("{e}")))?;
        let (s, b) = (lines(&s), lines(&b));
        for line in s.iter() {
            let Some(bigger) = b.iter().find(|l| identity(l) == identity(line)) else {
                return Err(Broke::Oracle(format!("unit `{}` went away:\n small: {s:?}\n big: {b:?}", identity(line))));
            };
            let have = feature_list(bigger);
            for f in feature_list(line) {
                if !have.contains(&f) {
                    return Err(Broke::Oracle(format!("feature `{f}` of `{}` went away", identity(line))));
                }
            }
        }
        Ok(())
    });
}

/// Property: the graph is a function of the facts, whatever order the work is taken in.
#[test]
fn any_work_order_gives_one_graph() {
    blaze_sim::check(Config::cases(400), |tape| {
        let world = draw_world(tape);
        let mut o = opts(*tape.one_of(&[Resolver::V1, Resolver::V2], "resolver"));
        o.tests = tape.bool("tests");
        let calm = solve(&world, &o).map_err(|e| Broke::Harness(format!("{e}")))?;
        let drawn = solve_by(&world, &o, &mut |n| tape.index(n, "pick")).map_err(|e| Broke::Oracle(format!("{e}")))?;
        let (calm, drawn) = (lines(&calm), lines(&drawn));
        if calm != drawn {
            return Err(Broke::Oracle(format!("orders differ:\n calm: {calm:?}\n drawn: {drawn:?}")));
        }
        Ok(())
    });
}

/// One case of the dev-dependency property: without tests, under resolver 2, a drawn world
/// gives the graph of the same world with its dev-dependency entries deleted.
fn dev_case(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let world = draw_world(tape);
    let mut without = world.clone();
    // Deleted with the values naming them (each drawn entry has a key of its own).
    let dev: StdVec<StdString> = without[0].deps.iter().filter(|d| d.kind == DepKind::Dev).map(|d| d.key.clone()).collect();
    without[0].deps.retain(|d| d.kind != DepKind::Dev);
    for (_, values) in without[0].features.iter_mut() {
        values.retain(|v| {
            let named = v.strip_prefix("dep:").unwrap_or(v).split(['/', '?']).next().unwrap_or("");
            !dev.iter().any(|k| k == named)
        });
    }
    let o = Opts { plant, ..opts(Resolver::V2) };
    let got = solve(&world, &o).map_err(|e| Broke::Harness(format!("{e}")))?;
    let want = solve(&without, &o).map_err(|e| Broke::Harness(format!("{e}")))?;
    let (got, want) = (lines(&got), lines(&want));
    if got != want {
        return Err(Broke::Oracle(format!("dev-dependencies took part:\n with: {got:?}\n without: {want:?}")));
    }
    Ok(())
}

/// Property: dev-dependencies take no part unless tests are built; the planted leak is found.
#[test]
fn dev_dependencies_take_no_part_without_tests() {
    blaze_sim::check(Config::cases(400), |tape| dev_case(tape, Plant::None));
    let verdict = blaze_sim::search(Config::cases(2000), |tape| dev_case(tape, Plant::DevLeaks));
    assert!(matches!(verdict, blaze_sim::Verdict::Failed(_)), "the planted dev leak went unseen");
}

/// Property: the record is its graph: decoding gives the graph back and encoding that again
/// the same bytes (rule 26), and any work order writes equal bytes (rule 25).
#[test]
fn the_record_round_trips_and_is_one_bytes() {
    blaze_sim::check(Config::cases(400), |tape| {
        let world = draw_world(tape);
        let mut o = opts(*tape.one_of(&[Resolver::V1, Resolver::V2], "resolver"));
        o.tests = tape.bool("tests");
        let calm = solve(&world, &o).map_err(|e| Broke::Harness(format!("{e}")))?;
        let drawn = solve_by(&world, &o, &mut |n| tape.index(n, "pick")).map_err(|e| Broke::Oracle(format!("{e}")))?;
        let host = mac();
        let encode = |g: &UnitGraph<'static>| record::encode(g, &o.target, &host, a()).map_err(|e| Broke::Oracle(format!("{e}")));
        let bytes = encode(&calm)?;
        if encode(&drawn)? != bytes {
            return Err(Broke::Oracle("two work orders wrote different bytes".to_string()));
        }
        let back = record::decode(&bytes, a()).map_err(|e| Broke::Oracle(format!("{e}")))?;
        if back.target != o.target || back.host != host {
            return Err(Broke::Oracle("the decoded platforms are not the encoded ones".to_string()));
        }
        let back = back.graph;
        if back.units != calm.units || back.resolver != calm.resolver || back.meta != calm.meta {
            return Err(Broke::Oracle("the decoded graph is not the encoded one".to_string()));
        }
        if encode(&back)? != bytes {
            return Err(Broke::Oracle("encoding the decoded graph gave other bytes".to_string()));
        }
        // Every proper prefix is refused, never read as a smaller graph.
        let cut = tape.index(bytes.len(), "cut");
        if record::decode(&bytes[..cut], a()).is_ok() {
            return Err(Broke::Oracle(format!("a record cut at {cut} of {} decoded", bytes.len())));
        }
        Ok(())
    });
}

/// A two-member workspace's files, `a` asking `b` for `features`; `a`'s manifest carries
/// `extra` (lines that change no resolution: a description, a comment, a key order).
fn workspace_files(features: &str, extra: &str) -> StdVec<(StdString, StdString)> {
    vec![
        ("Cargo.toml".to_string(), "[workspace]\nmembers = [\"a\", \"b\"]\nresolver = \"2\"\n".to_string()),
        (
            "a/Cargo.toml".to_string(),
            format!("[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2021\"\n{extra}\n[dependencies]\nb = {{ path = \"../b\", features = [{features}] }}\n"),
        ),
        ("a/src/lib.rs".to_string(), StdString::new()),
        ("b/Cargo.toml".to_string(), "[package]\nname = \"b\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[features]\nx = []\ny = []\n".to_string()),
        ("b/src/lib.rs".to_string(), StdString::new()),
    ]
}

/// The layout of in-memory `files` (sorted by path), each read put into `source`.
fn lay_out_files(files: &[(StdString, StdString)], source: &mut record::Source) -> blaze_manifest::layout::Units<'static> {
    use blaze_manifest::layout::{Entry, Layout, Need, Pass as LayoutPass};
    let mut reader = Layout::new("", a()).expect("layout");
    for _ in 0..64 {
        let needs = match reader.pass().expect("layout pass") {
            LayoutPass::Done(units) => return units,
            LayoutPass::Need(needs) => needs,
        };
        for need in needs.iter() {
            match need {
                Need::Manifest(path) => {
                    let text = files.iter().find(|(p, _)| p == path.as_str()).map(|(_, t)| t.as_str());
                    source.manifest(path, text);
                    reader.manifest(path, text).expect("manifest");
                }
                Need::Listing(dir) => {
                    let prefix = match dir.is_empty() {
                        true => StdString::new(),
                        false => format!("{dir}/"),
                    };
                    let mut entries: StdVec<Entry<'_>> = StdVec::new();
                    for (path, _) in files.iter() {
                        let Some(rest) = path.strip_prefix(prefix.as_str()) else { continue };
                        let (name, is_dir) = match rest.find('/') {
                            Some(cut) => (&rest[..cut], true),
                            None => (rest, false),
                        };
                        if !entries.iter().any(|e| e.name == name) {
                            entries.push(Entry { name, is_dir });
                        }
                    }
                    let listed = (!entries.is_empty() || dir.is_empty()).then_some(entries.as_slice());
                    source.listing(dir, listed);
                    reader.listing(dir, listed);
                }
            }
        }
    }
    panic!("no layout in 64 passes");
}

/// The record's bytes and source digest of the workspace of `files`, all members asked.
fn graph_record(files: &[(StdString, StdString)]) -> (StdVec<u8>, [u8; 32]) {
    let lock_text = "version = 4\n\n[[package]]\nname = \"a\"\nversion = \"0.1.0\"\ndependencies = [\n \"b\",\n]\n\n[[package]]\nname = \"b\"\nversion = \"0.1.0\"\n";
    let lock: &'static Lock<'static> = std::boxed::Box::leak(std::boxed::Box::new(Lock::read(lock_text.as_bytes(), a()).expect("lock")));
    let mut source = record::Source::new();
    source.lock(lock_text.as_bytes());
    let units = lay_out_files(files, &mut source);
    let mut roots = Vec::new_in(a());
    let mut members = StdVec::new();
    for m in units.members.iter() {
        let id = lock.named(&m.name).next().expect("member locked");
        roots.push(id);
        members.push((id, Package::from_member(m, a())));
    }
    let ask = Ask { roots, features: Vec::new_in(a()), all_features: false, no_default_features: false, tests: false };
    let target = mac();
    source.platform("target", &target).platform("host", &target).ask(&ask);
    let mut res = Resolution::new(lock, &target, &target, &ask, Resolver::V2, a());
    for (id, p) in members {
        res.member(id, p).expect("member");
    }
    let graph = match res.pass().expect("pass") {
        Pass::Done(graph) => graph,
        Pass::Need(_) | Pass::Partial(_) => panic!("a workspace of members needs nothing"),
    };
    (record::encode(&graph, &target, &target, a()).expect("its own cfg").iter().copied().collect(), source.finish().0)
}

/// Early cutoff: an edit to a member's manifest that changes no resolution changes the
/// record's source (its key) and leaves its bytes equal, so nothing keyed by the record's
/// digest downstream reruns; the control, an edit that does change resolution, changes them.
#[test]
fn an_edit_that_changes_no_resolution_leaves_the_record_equal() {
    let (before, key_before) = graph_record(&workspace_files("\"x\"", ""));
    let (after, key_after) = graph_record(&workspace_files("\"x\"", "# a comment\n\n# another\n"));
    assert_ne!(key_before, key_after, "the edit must change what was read");
    assert_eq!(before, after, "an edit that changes no resolution changed the record");
    // The metadata crates read as `CARGO_PKG_*` is the one thing kept beyond resolution: a
    // description edit changes the record (record.rs), a comment beside it does not.
    let (described, _) = graph_record(&workspace_files("\"x\"", "description = \"now described\"\n"));
    let (described_again, _) = graph_record(&workspace_files("\"x\"", "description = \"now described\" # a comment\n"));
    assert_ne!(before, described, "a description edit must reach CARGO_PKG_DESCRIPTION");
    assert_eq!(described, described_again, "a comment beside the description changed the record");
    let (asked_more, _) = graph_record(&workspace_files("\"x\", \"y\"", ""));
    assert_ne!(before, asked_more, "the control: asking b for `y` must change the record");
}

/// `[patch.crates-io]` path packages and transitive path dependencies outside `[workspace.members]`
/// resolve via `Resolution::package` (not `Resolution::member`), including multi-version lock
/// disambiguation when one version of a crate is patched to a path package while another remains
/// from crates.io, and never pull in the patched package's dev-dependencies or test harnesses.
#[test]
fn patched_path_package_and_multi_version_disambiguation() {
    let lock_text = "\
version = 4

[[package]]
name = \"app\"
version = \"0.1.0\"
dependencies = [
 \"foo 1.0.0\",
 \"foo 2.0.0\",
]

[[package]]
name = \"foo\"
version = \"1.0.0\"
dependencies = [
 \"helper\",
]

[[package]]
name = \"foo\"
version = \"2.0.0\"
source = \"registry+https://github.com/rust-lang/crates.io-index\"
checksum = \"0000000000000000000000000000000000000000000000000000000000000000\"

[[package]]
name = \"helper\"
version = \"0.1.0\"
";
    let lock = Lock::read(lock_text.as_bytes(), a()).expect("lock");
    let app_id = lock.named("app").next().expect("app");
    let foo1_id = lock.named("foo").find(|id| format!("{}", lock.package(*id).version) == "1.0.0").expect("foo 1.0.0");
    let foo2_id = lock.named("foo").find(|id| format!("{}", lock.package(*id).version) == "2.0.0").expect("foo 2.0.0");
    let helper_id = lock.named("helper").next().expect("helper");

    let empty_pkg = |name: &str, deps: StdVec<Dep<'static>>| -> Package<'static> {
        let mut d = Vec::new_in(a());
        for dep in deps {
            d.push(dep);
        }
        Package {
            name: t(name),
            edition: PackageEdition { syntax: Edition::Edition2021, blaze: None },
            lib: Some(Lib { crate_name: t(name), root: t("src/lib.rs"), proc_macro: false }),
            bins: Vec::new_in(a()),
            extras: Vec::new_in(a()),
            build: None,
            links: None,
            features: Vec::new_in(a()),
            deps: d,
            meta: crate::PackageMeta::unstated(a()),
            patched_from: Vec::new_in(a()),
        }
    };
    let make_dep = |key: &str, pkg: &str, kind: DepKind, source: DepSource<'static>| Dep {
        key: t(key),
        package: t(pkg),
        kind,
        platform: None,
        optional: false,
        default_features: true,
        features: Vec::new_in(a()),
        source,
    };

    let app_pkg = empty_pkg(
        "app",
        vec![
            make_dep("foo", "foo", DepKind::Normal, DepSource::Registry { req: Some(t("1.0")) }),
            make_dep("foo2", "foo", DepKind::Normal, DepSource::Registry { req: Some(t("2.0")) }),
        ],
    );
    let foo1_pkg = empty_pkg(
        "foo",
        vec![
            make_dep("helper", "helper", DepKind::Normal, DepSource::Path),
            // Not in Cargo.lock: must be ignored because foo 1.0.0 is NOT a workspace member.
            make_dep("missing_dev", "missing_dev", DepKind::Dev, DepSource::Registry { req: Some(t("1.0")) }),
        ],
    );
    let foo2_pkg = empty_pkg("foo", vec![]);
    let helper_pkg = empty_pkg("helper", vec![]);

    let mut roots = Vec::new_in(a());
    roots.push(app_id);
    let ask = Ask { roots, features: Vec::new_in(a()), all_features: false, no_default_features: false, tests: true };
    let target = mac();
    let mut res = Resolution::new(&lock, &target, &target, &ask, Resolver::V2, a());
    res.member(app_id, app_pkg.clone()).expect("member app");
    res.package(foo1_id, Some(foo1_pkg)).expect("patched foo 1.0.0");
    res.package(helper_id, Some(helper_pkg)).expect("transitive path helper");
    let needed = match res.pass().expect("pass 1") {
        Pass::Need(ids) => ids,
        Pass::Done(_) | Pass::Partial(_) => panic!("expected foo 2.0.0 to be requested"),
    };
    assert_eq!(needed.as_slice(), &[foo2_id], "only unpatched registry foo 2.0.0 should be requested");
    res.package(foo2_id, Some(foo2_pkg)).expect("registry foo 2.0.0");
    let graph = match res.pass().expect("pass 2") {
        Pass::Done(g) => g,
        Pass::Need(_) | Pass::Partial(_) => panic!("graph should resolve cleanly"),
    };
    let summary: StdVec<StdString> = graph.units.iter().map(|u| format!("{} {} {:?}", u.name.as_str(), u.version.as_str(), u.kind)).collect();
    assert!(summary.iter().any(|s| s == "foo 1.0.0 Lib"), "patched foo 1.0.0 missing: {summary:?}");
    assert!(summary.iter().any(|s| s == "foo 2.0.0 Lib"), "registry foo 2.0.0 missing: {summary:?}");
    assert!(summary.iter().any(|s| s == "helper 0.1.0 Lib"), "transitive helper missing: {summary:?}");
    assert!(!summary.iter().any(|s| s.starts_with("foo 1.0.0 Test")), "patched package must not get a test harness: {summary:?}");

    // CONTROL: without supplying the patched path package, `Pass::Partial` refuses with `foo 1.0.0` missing.
    let mut roots2 = Vec::new_in(a());
    roots2.push(app_id);
    let ask2 = Ask { roots: roots2, features: Vec::new_in(a()), all_features: false, no_default_features: false, tests: false };
    let mut missing = Resolution::new(&lock, &target, &target, &ask2, Resolver::V2, a());
    missing.member(app_id, app_pkg).expect("member app");
    match missing.pass().expect("pass missing") {
        Pass::Need(ids) => {
            for id in ids.iter().copied() {
                if id == foo2_id {
                    missing.package(id, Some(empty_pkg("foo", vec![]))).expect("foo2");
                } else {
                    missing.package(id, None).expect("unreadable");
                }
            }
        }
        Pass::Partial(_) => {}
        Pass::Done(_) => panic!("should not succeed without patched path package"),
    }
    let partial = match missing.pass().expect("pass partial") {
        Pass::Partial(p) => p,
        other => panic!("expected Partial, got {}", matches!(other, Pass::Done(_))),
    };
    assert!(partial.missing().contains(&foo1_id), "missing should include foo 1.0.0: {:?}", partial.missing());
}

#[test]
fn disambiguates_same_name_git_deps_patched_to_different_paths() {
    const LOCK: &str = r#"version = 4

[[package]]
name = "app"
version = "0.1.0"
dependencies = [
 "foo 1.0.0",
 "foo 2.0.0",
]

[[package]]
name = "foo"
version = "1.0.0"

[[package]]
name = "foo"
version = "2.0.0"
"#;
    let lock = Lock::read(LOCK.as_bytes(), a()).expect("lock");
    let app_id = lock.named("app").next().expect("app");
    let foo1_id = lock.named("foo").find(|id| lock.package(*id).version.major == 1).expect("foo 1.0.0");
    let foo2_id = lock.named("foo").find(|id| lock.package(*id).version.major == 2).expect("foo 2.0.0");

    let make_dep = |key: &str, url: &str| Dep {
        key: t(key),
        package: t("foo"),
        kind: DepKind::Normal,
        platform: None,
        optional: false,
        default_features: true,
        features: Vec::new_in(a()),
        source: DepSource::Git { url: t(url) },
    };
    let make_pkg = |name: &str, deps: StdVec<Dep<'static>>, patched_from: Option<&str>| -> Package<'static> {
        let mut d = Vec::new_in(a());
        for dep in deps {
            d.push(dep);
        }
        let mut pf = Vec::new_in(a());
        if let Some(src) = patched_from {
            pf.push(t(src));
        }
        Package {
            name: t(name),
            edition: PackageEdition { syntax: Edition::Edition2021, blaze: None },
            lib: Some(Lib { crate_name: t(name), root: t("src/lib.rs"), proc_macro: false }),
            bins: Vec::new_in(a()),
            extras: Vec::new_in(a()),
            build: None,
            links: None,
            features: Vec::new_in(a()),
            deps: d,
            meta: crate::PackageMeta::unstated(a()),
            patched_from: pf,
        }
    };
    let app_pkg = make_pkg(
        "app",
        vec![
            make_dep("foo_a", "https://example.com/a/foo.git"),
            make_dep("foo_b", "https://example.com/b/foo"),
        ],
        None,
    );
    let foo1_pkg = make_pkg("foo", vec![], Some("https://example.com/a/foo"));
    let foo2_pkg = make_pkg("foo", vec![], Some("https://example.com/b/foo.git"));

    let target = mac();
    let mut roots = Vec::new_in(a());
    roots.push(app_id);
    let ask = Ask { roots, features: Vec::new_in(a()), all_features: false, no_default_features: false, tests: false };
    let mut res = Resolution::new(&lock, &target, &target, &ask, Resolver::V2, a());
    res.member(app_id, app_pkg).expect("member app");
    res.package(foo1_id, Some(foo1_pkg)).expect("foo1");
    res.package(foo2_id, Some(foo2_pkg)).expect("foo2");
    let graph = match res.pass().expect("resolve patched same-name git deps") {
        Pass::Done(g) => g,
        Pass::Need(_) | Pass::Partial(_) => panic!("expected clean resolution"),
    };
    let app_unit = graph.units.iter().find(|u| u.name.as_str() == "app").expect("app unit");
    let dep_a = app_unit.deps.iter().find(|d| d.name.as_str() == "foo_a").expect("foo_a dep");
    let dep_b = app_unit.deps.iter().find(|d| d.name.as_str() == "foo_b").expect("foo_b dep");
    assert_eq!(graph.units[dep_a.unit.index()].version.as_str(), "1.0.0");
    assert_eq!(graph.units[dep_b.unit.index()].version.as_str(), "2.0.0");
}
