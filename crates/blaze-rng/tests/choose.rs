//! [`blaze_rng::choose!`] from another crate, as every caller uses it: inside blaze-rng its
//! `$crate::choose!` recursion is refused (`macro_expanded_macro_exports_accessed_by_absolute_paths`,
//! issue #52234), so its test lives here and blaze-rng itself never calls it.

use blaze_rng::{SplitMix64, choose};

/// Each open arm about as often as the others, a closed one never, and nothing to take is a
/// panic.
#[test]
fn choose_takes_open_arms_evenly() {
    let mut rng = SplitMix64::new(11);
    let mut taken = [0u32; 4];
    for _ in 0..12_000 {
        let arm: usize = choose!(rng; 0, if false => 1, 2, 3);
        taken[arm] += 1;
    }
    assert_eq!(taken[1], 0, "{taken:?}");
    assert!(taken.iter().all(|&n| n == 0 || (3600..4400).contains(&n)), "{taken:?}");
    let none = std::panic::catch_unwind(|| choose!(SplitMix64::new(0); if false => 0u8));
    assert!(none.is_err());
}
