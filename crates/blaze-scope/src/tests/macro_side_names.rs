//! The macro side, end to end through the names (census 13 brief 5, D89 (a)): a workspace
//! member's proc macro names its own dependencies, resolved as crates of the `wasm32-wasip1`
//! side under that target's cfg.
//!
//! Tempo's `tempo_precompiles_macros` (lib.rs:29 `Expr`, :35 `Parse`, :60 `TokenStream`) named
//! `syn`, `quote` and `proc_macro2` into an extern prelude with nothing in it: every such path
//! was NotFound at segment 0 (94 heads, 36 `quote!` calls, 16 `format_ident!`).
//!
//! The Reference, "Extern prelude": "External crates imported with `extern crate` in the root
//! module or provided to the compiler (as with the `--extern` flag with `rustc`) are added to
//! the extern prelude", and "Paths": the first segment of a path is looked up in the scope,
//! the extern prelude among it. A proc macro is compiled for the target it runs on (blaze:
//! `wasm32-wasip1`, AGENTS.md rule 10), so its dependencies are compiled, and their
//! `#[cfg]`s evaluated, for that target ("Conditional compilation": `target_arch` is "the
//! target's CPU architecture").

use blaze_manifest::lock::{Lock, LockId};
use blaze_own::{AVec, Alloc};
use blaze_surface::{Namespace, PathKind};
use blaze_toolchain::record::{CrateIn, TargetIn, UnitIn};
use blaze_toolchain::{Role, ToolchainView, UnitKind as ToolchainKind};
use blaze_unit_graph::{CfgAtom, Resolver, Text, Unit, UnitDep, UnitGraph, UnitId, UnitKind};
use edition::PackageEdition;

use super::{Member, UnitCrates, UnitsError, unit_crates};
use crate::names::{Why, Written};
use crate::resolve::{Names, SourceTexts, resolve_crates};
use crate::task::Serial;

pub(super) const HOST: &str = "aarch64-apple-darwin";
const MACRO: &str = "wasm32-wasip1";

pub(super) fn a() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

fn t(s: &str) -> Text<'static> {
    Text::from_str_in(s, a())
}

pub(super) const LOCK: &str = r#"version = 4

[[package]]
name = "app"
version = "0.1.0"
dependencies = [
 "dep",
 "pm",
]

[[package]]
name = "dep"
version = "1.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "0000000000000000000000000000000000000000000000000000000000000000"

[[package]]
name = "pm"
version = "0.1.0"
dependencies = [
 "dep",
]
"#;

/// Every toolchain crate and the member `app`: nothing of their own is asked.
const EMPTY: &str = "#![no_core]
";

/// The small library both sides build: one item on every side, one per side by `target_arch`.
const DEP: &str = "#![no_core]
pub struct Expr;
#[cfg(target_arch = \"wasm32\")]
pub struct OnWasm;
#[cfg(not(target_arch = \"wasm32\"))]
pub struct OnHost;
";

/// The member proc macro: its paths are asked from its root below.
const PM: &str = "#![no_core]
";

/// One toolchain record: core, alloc, std and proc_macro (and bstd, which the sysroot names)
/// for the host and, when `macro_side`, the same units for `wasm32-wasip1` under its cfg.
pub(super) fn toolchain(macro_side: bool) -> AVec<'static, u8> {
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
    let host_cfg = [("target_arch", Some("aarch64")), ("target_os", Some("macos")), ("unix", None)];
    let wasm_cfg = [
        ("target_arch", Some("wasm32")),
        ("target_endian", Some("little")),
        ("target_env", Some("p1")),
        ("target_family", Some("wasm")),
        ("target_os", Some("wasi")),
        ("target_pointer_width", Some("32")),
    ];
    let host = TargetIn { triple: HOST, units: &units, cfg: &host_cfg };
    let wasm = TargetIn { triple: MACRO, units: &units, cfg: &wasm_cfg };
    let targets: &[TargetIn<'_>] = if macro_side { &[host, wasm] } else { &[host] };
    let bytes = blaze_toolchain::record::write(&crates, targets, a()).expect("a toolchain record");
    let mut out = AVec::with_capacity_in(bytes.len(), a());
    out.extend_from_slice(&bytes);
    out
}

fn unit(package: u32, name: &str, kind: UnitKind<'static>, for_host: bool, deps: &[(&str, u32)]) -> Unit<'static> {
    let mut d = AVec::new_in(a());
    for (n, u) in deps {
        d.push(UnitDep { name: t(n), unit: UnitId(*u) });
    }
    // The unit graph's target atoms: the host's (no `target_arch = "wasm32"`).
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

/// `app` (member, lock 0) on `dep` (lock 1) and the member proc macro `pm` (lock 2), which
/// depends on `dep` too: the target's `dep` (unit 1) and the host's (unit 3), as the unit
/// graph gives a proc macro's dependencies.
pub(super) fn graph() -> UnitGraph<'static> {
    let mut units = AVec::new_in(a());
    units.push(unit(0, "app", UnitKind::Lib, false, &[("dep", 1), ("pm", 2)]));
    units.push(unit(1, "dep", UnitKind::Lib, false, &[]));
    units.push(unit(2, "pm", UnitKind::ProcMacro, true, &[("dep", 3)]));
    units.push(unit(1, "dep", UnitKind::Lib, true, &[]));
    UnitGraph { units, resolver: Resolver::V3, meta: AVec::new_in(a()), left_out: AVec::new_in(a()) }
}

/// Each crate's one file, at the root the list gave it: by its name.
pub(super) struct Texts<'c> {
    roots: AVec<'c, (&'c str, &'static str)>,
}

