//! The key an adaptive table switches to, from a key its caller drew (the ambient
//! `switch_key`, which drew from the operating system inside the table operation that saw the
//! flood, was deleted on 2026-11-20). A table that adapts holds a standby key, drawn where it was made from
//! the capability its maker was handed (bstd's `Io::sip_key`), and passes it here at the flood,
//! so the switch reads nothing ambient and a simulation repeats it by its seed.

use super::MODEL_KEY;
use crate::SipKey;

/// The key to switch to: `drawn`, or under the model checker (`cfg(blaze_check)`) the fixed
/// [`MODEL_KEY`] (the checker runs a model many times and needs each run to place keys alike).
#[inline]
pub fn switch_key_from(drawn: SipKey) -> SipKey {
    if cfg!(blaze_check) { MODEL_KEY } else { drawn }
}

#[cfg(test)]
mod tests {
    use super::switch_key_from;
    use crate::SipKey;

    /// Outside the model checker the switch key is the one handed in, unchanged.
    ///
    /// Expected line: `test adaptive::drawn::tests::switch_key_from_is_the_drawn_key ... ok`.
    #[test]
    fn switch_key_from_is_the_drawn_key() {
        let drawn = SipKey::new(0x1234, 0x5678);
        assert_eq!(switch_key_from(drawn), drawn);
        assert_ne!(switch_key_from(SipKey::new(1, 1)), drawn);
    }
}
