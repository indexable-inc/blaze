//! How the goal store and the item store (`crate::body_store`) keep their records in the
//! driver's store: both are `blaze_record`'s form (a base key, read-lists called **shapes** in
//! a chain of slots under it, an answer under a shape's bytes and its facts' values), and this
//! is what they share over it: the driver's store as `blaze_record` reads and writes it
//! ([`Records`]), the name a shape has for a fact ([`fact_name`], [`fact_of`]), the placing of
//! one key's shapes by one flush ([`filing`]), the order a flush writes in ([`groups`]) and
//! the words of an error ([`Refused`]).
//!
//! One flush can write several shapes of one key: two bodies alike enough to share an identity
//! whose steps read different facts, a goal evaluated again after a write. So a flush places
//! the entries of one key one after another, in an order that is of their bytes alone
//! ([`groups`]), against what the flush itself placed before (`blaze_record::Filing`): the
//! store's `get` need not show a put the driver has not made yet (blaze-cargo's buffers every
//! put until the flush has ended), and which slot a shape takes must not depend on which
//! thread wrote first.

use std::{cmp::Ordering, ops::Range};

use blaze_ids::ContentId;
use blaze_query::QueryKind;
use blaze_record::{Bounds, Doing, Filing, Kind};
use hir_def::deps::{ImageBuf, ImageStore};

use super::{FactObject, FactRef};

/// The driver's store as `blaze_record` reads and writes a record's parts. `at` gives the key
/// the store keeps a part under, from the part's own key: the goal store's is that key, the
/// item store's is under its step, a shape and an answer apart.
pub(crate) struct Records<'a, At> {
    // dyn: the driver's store, as the stores that flush hold it
    pub(crate) images: &'a dyn ImageStore,
    pub(crate) at: At,
}

impl<At: Fn(Kind, &ContentId) -> ContentId> blaze_record::Store for Records<'_, At> {
    type Held = ImageBuf;
    type Error = String;

    fn get(&self, kind: Kind, key: &ContentId) -> Result<Option<ImageBuf>, String> {
        self.images.get(&(self.at)(kind, key))
    }

    fn put(&self, kind: Kind, key: ContentId, bytes: &[u8]) -> Result<(), String> {
        // alloc: the bytes of a shape or an answer, once per one written
        self.images.put((self.at)(kind, &key), bytes.to_vec())
    }

    fn bytes(held: &ImageBuf) -> &[u8] {
        held.bytes()
    }
}

/// A fact as a shape names it: its kind, its object's tag, a pad byte, its subject, then its
/// object (a field as its first four bytes, the rest zero). Every number is little-endian.
pub(crate) const FACT_BYTES: usize = 2 + 1 + 1 + 32 + 32;

/// The name a shape has for `fact`: what `blaze_record` keeps as a read's name.
pub(crate) fn fact_name(fact: &FactRef) -> [u8; FACT_BYTES] {
    let mut bytes = [0; FACT_BYTES];
    bytes[..2].copy_from_slice(&fact.kind.0.to_le_bytes());
    bytes[4..36].copy_from_slice(&fact.subject.0);
    match fact.object {
        FactObject::None => bytes[2] = 0,
        FactObject::Field(field) => {
            bytes[2] = 1;
            bytes[36..40].copy_from_slice(&field.to_le_bytes());
        }
        FactObject::Id(id) => {
            bytes[2] = 2;
            bytes[36..].copy_from_slice(&id.0);
        }
    }
    bytes
}

