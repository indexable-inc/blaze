//! A stored goal's two records, as `hir_def::deps::serve` keeps a crate's images:
//!
//! - its **shape**, under `shape_key(goal, n)` for a slot `n` in `0..SLOTS`: the goal's key and
//!   stable form, the solver version, and the facts its evaluation read, each as kind, subject
//!   and object, sorted;
//! - its **answer**, under the shape's key and the values those facts had when it was solved.
//!
//! Every number is little-endian. A record is checked against its key when opened: a store
//! that gave a record under another key is an error, not a miss.

use blaze_ids::{ContentId, Hasher};
use blaze_query::QueryKind;

use super::{Answer, FactObject, FactRef};

/// The shapes a goal keeps at once: more than one, as an edit can make the same goal read
/// other facts (an impl added where none was) and both checkouts are built.
pub(super) const SLOTS: u32 = 4;

const SHAPE_DOMAIN: &str = "blaze v2 goal 1 shape";
const ANSWER_DOMAIN: &str = "blaze v2 goal 1 answer";
const SHAPE_MAGIC: [u8; 8] = *b"bzgoal1s";
const ANSWER_MAGIC: [u8; 8] = *b"bzgoal1a";

/// A fact: its kind, its object's tag, a pad byte, its subject, then its object (a field as its
/// first four bytes, the rest zero).
pub(super) const FACT_BYTES: usize = 2 + 1 + 1 + 32 + 32;

/// Magic, goal key, solver version, word count.
const SHAPE_HEAD: usize = 8 + 32 + 32 + 4;

/// Magic, answer key, answer.
pub(super) const ANSWER_BYTES: usize = 8 + 32 + 1;

pub(super) fn shape_key(goal: &ContentId, n: u32) -> ContentId {
    Hasher::new(SHAPE_DOMAIN).id(goal).update(&n.to_le_bytes()).finish()
}

/// The key of an answer: its shape's key, then each fact's value in the shape's order.
pub(super) struct AnswerKey {
    hasher: Hasher,
    values: u64,
}

impl AnswerKey {
    pub(super) fn new(shape_key: &ContentId) -> AnswerKey {
        let mut hasher = Hasher::new(ANSWER_DOMAIN);
        hasher.id(shape_key);
        AnswerKey { hasher, values: 0 }
    }

    pub(super) fn value(&mut self, value: &ContentId) {
        self.hasher.id(value);
        self.values += 1;
    }

    pub(super) fn finish(mut self) -> ContentId {
        self.hasher.update(&self.values.to_le_bytes());
        self.hasher.finish()
    }
}

/// The bytes of a shape of `words` and `facts`.
pub(super) fn shape_len(words: usize, facts: usize) -> usize {
    SHAPE_HEAD + 4 * words + 4 + FACT_BYTES * facts
}

/// Appends the shape of a goal to `out`.
pub(super) fn write_shape(
    out: &mut Vec<u8>,
    goal: &ContentId,
    version: &ContentId,
    words: &[u32],
    facts: &[FactRef],
) {
    let words_len = u32::try_from(words.len()).expect("a goal's stable form is at most `MAX_WORDS` words");
    let facts_len = u32::try_from(facts.len()).expect("a goal reads fewer than 2^32 facts");
    // alloc: the thread's pending buffer, of a capped size, reused from build to build
    out.reserve(shape_len(words.len(), facts.len()));
    out.extend_from_slice(&SHAPE_MAGIC);
    out.extend_from_slice(&goal.0);
    out.extend_from_slice(&version.0);
    out.extend_from_slice(&words_len.to_le_bytes());
    for &word in words {
        out.extend_from_slice(&word.to_le_bytes());
    }
    out.extend_from_slice(&facts_len.to_le_bytes());
    for fact in facts {
        out.extend_from_slice(&fact_bytes(fact));
    }
}

fn fact_bytes(fact: &FactRef) -> [u8; FACT_BYTES] {
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

fn read_fact(bytes: &[u8]) -> Result<FactRef, &'static str> {
    let id = |at: usize| ContentId(bytes[at..at + 32].try_into().expect("32 bytes"));
    let kind = QueryKind(u16::from_le_bytes([bytes[0], bytes[1]]));
    let rest_zero = |from: usize| bytes[from..].iter().all(|&b| b == 0);
    if bytes[3] != 0 {
        return Err("a fact's pad byte is not zero");
    }
    let object = match bytes[2] {
        0 if rest_zero(36) => FactObject::None,
        1 if rest_zero(40) => FactObject::Field(u32::from_le_bytes(bytes[36..40].try_into().expect("4 bytes"))),
        2 => FactObject::Id(id(36)),
        0 | 1 => return Err("a fact's object has bytes past its end"),
        _ => return Err("a fact's object has an unknown tag"),
    };
    Ok(FactRef { kind, subject: id(4), object })
}

/// A shape, read in place.
#[derive(Clone, Copy, Debug)]
pub(super) struct ShapeRef<'a> {
    words: &'a [u8],
    facts: &'a [u8],
}

impl<'a> ShapeRef<'a> {
    /// The shape in `bytes`, checked to be one of `goal` under `version` and to be whole.
    pub(super) fn open(bytes: &'a [u8], goal: &ContentId, version: &ContentId) -> Result<ShapeRef<'a>, &'static str> {
        let rest = bytes.strip_prefix(&SHAPE_MAGIC).ok_or("not a goal shape")?;
        let (kept_goal, rest) = rest.split_at_checked(32).ok_or("cut short")?;
        if kept_goal != goal.0 {
            return Err("the shape of another goal");
        }
        let (kept_version, rest) = rest.split_at_checked(32).ok_or("cut short")?;
        if kept_version != version.0 {
            return Err("a shape of another solver version under this one's key");
        }
        let (words, rest) = counted(rest, 4)?;
        let (facts, rest) = counted(rest, FACT_BYTES)?;
        if !rest.is_empty() {
            return Err("bytes past its end");
        }
        let shape = ShapeRef { words, facts };
        for at in (0..facts.len()).step_by(FACT_BYTES) {
            read_fact(&facts[at..at + FACT_BYTES])?;
        }
        Ok(shape)
    }

    pub(super) fn words(self) -> impl Iterator<Item = u32> + 'a {
        self.words.chunks_exact(4).map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]]))
    }

    pub(super) fn facts(self) -> impl Iterator<Item = FactRef> + 'a {
        self.facts
            .chunks_exact(FACT_BYTES)
            .map(|fact| read_fact(fact).expect("`ShapeRef::open` checked every fact"))
    }
}

/// A count, then that many items of `each` bytes.
fn counted(bytes: &[u8], each: usize) -> Result<(&[u8], &[u8]), &'static str> {
    let (count, rest) = bytes.split_at_checked(4).ok_or("cut short")?;
    let count = u32::from_le_bytes(count.try_into().expect("4 bytes")) as usize;
    rest.split_at_checked(count.checked_mul(each).ok_or("a count too large")?).ok_or("cut short")
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
