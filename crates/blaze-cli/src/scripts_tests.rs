//! A macro-side build-script run is answered from its own side's rows and made with its own
//! side's cfg (decision D115, ruling (2)).

use blaze_build_script::pinned::TEMPO;
use blaze_build_script::probe::Table;
use blaze_build_script::probe_ask::{Ask, By, Verdict, answer, classify};
use blaze_manifest::lock::LockId;
use blaze_own::AVec;
use blaze_unit_graph::{Resolver, Text, Unit, UnitDep, UnitGraph, UnitId, UnitKind};
use blaze_world::Side;
use edition::PackageEdition;

use super::{MACRO_TARGET, links_closure, side_atoms};

/// A side's pinned rows and ships facts as the shared setup makes them (blaze-world
/// `script_setting::pinned_table`, which `probe_table` sorts with the feature listings), sorted.
fn pinned_table<'a, 's>(side: Side, shipped: impl Iterator<Item = &'s str> + Clone, alloc: blaze_own::Alloc<'a>) -> AVec<'a, blaze_build_script::probe::Row> {
    let triple = match side {
        Side::Target => crate::graph::TRIPLE,
        Side::Macro => MACRO_TARGET,
    };
    let mut rows = blaze_world::script_setting::pinned_table(triple, shipped, alloc);
    rows.sort_by(|a, b| a.key.cmp(&b.key));
    rows
}

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
    UnitGraph { units, resolver: Resolver::V3, meta: AVec::new_in(alloc), left_out: AVec::new_in(alloc) }
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

/// The runtime keys a try-compile by its `--target` (blaze-proc-macro script_sandbox.rs,
/// `probe_ask::answer`), and a macro-side run's cargo passes `--target wasm32-wasip1`:
/// proc-macro2's `proc_macro_span` probe (a macro-side crate of every proc macro that uses
/// syn), with the exact line tempo's `blaze check` printed, is answered by the macro side's
/// table, and NOT by the target side's, whose rows are keyed for the host triple. A row whose
/// `needs` a side's sysroot lacks is not made for it.
/// Prints "ok". Control: in `pinned_table`, key the macro side's rows by `crate::graph::TRIPLE`
/// and it fails with "the macro side's table does not answer a wasm32-wasip1 probe".
#[test]
fn scripts_the_macro_sides_table_answers_its_probes_for_wasm32_wasip1() {
    let row = TEMPO.iter().find(|known| known.probe.starts_with("proc-macro2") && known.probe.contains("proc_macro_span (")).expect("pinned.rs holds proc-macro2's proc_macro_span probe");
    let line = [
        "--cfg=procmacro2_build_probe",
        "--edition=2021",
        "--crate-name=proc_macro2",
        "--crate-type=lib",
        "--cap-lints=allow",
        "--emit=dep-info,metadata",
        "--out-dir",
        "/target/debug/build/proc-macro2-0f3c/out/probe",
        "src/probe/proc_macro_span.rs",
        "--target",
        MACRO_TARGET,
    ];
    // The test's one frame, made at its top (its memory the OS's), handed down as a handle.
    let frame = bstd::mem::Frame::with_reserve(1 << 20, bstd::mem::Tag::Other).expect("the test's frame");
    let erased = blaze_own::Erased::new(&frame);
    let Ok(Ask::Compile(compile)) = classify(&line, erased.handle()) else { panic!("proc-macro2's line is not a try-compile") };
    let sysroot = ["alloc", "core", "proc_macro", "std"];
    let macro_rows = pinned_table(Side::Macro, sysroot.iter().copied(), erased.handle());
    let target_rows = pinned_table(Side::Target, sysroot.iter().copied(), erased.handle());
    let on = |rows: &[blaze_build_script::probe::Row]| answer(&compile, crate::graph::TRIPLE, row.snippet.as_bytes(), &Table { rows }).verdict;
    assert_eq!(on(&macro_rows), Verdict::Accepted(By::Table), "the macro side's table does not answer a wasm32-wasip1 probe");
    assert!(matches!(on(&target_rows), Verdict::Unanswered(_)), "the target side's table answers a wasm32-wasip1 probe: a row of another triple answered it");
    assert_eq!(macro_rows.len(), TEMPO.len() + sysroot.len(), "one row a known probe and one a shipped crate");
    // A sysroot of core alone (no std, no proc_macro) gets only the rows that rest on core.
    let core_only = pinned_table(Side::Macro, ["core"].iter().copied(), erased.handle());
    let resting_on_core = TEMPO.iter().filter(|known| known.needs == ["core"]).count();
    assert_eq!(core_only.len(), resting_on_core + 1, "a row was made for a sysroot that lacks what it needs");
    assert!(matches!(on(&core_only), Verdict::Unanswered(_)), "proc_macro_span answered where proc_macro is not shipped");
}

