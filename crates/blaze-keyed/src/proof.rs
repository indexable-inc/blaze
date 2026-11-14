//! Proofs: the result of a check as a value only its checker makes (decision 18, "Verify once,
//! by construction"). Code that needs a checked thing takes its proof, so a second check cannot
//! be written and a skipped one cannot either.
//!
//! - [`Snapshot`]: one request's view of the tree and the store. Every proof carries its
//!   lifetime `'s`, which [`scope`] makes generative (the closure is universal over `'s`), so a
//!   proof never outlives its request and never mixes with another request's.
//! - [`Holds<K>`]: a record of kind `K` under its key is valid in this snapshot. Made only by
//!   [`hit`] (an equal key: the head checked against the key asked), by
//!   [`Candidate::verify`] (a read list whose every digest is the current one), by a parent's
//!   proof ([`Hit::named`]: a held unit manifest settles every record it names, with no lookup
//!   each), or by an outside lookup's hit by address after its answers
//!   ([`crate::outside::Lookup`]).
//! - [`Certified`]: the request's certificate held; nothing below it is checked. Made only from a
//!   [`Holds`] of the certificate.
//! - Bytes: the store's proof that a segment was checked once is its own type, bound by
//!   [`crate::store::SegmentProof`].
//!
//! What a proof costs at run time: the brand is zero-sized; [`Certified`] is zero-sized;
//! [`Holds`] is the key it proves (32 bytes, the `Key` it stands in for); [`AnyHolds`] is that key
//! and its two-byte tag. No proof holds a pointer, a count or a flag.
//!
//! There is no function that takes a proof and checks again.

#![blaze::sans_io]

use core::{fmt, marker::PhantomData, slice};

use blaze_ids::ContentId;

use crate::{
    kind::{Certificate, Key, Kind, Tag, UnitManifest},
    record::{self, Entry, Fact, HasPayload, Headed, RecordError, StableId},
};

/// Invariant in `'s`: two snapshots' brands never unify.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Brand<'s>(PhantomData<fn(&'s ()) -> &'s ()>);

/// One request's snapshot: the brand every proof of the request carries.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Snapshot<'s> {
    brand: Brand<'s>,
}

impl fmt::Debug for Snapshot<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Snapshot")
    }
}

/// Runs `f` in a fresh snapshot: one request, start to outcome. Nothing branded with it leaves.
pub fn scope<R>(f: impl for<'s> FnOnce(Snapshot<'s>) -> R) -> R {
    f(Snapshot { brand: Brand(PhantomData) })
}

/// What the checks of one request cost, counted where each happens. The edit loop is judged by
/// it; `rechecks` must be zero (the simulation's oracle holds the driver to it).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Checks {
    /// Reads by key, of the memo or the store: a manifest, a record, an old record, a shape slot
    /// or an address.
    pub key_lookups: u32,
    /// Read lists compared against the facts now ([`Candidate::verify`], an outside shape's
    /// answers).
    pub read_lists: u32,
    /// Facts compared in those lists, up to the first that differs.
    pub facts_compared: u32,
    /// Tasks run to make a record.
    pub records_run: u32,
    /// Checks of an item already proved in this snapshot: a bug, always zero.
    pub rechecks: u32,
}

/// A record of kind `K` under `key()` is valid in snapshot `'s`.
pub struct Holds<'s, K> {
    key: Key<K>,
    brand: Brand<'s>,
}

impl<'s, K> Holds<'s, K> {
    pub fn key(&self) -> Key<K> {
        self.key
    }
}

impl<K> Clone for Holds<'_, K> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<K> Copy for Holds<'_, K> {}

impl<K> PartialEq for Holds<'_, K> {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}

impl<K> Eq for Holds<'_, K> {}

impl<K: Kind> fmt::Debug for Holds<'_, K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Holds({:?})", self.key)
    }
}

impl<'s> Holds<'s, Certificate> {
    /// The certificate held: the request is answered and nothing below is checked.
    pub fn certified(self) -> Certified<'s> {
        Certified { brand: self.brand }
    }
}

/// A proof whose kind is a value: what a driver keeps per item of a unit.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AnyHolds<'s> {
    tag: Tag,
    key: ContentId,
    brand: Brand<'s>,
}

impl<'s> AnyHolds<'s> {
    pub fn tag(&self) -> Tag {
        self.tag
    }

    pub fn key_id(&self) -> ContentId {
        self.key
    }

    /// The typed proof, when it is of kind `K`.
    pub fn get<K: Kind>(self) -> Option<Holds<'s, K>> {
        (self.tag == K::TAG).then_some(Holds { key: Key::from_id(self.key), brand: self.brand })
    }
}

impl<'s, K: Kind> From<Holds<'s, K>> for AnyHolds<'s> {
    fn from(holds: Holds<'s, K>) -> AnyHolds<'s> {
        AnyHolds { tag: K::TAG, key: holds.key.id(), brand: holds.brand }
    }
}

/// The request's certificate held in snapshot `'s`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Certified<'s> {
    brand: Brand<'s>,
}

/// A record found under the key asked, its head checked: the proof with the bytes it is of.
pub struct Hit<'s, 'b, K: Kind> {
    holds: Holds<'s, K>,
    root: &'b K::Root,
    bytes: &'b [u8],
}

