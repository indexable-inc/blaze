//! A proc-macro library's test harness has `proc_macro` in its extern prelude, the macro
//! side's, as the proc macro has (units/macro_crate.rs).
//!
//! The Reference, "Procedural macros": "Procedural macro crates ... implicitly link to the
//! compiler-provided `proc_macro` crate"; "Extern prelude": crates "provided to the compiler
//! (as with the `--extern` flag with `rustc`) are added to the extern prelude", and the first
//! segment of a `use` path is looked up there. So `use proc_macro::TokenStream;` with no
//! `extern crate` resolves in a proc-macro crate, and names nothing in any other crate.
//!
//! The real case: pm-small (member `mac` with `use proc_macro::TokenStream;` and
//! `#[proc_macro] pub fn same`; member `app` calling `mac::same!`), `blaze check` at f67d68144b:
//! "8e5jm  error[unresolved]  mac/src/lib.rs:1:5  no `proc_macro` in this scope", from `mac`'s
//! library test harness (the check asks test units: every other finding is printed twice).

use blaze_manifest::lock::{Lock, LockId};
use blaze_surface::Namespace;
use blaze_toolchain::ToolchainView;
use blaze_unit_graph::{CfgAtom, Resolver, Text, Unit, UnitDep, UnitGraph, UnitId, UnitKind, implicit};
use blaze_own::AVec;
use edition::PackageEdition;

use super::macro_side_names::{HOST, a, toolchain};
use super::{Member, Origin, UnitCrates, unit_crates};
use crate::resolve::{Names, SourceTexts, resolve_crates};
use crate::task::Serial;

const LOCK: &str = r#"version = 4

[[package]]
name = "app"
version = "0.1.0"
dependencies = [
 "pm",
]

[[package]]
name = "pm"
version = "0.1.0"
"#;

/// Both members write the same head: only the proc macro's code may resolve it.
const HEAD: &str = "#![no_core]
use proc_macro::TokenStream;
";

/// The toolchain's `proc_macro`, on either side.
const PROC_MACRO: &str = "#![no_core]
pub struct TokenStream;
";

const EMPTY: &str = "#![no_core]
";

fn t(s: &str) -> Text<'static> {
    Text::from_str_in(s, a())
}

fn edition() -> PackageEdition {
    PackageEdition::of_manifest("2021").expect("an edition")
}

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
        version: t("0.1.0"),
        kind,
        for_host,
        edition: edition(),
        crate_name: t(name),
        root: t("src/lib.rs"),
        features: AVec::new_in(a()),
        cfg,
        deps: d,
        build_script: None,
        links: None,
    }
}

/// Units 0 `app` (lib, on `pm`), 1 `pm` (the proc macro), 2 `pm`'s library test harness,
/// 3 `app`'s library test harness: as the unit graph gives them when tests are asked.
fn graph() -> UnitGraph<'static> {
    let mut units = AVec::new_in(a());
    units.push(unit(0, "app", UnitKind::Lib, false, &[("pm", 1)]));
    units.push(unit(1, "pm", UnitKind::ProcMacro, true, &[]));
    units.push(unit(1, "pm", UnitKind::Test { bin: None }, false, &[]));
    units.push(unit(0, "app", UnitKind::Test { bin: None }, false, &[("pm", 1)]));
    UnitGraph { units, resolver: Resolver::V3, meta: AVec::new_in(a()), left_out: AVec::new_in(a()) }
}

struct Texts<'c> {
    texts: AVec<'c, (&'c str, &'static str)>,
}

impl<'c> Texts<'c> {
    fn of(crates: &'c UnitCrates<'_>) -> Texts<'c> {
        let mut texts = AVec::with_capacity_in(crates.crates.len(), a());
        for c in crates.crates.iter() {
            let text = match c.name {
                "app" | "pm" => HEAD,
                "proc_macro" => PROC_MACRO,
                _ => EMPTY,
            };
            texts.push((c.root.as_str(), text));
        }
        Texts { texts }
    }
}

impl<'c> SourceTexts<'c> for Texts<'c> {
    fn read(&self, krate: usize, path: &str) -> Option<&'c str> {
        let (root, text) = self.texts.get(krate)?;
        (*root == path).then_some(*text)
    }
}

