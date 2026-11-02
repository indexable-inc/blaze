//! Randomness a macro reads. std's `HashMap` keys are fixed: one valid outcome of randomness,
//! so a macro that builds a `HashMap` is still kept. Every other source gives real random
//! bytes and makes what it made one that can't be kept ([`Untraceable::Random`]).

use blaze_core::Untraceable;
use blaze_usercode::trace::{Seen, untraceable_name};

use super::{
    Answer, Done, Noted, Os, fail, ok,
    target::{Errno, Target},
};
use crate::guest::Guest;

/// The keys every `RandomState` of a macro starts from.
const HASHMAP_KEYS: (u64, u64) = (0x0123_4567_89ab_cdef, 0xfedc_ba98_7654_3210);

/// The most `getentropy` gives at once.
const ENTROPY_MAX: u64 = 256;

/// `hashmap_random_keys() -> (u64, u64)`.
pub(super) fn hashmap_random_keys(ret: &mut [u8]) -> Answer {
    let (k0, k1) = HASHMAP_KEYS;
    let len = ret.len().min(8);
    ret[..len].copy_from_slice(&k0.to_le_bytes()[..len]);
    if ret.len() >= 16 {
        ret[8..16].copy_from_slice(&k1.to_le_bytes());
    }
    Answer::Answered
}

impl<'a> Os<'a> {
    /// Writes `len` random bytes at `buf`, and returns `returns`, or `len` for `None` and
    /// nothing for a `void` function.
    pub(super) fn fill_random(
        &mut self,
        g: &mut impl Guest,
        ret: &mut [u8],
        (buf, len): (u64, u64),
        returns: Option<u64>,
    ) -> Done {
        if !self.trace.random {
            self.trace.random = true;
            self.trace.note(Noted::Seen(Seen::Untraceable), untraceable_name(Untraceable::Random));
        }
        let mut chunk = [0; 256];
        let mut at = 0;
        while at < len {
            let take = (len - at).min(chunk.len() as u64) as usize;
            std::random::Rng::fill_bytes(&mut std::random::SystemRng, &mut chunk[..take]);
            g.write(buf + at, &chunk[..take])?;
            at += take as u64;
        }
        ok(ret, returns.unwrap_or(len))
    }

    /// `getrandom(buf, len, flags)`, as a function or through `syscall`.
    pub(super) fn getrandom(&mut self, g: &mut impl Guest, ret: &mut [u8], (buf, len): (u64, u64)) -> Done {
        self.fill_random(g, ret, (buf, len), None)
    }

    pub(super) fn getentropy(&mut self, t: Target, g: &mut impl Guest, ret: &mut [u8], (buf, len): (u64, u64)) -> Done {
        if len > ENTROPY_MAX {
            return fail(t, g, ret, Errno::Io);
        }
        self.fill_random(g, ret, (buf, len), Some(0))
    }
}
