//! A link as a patch of the image's file (docs/v2/plan/design/patch-in-place.md): the byte
//! ranges it changes, and the file's identity before and after, with no file made.
//!
//! [`Image::bytes`] gives the whole file, which costs its length each link however little
//! changed. [`Image::patch`] makes the same commit and gives what the commit's effects would
//! write ([`Patch::writes`]): for a link placed into the room the image kept, the changed
//! function's bytes, its jump-table cell and the changed blocks of the tables and headers. A
//! caller that holds the last file (a store's copy it clones, a code memory) applies them
//! there; the file's identity is [`Patch::digest`], kept by rehashing only the blocks written
//! ([`crate::digest`]).
//!
//! The file a client runs is still never written in place (`tests/splice.rs`): a caller
//! applies a patch to a clone of the last file and renames it over, as [`Image::commit`]
//! does.

#![forbid(unsafe_code)]

use crate::digest::{BLOCK, Digest, Hash};
use crate::file::FileBytes;
use crate::state::State;
use crate::{Image, LinkError, Refusal, Staged};

/// What one commit changes of the image's file.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Patch {
    /// Whether the file is written whole: the image's first link, or one that keeps no room.
    /// `writes` then hold every byte that is not zero, and the file before is not read.
    pub full: bool,
    /// The file's length after, and before (0 with no file before).
    pub len: u64,
    pub old_len: u64,
    /// Ranges of the file and their bytes, by offset, none overlapping.
    pub writes: Vec<(u64, Vec<u8>)>,
    /// The identity of the file the patch applies to ([`Image::digest`] before the commit):
    /// `None` for the image's first link. A patch that is not `full` gives the file of
    /// `digest` only when applied to the file of `base`.
    pub base: Option<Hash>,
    /// The identity of the file after.
    pub digest: Hash,
    /// The generation the image is at after.
    pub generation: u64,
}

impl Patch {
    /// How many bytes the patch writes.
    pub fn written(&self) -> u64 {
        self.writes.iter().map(|it| it.1.len() as u64).sum()
    }

    /// Applies the patch to `file`, which holds the file of [`Patch::base`]: what a commit's
    /// effects do to a clone of it. A full patch starts from zeros.
    pub fn apply(&self, file: &mut Vec<u8>) {
        if self.full {
            file.clear();
        }
        file.resize(self.len as usize, 0);
        for (at, bytes) in &self.writes {
            let end = *at as usize + bytes.len();
            if end > file.len() {
                file.resize(end, 0);
            }
            file[*at as usize..end].copy_from_slice(bytes);
        }
    }
}

/// The ranges of the file commits changed since its digest was last brought up to date.
enum Stale {
    /// Any of it: a full link, or a commit that failed and was put back.
    All,
    Ranges(Vec<(u64, u64)>),
}

/// More stale ranges than this are one rehash of the file: a caller that commits many times
/// and never asks for the digest holds a bounded list.
const STALE_MOST: usize = 1 << 16;

/// An image's file identity, kept across commits: brought up to date only when asked for, by
/// rehashing the blocks the commits since wrote.
pub(crate) struct Filed {
    digest: Digest,
    stale: Stale,
}

impl Filed {
    pub(crate) fn new() -> Filed {
        Filed { digest: Digest::new(), stale: Stale::All }
    }

    /// A commit or a rollback made another state the image's: `full` when its file is
    /// written whole, else `ranges` are the ranges of the file before it that it overwrote
    /// (what it wrote past that file's end is found by the length).
    pub(crate) fn wrote(&mut self, full: bool, ranges: &[(u64, u64)]) {
        let fits = match &self.stale {
            Stale::All => return,
            Stale::Ranges(stale) => !full && stale.len() + ranges.len() <= STALE_MOST,
        };
        match &mut self.stale {
            Stale::Ranges(stale) if fits => stale.extend_from_slice(ranges),
            _ => self.stale = Stale::All,
        }
    }

    /// The planted bug of `the_oracle_catches_a_write_the_digest_missed`: each update of the
    /// digest skips a block it was told of.
    #[cfg(test)]
    pub(crate) fn plant_skips_a_block(&mut self) {
        self.digest.plant_skips_a_block = true;
    }

    /// A state was put back with no record of what differs: a failed commit's revert.
    pub(crate) fn wrote_any(&mut self) {
        self.stale = Stale::All;
    }

