//! A stored step's two records, in `blaze_record`'s form (base key, read-lists, answer):
//!
//! - its **shape**, a read-list under the body's base key ([`base_key`]: its identity, the
//!   step, the step's answer schema, these records' schema and the solver version), in a slot
//!   of that key's chain (`blaze_record::list_key`), kept by the store under [`shape_at`]: the
//!   facts the step read, sorted, each by `slots::fact_name` (as the goal store's shapes name
//!   them), and no note;
//! - its **answer**, under the shape's bytes and the values those facts had when the step ran
//!   (`blaze_record::AnswerKey`), kept by the store under [`answer_at`]: a `blaze_format`
//!   buffer read in place, its answer key and the step's result, a buffer of the step's own.
//!
//! The keys of the two, the shape's layout and the chain are `blaze_record`'s. A record is
//! checked against its key when opened: a store that gave a record under another key is an
//! error, not a miss.

use std::mem::offset_of;

use blaze_format::{
    ALIGN, FormatError, Part, Record, SchemaHash, Slice, Writer, open_as, schema, schema_of,
};
use blaze_ids::{ContentId, Hasher};
use blaze_query::{QueryKey, QueryKind, kinds::FRONT};

use super::Step;
use crate::next_solver::goal_store::{
    FactRef,
    slots::{FACT_BYTES, fact_name},
};

/// The kind of a step's shape records: this, plus the step.
const SHAPE_KIND: u16 = 0x3800;
/// The kind of a step's answer records: this, plus the step.
const ANSWER_KIND: u16 = 0x3810;

const _: () = {
    assert!(FRONT.start <= SHAPE_KIND && ANSWER_KIND + 0x30 <= FRONT.end);
    assert!((Step::COUNT as u16) <= ANSWER_KIND - SHAPE_KIND);
};

const BASE_DOMAIN: &str = "blaze v2 item store 1 body";

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
    assert!(size_of::<AnswerRoot>() == 48);
};

/// The schema of both records: what [`base_key`] covers. A shape's is the layout of
/// `blaze_record`'s read-list and the length of a fact's name in it.
pub(super) const RECORDS: SchemaHash = schema_of(&[
    Part::Str("blaze v2 item store 1"),
    Part::U64(blaze_record::LAYOUT),
    Part::U64(FACT_BYTES as u64),
    Part::Schema(<AnswerRoot as Record>::SCHEMA),
]);

const ANSWER_SCHEMA: SchemaHash = schema("blaze v2 item store 1 answer record");

/// What every shape of `step` of the body whose identity is `identity` is filed under, before
/// its slot: `schema` is the schema of the step's answers, `version` the solver version.
pub(super) fn base_key(
    identity: &ContentId,
    step: Step,
    schema: &SchemaHash,
    version: &ContentId,
) -> ContentId {
    Hasher::new(BASE_DOMAIN)
        .id(identity)
        .update(&[step as u8])
        .id(&RECORDS.0)
        .id(&schema.0)
        .id(version)
        .finish()
}

/// The key of the shape in slot `n` of the first generation of `base`'s chain: where its first
/// shapes are.
#[cfg(test)]
pub(super) fn shape_key(base: &ContentId, n: u32) -> ContentId {
    blaze_record::list_key(base, blaze_record::Slot { generation: 0, n })
}

/// The store key of the shape whose key is `shape_key`.
pub(super) fn shape_at(step: Step, shape_key: &ContentId) -> ContentId {
    QueryKey { kind: QueryKind(SHAPE_KIND + step as u16), input: *shape_key }.id()
}

/// The store key of the answer whose key is `answer_key`.
pub(super) fn answer_at(step: Step, answer_key: &ContentId) -> ContentId {
    QueryKey { kind: QueryKind(ANSWER_KIND + step as u16), input: *answer_key }.id()
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "the `map_err` callback of every read, which hands it the error by value"
)]
fn format_error(error: FormatError) -> String {
    // alloc: the reason a record does not open
    error.to_string()
}

/// The shape of `base` whose facts are `facts` (sorted, each once), written over `out`.
pub(super) fn write_shape(out: &mut Vec<u8>, base: &ContentId, facts: &[FactRef]) {
    out.clear();
    // alloc: the caller's buffer, kept from offer to offer
    blaze_record::write_list(out, base, &[], facts.iter().map(fact_name));
}

/// A shape, read in place: what tests read one back with (a lookup reads a shape through
/// `blaze_record::lookup`, a fact at a time).
#[cfg(test)]
#[derive(Clone, Copy)]
pub(super) struct ShapeRef<'a> {
    list: blaze_record::ListRef<'a>,
}

#[cfg(test)]
impl<'a> ShapeRef<'a> {
    /// The shape in `bytes`, checked to be one of `base` and whole.
    pub(super) fn open(bytes: &'a [u8], base: &ContentId) -> Result<ShapeRef<'a>, String> {
        use crate::next_solver::goal_store::slots::fact_of;
        let list = blaze_record::ListRef::open(bytes, base).map_err(|why| why.to_string())?;
        if !list.note().is_empty() {
            return Err("a shape with a note".to_owned());
        }
        let mut last = None;
        for name in list.reads() {
            let fact = fact_of(name).map_err(str::to_owned)?;
            if last.is_some_and(|last| last >= fact) {
                return Err("a shape's facts out of order".to_owned());
            }
            last = Some(fact);
        }
        Ok(ShapeRef { list })
    }

    pub(super) fn facts(self) -> impl Iterator<Item = FactRef> + 'a {
        use crate::next_solver::goal_store::slots::fact_of;
        self.list.reads().map(|name| fact_of(name).expect("`ShapeRef::open` checked every fact"))
    }

    pub(super) fn len(self) -> usize {
        self.list.len()
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
