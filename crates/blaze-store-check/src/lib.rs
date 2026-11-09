//! The store check: `BLAZE_VERIFY_STORE=<rate>` recomputes that fraction of store hits and
//! compares the bytes (docs/v2/plan/design/verify.md).
//!
//! A store hit is a value the build took instead of computing it. Under the check a sampled
//! hit is computed again from its inputs, never from the store or a memo, and the two byte
//! strings must be equal; a difference is a [`Mismatch`] naming the function, the identity
//! it is stored under, the reads its entry recorded and the first byte where they differ.
//! Which hits are sampled is a pure function of the setting's seed and the hit's identity, so
//! one tree and one setting check the same entries every run (rule 25).
//!
//! The kernel implements [`Recompute`] for its compute steps and calls [`check`] on each hit;
//! a caller that recomputes in batches (blaze-codegen's driver) samples with
//! [`Setting::sampled`] and compares with [`compare`].
//!
//! This crate holds the two pieces every check of blaze's shares, so there is one of each:
//! [`Setting::sampled`], the one sampler (what to check when not everything is, a function of
//! the seed and the identity alone), and [`bytes`], the one byte comparison (where two byte
//! strings first differ, by the CPU's SIMD). The invalidation oracle, `blaze-v2-proof`, uses
//! both; it holds what compares two builds' records (`under`, `over`, `apart`).

pub mod bytes;

use std::fmt;

use blaze_ids::{ContentId, Hasher};

/// The variable, read from the request's environment (as `BLAZE_SANDBOX` is).
pub const VAR: &str = "BLAZE_VERIFY_STORE";

const SAMPLE_DOMAIN: &str = "blaze v2 verify store sample 1";

/// How much of the store's hits the build recomputes.
///
/// Written as a bare rate (`0.01`, `1`) or as one JSON object, which refuses unknown fields:
/// `{"rate": 0.01, "seed": 7}`. The seed picks which hits a rate below one samples, so CI can
/// rotate through the store. Unset, or a rate of zero, is off.
#[derive(Clone, Copy, PartialEq, Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Setting {
    /// The fraction of hits recomputed, from 0 (none) to 1 (every one).
    pub rate: f64,
    #[serde(default)]
    pub seed: u64,
}

/// A value of [`VAR`] that is no [`Setting`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SettingError {
    pub value: String,
    pub why: String,
}

impl fmt::Display for SettingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{VAR}={:?}: {}; it takes a rate from 0 to 1 (`0.01`) or `{{\"rate\": 0.01, \"seed\": 7}}`",
            self.value, self.why
        )
    }
}

impl std::error::Error for SettingError {}

impl Default for Setting {
    fn default() -> Setting {
        Setting::OFF
    }
}

impl Setting {
    pub const OFF: Setting = Setting { rate: 0.0, seed: 0 };

    /// Every hit, seed 0.
    pub const ALL: Setting = Setting { rate: 1.0, seed: 0 };

    /// The setting `value` spells.
    pub fn parse(value: &str) -> Result<Setting, SettingError> {
        let refused = |why: String| SettingError { value: value.to_owned(), why }; // alloc: an error
        let trimmed = value.trim();
        let setting = if trimmed.starts_with('{') {
            serde_json::from_str::<Setting>(trimmed).map_err(|e| refused(e.to_string()))?
        } else {
            let rate = trimmed.parse::<f64>().map_err(|e| refused(e.to_string()))?;
            Setting { rate, seed: 0 }
        };
        if !(0.0..=1.0).contains(&setting.rate) {
            return Err(refused(format!("the rate {} is not from 0 to 1", setting.rate))); // alloc: an error
        }
        Ok(setting)
    }

    /// The setting of a request whose environment is `env`: [`Setting::OFF`] when it names
    /// no [`VAR`], the last one when it names it more than once (as a shell's does).
    pub fn from_env(env: &[(String, String)]) -> Result<Setting, SettingError> {
        match env.iter().rev().find(|(name, _)| name == VAR) {
            Some((_, value)) => Setting::parse(value),
            None => Ok(Setting::OFF),
        }
    }

    pub fn is_off(&self) -> bool {
        self.rate <= 0.0
    }

    /// Whether the hit stored under `identity` is recomputed: the first eight bytes of
    /// `hash(seed, identity)` below `rate` of their range, so a rate of one takes every hit
    /// and zero none.
    pub fn sampled(&self, identity: &ContentId) -> bool {
        if self.is_off() {
            return false;
        }
        if self.rate >= 1.0 {
            return true;
        }
        let drawn =
            Hasher::new(SAMPLE_DOMAIN).update(&self.seed.to_le_bytes()).id(identity).finish();
        let word = u64::from_le_bytes(drawn.0[..8].try_into().expect("eight bytes"));
        // `rate * 2^64`, below `2^64` as the rate is below one.
        (word as f64) < self.rate * 18_446_744_073_709_551_616.0
    }
}

