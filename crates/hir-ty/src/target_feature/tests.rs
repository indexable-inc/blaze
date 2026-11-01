use blaze_hash::{HashMap, HashSet};
use intern::Symbol;

use super::*;

/// rustc's `implied_target_features`' map of `arch`'s table: each name, what its (last) line
/// says it implies.
fn rustc_map(arch: TargetArch) -> HashMap<&'static str, &'static [&'static str]> {
    arch.rustc_table().collect()
}

/// rustc's `implied_target_features` of each feature of `enabled`, added to it.
fn rustc_expand(map: &HashMap<&str, &[&str]>, enabled: &mut HashSet<Symbol>) {
    let mut queue = enabled.iter().cloned().collect::<Vec<_>>();
    while let Some(feature) = queue.pop() {
        if let Some(implications) = map.get(feature.as_str()) {
            for implication in implications.iter() {
                let implication = Symbol::intern(implication);
                if enabled.insert(implication.clone()) {
                    queue.push(implication);
                }
            }
        }
    }
}

fn features(declared: &Declared, implied: Option<TargetArch>) -> TargetFeatures<'_> {
    let raw = TargetFeatures::of(declared);
    match implied {
        Some(arch) => TargetFeatures { known: raw.known.implied_in(arch), ..raw },
        None => raw,
    }
}

#[test]
fn call_safety_follows_the_targets_table() {
    // `is_target_feature_call_safe`: an `aes` caller may call a `neon` callee on AArch64 only.
    let (caller, callee) = blaze_mem::frame::task(blaze_mem::Tag::Infer, |frame| {
        (Declared::of(frame, &[Symbol::intern("aes")]), Declared::of(frame, &[Symbol::intern("neon")]))
    });
    let callee = features(&callee, None);
    assert!(!features(&caller, Some(TargetArch::X86)).is_superset(&callee));
    assert!(features(&caller, Some(TargetArch::AArch64)).is_superset(&callee));
}

#[test]
fn enables_gives_rustcs_results() {
    let archs = [
        TargetArch::X86,
        TargetArch::AArch64,
        TargetArch::Arm,
        TargetArch::RiscV,
        TargetArch::Wasm,
        TargetArch::Other,
    ];
    for arch in archs {
        enables_gives_rustcs_results_on(arch);
    }
}

fn enables_gives_rustcs_results_on(arch: TargetArch) {
    let map = rustc_map(arch);
    let combos: &[&[&str]] = &[
        &[],
        &["avx2"],
        &["avx512f"],
        &["avx512f", "avx512vl"],
        &["avx512fp16", "fma"],
        &["sse4.1", " fma"],
        &["fma4"],
        &["f16c", "not-a-feature"],
        &["neon", "aes"],
        &["fp16"],
        &["sve2-aes"],
        &["v9.5a"],
        &["rva23u64"],
        &["avx10.2"],
        &["simd128"],
        &["relaxed-simd"],
        &["e2"],
        &["", "x", "AVX2"],
        &["zvl65536b", "ptx87", "sm_120a"],
        &["outline-atomics", "lse"],
    ];
    let mut probes: Vec<&str> = TargetFeature::all().map(TargetFeature::name).collect();
    probes.extend(["", "x", " fma", "AVX2", "not-a-feature", "avx 2"]);
    let olds: Vec<(HashSet<Symbol>, HashSet<Symbol>)> = combos
        .iter()
        .map(|names| {
            let raw: HashSet<Symbol> = names.iter().map(|it| Symbol::intern(it)).collect();
            let mut expanded = raw.clone();
            rustc_expand(&map, &mut expanded);
            (raw, expanded)
        })
        .collect();
    let declared: Vec<Declared> = blaze_mem::frame::task(blaze_mem::Tag::Infer, |frame| {
        olds.iter().map(|(raw, _)| Declared::of(frame, raw)).collect()
    });
    for ((raw, expanded), declared) in olds.iter().zip(&declared) {
        for (old, implied) in [(raw, None), (expanded, Some(arch))] {
            let new = features(declared, implied);
            for probe in &probes {
                let was = old.contains(&Symbol::intern(probe));
                assert_eq!(new.enables_name(probe), was, "{probe:?} of {old:?} on {arch:?}");
                if let Some(feature) = TargetFeature::from_name(probe) {
                    assert_eq!(new.enables(feature), was, "{probe:?} of {old:?} on {arch:?}");
                }
            }
        }
    }
    // `is_fn_unsafe_to_call`'s test, rustc's `is_target_feature_call_safe`: the caller's
    // features, implied ones too, against the callee's own.
    for ((_, caller_old), caller) in olds.iter().zip(&declared) {
        for ((callee_old, _), callee) in olds.iter().zip(&declared) {
            let was = caller_old.is_superset(callee_old);
            let now = features(caller, Some(arch)).is_superset(&features(callee, None));
            assert_eq!(now, was, "{caller_old:?} over {callee_old:?} on {arch:?}");
        }
    }
}