    /// The digest of `state`'s file: the stale blocks read from the state and hashed.
    fn of(&mut self, state: &State) -> &Digest {
        let bytes = FileBytes::of(state);
        let len = state.file_len;
        let mut read = |block: u64, out: &mut [u8; BLOCK]| {
            let at = block.saturating_mul(BLOCK as u64);
            let held = len.saturating_sub(at).min(BLOCK as u64) as usize;
            bytes.copy(at, &mut out[..held]);
        };
        let clip = |ranges: &mut Vec<(u64, u64)>| {
            ranges.retain_mut(|(at, held)| {
                *held = (*held).min(len.saturating_sub(*at));
                *held > 0
            });
        };
        // alloc: none, an empty `Vec` holds no buffer
        match std::mem::replace(&mut self.stale, Stale::Ranges(Vec::new())) {
            Stale::All => {
                #[cfg(test)]
                let planted = self.digest.plant_skips_a_block;
                self.digest = Digest::new();
                #[cfg(test)]
                {
                    self.digest.plant_skips_a_block = planted;
                }
                self.digest.resize(len, &mut read);
                let mut extents = bytes.extents();
                clip(&mut extents);
                self.digest.update(&extents, &mut read);
            }
            Stale::Ranges(mut ranges) => {
                let old = self.digest.len();
                self.digest.resize(len, &mut read);
                if len > old {
                    ranges.push((old, len - old));
                }
                clip(&mut ranges);
                self.digest.update(&ranges, &mut read);
                ranges.clear();
                self.stale = Stale::Ranges(ranges);
            }
        }
        &self.digest
    }
}

impl Image {
    /// The identity of the file the last commit left ([`crate::digest`]): what a caller that
    /// keeps the file names it by, and what a patch's [`Patch::base`] is. Costs the blocks
    /// the commits since the last call wrote. An error before the first commit or while one
    /// is in flight, naming the image.
    pub fn digest(&mut self) -> Result<Hash, LinkError> {
        if !self.state.laid_out || self.pending.is_some() {
            return Err(LinkError {
                function: None,
                refusal: None,
                // alloc: error path
                what: format!(
                    "{}: no committed file to name: the image is not laid out, or a commit is in flight",
                    self.to
                ),
            });
        }
        Ok(self.filed.of(&self.state).root())
    }

    /// Makes `staged` the image at once, as [`Image::bytes`] does (the same checks and
    /// history), and gives what it changes of the file instead of the file: the ranges a
    /// commit's effects would write, and the file's identity before and after. A commit that
    /// would tell running programs is refused as [`Image::bytes`] refuses it.
    pub fn patch(&mut self, mut staged: Staged) -> Result<Patch, LinkError> {
        self.can_commit(&staged)?;
        self.carry_placed_pairs(&mut staged.state)?;
        if staged.writes.message.is_some() {
            return Err(LinkError {
                function: None,
                refusal: None,
                // alloc: error path
                what: format!(
                    "{}: a live image's patches reach running programs only through the code memory: commit it with \
                     `commit_into`",
                    self.to
                ),
            });
        }
        let base = match self.state.laid_out {
            true => Some(self.filed.of(&self.state).root()),
            false => None,
        };
        let crate::commit::Writes { full, len, old_len, writes, message: _ } = staged.writes;
        self.take(staged.state, full, staged.undo);
        self.placed_pairs.clear();
        let digest = self.filed.of(&self.state).root();
        Ok(Patch { full, len, old_len, writes, base, digest, generation: self.state.generation })
    }

    /// Whether a file of `len` bytes is the one the last commit left: every block read
    /// through `read` (which fills a zeroed block with the file's bytes of that block) and
    /// hashed, and the root compared with [`Image::digest`]. Costs the file. For a caller
    /// about to patch a file it did not just write (after a crash, a restart, a copy taken
    /// from elsewhere): a file that fails is not patched, and the image is linked from
    /// scratch. The refusal ([`Refusal::Torn`]) names the first block that differs.
    pub fn verify(
        &mut self,
        len: u64,
        read: &mut impl FnMut(u64, &mut [u8; BLOCK]),
    ) -> Result<(), LinkError> {
        let want = self.digest()?;
        let mut file = Digest::new();
        file.resize(len, &mut *read);
        file.update(&[(0, len)], &mut *read);
        if file.root() == want {
            return Ok(());
        }
        let kept = self.filed.of(&self.state);
        let block = file.first_difference(kept).unwrap_or(0);
        Err(LinkError {
            function: None,
            refusal: Some(Refusal::Torn { block, len, kept: kept.len() }),
            // alloc: error path
            what: format!(
                "{}: the file is not the one its last link left (generation {}): {len:#x} bytes where the link \
                 state has {:#x}, first differing in block {block} (at {:#x}). A kept image patches only the file \
                 its digest names: link it from scratch",
                self.to,
                self.state.generation,
                kept.len(),
                block.saturating_mul(BLOCK as u64),
            ),
        })
    }
}