impl<'c> Texts<'c> {
    pub(super) fn of(crates: &'c UnitCrates<'_>) -> Texts<'c> {
        let mut roots = AVec::with_capacity_in(crates.crates.len(), a());
        for c in crates.crates.iter() {
            let text = match c.name {
                "dep" => DEP,
                "pm" => PM,
                _ => EMPTY,
            };
            roots.push((c.root.as_str(), text));
        }
        Texts { roots }
    }
}

impl<'c> SourceTexts<'c> for Texts<'c> {
    fn read(&self, krate: usize, path: &str) -> Option<&'c str> {
        let (root, text) = self.roots.get(krate)?;
        (*root == path).then_some(*text)
    }
}

/// What the plain path `dep::<item>`, written in crate `from`'s root, names: the crate it is
/// in, or why it names nothing.
pub(super) fn seen(names: &Names<'_>, from: usize, item: &str) -> Result<usize, Why> {
    let segments = ["dep", item];
    let written = Written { kind: PathKind::Plain, segments: &segments };
    names.item_at(names.root(from), Namespace::Types, written, a()).map(|found| found.krate).map_err(|e| e.why)
}

/// The index of the crate named `name` past `from` in the list.
pub(super) fn index_of(crates: &UnitCrates<'_>, name: &str, from: usize) -> usize {
    crates.crates.iter().skip(from).position(|c| c.name == name).map(|at| at + from).unwrap_or_else(|| panic!("no crate `{name}` at or past {from}"))
}

/// Expected line: `test units::macro_side_names::a_member_proc_macro_names_its_dependency_on_the_macro_side ... ok`.
///
/// With the `wasm32-wasip1` cut, `dep::Expr` written in the member proc macro `pm` is the
/// macro side's `dep` (a crate of its own, not the target's), whose cfg is wasm32's:
/// `dep::OnWasm` is there and `dep::OnHost` is not; from `app` the same paths name the
/// target's `dep` the other way round. The control removes the cause (the proc macro's deps
/// left empty, as before D89 (a)): `dep::Expr` is NotFound at segment 0, tempo's class.
/// Without the cut, the proc macro is refused by name and given no crate of another side.
#[test]
fn a_member_proc_macro_names_its_dependency_on_the_macro_side() {
    let lock = Lock::read(LOCK.as_bytes(), a()).expect("the lock");
    let members = [Member::new(LockId(0), "m0").expect("a plain dir"), Member::new(LockId(2), "m2").expect("a plain dir")];
    let g = graph();
    let bytes = toolchain(true);
    let view = ToolchainView::open(&bytes, ToolchainView::id_of_bytes(&bytes)).expect("its own id");
    let crates = unit_crates(&g, &lock, &members, view, HOST, a()).expect("the crates");
    assert!(crates.refused.is_empty(), "with the cut, nothing is refused");

    let app = index_of(&crates, "app", 0);
    let target_dep = index_of(&crates, "dep", 0);
    let pm = index_of(&crates, "pm", 0);
    let side_dep = index_of(&crates, "dep", pm + 1);
    assert_ne!(crates.crates[side_dep].key, crates.crates[target_dep].key, "one package on two sides: two crates (rule 21)");
    assert!(crates.crates[pm].deps.iter().any(|&(name, to)| name == "dep" && to == side_dep), "pm's `dep` is the macro side's");

    {
        let texts = Texts::of(&crates);
        let inputs = crates.inputs();
        let names = resolve_crates(&inputs, crates.sysroot, &texts, &Serial, crate::tests::SyncFrame::leaked()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
        assert_eq!(seen(&names, pm, "Expr"), Ok(side_dep), "`dep::Expr` in the proc macro: the macro side's dep");
        assert_eq!(seen(&names, pm, "OnWasm"), Ok(side_dep), "the macro side's cfg is wasm32-wasip1's");
        assert_eq!(seen(&names, pm, "OnHost"), Err(Why::NotFound), "`not(target_arch = \"wasm32\")` is false on the macro side");
        assert_eq!(seen(&names, app, "Expr"), Ok(target_dep), "the target side names its own dep");
        assert_eq!(seen(&names, app, "OnHost"), Ok(target_dep), "the target's cfg is not wasm32");
        assert_eq!(seen(&names, app, "OnWasm"), Err(Why::NotFound));
    }

    // The control: the cause removed (the proc macro's deps empty, as before D89 (a)).
    let mut control = unit_crates(&g, &lock, &members, view, HOST, a()).expect("the crates");
    control.crates[pm].deps.clear();
    {
        let texts = Texts::of(&control);
        let inputs = control.inputs();
        let names = resolve_crates(&inputs, control.sysroot, &texts, &Serial, crate::tests::SyncFrame::leaked()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
        assert_eq!(seen(&names, pm, "Expr"), Err(Why::NotFound), "with no deps, `dep` names nothing: tempo's NotFound at segment 0");
    }

    // No `wasm32-wasip1` cut: the proc macro alone is refused by name, never given the host's.
    let bytes = toolchain(false);
    let view = ToolchainView::open(&bytes, ToolchainView::id_of_bytes(&bytes)).expect("its own id");
    let uncut = unit_crates(&g, &lock, &members, view, HOST, a()).expect("the list is made");
    let pm = index_of(&uncut, "pm", 0);
    assert!(uncut.crates[pm].deps.is_empty(), "the refused proc macro is given no crate of another side");
    assert_eq!(uncut.refused.len(), 1);
    assert_eq!(uncut.refused[0].krate, pm);
    assert_eq!(uncut.refusal(pm), Some(&UnitsError::NoMacroToolchain { krate: "pm", target: MACRO }), "pm's refusal names it");
}
