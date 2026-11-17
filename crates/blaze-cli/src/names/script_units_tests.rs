//! Which crates of a world read a build script's run ([`super::script_units`]): every crate
//! made from a unit with a build script, the macro side's copies too (ruling D115 (2)).
//!
//! The world here is tempo's shape on 2026-11-16: a target-side library with a build script
//! (`icu_properties_data` 2.x, whose published manifest says `build = "build.rs"`), and the
//! same package's HOST unit copied to the macro side because a member proc macro depends on
//! it. The macro-side copy reads its own side's run (`Script::Read`), and its unit is no
//! target-side crate, so `World::crate_of_unit` holds nothing for it.

use blaze_unit_graph::UnitId;
use blaze_world::Origin;

/// Units: 0 the target library, 1 its build script, 2 the host library (for a proc macro),
/// 3 its build script, 4 a library with none.
fn build_script(unit: UnitId) -> Option<UnitId> {
    match unit.0 {
        0 => Some(UnitId(1)),
        2 => Some(UnitId(3)),
        _ => None,
    }
}

/// World order: 0 a toolchain crate, 1 the target library (unit 0), 2 the macro-side copy
/// (unit 2), 3 the scriptless library (unit 4).
const ORIGINS: [Origin; 4] = [Origin::Toolchain(0), Origin::Unit(UnitId(0)), Origin::Unit(UnitId(2)), Origin::Unit(UnitId(4))];

/// Expected: `test names::script_units_tests::a_macro_side_copy_reads_its_units_script ... ok`.
#[test]
fn a_macro_side_copy_reads_its_units_script() {
    let found: std::vec::Vec<(usize, UnitId)> = super::script_units(ORIGINS.into_iter(), build_script).collect();
    assert_eq!(found, [(1, UnitId(1)), (2, UnitId(3))], "both sides' crates read their unit's build script; the toolchain's and the scriptless one none");
}

/// The cause, removed and watched (rule 23): the old derivation walked `crate_of_unit`, the
/// target side's map (blaze-world list.rs: `first..macro_from` only), and so missed crate 2,
/// whose `Script::Read` then had no placement: "the crate `icu_properties_data` reads a build
/// script's run, and no unit of it has a build script (ruling D115 (3))".
/// Expected: `test names::script_units_tests::the_target_sides_map_misses_the_macro_side ... ok`.
#[test]
fn the_target_sides_map_misses_the_macro_side() {
    // What the world's `crate_of_unit` is for ORIGINS: units 0 and 4 are target-side crates.
    let crate_of_unit: [Option<u32>; 5] = [Some(1), None, None, None, Some(3)];
    let old: std::vec::Vec<(usize, UnitId)> = crate_of_unit
        .iter()
        .enumerate()
        .filter_map(|(unit, at)| Some(((*at)? as usize, build_script(UnitId(unit as u32))?)))
        .collect();
    assert_eq!(old, [(1, UnitId(1))], "the target side's map has no row for the macro-side copy");
    let new: std::vec::Vec<(usize, UnitId)> = super::script_units(ORIGINS.into_iter(), build_script).collect();
    assert!(new.contains(&(2, UnitId(3))), "the crates themselves name the macro-side copy's script");
}
