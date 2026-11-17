//! The feature table, its implications, the base sets and the first-tier route
//! (crates/blaze-tier/src/features.rs, route.rs; INTRINSICS.md P8).

#![feature(allocator_api)]

use std::alloc::Global;

use blaze_lir::{Func, Head, Op};
use blaze_tier::features::{self, FeatureError};
use blaze_tier::route::first_tier;
use blaze_tier::{Arch, Machine, Os, TierName};

const MAC: Machine = Machine { arch: Arch::Arm64, os: Os::MacOs, features: Machine::LSE };

/// The Rust Reference, attributes/codegen.html, "Available features", aarch64 table
/// ("Feature | Implicitly Enables"): "sha2 | neon", "sha3 | sha2", "fp16 | neon", "fhm | fp16".
/// Enabling a feature enables what it implies, transitively: `sha3` gives `sha2` and `neon`;
/// `fhm` gives `fp16` and `neon`; and nothing else (a closure over the whole table, or one
/// that stops after one step, fails). A name of no feature is refused by name.
/// Expected line: `test implications_close ... ok`.
#[test]
fn implications_close() {
    let sha3 = features::parse(Arch::Arm64, "sha3").expect("sha3 is an aarch64 feature");
    let names: Vec<&str> = sha3.iter().map(|it| it.rust).collect();
    assert_eq!(names, ["neon", "sha2", "sha3"], "sha3's closure");

    let fhm = features::parse(Arch::Arm64, "fhm").expect("fhm is an aarch64 feature");
    let names: Vec<&str> = fhm.iter().map(|it| it.rust).collect();
    assert_eq!(names, ["fhm", "fp16", "neon"], "fhm's closure");
    assert_eq!(fhm.contains("sve"), Some(false));
    let llvm: Vec<&str> = fhm.llvm_names().collect();
    assert_eq!(llvm, ["fp16fml", "fullfp16", "neon", "fp-armv8"], "fhm's LLVM names");

    let both = features::parse(Arch::Arm64, "sha3,fhm").expect("two aarch64 features");
    assert_eq!(both, sha3.union(fhm));
    assert!(sha3.is_subset(both) && !both.is_subset(sha3));

    match features::parse(Arch::Arm64, "sha3,avx2") {
        Err(FeatureError::Unknown { arch: Arch::Arm64, name }) => assert_eq!(name.as_str(), "avx2"),
        other => panic!("avx2 on aarch64: {other:?}"),
    }
    assert!(matches!(
        features::parse(Arch::Arm64, "fp-armv8"),
        Err(FeatureError::Internal { name: "fp-armv8", .. })
    ));
    assert!(matches!(features::parse(Arch::Arm64, "sha3,"), Err(FeatureError::Empty { at: 5, .. })));
}

/// INTRINSICS.md 3.3 and the Rust Reference, attributes/codegen.html: "It is undefined
/// behavior to call a function that is compiled with a feature that is not supported on the
/// current platform the code is running on". The Reference and Fast tiers emit base code
/// only; an `asm!` block, a target intrinsic or a feature beyond the base set goes to the
/// Optimized tier, a dependency's plain instance to Reference (never Fast), the workspace's to
/// Fast. `+sha3` is in apple-m1's base set (rustc.json's cfg for aarch64-apple-darwin), `+sve`
/// is not, and `-lse` turns a base feature off.
/// Expected line: `test route_asm_to_optimized ... ok`.
#[test]
fn route_asm_to_optimized() {
    fn func(ops: &[Op], features: &str) -> Func {
        let mut func = Func::new_in(Head::default(), Global);
        for &op in ops {
            func.ops.push(op as u8);
        }
        func.features.extend_from_slice(features.as_bytes());
        func
    }
    for op in [Op::Asm, Op::AsmBr, Op::TargetIntrinsic] {
        assert_eq!(first_tier(func(&[op], "").view(), MAC, false), TierName::Optimized, "{op:?}");
        assert_eq!(first_tier(func(&[op], "").view(), MAC, true), TierName::Optimized, "{op:?}");
    }
    let plain = func(&[], "");
    assert_eq!(first_tier(plain.view(), MAC, false), TierName::Fast);
    assert_eq!(first_tier(plain.view(), MAC, true), TierName::Reference);
    assert_eq!(first_tier(func(&[], "+sha3,+fullfp16").view(), MAC, false), TierName::Fast);
    assert_eq!(first_tier(func(&[], "+sha3,+sve").view(), MAC, false), TierName::Optimized);
    assert_eq!(first_tier(func(&[], "-lse").view(), MAC, false), TierName::Optimized);
    assert_eq!(first_tier(func(&[], "+sve").view(), MAC, true), TierName::Optimized);
}

/// The table is rustc's (rustc_target's tables as crates/target-features holds them), row by
/// row, less the `InternalOnly` rows, which are refused by name; and aarch64-apple-darwin's
/// base set is CPU `apple-m1`'s (rustc.json's spec: `"cpu": "apple-m1"`), closed under
/// implication. A row added, dropped or implying otherwise fails here.
/// Expected line: `test table_is_rustcs ... ok`.
#[test]
fn table_is_rustcs() {
    let arches = [
        (Arch::Arm64, target_features::TargetArch::AArch64, features::INTERNAL_AARCH64),
        (Arch::X86_64, target_features::TargetArch::X86, features::INTERNAL_X86_64),
    ];
    for (arch, theirs, internal) in arches {
        let ours = features::table(arch);
        let mut distinct: Vec<&str> = Vec::new();
        for (name, implied) in theirs.rustc_table() {
            if internal.iter().any(|(it, _)| *it == name) {
                assert!(ours.iter().all(|row| row.rust != name), "{arch:?}: {name} is internal");
                continue;
            }
            let row = ours.iter().find(|row| row.rust == name).unwrap_or_else(|| panic!("{arch:?}: no row {name}"));
            assert_eq!(row.implies, implied, "{arch:?}: what {name} implies");
            if !distinct.contains(&name) {
                distinct.push(name);
            }
        }
        assert_eq!(ours.len(), distinct.len(), "{arch:?}: rows beyond rustc's");
    }

    let base = features::base(Arch::Arm64, Os::MacOs);
    let mut names: Vec<&str> = base.iter().map(|it| it.rust).collect();
    names.sort_unstable();
    let m1 = target_features::cpus::features("aarch64-apple-darwin", "apple-m1").expect("apple-m1's features");
    assert_eq!(names, m1, "aarch64-apple-darwin's base set");
    assert_eq!(base.closed(), base, "the base set is closed under implication");
    assert_eq!(base.contains("lse"), Some(true));
}
