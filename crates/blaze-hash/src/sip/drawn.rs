//! A [`LazyKey`] filled from a key its caller drew: the capability form of [`LazyKey::get`],
//! which draws from the operating system inside the library call. Here the caller says where
//! the key comes from (bstd's `Io::sip_key`, a seeded world's in a simulation), so a run keyed
//! from a seed places every key alike each time it is replayed.

use super::{LazyKey, SipKey};

impl LazyKey {
    /// The key: the one already held, or else `draw()`'s, held from now on. `draw` runs at most
    /// once over the key's life, at the first ask; a later ask's `draw` is not called, so every
    /// ask gives the first key (two threads asking at once agree on one of their draws).
    #[inline]
    pub fn get_or(&self, draw: impl FnOnce() -> SipKey) -> SipKey {
        *self.0.get_or_init(draw)
    }
}

#[cfg(test)]
mod tests {
    use super::{LazyKey, SipKey};

    /// A lazy key asked twice, with draws that would give different keys, gives the first
    /// draw's key both times, and the second draw never runs.
    ///
    /// Expected line: `test sip::drawn::tests::get_or_draws_once ... ok`.
    #[test]
    fn get_or_draws_once() {
        let key = LazyKey::new();
        let first = key.get_or(|| SipKey::new(1, 2));
        let second = key.get_or(|| panic!("a second draw ran: the key was held"));
        assert_eq!(first, SipKey::new(1, 2));
        assert_eq!(second, first);
        // Control: a fresh key takes its own draw, so the test above is not passing on a
        // `get_or` that ignores `draw`.
        assert_eq!(LazyKey::new().get_or(|| SipKey::new(3, 4)), SipKey::new(3, 4));
    }
}
