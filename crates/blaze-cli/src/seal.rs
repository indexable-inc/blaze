//! The last answer's record (drive.md ruling 3): the drive's last-answer pointer and the leaf
//! table in one record, named by the checkout's root for one (tool, request shape) in the one
//! store (store.rs `last_root`), sealed by a header that holds the body's length and a check
//! over the length and the body. (Before: a pointer file under `.blaze/v2/at`, an old form
//! reported once by store.rs `old_home` and not read.)
//!
//! Why this form: a write-then-rename of each of the two files cost about 260 µs a file, a
//! quarter of an edit's time in main. One file written in place is one truncate and one write.
//! A crash during the write leaves a file whose check fails: it reads as ABSENT (one line on
//! stderr, then the request answers in full and writes it anew), never as an error that stops
//! the command and never as valid. Absent is always safe: the pointer is derived data, and a
//! request with none walks in full against the store's records.
//!
//! ```text
//! magic "BCS1" | len: u32 | check: [u8; 16] = H(len, body)[..16] | body: [u8; len]
//! body = last's length: u32 | last (the drive's BDL form) | the leaf table (BCL form)
//! ```

use core::fmt;

use blaze_ids::ContentId;
use bstd::mem::Frame;
use blaze_own::Vec;

use crate::CliError;

/// The pointer file's form.
pub(crate) const SEAL_MAGIC: &[u8; 4] = b"BCS1";
/// What a note or an error calls a pointer file.
pub(crate) const PATH: &str = "the last answer's record in the store";
const HEADER: usize = 4 + 4 + 16;
const CHECK_DOMAIN: &str = "blaze-cli seal check 1";

/// A bug the seal's test plants, to show the test catches it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Plant {
    None,
    /// The reader takes the header's length without comparing the check.
    CheckIgnored,
}

/// Why a pointer file of this form is not whole: a write that a crash cut short.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Torn {
    /// The file's length, and the length its header claims (0 when no header).
    pub len: u32,
    pub claims: u32,
}

impl fmt::Display for Torn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{PATH} is torn ({} bytes, its header claims a {}-byte body, its check fails): a write a crash cut short; answering in full and writing it anew",
            self.len, self.claims
        )
    }
}

/// The length is in the check for a reader's sake, not for safety: the body is sliced by it, so a
/// check of the body alone already fails on a wrong length (a planted check without the length
/// was tried and no crash told it apart; it is not kept as a control).
fn check(len: u32, body: &[u8]) -> [u8; 16] {
    let mut h = blaze_ids::Hasher::new(CHECK_DOMAIN);
    h.update(&len.to_le_bytes());
    h.update(body);
    let id: ContentId = h.finish();
    let mut out = [0; 16];
    out.copy_from_slice(&id.0[..16]);
    out
}

/// The sealed bytes of `last` and `leaves`.
pub(crate) fn seal<'f>(last: &[u8], leaves: &[u8], frame: &'f Frame) -> Result<Vec<u8, &'f Frame>, CliError> {
    let too_big = |_| CliError::Text { what: "the checkout's pointer file past 4 GiB" };
    let last_len = u32::try_from(last.len()).map_err(too_big)?;
    let len = u32::try_from(4 + last.len() + leaves.len()).map_err(too_big)?;
    let mut bytes = Vec::with_capacity_in(HEADER + len as usize, frame);
    bytes.extend_from_slice(SEAL_MAGIC);
    bytes.extend_from_slice(&len.to_le_bytes());
    bytes.extend_from_slice(&[0; 16]);
    bytes.extend_from_slice(&last_len.to_le_bytes());
    bytes.extend_from_slice(last);
    bytes.extend_from_slice(leaves);
    let sum = check(len, &bytes[HEADER..]);
    bytes[8..HEADER].copy_from_slice(&sum);
    Ok(bytes)
}

