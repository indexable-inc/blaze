//! A stored step's two records, `blaze_format` buffers read in place from the store:
//!
//! - its **shape**, under [`shape_at`] of `shape_key(base, n)` for a slot `n` in `0..SLOTS`: the
//!   body's base key ([`base_key`]: its identity, the step, the step's answer schema, these
//!   records' schema and the solver version) and the facts the step read, sorted;
//! - its **answer**, under [`answer_at`] of the shape's key and the values those facts had when
//!   the step ran: its answer key and the step's result, a buffer of the step's own.
//!
//! A record is checked against its key when opened: a store that gave a record under another key
//! is an error, not a miss.

use std::mem::offset_of;

use blaze_format::{
    ALIGN, FormatError, Part, Record, SchemaHash, Slice, Writer, open_as, schema, schema_of,
};
use blaze_ids::{ContentId, Hasher};
use blaze_query::{QueryKey, QueryKind, kinds::FRONT};

use super::Step;
use crate::next_solver::goal_store::{FactObject, FactRef};

/// The shapes a body's step keeps at once: more than one, as an edit can make the same body
/// read other facts (an impl added where none was) and both checkouts are built.
pub(super) const SLOTS: u32 = 4;

/// The kind of a step's shape records: this, plus the step.
const SHAPE_KIND: u16 = 0x3800;
/// The kind of a step's answer records: this, plus the step.
const ANSWER_KIND: u16 = 0x3810;

const _: () = {
    assert!(FRONT.start <= SHAPE_KIND && ANSWER_KIND + 0x30 <= FRONT.end);
    assert!((Step::Borrowck as u16) < ANSWER_KIND - SHAPE_KIND);
};

const BASE_DOMAIN: &str = "blaze v2 item store 1 body";
const SHAPE_DOMAIN: &str = "blaze v2 item store 1 shape";
const ANSWER_DOMAIN: &str = "blaze v2 item store 1 answer";

blaze_format::record! {
    /// A fact: its subject, its object (an id, or zero), its kind, its object's tag (0 none, 1 a
    /// field, 2 an id), a pad byte, and a field's position (or zero).
    #[derive(Clone, Copy, Debug)]
    struct FactRow: 1 {
        subject: ContentId,
        object: ContentId,
        kind: u16,
        tag: u8,
        pad: u8,
        field: u32,
    }
}

blaze_format::record! {
    /// A shape: the base key of the body and step it is of, and the facts the step read.
    #[derive(Clone, Copy)]
    struct ShapeRoot: 1 {
        base: ContentId,
        facts: Slice<FactRow>,
        reserved: u64,
    }
}

blaze_format::record! {
    /// An answer: its own key, and the step's result.
    #[derive(Clone, Copy)]
    struct AnswerRoot: 1 {
        key: ContentId,
        body: Slice<u8>,
        reserved: u64,
    }
}

const _: () = {
    assert!(size_of::<FactRow>() == 72);
    assert!(size_of::<ShapeRoot>() == 48);
    assert!(size_of::<AnswerRoot>() == 48);
};

/// The schema of both records: what [`base_key`] covers.
pub(super) const RECORDS: SchemaHash = schema_of(&[
    Part::Str("blaze v2 item store 1"),
    Part::Schema(<FactRow as Record>::SCHEMA),
    Part::Schema(<ShapeRoot as Record>::SCHEMA),
    Part::Schema(<AnswerRoot as Record>::SCHEMA),
]);

const SHAPE_SCHEMA: SchemaHash = schema("blaze v2 item store 1 shape record");
const ANSWER_SCHEMA: SchemaHash = schema("blaze v2 item store 1 answer record");

/// What every shape of `step` of the body whose identity is `identity` is filed under, before
/// its slot: `schema` is the schema of the step's answers, `version` the solver version.
pub(super) fn base_key(identity: &ContentId, step: Step, schema: &SchemaHash, version: &ContentId) -> ContentId {
    Hasher::new(BASE_DOMAIN).id(identity).update(&[step as u8]).id(&RECORDS.0).id(&schema.0).id(version).finish()
}

/// The key of the shape in slot `n` of `base`.
pub(super) fn shape_key(base: &ContentId, n: u32) -> ContentId {
    Hasher::new(SHAPE_DOMAIN).id(base).update(&n.to_le_bytes()).finish()
}

/// The store key of the shape whose key is `shape_key`.
pub(super) fn shape_at(step: Step, shape_key: &ContentId) -> ContentId {
    QueryKey { kind: QueryKind(SHAPE_KIND + step as u16), input: *shape_key }.id()
}

