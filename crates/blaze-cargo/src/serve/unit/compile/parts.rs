//! What a unit read of the crates below it, part by part, and the check of those parts
//! (docs/v2/plan/design/store.md, "Early cutoff between crates"): the rule every key
//! follows, that a key covers what was read and nothing else, for a unit's dependencies.
//!
//! A unit made records each thing it read of a crate below it as a [`Part`] with the value
//! it had ([`Parts`]). Its record is filed under its own inputs and those parts; a lookup
//! works each part's value out again and finds the record only when none moved
//! ([`Parts::check`]). A part is a fact of the item store's vocabulary
//! ([`blaze_items::Fact`]): a signature for a call, a layout for a type used by value, a body
//! for a copy compiled, a constant's value, the impls a trait was selected through, what a
//! name resolved to in a scope. The last two are asked of indexes, so a read that found
//! nothing is a part too (a name not in a glob's set, no impl of a trait for a type) and an
//! item or impl that would change the answer moves it. Positions are parts of their own
//! ([`Part::Spans`]), read only by a unit that holds a copy of code written elsewhere or
//! links it.
//!
//! The quick check ([`Quick`]): of each crate read, one digest of everything its parts could
//! be made from. Where it is as it was no part of that crate is worked out. It is never the
//! key: where it moved, the parts decide.
//!
//! A core with no IO and no database: the values come from its caller. What fills it from a
//! unit's making (the item store's traces of each body, and the unit's own steps) is not
//! built; nothing calls this yet but its tests.

use blaze_ids::{ContentId, Hasher};
use blaze_items::{Fact, ItemKey};
use serde::{Deserialize, Serialize};

/// The record's layout, in its bytes and in [`Parts::filed`]'s domain: bumped by hand on any
/// change to [`Parts`], [`Part`], [`Cover`] or how they are encoded.
pub(crate) const LAYOUT: u32 = 1;

/// The domain of the key a record is filed under with its parts ([`Parts::filed`]).
const FILED: &str = "blaze v2 cargo unit record parts 1";

/// The most parts one unit's record lists. A unit that read more keeps the key it had
/// before parts (its dependencies' surfaces), named as why.
pub(crate) const MOST_PARTS: usize = 1 << 20;

/// One thing a unit read of a crate below it.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum Part {
    /// A fact of an item or of an index, as the item store names what a step read. Its value
    /// has no position in it.
    Fact(Fact),
    /// Where an item's code is written: its span table, by the item's key. Read by a unit
    /// that compiles a copy of the item (a generic, an `#[inline]` function), builds it into
    /// its own code, or links it: what a panic's location and debug info are made from.
    Spans(ItemKey),
}

impl Part {
    /// Whether the part's value is made from what a crate's surface holds
    /// (`blaze_lower`'s `CodeText::surface`): everything a front end reads. A body's code
    /// and an item's positions are not: a crate read for either is covered by its code text
    /// ([`Cover::Code`]).
    fn in_surface(&self) -> bool {
        match self {
            Part::Spans(_) => false,
            Part::Fact(Fact::Code(_) | Fact::Crate(_)) => false,
            Part::Fact(_) => true,
        }
    }
}

/// What a crate's quick digest is of.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum Cover {
    /// Its surface: every part a front end reads is made from it.
    Surface,
    /// Its code text: every part of it is.
    Code,
}

/// The quick digest of one crate a unit read.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Quick {
    /// Which crate, by its key in the front end (`blaze_items::key::crate_key`).
    pub(crate) krate: ContentId,
    pub(crate) cover: Cover,
    pub(crate) digest: ContentId,
}

/// One part with the crate it is of and the value it had.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Read {
    pub(crate) krate: ContentId,
    pub(crate) part: Part,
    pub(crate) value: ContentId,
}

/// What a unit read of the crates below it, as its record's slot keeps it: sorted, each
/// part once.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub(crate) struct Parts {
    layout: u32,
    /// Sorted by crate, one a crate read.
    quick: Vec<Quick>,
    /// Sorted by crate, then by the part's bytes.
    reads: Vec<Read>,
}

/// Why a unit's reads are no [`Parts`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum Refused {
    /// One part read with two values (rule keys-cover-every-input): the trace is of two
    /// states of the front end.
    TwoValues(Part),
    /// A part of a crate with no quick digest, or two digests for one crate.
    NoQuick(ContentId),
    /// A crate's quick digest is of its surface, and a part of it is not made from one: an
    /// equal digest would pass a body or a position that moved.
    QuickTooNarrow(Part),
    /// More than [`MOST_PARTS`].
    TooMany(usize),
    /// Bytes that are no record of this layout.
    Bytes(String),
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refused::TwoValues(part) => write!(f, "the part {part:?} was read with two values"),
            Refused::NoQuick(krate) => {
                write!(f, "the crate {krate:x} has not exactly one quick digest")
            }
            Refused::QuickTooNarrow(part) => write!(
                f,
                "the part {part:?} is not made from its crate's surface, which is all the crate's quick digest covers"
            ),
            Refused::TooMany(count) => {
                write!(f, "{count} parts, where a record lists at most {MOST_PARTS}")
            }
            Refused::Bytes(why) => write!(f, "its bytes are no parts of layout {LAYOUT}: {why}"),
        }
    }
}

/// What a check of a record's parts found.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum Verdict {
    /// No part moved: the record is the unit's. `worked_out` parts were asked for their
    /// values; the rest were of crates whose quick digest was as it was.
    Unmoved { worked_out: usize },
    /// A part has another value: the first, in the record's order.
    Moved { read: Read, now: ContentId },
    /// A part has no value now (its item is gone, or cannot be hashed): the record is not
    /// the unit's, and `why` says what the front end said.
    Unknown { read: Read, why: String },
}

