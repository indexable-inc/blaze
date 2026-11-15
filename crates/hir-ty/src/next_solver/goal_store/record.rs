//! A stored goal's two records, in `blaze_record`'s form (base key, read-lists, answer):
//!
//! - its **shape**, a read-list under the goal's key, in a slot of that key's chain
//!   (`blaze_record::list_key`): its note is the solver version and the goal's stable form,
//!   its reads the facts the evaluation read, each as kind, subject and object, sorted;
//! - its **answer**, under the shape's bytes and the values those facts had when it was solved
//!   (`blaze_record::AnswerKey`).
//!
//! The keys, the shape's layout and the chain are `blaze_record`'s, a fact's name is
//! `slots::fact_name`'s (the item store's shapes name facts the same way); what is here is the
//! note and the answer's bytes. Every number is little-endian. A record is checked
//! against its key when opened: a store that gave a record under another key is an error, not
//! a miss.

use blaze_ids::ContentId;

use super::{
    Answer, FactRef,
    slots::{FACT_BYTES, fact_name},
};

const ANSWER_MAGIC: [u8; 8] = *b"bzgoal1a";

/// A read-list's head (`blaze_record`'s layout: layout word, base key, the note's length, how
/// many reads), then the solver version that starts the note.
const SHAPE_HEAD: usize = 8 + 32 + 4 + 4 + 32;

/// Magic, answer key, answer.
pub(super) const ANSWER_BYTES: usize = 8 + 32 + 1;

/// The key of slot `n` of the first generation of `goal`'s chain: where its first shapes are.
#[cfg(test)]
pub(super) fn shape_key(goal: &ContentId, n: u32) -> ContentId {
    blaze_record::list_key(goal, blaze_record::Slot { generation: 0, n })
}

/// The bytes of a shape of `words` and `facts`.
pub(super) fn shape_len(words: usize, facts: usize) -> usize {
    SHAPE_HEAD + 4 * words + (4 + FACT_BYTES) * facts
}

/// Appends the shape of a goal to `out`.
pub(super) fn write_shape(
    out: &mut Vec<u8>,
    goal: &ContentId,
    version: &ContentId,
    words: &[u32],
    facts: &[FactRef],
) {
    // alloc: the thread's pending buffer, of a capped size, reused from build to build
    out.reserve(shape_len(words.len(), facts.len()));
    blaze_record::write_list_with(
        out,
        goal,
        |out| {
            // alloc: reserved above
            out.extend_from_slice(&version.0);
            for &word in words {
                out.extend_from_slice(&word.to_le_bytes());
            }
        },
        facts.iter().map(fact_name),
    );
}

/// A shape, read in place: what tests read one back with (a lookup reads a shape through
/// `blaze_record::lookup`, a fact at a time).
#[cfg(test)]
#[derive(Clone, Copy, Debug)]
pub(super) struct ShapeRef<'a> {
    words: &'a [u8],
    list: blaze_record::ListRef<'a>,
}

#[cfg(test)]
impl<'a> ShapeRef<'a> {
    /// The shape in `bytes`, checked to be one of `goal` under `version` and to be whole.
    pub(super) fn open(
        bytes: &'a [u8],
        goal: &ContentId,
        version: &ContentId,
    ) -> Result<ShapeRef<'a>, &'static str> {
        use blaze_record::ListError;
        let list = blaze_record::ListRef::open(bytes, goal).map_err(|why| match why {
            ListError::Layout(_) => "not a goal shape",
            ListError::Short => "cut short",
            ListError::OtherBase => "the shape of another goal",
            ListError::Trailing => "bytes past its end",
        })?;
        let (kept_version, words) = list.note().split_at_checked(32).ok_or("cut short")?;
        if kept_version != version.0 {
            return Err("a shape of another solver version under this one's key");
        }
        if !words.len().is_multiple_of(4) {
            return Err("a stable form that is not whole words");
        }
        for name in list.reads() {
            super::slots::fact_of(name)?;
        }
        Ok(ShapeRef { words, list })
    }

    pub(super) fn words(self) -> impl Iterator<Item = u32> + 'a {
        self.words.chunks_exact(4).map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]]))
    }

    pub(super) fn facts(self) -> impl Iterator<Item = FactRef> + 'a {
        self.list
            .reads()
            .map(|name| super::slots::fact_of(name).expect("`ShapeRef::open` checked every fact"))
    }
}

pub(super) fn answer_record(key: &ContentId, answer: Answer) -> [u8; ANSWER_BYTES] {
    let mut bytes = [0; ANSWER_BYTES];
    bytes[..8].copy_from_slice(&ANSWER_MAGIC);
    bytes[8..40].copy_from_slice(&key.0);
    bytes[40] = match answer {
        Answer::Fails => 0,
        Answer::Holds => 1,
    };
    bytes
}

pub(super) fn open_answer(bytes: &[u8], key: &ContentId) -> Result<Answer, &'static str> {
    let bytes: &[u8; ANSWER_BYTES] = bytes.try_into().map_err(|_| "not an answer's length")?;
    if bytes[..8] != ANSWER_MAGIC {
        return Err("not a goal answer");
    }
    if bytes[8..40] != key.0 {
        return Err("the answer under another key");
    }
    match bytes[40] {
        0 => Ok(Answer::Fails),
        1 => Ok(Answer::Holds),
        _ => Err("an unknown answer"),
    }
}

/// A shape, owned: what tests compare.
#[cfg(test)]
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) struct Shape {
    pub(super) words: Vec<u32>,
    pub(super) facts: Vec<FactRef>,
}

#[cfg(test)]
impl ShapeRef<'_> {
    pub(super) fn to_shape(self) -> Shape {
        Shape { words: self.words().collect(), facts: self.facts().collect() }
    }
}
