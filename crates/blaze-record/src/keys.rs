//! The keys of a record's parts: every hash domain of this crate is here.

#![blaze::sans_io]

use blaze_ids::{ContentId, Hasher};

/// The key of a slot of a base's chain: the base, the generation, the slot.
const LIST_DOMAIN: &str = "blaze v2 record list 1";
/// The key of an answer: a read-list's bytes, then the digest of each of its reads.
const ANSWER_DOMAIN: &str = "blaze v2 record answer 1";

/// A place in a base's chain of read-lists.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Slot {
    pub generation: u32,
    pub n: u32,
}

/// The key the read-list in slot `at` of `base`'s chain is under.
pub fn list_key(base: &ContentId, at: Slot) -> ContentId {
    Hasher::new(LIST_DOMAIN)
        .id(base)
        .update(&at.generation.to_le_bytes())
        .update(&at.n.to_le_bytes())
        .finish()
}

/// What a read has now, or had when the work ran.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Digest {
    /// Looked for and absent: a negative read. Never equal to any [`Digest::Is`].
    Nothing,
    Is(ContentId),
}

/// The key of an answer: its read-list's bytes (which hold the base), then the digest of each
/// read in the list's order, then how many.
pub struct AnswerKey {
    hasher: Hasher,
    values: u64,
}

impl AnswerKey {
    pub fn new(list: &[u8]) -> AnswerKey {
        let mut hasher = Hasher::new(ANSWER_DOMAIN);
        hasher.update(&(list.len() as u64).to_le_bytes()).update(list);
        AnswerKey { hasher, values: 0 }
    }

    /// The digest of the list's next read.
    pub fn value(&mut self, digest: Digest) {
        match digest {
            Digest::Nothing => self.hasher.update(&[0]),
            Digest::Is(id) => self.hasher.update(&[1]).id(&id),
        };
        self.values += 1;
    }

    pub fn finish(mut self) -> ContentId {
        self.hasher.update(&self.values.to_le_bytes());
        self.hasher.finish()
    }
}