/// The fact a shape names `name`: [`fact_name`]'s inverse, refusing every other name.
pub(crate) fn fact_of(name: &[u8]) -> Result<FactRef, &'static str> {
    let Ok(bytes) = <&[u8; FACT_BYTES]>::try_from(name) else {
        return Err("a fact's name is not a fact's length");
    };
    let id = |at: usize| ContentId(bytes[at..at + 32].try_into().expect("32 bytes"));
    let kind = QueryKind(u16::from_le_bytes([bytes[0], bytes[1]]));
    let rest_zero = |from: usize| bytes[from..].iter().all(|&b| b == 0);
    if bytes[3] != 0 {
        return Err("a fact's pad byte is not zero");
    }
    let object = match bytes[2] {
        0 if rest_zero(36) => FactObject::None,
        1 if rest_zero(40) => {
            FactObject::Field(u32::from_le_bytes(bytes[36..40].try_into().expect("4 bytes")))
        }
        2 => FactObject::Id(id(36)),
        0 | 1 => return Err("a fact's object has bytes past its end"),
        _ => return Err("a fact's object has an unknown tag"),
    };
    Ok(FactRef { kind, subject: id(4), object })
}

/// The placing of the shapes of the key `base` by one flush, one after another: what it
/// placed before is looked at before the store.
#[cfg(not(test))]
pub(crate) fn filing<'a>(base: ContentId) -> Filing<'a> {
    Filing::new(base, Bounds::STANDARD)
}

/// As above; on a thread whose control set [`FORGETFUL`], the planted bug.
#[cfg(test)]
pub(crate) fn filing<'a>(base: ContentId) -> Filing<'a> {
    let filing = Filing::new(base, Bounds::STANDARD);
    match FORGETFUL.get() {
        true => filing.planted_forgetful(),
        false => filing,
    }
}

/// How many shapes a key's chain holds at its bound, where [`filing`]'s placing says
/// `blaze_record::Placed::Full`.
pub(crate) fn full_chain() -> u32 {
    Bounds::STANDARD.width() * Bounds::STANDARD.generations()
}

#[cfg(test)]
thread_local! {
    /// Set by a control on its own thread: every [`filing`] made on it forgets what it placed,
    /// so the second shape of a key is placed against the store alone.
    pub(crate) static FORGETFUL: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Why a record was not read or kept: what the store was `doing` and the reason. `doing` ends
/// where the store names its thing (a goal, a step of a body).
#[derive(Debug)]
pub(crate) struct Refused {
    pub(crate) doing: &'static str,
    pub(crate) why: String,
}

impl From<blaze_record::Error<String>> for Refused {
    fn from(error: blaze_record::Error<String>) -> Refused {
        match error {
            blaze_record::Error::Store { doing, error, key: _ } => Refused {
                doing: match doing {
                    Doing::ReadingList => "reading the shape of",
                    Doing::KeepingList => "keeping the shape of",
                    Doing::ReadingAnswer => "reading the answer of",
                    Doing::KeepingAnswer => "keeping the answer of",
                },
                why: error,
            },
            blaze_record::Error::List { at, why } => Refused {
                doing: "opening the shape of",
                // alloc: error text
                why: format!("generation {} slot {} of its chain holds {why}", at.generation, at.n),
            },
            blaze_record::Error::TwoAnswers { key: _ } => Refused {
                doing: "two answers under the same fact values of",
                // alloc: error text
                why: String::from("the work read something its facts do not cover"),
            },
        }
    }
}

/// The entries `0..count` of a flush in the order it writes them (`order`, by `cmp`: of the
/// entries' bytes alone, so the same entries are placed alike whichever thread offered them),
/// and each run of entries of one key (`same`) as a range of that order: the runs are written
/// at once, the entries of a run one after another.
pub(crate) fn groups(
    count: usize,
    cmp: impl Fn(usize, usize) -> Ordering,
    same: impl Fn(usize, usize) -> bool,
) -> (Vec<usize>, Vec<Range<usize>>) {
    // alloc: flush, once per build: one index per entry
    let mut order: Vec<usize> = (0..count).collect();
    order.sort_by(|&a, &b| cmp(a, b));
    // alloc: flush, once per build: one range per key
    let mut runs = Vec::new();
    let mut start = 0;
    for at in 1..=order.len() {
        if at == order.len() || !same(order[at - 1], order[at]) {
            runs.push(start..at);
            start = at;
        }
    }
    (order, runs)
}
