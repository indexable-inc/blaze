//! The checkout's last-answer pointers (read-addressed.md, "End state: what an edit costs",
//! piece 1): one per (tool, request shape), each a file `.blaze/v2/at/<name>` whose name is 16
//! hex digits of H(tool, shape), at most [`KEPT`] of them.
//!
//! Why one per (tool, shape): a command walks forward only from its own tool's and shape's last
//! answer. With one pointer a checkout, a `test` between two `check`s, or a second build of
//! blaze, replaced the pointer the next `check` would have walked from, so that `check` walked
//! the whole build from the units again (blaze's own checkout, 2026-11-16). With one pointer per
//! (tool, shape) each command of each build finds its own last answer.
//!
//! Why at most [`KEPT`]: every rebuilt blaze is another tool, so the folder would grow by a
//! pointer a build. A pointer is derived data (a request with none answers in full against the
//! store's records), so the least recently written ones go when a new one is written.
//!
//! Sans-IO: this file names a pointer and says which to remove; the command (blaze-cli seal.rs)
//! lists the folder, stats the pointers and removes what [`evicted`] names. Proof: this file's
//! tests and their planted bugs.

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
    /// The name of `tool`'s pointer for commands of `shape` (the digest of the command and its
    /// arguments, the caller's).
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

#[cfg(test)]
mod tests {
    //! The pointer properties the deleted request simulation held through whole commands
    //! (`each_tool_and_shape_walks_forward_from_its_own_pointer`,
    //! `a_checkout_keeps_the_pointers_written_last`), at their one home: names and eviction.
    //! Expected: "test pointers::tests::<name> ... ok"; each control passes when its plant is
    //! caught, with the message it names.

    use std::collections::{BTreeMap, BTreeSet};
    use std::format;
    use std::vec::Vec as StdVec;

    use blaze_id::ContentId;
    use blaze_own::Vec;
    use blaze_sim::{Broke, Config as SimConfig, Tape, check, control};

    use super::{KEPT, Plant, PointerName, evicted};
    use crate::key::Tool;

    fn tool(n: u8) -> Tool {
        Tool(ContentId([n; 32]))
    }

    fn shape(n: u8) -> ContentId {
        ContentId::of("blaze-drive pointers test shape", &[n])
    }

    /// Distinct (tool, shape) pairs, drawn: each has its own name, so a command of one never
    /// reads another's last answer; the same pair names one pointer, and a name parses back.
    fn own_names(tape: &Tape, plant: Plant) -> Result<(), Broke> {
        let pairs = 2 + tape.index(6, "pairs");
        let mut seen: BTreeMap<PointerName, (u8, u8)> = BTreeMap::new();
        for at in 0..pairs {
            let pair = (at as u8, tape.index(3, "shape") as u8);
            let name = PointerName::planted(tool(pair.0), &shape(pair.1), plant);
            if name != PointerName::planted(tool(pair.0), &shape(pair.1), plant) {
                return Err(Broke::Oracle(format!("the pair {pair:?} named two pointers")));
            }
            let text = core::str::from_utf8(name.bytes()).map_err(|_| Broke::Oracle(format!("the pointer of {pair:?} is not text")))?;
            if PointerName::parse(text) != Some(name) {
                return Err(Broke::Oracle(format!("the pointer of {pair:?} does not parse back")));
            }
            if let Some(other) = seen.insert(name, pair) {
                return Err(Broke::Oracle(format!("one pointer for {other:?} and {pair:?}: a command walks from another's answer")));
            }
        }
        Ok(())
    }

    #[test]
    fn each_tool_and_shape_has_its_own_pointer() {
        check(SimConfig::cases(200), |tape| own_names(tape, Plant::None));
    }

    #[test]
    fn control_one_pointer_for_every_tool_and_shape_is_caught() {
        control(SimConfig::cases(200), Plant::None, Plant::OneName, "a command walks from another's answer", own_names);
    }

    /// More builds than [`KEPT`] write their pointers in one folder, in a drawn order with
    /// repeats; after each write the folder (what blaze-cli keeps, removing what [`evicted`]
    /// names) holds exactly the at most `KEPT` names written last, the one written included.
    fn kept_last(tape: &Tape, plant: Plant) -> Result<(), Broke> {
        let builds = KEPT + 1 + tape.index(8, "builds past the cap");
        let mut held: BTreeMap<PointerName, u64> = BTreeMap::new();
        let mut order: StdVec<PointerName> = StdVec::new();
        for stamp in 1..=builds as u64 + tape.index(8, "repeats") as u64 {
            let build = match stamp as usize <= builds {
                true => stamp as u8,
                false => 1 + tape.index(builds, "a repeat") as u8,
            };
            let name = PointerName::of(tool(build), &shape(0));
            let mut listed: StdVec<(PointerName, u64)> = held.iter().map(|(name, at)| (*name, *at)).collect();
            let mut gone = Vec::new_in(blaze_own::testing::heap().alloc());
            evicted(&mut listed, name, plant, &mut gone);
            for removed in gone.iter() {
                held.remove(removed);
            }
            held.insert(name, stamp);
            order.retain(|n| *n != name);
            order.push(name);
            let want: BTreeSet<PointerName> = order.iter().rev().take(KEPT).copied().collect();
            let have: BTreeSet<PointerName> = held.keys().copied().collect();
            if have != want {
                return Err(Broke::Oracle(format!(
                    "after write {stamp} the folder holds {} pointers, {} of the {} written last",
                    have.len(),
                    have.intersection(&want).count(),
                    want.len()
                )));
            }
        }
        Ok(())
    }

    #[test]
    fn a_checkout_keeps_the_pointers_written_last() {
        check(SimConfig::cases(200), |tape| kept_last(tape, Plant::None));
    }

    #[test]
    fn control_evicting_the_newest_is_caught() {
        control(SimConfig::cases(200), Plant::None, Plant::EvictNewest, "written last", kept_last);
    }
}