fn part_bytes(part: &Part) -> Vec<u8> {
    // A part is plain data: its encoding cannot fail. alloc: a part's few bytes, to order by
    blaze_protocol::cbor::to_vec(part).unwrap_or_default()
}

impl Parts {
    /// The record of `reads` with the quick digests `quick`: ordered, each part once.
    pub(crate) fn new(mut quick: Vec<Quick>, reads: Vec<Read>) -> Result<Parts, Refused> {
        if reads.len() > MOST_PARTS {
            return Err(Refused::TooMany(reads.len()));
        }
        quick.sort_unstable_by(|a, b| a.krate.cmp(&b.krate));
        if let Some(pair) = quick.windows(2).find(|pair| pair[0].krate == pair[1].krate) {
            return Err(Refused::NoQuick(pair[0].krate));
        }
        // alloc: once per unit made, a part's bytes each, to order by
        let mut keyed: Vec<(ContentId, Vec<u8>, Read)> =
            reads.into_iter().map(|it| (it.krate, part_bytes(&it.part), it)).collect();
        keyed.sort_unstable_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
        // alloc: as above, the record's list
        let mut reads: Vec<Read> = Vec::with_capacity(keyed.len());
        for (_, _, read) in keyed {
            match reads.last() {
                Some(last) if last.krate == read.krate && last.part == read.part => {
                    if last.value != read.value {
                        return Err(Refused::TwoValues(read.part));
                    }
                }
                _ => reads.push(read),
            }
        }
        for read in &reads {
            let Ok(at) = quick.binary_search_by(|it| it.krate.cmp(&read.krate)) else {
                return Err(Refused::NoQuick(read.krate));
            };
            if quick[at].cover == Cover::Surface && !read.part.in_surface() && !narrow_planted() {
                return Err(Refused::QuickTooNarrow(read.part));
            }
        }
        Ok(Parts { layout: LAYOUT, quick, reads })
    }

    pub(crate) fn reads(&self) -> &[Read] {
        &self.reads
    }

    /// Whether the record is still the unit's: no part has another value than it was read
    /// with. `quick_now` is a crate's quick digest now, of the cover the record has for it
    /// (none: not known, so its parts are worked out); `value_now` a part's value now.
    ///
    /// A crate whose digest is as it was is passed over whole: its parts are made from what
    /// the digest covers ([`Parts::new`] refused any that is not). Every other crate's parts
    /// are worked out, in order, to the first that moved.
    pub(crate) fn check(
        &self,
        quick_now: impl Fn(&Quick) -> Option<ContentId>,
        mut value_now: impl FnMut(&Read) -> Result<ContentId, String>,
    ) -> Verdict {
        let mut worked_out = 0;
        let mut at = 0;
        for quick in &self.quick {
            let from = at;
            while at < self.reads.len() && self.reads[at].krate == quick.krate {
                at += 1;
            }
            if quick_now(quick) == Some(quick.digest) {
                continue;
            }
            for read in &self.reads[from..at] {
                worked_out += 1;
                match value_now(read) {
                    Ok(now) if now == read.value => {}
                    Ok(now) => return Verdict::Moved { read: *read, now },
                    Err(why) => return Verdict::Unknown { read: *read, why },
                }
            }
        }
        Verdict::Unmoved { worked_out }
    }

    /// The key the unit's record is under: its base key (its own inputs, and which crates it
    /// depends on by name alone) with every part and its value. The quick digests are not in
    /// it: two builds that read the same parts with the same values find one record,
    /// whatever else of the crates below moved.
    pub(crate) fn filed(&self, base: &ContentId) -> ContentId {
        let mut hasher = Hasher::new(FILED);
        hasher.id(base).update(&(self.reads.len() as u64).to_le_bytes());
        for read in &self.reads {
            hasher.id(&read.krate).part(&part_bytes(&read.part)).id(&read.value);
        }
        hasher.finish()
    }

    /// The record's bytes.
    pub(crate) fn encoded(&self) -> Result<Vec<u8>, String> {
        blaze_protocol::cbor::to_vec(self).map_err(|why| why.to_string())
    }

    /// The record `bytes` are: of this layout, ordered as [`Parts::new`] orders one, or
    /// refused saying why. Never an empty record for bytes that do not read.
    pub(crate) fn decoded(bytes: &[u8]) -> Result<Parts, Refused> {
        let read: Parts = blaze_protocol::cbor::from_slice(bytes)
            .map_err(|why| Refused::Bytes(why.to_string()))?;
        if read.layout != LAYOUT {
            return Err(Refused::Bytes(format!("layout {}", read.layout))); // alloc: error path
        }
        // clone: once per record read, to hold it to the order a made one has
        let made = Parts::new(read.quick.clone(), read.reads.clone())?;
        if made != read {
            // alloc: error path
            return Err(Refused::Bytes("its parts are not in a record's order".to_owned()));
        }
        Ok(read)
    }
}

#[cfg(test)]
thread_local! {
    /// The planted bug a test's oracle has to catch: a part that no surface holds is taken
    /// under a quick digest of the surface ([`Refused::QuickTooNarrow`] not raised).
    pub(crate) static NARROW_PLANTED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether a test planted [`NARROW_PLANTED`]: never, outside a test.
#[cfg(test)]
fn narrow_planted() -> bool {
    NARROW_PLANTED.with(|it| it.get())
}

#[cfg(not(test))]
fn narrow_planted() -> bool {
    false
}

#[cfg(test)]
mod tests;