/// The body of sealed `bytes` (of this form: the caller checked the magic), split into the last
/// pointer and the leaf table; [`Torn`] when the header or the check says the write was cut.
pub(crate) fn open(bytes: &[u8], plant: Plant) -> Result<(&[u8], &[u8]), Torn> {
    let file_len = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
    let Some(header) = bytes.get(..HEADER) else { return Err(Torn { len: file_len, claims: 0 }) };
    let claims = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);
    let torn = Torn { len: file_len, claims };
    let body = bytes.get(HEADER..HEADER + claims as usize).ok_or(torn)?;
    if plant != Plant::CheckIgnored && check(claims, body) != header[8..HEADER] {
        return Err(torn);
    }
    let last_len = body.first_chunk::<4>().map(|word| u32::from_le_bytes(*word) as usize).ok_or(torn)?;
    let last = body.get(4..4 + last_len).ok_or(torn)?;
    Ok((last, &body[4 + last_len..]))
}

#[cfg(test)]
mod tests {
    use blaze_sim::{Broke, Config, Tape, check as sim_check, control};
    use bstd::io::{Io, Limits, SimWorld};
    use bstd::mem::TASK_FRAME;
    use bstd::test::BUDGET;

    use super::*;

    fn draw(tape: &Tape, what: &'static str) -> (std::vec::Vec<u8>, std::vec::Vec<u8>) {
        let last = tape.bytes(0..=40, what);
        let leaves = tape.bytes(0..=80, what);
        (last, leaves)
    }

    /// Every file a crash can leave while new sealed bytes are written over old ones: a write
    /// cut after any byte, (a) after the truncate, as `write` does, and (b) in place over the old
    /// bytes, and (c) with any subset of its 4-byte words reaching the disk, the file as long as
    /// either version. Each reads as whole-old, whole-new or absent.
    fn crashes(tape: &Tape, plant: Plant) -> Result<(), Broke> {
        let io = Io::simulated(SimWorld::new(1, Limits::USUAL));
        let cx = io.cx(BUDGET, TASK_FRAME).map_err(|e| Broke::Harness(std::format!("{e}")))?;
        let frame = cx.frame();
        let (old_last, old_leaves) = draw(tape, "old");
        let (new_last, new_leaves) = draw(tape, "new");
        let harness = |e: CliError| Broke::Harness(std::format!("{e}"));
        let old = seal(&old_last, &old_leaves, frame).map_err(harness)?.as_slice().to_vec();
        let new = seal(&new_last, &new_leaves, frame).map_err(harness)?.as_slice().to_vec();
        let whole = |bytes: &[u8], what: &str| -> Result<(), Broke> {
            match open(bytes, plant) {
                Err(_) => Ok(()),
                Ok((last, leaves)) if (last, leaves) == (&old_last[..], &old_leaves[..]) => Ok(()),
                Ok((last, leaves)) if (last, leaves) == (&new_last[..], &new_leaves[..]) => Ok(()),
                Ok(_) => Err(Broke::Oracle(std::format!("a crash {what} left a pointer read as neither old nor new"))),
            }
        };
        if open(&new, plant).map(|(a, b)| (a.to_vec(), b.to_vec())) != Ok((new_last.clone(), new_leaves.clone())) {
            return Err(Broke::Oracle(std::string::String::from("a whole write does not read back")));
        }
        for cut in 0..=new.len() {
            whole(&new[..cut], "after the truncate")?;
            let mut over = old.clone();
            over.resize(old.len().max(cut), 0);
            over[..cut].copy_from_slice(&new[..cut]);
            whole(&over, "in place")?;
        }
        let len = if tape.one_in(2, "torn length") { old.len() } else { new.len() };
        let mut torn = old.clone();
        torn.resize(len, 0);
        for (word, chunk) in new.chunks(4).enumerate() {
            if tape.one_in(2, "word reached the disk") {
                for (k, byte) in chunk.iter().enumerate() {
                    if let Some(slot) = torn.get_mut(word * 4 + k) {
                        *slot = *byte;
                    }
                }
            }
        }
        whole(&torn, "with words torn")
    }

    /// The invariant: a sealed pointer record interrupted or torn at any point reads back whole old, whole new, or absent, and never accepts bytes whose header check fails.
    #[test]
    fn a_crash_at_any_point_reads_whole_old_whole_new_or_absent() {
        sim_check(Config::cases(500), |tape| crashes(tape, Plant::None));
    }

    #[test]
    fn the_oracle_catches_an_ignored_check() {
        control(Config::cases(2000), Plant::None, Plant::CheckIgnored, "neither old nor new", crashes);
    }
}
