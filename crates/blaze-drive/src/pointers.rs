//! The checkout's last-answer pointers (read-addressed.md, "End state: what an edit costs",
//! piece 1): one per (tool, request shape), each a file `.blaze/v2/at/<name>` whose name is 16
//! hex digits of H(tool, shape), at most [`KEPT`] of them.
//!
//! Why one per (tool, shape): the machine walks forward only from its own tool's and shape's
//! last answer (request.rs `begin`). With one pointer a checkout, a `test` between two `check`s,
//! or a second build of blaze, replaced the pointer the next `check` would have walked from, so
//! that `check` walked the whole build from the units again (and, before request.rs
//! `walks_from`, was handed no dependency graph at all: blaze's own checkout, 2026-11-16). With
//! one pointer per (tool, shape) each command of each build finds its own last answer.
//!
//! Why at most [`KEPT`]: every rebuilt blaze is another tool, so the folder would grow by a
//! pointer a build. A pointer is derived data (a request with none answers in full against the
//! store's records), so the least recently written ones go when a new one is written.
//!
//! Sans-IO: this file names a pointer and says which to remove; the command (blaze-cli seal.rs)
//! lists the folder, stats the pointers and removes what [`evicted`] names. Proof: sim.rs,
//! `each_tool_and_shape_walks_forward_from_its_own_pointer` and its planted bugs.

use blaze_id::ContentId;
use blaze_ids::Hasher;
use blaze_own::AVec;

use crate::key::Tool;

/// The most pointers a checkout keeps.
pub const KEPT: usize = 8;

/// The pointers' folder, checkout-relative.
pub const FOLDER: &str = ".blaze/v2/at";

const DOMAIN: &str = "blaze-drive pointer name 1";

/// A bug the simulation plants, to show its oracle catches it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Plant {
    None,
    /// Every tool and shape is given one name: the pointer the old `.blaze/v2/last` was.
    OneName,
    /// The newest pointers go and the oldest stay.
    EvictNewest,
}

/// A pointer file's name: 16 lowercase hex digits.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct PointerName([u8; 16]);

impl PointerName {
    /// The name of `tool`'s pointer for requests of `shape` (request.rs `shape_of`).
    pub fn of(tool: Tool, shape: &ContentId) -> PointerName {
        PointerName::planted(tool, shape, Plant::None)
    }

    /// [`PointerName::of`], with the sim's planted bug.
    pub fn planted(tool: Tool, shape: &ContentId, plant: Plant) -> PointerName {
        let mut h = Hasher::new(DOMAIN);
        if plant != Plant::OneName {
            h.update(&tool.0.0).update(&shape.0);
        }
        let id: ContentId = h.finish();
        let mut name = [0; 16];
        for (at, byte) in id.0[..8].iter().enumerate() {
            name[2 * at] = HEX[usize::from(byte >> 4)];
            name[2 * at + 1] = HEX[usize::from(byte & 15)];
        }
        PointerName(name)
    }

    /// The name a folder entry `name` is, when it is a pointer's: exactly 16 lowercase hex
    /// digits. Anything else in the folder is not a pointer and is left alone.
    pub fn parse(name: &str) -> Option<PointerName> {
        let bytes: &[u8; 16] = name.as_bytes().try_into().ok()?;
        bytes.iter().all(|byte| HEX.contains(byte)).then_some(PointerName(*bytes))
    }

    /// The name's 16 hex digits.
    pub fn bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

const HEX: &[u8; 16] = b"0123456789abcdef";

/// The pointers to remove when `writing` is written: `held` is each pointer in the folder and
/// when it was last written (any clock that orders writes: the file's modified stamp); after
/// the write and the removals the folder holds `writing` and the at most [`KEPT`] - 1 others
/// written last, a tie going by name. `held` is sorted in place; the removed are pushed to `out`.
pub fn evicted(held: &mut [(PointerName, u64)], writing: PointerName, plant: Plant, out: &mut AVec<'_, PointerName>) {
    match plant {
        Plant::EvictNewest => held.sort_unstable_by(|a, b| a.1.cmp(&b.1).then(a.0.cmp(&b.0))),
        Plant::None | Plant::OneName => held.sort_unstable_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0))),
    }
    let mut others = 0;
    for (name, _) in held.iter() {
        if *name == writing {
            continue;
        }
        others += 1;
        if others >= KEPT {
            out.push(*name);
        }
    }
}
