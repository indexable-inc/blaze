//! blaze-scope's `aarch64-apple-darwin` target facts are the pinned toolchain's, so the code a
//! crate's `cfg` picks is the code its authors built and tested.
//!
//! The rule, from the Rust Reference (conditional-compilation.html, "`target_feature`"):
//! "Key-value option set for each platform feature available for the current compilation
//! target." The target's CPU is `apple-m1` (the pinned rustc's target spec for
//! `aarch64-apple-darwin`, `"cpu": "apple-m1"`, recorded in
//! pins/answers/rustc.json `target_spec_json`), so the features on are that
//! CPU's: `target_features::cpus`, which tools/cpus.py writes from the pinned nightly's
//! `rustc --print cfg -C target-cpu=<cpu>` (rustc 1.101.0-nightly 16d1ffac1 2026-10-26).
//!
//! `target_has_reliable_f128` and `target_has_reliable_f128_math` are the library's own cfgs
//! (library/core/Cargo.toml: "Internal features aren't marked known config by default, we use
//! these to gate tests." then `'cfg(target_has_reliable_f128)'`,
//! `'cfg(target_has_reliable_f128_math)'`). The pinned rustc's `--print cfg` for
//! `aarch64-apple-darwin` (rustc.json `nightly_cfg`) holds `target_has_reliable_f128` and
//! not `target_has_reliable_f128_math`: f128 is reliable there, its math functions are not.
//!
//! Expected line: `test blaze-scope: exit 0; 1 passed, 0 failed`.

use blaze_own::Alloc;
use blaze_scope::task::{Cfg, Target};

/// The triple and CPU the facts are of.
const TRIPLE: &str = "aarch64-apple-darwin";
const CPU: &str = "apple-m1";

/// A wrong list fails here: the six features blaze listed before (with `fp-armv8`, which Rust
/// ties to `neon` and never sets as a cfg, target-features/src/lib.rs "Rust ties `fp-armv8` to
/// `neon`"), or a list missing `dit`, `sb`, `fp16` or `sha3`, which steer constant_time_eq,
/// half and crc-fast; and a missing or extra reliable-f128 atom.
#[test]
fn aarch64_apple_darwin_features_match_table() {
    let target = Target::AARCH64_APPLE_DARWIN;
    let table = target_features::cpus::features(TRIPLE, CPU)
        .unwrap_or_else(|err| panic!("the CPU table has no {CPU} of {TRIPLE}: {err}"));

    // The constant is written sorted, as the table is, so the two compare as slices.
    assert!(
        target.target_features.is_sorted_by(|a, b| a < b),
        "Target::AARCH64_APPLE_DARWIN.target_features is not sorted and unique: {:?}",
        target.target_features
    );
    assert_eq!(
        target.target_features, table,
        "Target::AARCH64_APPLE_DARWIN.target_features differs from target_features::cpus of \
         {TRIPLE} {CPU}"
    );
    for name in ["dit", "sb", "fp16", "sha3", "neon", "aes", "sha2", "crc", "lse"] {
        assert!(target.target_features.contains(&name), "`{name}` missing from {TRIPLE}");
    }
    assert!(
        !target.target_features.contains(&"fp-armv8"),
        "`fp-armv8` is no cfg target feature of {TRIPLE}"
    );
    assert!(target.reliable_f128, "{TRIPLE} has target_has_reliable_f128");
    assert!(!target.reliable_f128_math, "{TRIPLE} has no target_has_reliable_f128_math");

    // What the configuration sets: each feature as `target_feature = ".."`, and the f128 atom.
    let cfg = Cfg::of(&target, &[], Alloc::global());
    let atoms = cfg.atoms();
    for &name in table {
        assert!(
            atoms.contains(&("target_feature", Some(name))),
            "cfg of {TRIPLE} lacks target_feature = \"{name}\""
        );
    }
    let feature_atoms = atoms.iter().filter(|(key, value)| *key == "target_feature" && value.is_some()).count();
    assert_eq!(feature_atoms, table.len(), "cfg of {TRIPLE} sets features beyond {CPU}'s");
    assert!(atoms.contains(&("target_has_reliable_f128", None)), "cfg of {TRIPLE} lacks target_has_reliable_f128");
    assert!(
        !atoms.contains(&("target_has_reliable_f128_math", None)),
        "cfg of {TRIPLE} sets target_has_reliable_f128_math, which the pinned rustc does not"
    );
    assert!(atoms.contains(&("target_has_reliable_f16", None)), "cfg of {TRIPLE} lacks target_has_reliable_f16");
}
