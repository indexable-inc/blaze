//! Hash keys from the randomness capability: a [`SipKey`] for a table whose keys an attacker
//! may pick (blaze-hash's `Map<K, V, SipKey, A>`, an adaptive table's standby key), drawn as one
//! [`Effect::Random`] of sixteen bytes through the world's driver. On a native world that is
//! the system's secure source, so the key is never known outside the process; in a simulation
//! the bytes are the seeded world's, so a run keyed this way places every key alike when it is
//! replayed by its seed.
//!
//! Why not from an [`Rng`](crate::rng::Rng): bstd's generator is SplitMix64, whose every output
//! inverts to its state, and every generator of a world is forked from one stream; one number
//! of any of them shown to an attacker (a temporary name, a request id) would give every key
//! drawn from that stream. Sixteen fresh bytes of the driver's source share nothing with any
//! other draw and fill all 128 bits of SipHash's key.

use blaze_core::{Done, Effect, wait::Deadline};
use blaze_hash::SipKey;

use super::{Io, Missing};

impl Io {
    /// A fresh hash key, sixteen bytes of the world's random source, if the host offers
    /// randomness (the same offer as [`Io::rng`]). Each call draws anew, so no two keys are
    /// related. A driver that does not answer within the world's `abandon_wait`, or fails,
    /// is a host that cannot give the randomness it offered: that panics, naming it, as the
    /// generator's seed does.
    pub fn sip_key(&self) -> Option<SipKey> {
        self.offered(|it| it.rng).then(|| self.draw_sip_key())
    }

    pub fn need_sip_key(&self) -> Result<SipKey, Missing> {
        self.sip_key().ok_or(Missing { capability: "rng" })
    }

    fn draw_sip_key(&self) -> SipKey {
        let tag = self.submit(|tag| Effect::Random { len: 16, tag });
        let until = Deadline::after(self.now(), self.limits.abandon_wait);
        match self.settle(tag, until, "bstd.io.sip_key") {
            Ok(Done::Random(bytes)) if bytes.len() == 16 => {
                let mut low = [0; 8];
                let mut high = [0; 8];
                low.copy_from_slice(&bytes[..8]);
                high.copy_from_slice(&bytes[8..]);
                SipKey::new(u64::from_le_bytes(low), u64::from_le_bytes(high))
            }
            Ok(done) => panic!("bstd: Io::sip_key: the driver answered {done:?}, not 16 random bytes"),
            Err(stopped) => panic!("bstd: Io::sip_key: the driver did not answer: {stopped}"),
        }
    }
}
