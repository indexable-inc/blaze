//! `unit_crates`: which units become crates, their dependencies by index, and the refusal of a
//! dependency on a unit not kept.

use std::vec::Vec;

use blaze_manifest::lock::{Lock, LockId};
use blaze_own::{AVec, Alloc};
use blaze_toolchain::record::{CrateIn, TargetIn, UnitIn};
use blaze_toolchain::{Role, ToolchainView, UnitKind as ToolchainKind};
use blaze_unit_graph::{CfgAtom, Resolver, Text, Unit, UnitDep, UnitGraph, UnitId, UnitKind};
use edition::PackageEdition;

use super::{Member, Origin, RefusedCrate, UnitsError, unit_crates};

/// The workspace member of lock package `id`, at `m<id>` under the workspace.
fn member(id: u32) -> Member<'static> {
    let dir: &'static str = std::boxed::Box::leak(std::format!("m{id}").into_boxed_str());
    Member::new(LockId(id), dir).expect("a plain dir")
}

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
    let lib = |krate: u32, deps: &'static [(&'static str, u32)]| UnitIn { krate, kind: ToolchainKind::Lib, for_host: false, root: "src/lib.rs", features: &[], deps, sysroot: blaze_toolchain::ImplicitSysroot::Core };
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
    }
}

fn graph(units: Vec<Unit<'static>>) -> UnitGraph<'static> {
    let mut all = AVec::new_in(a());
    all.extend(units);
    UnitGraph { units: all, resolver: Resolver::V3, meta: AVec::new_in(a()), left_out: AVec::new_in(a()) }
}

#[test]
fn a_proc_macro_is_kept_without_its_host_deps_and_a_dropped_dep_is_refused() {
    let bytes = toolchain();
    let view = ToolchainView::open(&bytes, ToolchainView::id_of_bytes(&bytes)).expect("its own id");
    let lock = Lock::read(LOCK.as_bytes(), a()).expect("the lock");
    let members = [member(0)];

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

/// A member's test units (the graph makes them when tests are asked) are crates of the list:
/// roots, with `test` in their cfg, their dev-dependencies followed, each under its own key.
/// Before: only `Lib` and `Bin` units were roots and kept, so `--tests` solved no unit with
/// `test` on (the names here were `core, alloc, std, app`).
///
/// Expected line: `test units::tests::a_members_test_units_are_crates_with_test_in_their_cfg ... ok`.
#[test]
fn a_members_test_units_are_crates_with_test_in_their_cfg() {
    let bytes = toolchain();
    let view = ToolchainView::open(&bytes, ToolchainView::id_of_bytes(&bytes)).expect("its own id");
    let lock = Lock::read(LOCK.as_bytes(), a()).expect("the lock");
    let members = [member(0)];
    let test_of = |bin: Option<&str>, deps: &[(&str, u32)]| {
        let mut u = unit(0, "app", UnitKind::Test { bin: bin.map(t) }, false, deps);
        u.cfg.push(CfgAtom::flag("test", a()));
        u
    };
    // app's library, its test unit on the dev-dependency dep, and the test units of two binaries.
    let g = graph(std::vec![
        unit(0, "app", UnitKind::Lib, false, &[]),
        unit(1, "dep", UnitKind::Lib, false, &[]),
        test_of(None, &[("dep", 1)]),
        test_of(Some("one"), &[("app", 0)]),
        test_of(Some("two"), &[("app", 0)]),
    ]);
    let crates = unit_crates(&g, &lock, &members, view, TRIPLE, a()).expect("the crates");
    let names: Vec<&str> = crates.crates.iter().map(|c| c.name).collect();
    assert_eq!(names, ["core", "alloc", "std", "app", "dep", "app", "app", "app"]);
    let has_test = |at: usize| crates.crates[at].cfg.iter().any(|(name, value)| *name == "test" && value.is_none());
    assert!(!has_test(3), "the library's own unit is compiled without `test`");
    assert!(has_test(5) && has_test(6) && has_test(7), "each test unit is compiled with `test`");
    assert_eq!(crates.crates[5].origin, Origin::Unit(UnitId(2)));
    assert_eq!(crates.crates[5].deps.as_slice(), [("dep", 4)], "the dev-dependency is followed from the test unit");
    assert_eq!(crates.crates[6].deps.as_slice(), [("app", 3)]);
    let keys: std::collections::BTreeSet<_> = crates.crates.iter().map(|c| c.key).collect();
    assert_eq!(keys.len(), 8, "every crate's key is its own: the library's, its test unit's and each binary's test unit's");
}

/// D89 (a): a MEMBER's proc macro (`pm`, package 2) is built for `wasm32-wasip1` (rule 10), so
/// its dependency `syn` is a crate of the macro side: the same unit under that target's cfg
/// and a key of its own (rule 21), with the side's `core`, `alloc`, `std` and `proc_macro`.
/// The proc macro's `deps` point at those copies. Before, its deps were empty, so tempo's
/// `tempo_precompiles_macros` named `syn::Expr` into nothing (census brief 5). Without a
/// `wasm32-wasip1` cut the list is still made: the normal member `app` is a crate as before,
/// and the proc macro alone is refused by name (rule 20), never given the host's crates.
///
/// Expected line: `test units::tests::a_member_proc_macro_resolves_its_dependencies_on_the_macro_side ... ok`.
#[test]
fn a_member_proc_macro_resolves_its_dependencies_on_the_macro_side() {
    const MACRO: &str = "wasm32-wasip1";
    let toolchain_with = |macro_side: bool| -> Vec<u8> {
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
        let lib = |krate: u32, deps: &'static [(&'static str, u32)]| UnitIn { krate, kind: ToolchainKind::Lib, for_host: false, root: "src/lib.rs", features: &[], deps, sysroot: blaze_toolchain::ImplicitSysroot::Core };
        let units = [lib(0, &[]), lib(1, &[("core", 0)]), lib(2, &[("alloc", 1), ("core", 0)]), lib(3, &[("core", 0), ("std", 2)]), lib(4, &[("core", 0)])];
        let host_cfg = [("target_os", Some("macos")), ("unix", None)];
        let wasm_cfg = [
            ("target_arch", Some("wasm32")),
            ("target_endian", Some("little")),
            ("target_env", Some("p1")),
            ("target_family", Some("wasm")),
            ("target_os", Some("wasi")),
            ("target_pointer_width", Some("32")),
        ];
        let host = TargetIn { triple: TRIPLE, units: &units, cfg: &host_cfg };
        let wasm = TargetIn { triple: MACRO, units: &units, cfg: &wasm_cfg };
        let targets: &[TargetIn<'_>] = if macro_side { &[host, wasm] } else { &[host] };
        blaze_toolchain::record::write(&crates, targets, a()).expect("a record").to_vec()
    };
    let lock = Lock::read(LOCK.as_bytes(), a()).expect("the lock");
    // app and its proc macro pm are both members; pm's host-side syn is followed onto the
    // macro side, app's target-side syn stays the target's.
    let members = [member(0), member(2)];
    let g = || {
        graph(std::vec![
            unit(0, "app", UnitKind::Lib, false, &[("pm", 2), ("syn", 1)]),
            unit(3, "syn", UnitKind::Lib, false, &[]),
            unit(2, "pm", UnitKind::ProcMacro, true, &[("syn", 3)]),
            unit(3, "syn", UnitKind::Lib, true, &[]),
        ])
    };

    let bytes = toolchain_with(true);
    let view = ToolchainView::open(&bytes, ToolchainView::id_of_bytes(&bytes)).expect("its own id");
    let g1 = g();
    let crates = unit_crates(&g1, &lock, &members, view, TRIPLE, a()).expect("the crates");
    let names: Vec<&str> = crates.crates.iter().map(|c| c.name).collect();
    assert_eq!(names, ["core", "alloc", "std", "app", "syn", "pm", "core", "alloc", "std", "proc_macro", "syn"]);
    assert!(crates.refused.is_empty(), "with the cut, nothing is refused");
    let (target_syn, pm, side_core, side_syn) = (&crates.crates[4], &crates.crates[5], &crates.crates[6], &crates.crates[10]);
    assert_eq!(side_syn.origin, Origin::Unit(UnitId(3)));
    assert_eq!(side_core.origin, Origin::Toolchain { unit: 0, krate: 0 });
    assert_eq!(side_core.root.as_str(), "/macro/toolchain/0/core/src/lib.rs", "never the target side's `/toolchain/0/`");
    assert_eq!(pm.deps.as_slice(), [("syn", 10), ("core", 6), ("alloc", 7), ("std", 8), ("proc_macro", 9)]);
    assert_eq!(side_syn.deps.as_slice(), [("core", 6), ("alloc", 7), ("std", 8), ("proc_macro", 9)]);
    assert_eq!(crates.crates[3].deps.as_slice(), [("pm", 5), ("syn", 4)], "the target side is unchanged");
    assert_ne!(side_syn.key, target_syn.key, "the macro side's syn is another crate than the target's");
    assert_ne!(side_core.key, crates.crates[0].key, "one pack on two sides: two keys (rule 21)");
    assert!(side_syn.cfg.contains(&("target_arch", Some("wasm32"))) && !side_syn.cfg.contains(&("unix", None)), "the macro side's cfg is wasm32-wasip1's");
    assert!(target_syn.cfg.contains(&("unix", None)));
    let keys: std::collections::BTreeSet<_> = crates.crates.iter().map(|c| c.key).collect();
    assert_eq!(keys.len(), crates.crates.len(), "every crate's key is its own");

    // No wasm32-wasip1 cut: the list is made; the normal member `app` and its target-side
    // `syn` are crates as before, and the member proc macro alone is refused by name.
    let bytes = toolchain_with(false);
    let view = ToolchainView::open(&bytes, ToolchainView::id_of_bytes(&bytes)).expect("its own id");
    let g2 = g();
    let crates = unit_crates(&g2, &lock, &members, view, TRIPLE, a()).expect("the list is made: one crate's refusal is that crate's");
    let names: Vec<&str> = crates.crates.iter().map(|c| c.name).collect();
    assert_eq!(names, ["core", "alloc", "std", "app", "syn", "pm"]);
    assert_eq!(crates.crates[3].deps.as_slice(), [("pm", 5), ("syn", 4)]);
    assert_eq!(crates.refusal(3), None, "the normal member is not refused");
    assert!(crates.crates[5].deps.is_empty(), "the refused proc macro is given no host crates");
    assert_eq!(
        crates.refused.as_slice(),
        [RefusedCrate { krate: 5, why: UnitsError::NoMacroToolchain { krate: "pm", target: MACRO } }]
    );
    assert_eq!(crates.refusal(5), Some(&UnitsError::NoMacroToolchain { krate: "pm", target: MACRO }));
}