/// The list's crate of graph unit `unit`.
fn of_unit(crates: &UnitCrates<'_>, unit: u32) -> usize {
    crates
        .crates
        .iter()
        .position(|c| matches!(c.origin, Origin::Unit(id) if id == UnitId(unit)))
        .unwrap_or_else(|| panic!("no crate of unit {unit}"))
}

/// The crate the root's imported `TokenStream` is of, if the import resolved.
fn imported(names: &Names<'_>, krate: usize) -> Option<usize> {
    names.lookup(names.root(krate), Namespace::Types, "TokenStream").and_then(|b| names.crate_of(b.def.key))
}

/// Expected line: `test units::macro_crate_prelude::a_proc_macro_test_harness_has_proc_macro_in_its_extern_prelude ... ok`.
///
/// With the `wasm32-wasip1` cut: `use proc_macro::TokenStream;` in the proc macro `pm` AND in
/// its library test harness names the macro side's `proc_macro` (never the target's); in `app`
/// and its test harness, plain library code, it names nothing. The control removes the cause
/// (the harness given a plain library's inputs, as before units/macro_crate.rs): its import
/// names nothing, pm-small's "no `proc_macro` in this scope".
#[test]
fn a_proc_macro_test_harness_has_proc_macro_in_its_extern_prelude() {
    let lock = Lock::read(LOCK.as_bytes(), a()).expect("the lock");
    let members = [Member::new(LockId(0), "app").expect("a plain dir"), Member::new(LockId(1), "pm").expect("a plain dir")];
    let g = graph();
    let bytes = toolchain(true);
    let view = ToolchainView::open(&bytes, ToolchainView::id_of_bytes(&bytes)).expect("its own id");
    let crates = unit_crates(&g, &lock, &members, view, HOST, a()).expect("the crates");
    assert!(crates.refused.is_empty(), "with the cut, nothing is refused");

    let (app, pm, pm_test, app_test) = (of_unit(&crates, 0), of_unit(&crates, 1), of_unit(&crates, 2), of_unit(&crates, 3));
    let side_proc_macro = crates
        .crates
        .iter()
        .enumerate()
        .skip(crates.macro_from)
        .find(|(_, c)| c.name == "proc_macro")
        .map(|(at, _)| at)
        .expect("the macro side's `proc_macro`");
    for (krate, what) in [(pm, "the proc macro"), (pm_test, "its test harness")] {
        assert!(crates.crates[krate].implicit.proc_macro, "{what} is a proc-macro crate's code");
        assert!(crates.crates[krate].deps.iter().any(|&(name, to)| name == "proc_macro" && to == side_proc_macro), "{what} names the macro side's `proc_macro`");
    }
    for (krate, what) in [(app, "app"), (app_test, "app's test harness")] {
        assert!(!crates.crates[krate].implicit.proc_macro, "{what} is no proc macro's code");
        assert!(crates.crates[krate].deps.iter().all(|&(name, _)| name != "proc_macro"), "{what} has no `proc_macro`");
    }

    {
        let texts = Texts::of(&crates);
        let inputs = crates.inputs();
        let names = resolve_crates(&inputs, crates.sysroot, &texts, &Serial, crate::tests::SyncFrame::leaked()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
        assert_eq!(imported(&names, pm), Some(side_proc_macro), "`use proc_macro::TokenStream;` in the proc macro");
        assert_eq!(imported(&names, pm_test), Some(side_proc_macro), "the same head in its test harness: pm-small's error gone");
        assert_eq!(imported(&names, app), None, "a plain library has no `proc_macro` in its extern prelude");
        assert_eq!(imported(&names, app_test), None, "nor has its test harness");
    }

    // The control: the cause removed (the harness given a plain library's inputs: its own
    // dependencies, none, and `implicit` of its kind alone).
    let mut control = unit_crates(&g, &lock, &members, view, HOST, a()).expect("the crates");
    control.crates[pm_test].deps.clear();
    control.crates[pm_test].implicit = implicit(edition(), &UnitKind::Test { bin: None });
    {
        let texts = Texts::of(&control);
        let inputs = control.inputs();
        let names = resolve_crates(&inputs, control.sysroot, &texts, &Serial, crate::tests::SyncFrame::leaked()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
        assert_eq!(imported(&names, pm_test), None, "today's error: no `proc_macro` in this scope");
        assert_eq!(imported(&names, pm), Some(side_proc_macro), "the proc macro itself was never the case");
    }
}
