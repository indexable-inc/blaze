//! Verified bytes (the coordinator's ruling, 2026-11-16 20:00, a soundness hole closed): a task
//! that reads a file by path files its record under a key made from the SWEEP's content id. An
//! edit that lands between the sweep and the task's read would file the new text's record under
//! the old text's key (rule 21: two values under one key). So every such task hashes the bytes
//! it read and compares them with the content id its key was made from, here, once; the parse
//! takes the [`Verified`] this returns, so parsing unverified bytes does not compile. On a
//! difference the task fails with [`Why::Changed`]: it puts nothing, nothing re-reads or
//! retries (rule 3), and the command's answer is refused naming the file.

use blaze_id::ContentId;

use crate::key::Name;
use crate::task::{TaskFailure, Why};

/// A bug a test plants, to show its oracle catches it: [`Plant::None`] outside the tests.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Plant {
    None,
    /// The bytes read are taken without comparing them with the content id the key holds.
    SkipContentCompare,
}

/// Bytes whose content id is the one the task's key was made from: the proof the parse takes.
/// Its field is private: [`verify`] is the one way to make one.
#[derive(Clone, Copy, Debug)]
pub struct Verified<'b> {
    bytes: &'b [u8],
}

impl<'b> Verified<'b> {
    /// The bytes read, proven to be the swept ones.
    pub fn bytes(self) -> &'b [u8] {
        self.bytes
    }
}

/// Compares the `bytes` task `name` read with `expect`, the content id its key holds; `id` is
/// the same digest the sweep made `expect` with (the caller's: the drive does not know it).
/// `plant` is [`Plant::SkipContentCompare`] only in a test's control.
pub fn verify<'b>(name: Name, bytes: &'b [u8], expect: ContentId, id: impl FnOnce(&[u8]) -> ContentId, plant: Plant) -> Result<Verified<'b>, TaskFailure> {
    if plant == Plant::SkipContentCompare || id(bytes) == expect {
        return Ok(Verified { bytes });
    }
    Err(TaskFailure { name, why: Why::Changed })
}

#[cfg(test)]
mod tests {
    //! The property the deleted request simulation held (`a_file_changed_after_the_sweep_puts_nothing`),
    //! at its one home: bytes edited after the sweep are refused `Why::Changed`, naming the file;
    //! the swept bytes pass. Expected: "test verified::tests::<name> ... ok".

    use std::format;
    use std::vec::Vec;

    use blaze_sim::{Broke, Config as SimConfig, Tape, check, control};

    use super::{Plant, verify};
    use crate::key::Name;
    use crate::task::{TaskFailure, Why};

    const DOMAIN: &str = "blaze-drive verified test";

    fn id(bytes: &[u8]) -> blaze_id::ContentId {
        blaze_id::ContentId::of(DOMAIN, bytes)
    }

    /// A file swept as `swept`, then read as `read` (equal, or edited in one byte, cut short, or
    /// grown): the read verifies exactly when the bytes are the swept ones.
    fn read_after_sweep(tape: &Tape, plant: Plant) -> Result<(), Broke> {
        let len = tape.index(16, "file length");
        let swept: Vec<u8> = (0..len).map(|i| tape.index(256, "byte") as u8 ^ i as u8).collect();
        let mut read = swept.clone();
        match tape.index(4, "edit after the sweep") {
            0 => {}
            1 if !read.is_empty() => {
                let at = tape.index(read.len(), "edited byte");
                read[at] = read[at].wrapping_add(1);
            }
            2 if !read.is_empty() => {
                read.pop();
            }
            _ => read.push(b'x'),
        }
        let name = Name::file(b"src/lib.rs");
        let verdict = verify(name, &read, id(&swept), id, plant);
        match (read == swept, verdict) {
            (true, Ok(verified)) if verified.bytes() == read.as_slice() => Ok(()),
            (false, Err(TaskFailure { name: failed, why: Why::Changed })) if failed == name => Ok(()),
            (same, verdict) => Err(Broke::Oracle(format!("bytes equal to the swept ones: {same}; verify answered {:?}", verdict.map(|v| v.bytes().len())))),
        }
    }

    #[test]
    fn bytes_edited_after_the_sweep_are_refused_by_name() {
        check(SimConfig::cases(300), |tape| read_after_sweep(tape, Plant::None));
    }

    #[test]
    fn control_a_skipped_content_compare_is_caught() {
        control(SimConfig::cases(300), Plant::None, Plant::SkipContentCompare, "verify answered", read_after_sweep);
    }
}