/// One read a stored entry recorded: what was read and the content hash it had.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RecordedRead<'a> {
    pub what: &'a str,
    pub hash: ContentId,
}

/// A store hit the build took instead of computing it.
#[derive(Clone, Copy, Debug)]
pub struct Hit<'a> {
    /// The function, instance or value's path, as an error names it.
    pub function: &'a str,
    /// What it is stored under: the kernel's identity (the item's code identity, the compute
    /// step's `CodeId` and the target and flags), or a codegen template's store key.
    pub identity: ContentId,
    /// The reads its entry recorded; none for an entry keyed by its whole content, as
    /// codegen's templates are today.
    pub reads: &'a [RecordedRead<'a>],
    /// The bytes the store gave.
    pub stored: &'a [u8],
}

/// What a compute step implements to be checked: the value of `hit` computed again from its
/// inputs, never read from the store or a memo. `Err` is why it could not be computed.
pub trait Recompute {
    fn recompute(&self, hit: &Hit<'_>) -> Result<Vec<u8>, String>;
}

/// A recomputed value that is not the stored one.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Mismatch {
    pub function: String,
    pub identity: ContentId,
    /// The entry's reads, as `(what, hash)`.
    pub reads: Vec<(String, ContentId)>,
    /// The first byte offset at which they differ: the shorter's length when one is the
    /// other's prefix.
    pub at: usize,
    /// The stored byte at `at`, `None` past its end.
    pub stored: Option<u8>,
    /// The recomputed byte at `at`, `None` past its end.
    pub recomputed: Option<u8>,
    pub stored_len: usize,
    pub recomputed_len: usize,
}

struct Byte(Option<u8>);

impl fmt::Display for Byte {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(byte) => write!(f, "{byte:#04x}"),
            None => f.write_str("its end"),
        }
    }
}

impl fmt::Display for Mismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{VAR}: the store's value of `{}` (identity {:x}) is not what computing it again gives: \
             they differ first at byte {} (stored {}, recomputed {}; {} bytes stored, {} recomputed); ",
            self.function,
            self.identity,
            self.at,
            Byte(self.stored),
            Byte(self.recomputed),
            self.stored_len,
            self.recomputed_len,
        )?;
        if self.reads.is_empty() {
            return f.write_str("its entry recorded no reads (it is keyed by its whole content)");
        }
        f.write_str("its entry's reads: ")?;
        for (n, (what, hash)) in self.reads.iter().enumerate() {
            if n > 0 {
                f.write_str(", ")?;
            }
            write!(f, "{what} = {hash}")?;
        }
        Ok(())
    }
}

impl std::error::Error for Mismatch {}

/// Why a sampled hit failed its check.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum VerifyError {
    Mismatch(Mismatch),
    /// Computing it again failed, where the store had a value.
    Recompute {
        function: String,
        identity: ContentId,
        why: String,
    },
}

impl fmt::Display for VerifyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VerifyError::Mismatch(it) => it.fmt(f),
            VerifyError::Recompute { function, identity, why } => write!(
                f,
                "{VAR}: the store has a value of `{function}` (identity {identity:x}), and computing it again failed: {why}"
            ),
        }
    }
}

impl std::error::Error for VerifyError {}

/// `Ok` when `recomputed` is the bytes `hit` holds, else the [`Mismatch`] naming where.
pub fn compare(hit: &Hit<'_>, recomputed: &[u8]) -> Result<(), Mismatch> {
    let Some(at) = bytes::first_difference(hit.stored, recomputed) else { return Ok(()) };
    Err(Mismatch {
        function: hit.function.to_owned(), // alloc: an error
        identity: hit.identity,
        // alloc: an error
        reads: hit.reads.iter().map(|it| (it.what.to_owned(), it.hash)).collect(),
        at,
        stored: hit.stored.get(at).copied(),
        recomputed: recomputed.get(at).copied(),
        stored_len: hit.stored.len(),
        recomputed_len: recomputed.len(),
    })
}

/// Checks `hit` when `setting` samples it: `Ok(false)` when it does not, `Ok(true)` when
/// `step` computed it again to the same bytes.
pub fn check(setting: &Setting, hit: &Hit<'_>, step: &impl Recompute) -> Result<bool, VerifyError> {
    if !setting.sampled(&hit.identity) {
        return Ok(false);
    }
    let recomputed = step.recompute(hit).map_err(|why| VerifyError::Recompute {
        function: hit.function.to_owned(), // alloc: an error
        identity: hit.identity,
        why,
    })?;
    compare(hit, &recomputed).map_err(VerifyError::Mismatch)?;
    Ok(true)
}

#[cfg(test)]
mod tests;
