//! `unit_crates`: which units become crates, their dependencies by index, and the refusal of a
//! dependency on a unit not kept.

use std::vec::Vec;

use blaze_manifest::lock::{Lock, LockId};
use blaze_own::{AVec, Alloc};
use blaze_toolchain::record::{CrateIn, TargetIn, UnitIn};
use blaze_toolchain::{Role, ToolchainView, UnitKind as ToolchainKind};
use blaze_unit_graph::{CfgAtom, Resolver, Text, Unit, UnitDep, UnitGraph, UnitId, UnitKind};
use edition::PackageEdition;

use super::{Origin, UnitsError, unit_crates};

const TRIPLE: &str = "aarch64-apple-darwin";

fn a() -> Alloc<'static> {
    Alloc::global()
}

fn t(s: &str) -> Text<'static> {
    Text::from_str_in(s, a())
}

/// core, alloc on core, std on both: one target.
fn toolchain() -> Vec<u8> {
    let krate = |name: &'static str, role: Role, digest: u8| CrateIn {
        name,
        version: "0.0.0",
        edition: "2024",
        lib: name,
        root: "src/lib.rs",
        dir: name,
        role,
        pack_kind: 3,
        pack_len: 1,
        pack: [digest; 32],
    };
    let crates = [
        krate("core", Role::Core, 1),
        krate("alloc", Role::Alloc, 2),
        krate("std", Role::Std, 3),
        krate("proc_macro", Role::ProcMacro, 4),
        krate("bstd", Role::Bstd, 5),
    ];
    let lib = |krate: u32, deps: &'static [(&'static str, u32)]| UnitIn { krate, kind: ToolchainKind::Lib, for_host: false, root: "src/lib.rs", features: &[], deps };
    // proc_macro and bstd: the sysroot names every role; neither is reached here.
    let units = [lib(0, &[]), lib(1, &[("core", 0)]), lib(2, &[("alloc", 1), ("core", 0)]), lib(3, &[("core", 0), ("std", 2)]), lib(4, &[("core", 0)])];
    let cfg = [("target_os", Some("macos")), ("unix", None)];
    let targets = [TargetIn { triple: TRIPLE, units: &units, cfg: &cfg }];
    blaze_toolchain::record::write(&crates, &targets, a()).expect("a record").to_vec()
}

const LOCK: &str = r#"version = 4

[[package]]
name = "app"
version = "0.1.0"
dependencies = [
 "dep",
 "pm",
 "syn",
]

[[package]]
name = "dep"
version = "1.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "0000000000000000000000000000000000000000000000000000000000000000"

[[package]]
name = "pm"
version = "1.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "0000000000000000000000000000000000000000000000000000000000000000"
dependencies = [
 "syn",
]

[[package]]
name = "syn"
version = "2.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "0000000000000000000000000000000000000000000000000000000000000000"
"#;

fn unit(package: u32, name: &str, kind: UnitKind<'static>, for_host: bool, deps: &[(&str, u32)]) -> Unit<'static> {
    let mut d = AVec::new_in(a());
    for (n, u) in deps {
        d.push(UnitDep { name: t(n), unit: UnitId(*u) });
    }
    let mut cfg = AVec::new_in(a());
    cfg.push(CfgAtom::flag("unix", a()));
    Unit {
        package: LockId(package),
        name: t(name),
        version: t("1.0.0"),
        kind,
        for_host,
        edition: PackageEdition::of_manifest("2021").expect("an edition"),
        crate_name: t(name),
        root: t("src/lib.rs"),
        features: AVec::new_in(a()),
        cfg,
        deps: d,
        build_script: None,
        links: None,
        incomplete: false,
    }
}

fn graph(units: Vec<Unit<'static>>) -> UnitGraph<'static> {
    let mut all = AVec::new_in(a());
    all.extend(units);
    UnitGraph { units: all, unavailable: AVec::new_in(a()), resolver: Resolver::V3 }
}

#[test]
fn a_proc_macro_is_kept_without_its_host_deps_and_a_dropped_dep_is_refused() {
    let bytes = toolchain();
    let view = ToolchainView::open(&bytes, ToolchainView::id_of_bytes(&bytes)).expect("its own id");
    let lock = Lock::read(LOCK.as_bytes(), a()).expect("the lock");
    let members = [LockId(0)];

    // app (lib, with a build script) on dep and on the proc macro pm, whose host dep syn is
    // not followed.
    let mut app = unit(0, "app", UnitKind::Lib, false, &[("dep", 1), ("pm", 2)]);
    app.build_script = Some(UnitId(4));
    let g = graph(std::vec![
        app,
        unit(1, "dep", UnitKind::Lib, false, &[]),
        unit(2, "pm", UnitKind::ProcMacro, true, &[("syn", 3)]),
        unit(3, "syn", UnitKind::Lib, true, &[]),
        unit(0, "app", UnitKind::BuildScript, true, &[]),
    ]);
    let crates = unit_crates(&g, &lock, &members, view, TRIPLE, a()).expect("the crates");
    let names: Vec<&str> = crates.crates.iter().map(|c| c.name).collect();
    assert_eq!(names, ["core", "alloc", "std", "app", "dep", "pm"]);
    assert_eq!(crates.toolchain, 3);
    assert_eq!(crates.build_scripts, 1);
    let app = &crates.crates[3];
    assert_eq!(app.origin, Origin::Unit(UnitId(0)));
    assert_eq!(app.deps.as_slice(), [("dep", 4), ("pm", 5)]);
    assert_eq!(app.root.as_str(), "/package/0/src/lib.rs");
    assert!(crates.crates[5].deps.is_empty(), "a proc macro's host deps are not followed");
    assert!(crates.crates[5].implicit.proc_macro);
    assert_eq!(crates.crates[2].deps.as_slice(), [("alloc", 1), ("core", 0)]);
    assert_eq!(crates.sysroot.std.map(|it| it.0), Some(2));
    assert_eq!(crates.sysroot.bstd, None);
    let keys: std::collections::BTreeSet<_> = crates.crates.iter().map(|c| c.key).collect();
    assert_eq!(keys.len(), 6, "every crate's key is its own");

    // app on syn directly: syn is a host unit, not kept.
    let g = graph(std::vec![unit(0, "app", UnitKind::Lib, false, &[("syn", 1)]), unit(3, "syn", UnitKind::Lib, true, &[])]);
    let refused = unit_crates(&g, &lock, &members, view, TRIPLE, a()).err();
    assert_eq!(
        refused,
        Some(UnitsError::DependencyNotKept { krate: "app", kind: "lib", dep: "syn", dep_kind: "lib", dep_for_host: true })
    );
}