/// A world row's cfg becomes the script's atoms in the same order, each value kept: the
/// environment's `CARGO_CFG_<NAME>` joins a name's values (blaze-build-script env.rs), so a
/// lost value would turn `target_arch="wasm32"` into an empty `CARGO_CFG_TARGET_ARCH`.
/// Prints "ok". Control: build every atom with `CfgAtom::flag` and it fails with "a valued atom
/// lost its value".
#[test]
fn scripts_a_side_cfg_atom_list_keeps_the_rows_order_and_values() {
    let row: [(&str, Option<&str>); 4] = [("debug_assertions", None), ("feature", Some("std")), ("target_arch", Some("wasm32")), ("target_os", Some("wasi"))];
    // The test's one frame, made at its top (its memory the OS's), handed down as a handle.
    let frame = bstd::mem::Frame::with_reserve(1 << 16, bstd::mem::Tag::Other).expect("the test's frame");
    let erased = blaze_own::Erased::new(&frame);
    let atoms = side_atoms(&row, erased.handle());
    assert_eq!(atoms.len(), row.len(), "one atom a row atom");
    for ((name, value), atom) in row.iter().zip(atoms.iter()) {
        assert_eq!(atom.name.as_str(), *name, "the atoms are not in the row's order");
        assert_eq!(atom.value.as_ref().map(|v| v.as_str()), *value, "a valued atom lost its value");
    }
}

/// A native run's key holds the macOS SDK's identity (its version) and never its folder: two
/// SDKs of one version in two folders key one run, two versions key two, and a wasm run's key
/// moves with neither. Prints "sdk keyed by version". Control: drop `sdk_bytes` from `parts` and
/// the second assertion fails with "two SDK versions keyed one native run"; hash the folder in
/// and the first fails.
#[test]
fn scripts_a_native_runs_key_holds_the_sdks_version_not_its_folder() {
    use blaze_apple_sdk::{Sdk, Unread, XcrunFailed, resolve};
    use blaze_ids::ContentId;

    use super::{RanAs, parts};

    let sdk = |folder: &'static str, settings: &'static str, build: &'static str| -> Sdk<'static> {
        resolve(
            Some(folder),
            || -> Result<&'static str, XcrunFailed<'static>> { panic!("SDKROOT is set") },
            |_| -> Result<&'static str, Unread<'static>> { Ok(settings) },
            |_| -> Result<&'static str, Unread<'static>> { Ok(build) },
        )
        .expect("an SDK")
    };
    let b118 = "<key>ProductBuildVersion</key><string>26B118</string>";
    let xcode = sdk("/Applications/Xcode.app/Contents/Developer/Platforms/MacOSX.platform/Developer/SDKs/MacOSX.sdk", r#"{"Version":"27.2"}"#, b118);
    let tools = sdk("/Library/Developer/CommandLineTools/SDKs/MacOSX.sdk", r#"{"Version":"27.2"}"#, b118);
    let older = sdk("/Library/Developer/CommandLineTools/SDKs/MacOSX26.5.sdk", r#"{"Version":"26.5"}"#, "<key>ProductBuildVersion</key><string>25F70</string>");
    let beta = sdk("/Library/Developer/CommandLineTools/SDKs/MacOSX27.2.sdk", r#"{"Version":"27.2"}"#, "<key>ProductBuildVersion</key><string>26B5044k</string>");
    let key = |ran_as: RanAs, sdk: Option<&Sdk<'_>>| parts([(ContentId([1; 32]), ContentId([2; 32]), ran_as)].into_iter(), sdk.map(|sdk| sdk.identity()))[0].1;

    assert_eq!(key(RanAs::Native, Some(&xcode)), key(RanAs::Native, Some(&tools)), "one SDK version in two folders keyed two native runs");
    assert_ne!(key(RanAs::Native, Some(&xcode)), key(RanAs::Native, Some(&older)), "two SDK versions keyed one native run");
    assert_ne!(key(RanAs::Native, Some(&xcode)), key(RanAs::Native, Some(&beta)), "two builds of one SDK version keyed one native run");
    assert_ne!(key(RanAs::Native, Some(&xcode)), key(RanAs::Native, None), "a native run with no SDK keyed as one with an SDK");
    assert_eq!(key(RanAs::Wasm, Some(&xcode)), key(RanAs::Wasm, Some(&older)), "a wasm run's key moved with the SDK");
    assert_eq!(key(RanAs::Wasm, Some(&xcode)), key(RanAs::Wasm, None), "a wasm run's key moved with the SDK's presence");
    println!("sdk keyed by version");
}

/// The refusal a native script fails with when the command has no SDK is the resolution's own
/// words: what was tried and how to set `SDKROOT`, never "SDKROOT is not set" alone (the old
/// refusal, which stopped tempo's check on a plain shell). Prints "refusal names both".
#[test]
fn scripts_no_sdk_refusal_names_both_attempts() {
    use blaze_apple_sdk::{SdkRefused, XcrunFailed};
    let refused = SdkRefused::Neither { xcrun: XcrunFailed::Exited { status: 1, said: "xcrun: error: invalid active developer path" } };
    let words = format!("{refused}");
    assert!(words.contains("SDKROOT is not set, and `xcrun --sdk macosx --show-sdk-path` exited with status 1"), "{words}");
    assert!(words.contains("export SDKROOT="), "{words}");
    println!("refusal names both");
}
