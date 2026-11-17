//! Verified bytes (the coordinator's ruling, 2026-11-16 20:00, a soundness hole closed): a task
//! that reads a file by path files its record under a key made from the SWEEP's content id. An
//! edit that lands between the sweep and the task's read would file the new text's record under
//! the old text's key (rule 21: two values under one key). So every such task hashes the bytes
//! it read and compares them with the content id its key was made from, here, once; the parse
//! takes the [`Verified`] this returns, so parsing unverified bytes does not compile. On a
//! difference the task fails with [`Why::Changed`]: it puts nothing, nothing re-reads or
//! retries (rule 3), and the command's answer is refused naming the file.

use blaze_id::ContentId;

use crate::Plant;
use crate::key::Name;
use crate::request::{TaskFailure, Why};

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
/// `plant` is `Plant::SkipContentCompare` only in the simulation's control.
pub fn verify<'b>(name: Name, bytes: &'b [u8], expect: ContentId, id: impl FnOnce(&[u8]) -> ContentId, plant: Plant) -> Result<Verified<'b>, TaskFailure> {
    if plant == Plant::SkipContentCompare || id(bytes) == expect {
        return Ok(Verified { bytes });
    }
    Err(TaskFailure { name, why: Why::Changed })
}