/// The store key of the answer whose key is `answer_key`.
pub(super) fn answer_at(step: Step, answer_key: &ContentId) -> ContentId {
    QueryKey { kind: QueryKind(ANSWER_KIND + step as u16), input: *answer_key }.id()
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

fn row_of(fact: &FactRef) -> FactRow {
    let (tag, object, field) = match fact.object {
        FactObject::None => (0, ContentId([0; 32]), 0),
        FactObject::Field(field) => (1, ContentId([0; 32]), field),
        FactObject::Id(id) => (2, id, 0),
    };
    FactRow { subject: fact.subject, object, kind: fact.kind.0, tag, pad: 0, field }
}

fn fact_of(row: &FactRow) -> Result<FactRef, &'static str> {
    let zero = ContentId([0; 32]);
    if row.pad != 0 {
        return Err("a fact's pad byte is not zero");
    }
    let object = match row.tag {
        0 if row.object == zero && row.field == 0 => FactObject::None,
        1 if row.object == zero => FactObject::Field(row.field),
        2 if row.field == 0 => FactObject::Id(row.object),
        0..=2 => return Err("a fact's object has bytes past its end"),
        _ => return Err("a fact's object has an unknown tag"),
    };
    Ok(FactRef { kind: QueryKind(row.kind), subject: row.subject, object })
}

fn format_error(error: FormatError) -> String {
    // alloc: the reason a record does not open
    error.to_string()
}

/// The shape of `base` whose facts are `facts` (sorted, each once), written over `out`.
pub(super) fn write_shape(out: &mut Vec<u8>, base: &ContentId, facts: &[FactRef]) {
    let mut writer = Writer::with_buffer(SHAPE_SCHEMA, std::mem::take(out));
    let rows_at = match facts.is_empty() {
        true => None,
        false => {
            let at = writer.next::<FactRow>();
            for fact in facts {
                writer.push(&row_of(fact));
            }
            Some(at)
        }
    };
    let at = writer.next::<ShapeRoot>();
    let field = at + offset_of!(ShapeRoot, facts) as u64;
    let root = ShapeRoot {
        base: *base,
        facts: rows_at.map_or(Slice::EMPTY, |rows| Slice::between(field, rows, facts.len())),
        reserved: 0,
    };
    let written = writer.push(&root);
    debug_assert_eq!(written, at, "`Writer::next` is not where `push` writes");
    *out = writer.finish(at);
}

/// A shape, read in place.
#[derive(Clone, Copy)]
pub(super) struct ShapeRef<'a> {
    facts: &'a [FactRow],
}

impl<'a> ShapeRef<'a> {
    /// The shape in `bytes`, checked to be one of `base` and whole: nothing allocated but the
    /// reason it does not open.
    pub(super) fn open(bytes: &'a [u8], base: &ContentId) -> Result<ShapeRef<'a>, String> {
        let root = open_as::<ShapeRoot>(bytes, SHAPE_SCHEMA).map_err(format_error)?;
        if root.base != *base {
            // alloc: the reason a record does not open
            return Err("the shape of another body or step".to_owned());
        }
        if root.reserved != 0 {
            // alloc: as above
            return Err("a shape with reserved bytes set".to_owned());
        }
        let facts = root.facts.follow(bytes).map_err(format_error)?;
        let mut last = None;
        for row in facts {
            // alloc: as above
            let fact = fact_of(row).map_err(str::to_owned)?;
            if last.is_some_and(|last| last >= fact) {
                // alloc: as above
                return Err("a shape's facts out of order".to_owned());
            }
            last = Some(fact);
        }
        Ok(ShapeRef { facts })
    }

    pub(super) fn facts(self) -> impl Iterator<Item = FactRef> + 'a {
        self.facts.iter().map(|row| fact_of(row).expect("`ShapeRef::open` checked every fact"))
    }

    #[cfg(test)]
    pub(super) fn len(self) -> usize {
        self.facts.len()
    }
}

/// The answer record of `key`, the step's result `body`.
pub(super) fn write_answer(key: &ContentId, body: &[u8]) -> Vec<u8> {
    let mut writer = Writer::new(ANSWER_SCHEMA);
    let body_at = match body.is_empty() {
        true => None,
        false => {
            // The step's result is a buffer of its own, read in place: at an aligned offset.
            let at = writer.bytes.len().next_multiple_of(ALIGN);
            writer.bytes.resize(at, 0);
            writer.bytes.extend_from_slice(body);
            Some(at as u64)
        }
    };
    let at = writer.next::<AnswerRoot>();
    let field = at + offset_of!(AnswerRoot, body) as u64;
    let root = AnswerRoot {
        key: *key,
        body: body_at.map_or(Slice::EMPTY, |body_at| Slice::between(field, body_at, body.len())),
        reserved: 0,
    };
    let written = writer.push(&root);
    debug_assert_eq!(written, at, "`Writer::next` is not where `push` writes");
    writer.finish(at)
}

/// The step's result in the answer record `bytes`, checked to be the answer of `key`.
pub(super) fn open_answer<'a>(bytes: &'a [u8], key: &ContentId) -> Result<&'a [u8], String> {
    let root = open_as::<AnswerRoot>(bytes, ANSWER_SCHEMA).map_err(format_error)?;
    if root.key != *key {
        // alloc: the reason a record does not open
        return Err("the answer under another key".to_owned());
    }
    if root.reserved != 0 {
        // alloc: as above
        return Err("an answer with reserved bytes set".to_owned());
    }
    root.body.follow(bytes).map_err(format_error)
}
