//! A macro-side build-script run is answered from its own side's rows and made with its own
//! side's cfg (decision D115, ruling (2)).

use blaze_build_script::pinned::TEMPO;
use blaze_build_script::probe::{Table, probe_key};
use blaze_manifest::lock::LockId;
use blaze_own::{AVec, Alloc};
use blaze_unit_graph::{Resolver, Text, Unit, UnitDep, UnitGraph, UnitId, UnitKind};
use blaze_world::Side;
use edition::PackageEdition;

use super::{MACRO_TARGET, links_closure, pinned_table, side_atoms};

fn t(s: &str) -> Text<'static> {
    Text::from_str_in(s, blaze_own::testing::heap().alloc())
}

/// One unit of package `name` (its own lock entry `package`), with `deps` by unit index.
fn unit(package: u32, name: &str, kind: UnitKind<'static>, deps: &[u32], build_script: Option<u32>, links: Option<&str>) -> Unit<'static> {
    let alloc = blaze_own::testing::heap().alloc();
    let mut d = AVec::new_in(alloc.clone());
    for u in deps {
        d.push(UnitDep { name: t("dep"), unit: UnitId(*u) });
    }
    Unit {
        package: LockId(package),
        name: t(name),
        version: t("1.0.0"),
        kind,
        for_host: false,
        edition: PackageEdition::of_manifest("2021").expect("an edition"),
        crate_name: t(name),
        root: t("src/lib.rs"),
        features: AVec::new_in(alloc.clone()),
        cfg: AVec::new_in(alloc),
        deps: d,
        build_script: build_script.map(UnitId),
        links: links.map(t),
        incomplete: false,
    }
}

/// A links graph: `a`'s script (0) is given `b`'s metadata (`links = "b"`, script 2); `b` depends
/// on `c` (`links = "c"`, script 1), so `c`'s run is upstream of `b`'s (a forwarded path, brief
/// S1b); `e` has a script (7) but no `links`, so it is no producer. With `a_reaches_c`, `a`
/// depends on `c` directly as well. Script ids are chosen so the upstream path to `c` is walked
/// BEFORE the direct one (the work list pops the highest unit first).
fn links_graph(a_reaches_c: bool) -> UnitGraph<'static> {
    let alloc = blaze_own::testing::heap().alloc();
    let mut units = AVec::new_in(alloc.clone());
    let a_deps: &[u32] = if a_reaches_c { &[4, 5, 6] } else { &[4, 6] };
    units.push(unit(0, "a", UnitKind::BuildScript, &[], None, None)); // 0
    units.push(unit(2, "c", UnitKind::BuildScript, &[], None, None)); // 1
    units.push(unit(1, "b", UnitKind::BuildScript, &[], None, None)); // 2
    units.push(unit(0, "a", UnitKind::Lib, a_deps, Some(0), None)); // 3
    units.push(unit(1, "b", UnitKind::Lib, &[5], Some(2), Some("b"))); // 4
    units.push(unit(2, "c", UnitKind::Lib, &[], Some(1), Some("c"))); // 5
    units.push(unit(3, "e", UnitKind::Lib, &[5], Some(7), None)); // 6
    units.push(unit(3, "e", UnitKind::BuildScript, &[], None, None)); // 7
    UnitGraph { units, unavailable: AVec::new_in(alloc), resolver: Resolver::V3 }
}