/// The one key check: `bytes` open as a record of kind `K` whose head is `key` (schema, kind,
/// version, the full key). An equal key is not followed by any other check.
pub fn hit<'s, 'b, K: Kind>(snap: Snapshot<'s>, key: Key<K>, bytes: &'b [u8]) -> Result<Hit<'s, 'b, K>, RecordError> {
    let root = record::open::<K>(bytes, &key)?;
    Ok(Hit { holds: Holds { key, brand: snap.brand }, root, bytes })
}

impl<'s, 'b, K: Kind> Hit<'s, 'b, K> {
    pub fn holds(&self) -> Holds<'s, K> {
        self.holds
    }

    pub fn root(&self) -> &'b K::Root {
        self.root
    }

    pub fn bytes(&self) -> &'b [u8] {
        self.bytes
    }
}

impl<'s, 'b, K: Kind> Hit<'s, 'b, K>
where
    K::Root: HasPayload,
{
    /// The payload of the record the hit opened: nothing is opened or checked again.
    pub fn payload(&self) -> Result<&'b [u8], RecordError> {
        self.root.payload().follow(self.bytes).map_err(RecordError::Format)
    }
}

/// The payload of `record`, the bytes the store holds under `holds`'s key, by the proof: the one
/// reader of an [`Opaque`](crate::record::Opaque) or [`Traced`](crate::record::Traced) record's
/// payload for code that holds a proof but not the [`Hit`] that made it (a manifest's named
/// record, an outside value found by address, an action's input).
///
/// No head is compared and nothing is verified: the proof was made by the record's one check,
/// and a key's bytes never change in the store. What runs is the format's bounds on the buffer,
/// which reading any byte of it needs (the root's place and the payload's range), and in debug
/// builds an assertion that the head is the proof's key.
pub fn payload<'b, K: Kind>(holds: &Holds<'_, K>, record: &'b [u8]) -> Result<&'b [u8], RecordError>
where
    K::Root: HasPayload,
{
    let root = blaze_format::open::<K::Root>(record).map_err(RecordError::Format)?;
    debug_assert!(
        root.head().key == holds.key().id() && root.head().tag == K::TAG.0,
        "a proof of {:?} given another record",
        holds.key().id()
    );
    root.payload().follow(record).map_err(RecordError::Format)
}

impl<'s, 'b> Hit<'s, 'b, UnitManifest> {
    /// Every record the held manifest names, each with its proof: no lookup each (a manifest is
    /// filed after every record it names, so they are in the store).
    pub fn named(&self) -> Result<Named<'s, 'b>, RecordError> {
        let entries = self.root.entries.follow(self.bytes).map_err(RecordError::Format)?;
        Ok(Named { entries: entries.iter(), brand: self.holds.brand })
    }
}

/// The records a held manifest names, in its order.
pub struct Named<'s, 'b> {
    entries: slice::Iter<'b, Entry>,
    brand: Brand<'s>,
}

impl<'s> Iterator for Named<'s, '_> {
    type Item = (StableId, AnyHolds<'s>);

    fn next(&mut self) -> Option<Self::Item> {
        let entry = self.entries.next()?;
        Some((StableId(entry.item), AnyHolds { tag: Tag(entry.tag), key: entry.key, brand: self.brand }))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.entries.size_hint()
    }
}

/// The record the last manifest names for an item whose key moved: valid under the new key if
/// its read list still holds. Consumed by its one check.
#[derive(Debug)]
pub struct Candidate<'s> {
    tag: Tag,
    new: ContentId,
    brand: Brand<'s>,
}

/// `bytes`, the record filed under `old`, as a candidate for `new`: its head checked against
/// `old`.
pub fn candidate<'s, K: Kind>(snap: Snapshot<'s>, new: Key<K>, old: Key<K>, bytes: &[u8]) -> Result<Candidate<'s>, RecordError> {
    record::open::<K>(bytes, &old)?;
    Ok(Candidate { tag: K::TAG, new: new.id(), brand: snap.brand })
}

/// What a candidate's one check gives.
#[derive(Debug)]
pub enum Verdict<'s> {
    /// Every read is the current fact: the record holds under the new key (the caller files it
    /// there re-headed, so the next request hits by key).
    Holds(AnyHolds<'s>),
    /// Read `at` differs or is gone: the work is redone.
    Redo { at: u32 },
}

/// The host answered a check with another number of facts than it was asked.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Misanswered {
    pub asked: u32,
    pub answered: u32,
}

impl<'s> Candidate<'s> {
    pub fn tag(&self) -> Tag {
        self.tag
    }

    /// The one read-list check: `reads` is the candidate record's read list and `now[i]` the
    /// current digest of `reads[i]`'s fact (`None`: gone). Compares up to the first difference.
    pub fn verify(self, reads: &[Fact], now: &[Option<ContentId>], checks: &mut Checks) -> Result<Verdict<'s>, Misanswered> {
        if reads.len() != now.len() {
            return Err(Misanswered { asked: reads.len() as u32, answered: now.len() as u32 });
        }
        checks.read_lists += 1;
        for (at, (read, now)) in reads.iter().zip(now).enumerate() {
            checks.facts_compared += 1;
            if *now != Some(read.digest) {
                return Ok(Verdict::Redo { at: at as u32 });
            }
        }
        Ok(Verdict::Holds(AnyHolds { tag: self.tag, key: self.new, brand: self.brand }))
    }
}