/// D115 piece 3: the `Script` records a run Needs are its `links` closure: each direct `links`
/// input (its `DEP_<LINKS>_*`) and each producer upstream of those (brief S1b serves a
/// forwarded path from its files), sorted by unit, each once; a producer reached both directly
/// and upstream is direct (its metadata IS given). A script with no `links` is no producer.
/// Prints "ok". Control: in `links_closure`, replace `seen.2 |= direct` by nothing and it fails
/// with "a producer reached directly and upstream is not direct".
#[test]
fn scripts_a_runs_links_closure_holds_its_inputs_and_whats_upstream_of_them() {
    let upstream_only = links_graph(false);
    let found = links_closure(&upstream_only, UnitId(0));
    let found: std::vec::Vec<(u32, &str, bool)> = found.iter().map(|(unit, links, direct)| (unit.0, *links, *direct)).collect();
    assert_eq!(found, [(1, "c", false), (2, "b", true)], "the closure of a's script: b direct, c upstream of b, e's script none");

    let both = links_graph(true);
    let found = links_closure(&both, UnitId(0));
    let found: std::vec::Vec<(u32, &str, bool)> = found.iter().map(|(unit, links, direct)| (unit.0, *links, *direct)).collect();
    assert_eq!(found[0], (1, "c", true), "a producer reached directly and upstream is not direct: {found:?}");
    assert_eq!(found, [(1, "c", true), (2, "b", true)], "each producer once: {found:?}");

    // A producer's own closure: b's script is given c's metadata; c's script is given none.
    let of_b: std::vec::Vec<u32> = links_closure(&both, UnitId(2)).iter().map(|(unit, _, _)| unit.0).collect();
    assert_eq!(of_b, [1], "b's script reads c's run");
    assert!(links_closure(&both, UnitId(1)).is_empty(), "c's script reads no run");
}

/// The runtime keys a try-compile by the run's TARGET (blaze-proc-macro script_sandbox.rs,
/// `probe::compile_key(&self.script.target, ..)`), and a macro-side run's TARGET is
/// wasm32-wasip1: proc-macro2's `proc_macro_span` probe (a macro-side crate of every proc macro
/// that uses syn) asked for that triple is answered by the macro side's table, and NOT by the
/// target side's, whose rows are keyed for the host triple.
/// Prints "ok". Control: in `pinned_table`, key the macro side's rows by `crate::graph::TRIPLE`
/// and it fails with "the macro side's table does not answer a wasm32-wasip1 probe".
#[test]
fn scripts_the_macro_sides_table_answers_its_probes_for_wasm32_wasip1() {
    let row = TEMPO.iter().find(|known| known.probe.starts_with("proc-macro2") && known.probe.contains("proc_macro_span (")).expect("pinned.rs holds proc-macro2's proc_macro_span probe");
    let key = probe_key(MACRO_TARGET, row.args.iter().copied(), row.snippet.as_bytes());
    let macro_rows = pinned_table(Side::Macro, Alloc::global());
    let target_rows = pinned_table(Side::Target, Alloc::global());
    assert_eq!(Table { rows: &macro_rows }.answer(&key), Some(row.accepted), "the macro side's table does not answer a wasm32-wasip1 probe");
    assert_eq!(Table { rows: &target_rows }.answer(&key), None, "the target side's table answers a wasm32-wasip1 probe: a row of another triple answered it");
    assert_eq!(macro_rows.len(), TEMPO.len(), "one row a known probe");
}

/// A world row's cfg becomes the script's atoms in the same order, each value kept: the
/// environment's `CARGO_CFG_<NAME>` joins a name's values (blaze-build-script env.rs), so a
/// lost value would turn `target_arch="wasm32"` into an empty `CARGO_CFG_TARGET_ARCH`.
/// Prints "ok". Control: build every atom with `CfgAtom::flag` and it fails with "a valued atom
/// lost its value".
#[test]
fn scripts_a_side_cfg_atom_list_keeps_the_rows_order_and_values() {
    let row: [(&str, Option<&str>); 4] = [("debug_assertions", None), ("feature", Some("std")), ("target_arch", Some("wasm32")), ("target_os", Some("wasi"))];
    let atoms = side_atoms(&row, Alloc::global());
    assert_eq!(atoms.len(), row.len(), "one atom a row atom");
    for ((name, value), atom) in row.iter().zip(atoms.iter()) {
        assert_eq!(atom.name.as_str(), *name, "the atoms are not in the row's order");
        assert_eq!(atom.value.as_ref().map(|v| v.as_str()), *value, "a valued atom lost its value");
    }
}
