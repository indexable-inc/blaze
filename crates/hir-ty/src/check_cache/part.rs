//! The bodies part of a crate: one `blaze_format` buffer, a key column (each body's owner by its
//! signature key, sorted) and, per body, a buffer of its own with its inference record, its ids
//! in portable form ([`super::ids`]), the types its anonymous constants are interned with, and
//! whether its borrow check passed. The crate's items, which every body's ids name, are one
//! portable table beside the keys, decoded once per crate. Each body [`encode`] left out is
//! listed by its key with why, so a reader tells a body the part has no record of from one whose
//! key the writer never gave (a bug in the key, never a body inferred instead).

use std::mem::offset_of;

use base_db::{Crate, Db};
use blaze_format::{
    ALIGN, FormatError, HEADER_LEN, Part as SchemaPart, Persist, Placed, Record, SchemaHash, Slice,
    Writer, open_as, schema_of,
};
use blaze_ids::ContentId;
use hir_def::{
    DefWithBodyId,
    deps::{BODY_KEY_DOMAIN, KEY_DOMAIN, PortableIds, body_key},
    nameres::{demand::crate_body_owners, frozen},
};

use super::ids::{self, IdKind, Items, Table};
use crate::{
    InferenceResult,
    infer::persist::InferenceRecord,
    next_solver::{
        DbInterner,
        persist::{TABLE_SCHEMA, TyTableWriter},
    },
};

type R<T = ()> = Result<T, String>;

/// The version of what [`super::ids`] writes of a body's ids.
const IDS_VERSION: u64 = ids::VERSION;

blaze_format::record! {
    /// Where a body's buffer is in the part, from the part's start.
    #[derive(Clone, Copy, Debug)]
    struct Place: 1 {
        at: u32,
        len: u32,
    }
}

blaze_format::record! {
    /// The part: each body's key, in increasing order, and where its buffer is, at the same
    /// index; the crate's items as frontend-deps' portable table writes them, and the kind of
    /// each of its entries; the key of each body left out, in increasing order, and why, its
    /// reasons one UTF-8 text with each one's end at the same index.
    #[derive(Clone, Copy)]
    struct PartRoot: 2 {
        keys: Slice<ContentId>,
        places: Slice<Place>,
        items: Slice<u8>,
        kinds: Slice<u8>,
        left: Slice<ContentId>,
        left_ends: Slice<u32>,
        left_why: Slice<u8>,
        reserved: u64,
    }
}

blaze_format::record! {
    /// One body: where its constants' type table is, its inference record's buffer, its ids,
    /// and its borrow check's verdict.
    #[derive(Clone, Copy)]
    struct BodyRoot: 1 {
        locs: u64,
        record: Slice<u8>,
        ids: Slice<u8>,
        passed: u32,
        reserved: u32,
    }
}

const _: () = {
    assert!(size_of::<Place>() == 8);
    assert!(size_of::<PartRoot>() == 64);
    assert!(size_of::<BodyRoot>() == 32);
};

/// The schema of the part, and the version of everything its bytes are made with: the records,
/// the type tables, the ids, frontend-deps' portable table and the key a body is found by.
pub(crate) const SCHEMA: SchemaHash = schema_of(&[
    SchemaPart::Str("blaze v2 dependency bodies 1"),
    SchemaPart::Schema(<PartRoot as Record>::SCHEMA),
    SchemaPart::Schema(<BodyRoot as Record>::SCHEMA),
    SchemaPart::Schema(<InferenceRecord as Persist>::SCHEMA),
    SchemaPart::Schema(TABLE_SCHEMA),
    SchemaPart::U64(IDS_VERSION),
    SchemaPart::Str(frozen::ENCODING),
    SchemaPart::Str(KEY_DOMAIN),
    SchemaPart::Str(BODY_KEY_DOMAIN),
]);

/// The schema of one body's buffer.
const BODY_SCHEMA: SchemaHash = schema_of(&[
    SchemaPart::Str("blaze v2 dependency body 1"),
    SchemaPart::Schema(<BodyRoot as Record>::SCHEMA),
]);

fn format_error(error: FormatError) -> String {
    // alloc: the reason of a refusal or a panic
    error.to_string()
}

/// The part of `krate`, from its bodies as this database inferred them, `passed` giving each
/// one's verdict. A body with no portable form, or one `leave` gives a reason for, is left out
/// and listed with why; one in a block
/// has no key and no entry. The part fails only when its items do not encode or two bodies have
/// one key.
pub(crate) fn encode(
    db: &Db,
    krate: Crate,
    // dyn: the driver's verdicts, one call per body at store time
    passed: &(dyn Fn(DefWithBodyId) -> bool + Sync),
    // dyn: one call per body at store time
    leave: &dyn Fn(DefWithBodyId) -> Option<String>,
) -> R<Vec<u8>> {
    let interner = DbInterner::new_with(db, krate);
    let mut items = Items::default();
    let owners = crate_body_owners(db, krate);
    // alloc: the bodies of one crate, written once at store time
    let mut bodies = Vec::with_capacity(owners.len());
    // alloc: the bodies left out of one crate's part, written once at store time
    let mut left: Vec<(ContentId, String)> = Vec::new();
    for def in owners {
        let Some(key) = body_key(db, def) else {
            tracing::debug!(?def, "a body in a block, left out of its crate's part");
            continue;
        };
        let made = match leave(def) {
            Some(why) => Err(why),
            None => body(db, interner, krate, def, passed(def), &mut items),
        };
        match made {
            Ok(bytes) => bodies.push((key, bytes)),
            Err(why) => {
                tracing::debug!(?def, %why, "a body left out of its crate's part");
                left.push((key, why));
            }
        }
    }
    bodies.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    left.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    let twice = bodies.windows(2).any(|pair| pair[0].0 == pair[1].0)
        || left.windows(2).any(|pair| pair[0].0 == pair[1].0)
        || left.iter().any(|(key, _)| bodies.binary_search_by(|it| it.0.cmp(key)).is_ok());
    if twice {
        // alloc: the error that fails the part
        return Err("two bodies have one key".to_owned());
    }
    let item_bytes = items.ids.encode(db, krate)?;

    let mut out = Writer::new(SCHEMA);
    // alloc: the place column, written once per crate
    let mut places = Vec::with_capacity(bodies.len());
    // alloc: the error that fails the part
    let place = |it: usize| u32::try_from(it).map_err(|_| "a part of 4 GiB or more".to_owned());
    for (_, bytes) in &bodies {
        let at = out.bytes.len().next_multiple_of(ALIGN);
        out.bytes.resize(at, 0);
        out.bytes.extend_from_slice(bytes);
        places.push(Place { at: place(at)?, len: place(bytes.len())? });
    }
    // alloc: the key column, written once per crate
    let keys: Vec<ContentId> = bodies.iter().map(|&(key, _)| key).collect();
    let keys = Placed::push(&mut out, &keys);
    let places = Placed::push(&mut out, &places);
    let item_bytes = Placed::push(&mut out, &item_bytes);
    let kinds = Placed::push(&mut out, &items.kinds);
    // alloc: the left-out columns, written once per crate
    let left_keys: Vec<ContentId> = left.iter().map(|&(key, _)| key).collect();
    // alloc: as above
    let mut left_why: Vec<u8> = Vec::new();
    // alloc: as above
    let mut left_ends: Vec<u32> = Vec::with_capacity(left.len());
    for (_, why) in &left {
        left_why.extend_from_slice(why.as_bytes());
        left_ends.push(place(left_why.len())?);
    }
    let left_keys = Placed::push(&mut out, &left_keys);
    let left_ends = Placed::push(&mut out, &left_ends);
    let left_why = Placed::push(&mut out, &left_why);
    let at = out.next::<PartRoot>();
    let field = |offset: usize| at + offset as u64;
    let root = PartRoot {
        keys: keys.slice(field(offset_of!(PartRoot, keys))),
        places: places.slice(field(offset_of!(PartRoot, places))),
        items: item_bytes.slice(field(offset_of!(PartRoot, items))),
        kinds: kinds.slice(field(offset_of!(PartRoot, kinds))),
        left: left_keys.slice(field(offset_of!(PartRoot, left))),
        left_ends: left_ends.slice(field(offset_of!(PartRoot, left_ends))),
        left_why: left_why.slice(field(offset_of!(PartRoot, left_why))),
        reserved: 0,
    };
    let written = out.push(&root);
    debug_assert_eq!(written, at, "`Writer::next` is not where `push` writes");
    Ok(out.finish(at))
}

/// The buffer of `def`'s body: its record written inside a relocating scope, then its ids.
fn body<'db>(
    db: &'db Db,
    interner: DbInterner<'db>,
    krate: Crate,
    def: DefWithBodyId,
    passed: bool,
    items: &mut Items,
) -> R<Vec<u8>> {
    let result = InferenceResult::of(db, def);
    let mut table = Table::default();
    let record = ids::relocating(&mut table, || blaze_format::to_bytes(&result.to_record(interner)));
    let mut locs = TyTableWriter::new(interner);
    let words = ids::write(db, krate, &mut table, &mut locs, items)?;

    let mut out = Writer::new(BODY_SCHEMA);
    let words = Placed::push(&mut out, &words);
    // The record is a buffer of its own, read in place: at an aligned offset.
    let record_at = out.bytes.len().next_multiple_of(ALIGN);
    out.bytes.resize(record_at, 0);
    out.bytes.extend_from_slice(&record);
    let locs = locs.write(&mut out);
    let at = out.next::<BodyRoot>();
    let field = |offset: usize| at + offset as u64;
    let root = BodyRoot {
        locs,
        record: Slice::between(field(offset_of!(BodyRoot, record)), record_at as u64, record.len()),
        ids: words.slice(field(offset_of!(BodyRoot, ids))),
        passed: u32::from(passed),
        reserved: 0,
    };
    let written = out.push(&root);
    debug_assert_eq!(written, at, "`Writer::next` is not where `push` writes");
    Ok(out.finish(at))
}

/// A part, opened: its columns, checked in place.
#[derive(Clone, Copy)]
pub(crate) struct PartRef<'a> {
    bytes: &'a [u8],
    keys: &'a [ContentId],
    places: &'a [Place],
    items: &'a [u8],
    kinds: &'a [u8],
    left: &'a [ContentId],
    left_ends: &'a [u32],
    left_why: &'a [u8],
}

/// `bytes` as a part, its root and columns checked in place ([`open_as`]): what a served
/// lookup does, for bytes [`open`] accepted when they were given to the database.
pub(crate) fn view(bytes: &[u8]) -> R<PartRef<'_>> {
    let root = open_as::<PartRoot>(bytes, SCHEMA).map_err(format_error)?;
    let keys = root.keys.follow(bytes).map_err(format_error)?;
    let places = root.places.follow(bytes).map_err(format_error)?;
    let items = root.items.follow(bytes).map_err(format_error)?;
    let kinds = root.kinds.follow(bytes).map_err(format_error)?;
    let left = root.left.follow(bytes).map_err(format_error)?;
    let left_ends = root.left_ends.follow(bytes).map_err(format_error)?;
    let left_why = root.left_why.follow(bytes).map_err(format_error)?;
    if keys.len() != places.len() {
        // alloc: the reason of a refusal
        return Err("its keys and bodies are not one each".to_owned());
    }
    if left.len() != left_ends.len() || root.reserved != 0 {
        // alloc: the reason of a refusal
        return Err("its left-out bodies and their reasons are not one each".to_owned());
    }
    Ok(PartRef { bytes, keys, places, items, kinds, left, left_ends, left_why })
}

/// `bytes` as a part: [`view`], then that its keys increase, that each body's place is aligned
/// and inside it, that each item's kind is one, and that the left-out keys increase, are none of
/// its bodies' and have reasons that end in order at the end of their text, which is UTF-8.
/// Nothing allocated but the reason of a refusal; a body's own bytes are checked when it is read.
pub(crate) fn open(bytes: &[u8]) -> R<PartRef<'_>> {
    let part = view(bytes)?;
    let PartRef { keys, places, kinds, left, left_ends, left_why, .. } = part;
    if !keys.windows(2).all(|pair| pair[0] < pair[1]) {
        // alloc: the reason of a refusal
        return Err("its keys out of order".to_owned());
    }
    if !left.windows(2).all(|pair| pair[0] < pair[1]) {
        // alloc: the reason of a refusal
        return Err("its left-out keys out of order".to_owned());
    }
    if left.iter().any(|key| keys.binary_search(key).is_ok()) {
        // alloc: the reason of a refusal
        return Err("a body both recorded and left out".to_owned());
    }
    let ends_in_order = left_ends.windows(2).all(|pair| pair[0] <= pair[1])
        && left_ends.last().map_or(left_why.is_empty(), |&end| end as usize == left_why.len());
    if !ends_in_order || std::str::from_utf8(left_why).is_err() {
        // alloc: the reason of a refusal
        return Err("the reasons of its left-out bodies do not read".to_owned());
    }
    let inside = |&Place { at, len }: &Place| {
        (at as usize).is_multiple_of(ALIGN)
            && at as usize >= HEADER_LEN
            && (at as usize).checked_add(len as usize).is_some_and(|end| end <= bytes.len())
    };
    if !places.iter().all(inside) {
        // alloc: the reason of a refusal
        return Err("a body outside the part".to_owned());
    }
    if !kinds.iter().all(|&kind| ids::item_kind_of(kind).is_some()) {
        // alloc: the reason of a refusal
        return Err("an item of no kind".to_owned());
    }
    Ok(part)
}

impl<'a> PartRef<'a> {
    /// The buffer of the body whose key is `key`, `None` when the part has none: a binary
    /// search and a slice, nothing allocated.
    pub(crate) fn find(&self, key: &ContentId) -> Option<&'a [u8]> {
        let at = self.keys.binary_search(key).ok()?;
        let Place { at, len } = self.places[at];
        self.bytes.get(at as usize..(at as usize).checked_add(len as usize)?)
    }

    /// Why the body whose key is `key` was left out of the part, `None` when it was not: a
    /// binary search and a slice, nothing allocated. A reason that does not read (in bytes only
    /// [`view`] checked) is `Some("")`: the body is still listed.
    pub(crate) fn left_out(&self, key: &ContentId) -> Option<&'a str> {
        let at = self.left.binary_search(key).ok()?;
        let start = at.checked_sub(1).map_or(0, |it| self.left_ends[it] as usize);
        let end = self.left_ends[at] as usize;
        Some(self.left_why.get(start..end).and_then(|it| std::str::from_utf8(it).ok()).unwrap_or(""))
    }

    /// How many bodies the part has.
    pub(crate) fn len(&self) -> usize {
        self.keys.len()
    }

    /// The keys of its bodies, in increasing order.
    pub(crate) fn keys(&self) -> &'a [ContentId] {
        self.keys
    }

    /// The crate's items, made in this database: what each body's ids name by index.
    pub(crate) fn items(&self, db: &Db, krate: Crate) -> R<Box<[(IdKind, u64)]>> {
        let ids = PortableIds::decode(db, krate, self.items)?;
        if ids.len() != self.kinds.len() {
            // alloc: the reason of a refusal
            return Err(format!("{} items and {} kinds", ids.len(), self.kinds.len()));
        }
        self.kinds
            .iter()
            .enumerate()
            .map(|(at, &kind)| {
                let kind = ids::item_kind_of(kind).ok_or("an item of no kind")?;
                let index = u32::try_from(at).map_err(|_| "more than 2^32 items")?;
                let bits = ids::item_bits(&ids, kind, index)
                    // alloc: the reason of a refusal
                    .ok_or_else(|| format!("item {at} is no {kind:?}"))?;
                Ok((kind, bits))
            })
            // alloc: the crate's items, once per crate served (`served_items` keeps them)
            .collect()
    }
}

/// A body's buffer, opened.
#[derive(Clone, Copy)]
pub(crate) struct BodyRef<'a> {
    bytes: &'a [u8],
    root: &'a BodyRoot,
}

/// `bytes`, a body's buffer from [`PartRef::find`], opened: its header, its schema and its
/// columns.
pub(crate) fn open_body(bytes: &[u8]) -> R<BodyRef<'_>> {
    let root = open_as::<BodyRoot>(bytes, BODY_SCHEMA).map_err(format_error)?;
    if root.passed > 1 || root.reserved != 0 {
        // alloc: the reason of a refusal
        return Err("a verdict that is neither passed nor failed".to_owned());
    }
    Ok(BodyRef { bytes, root })
}

impl BodyRef<'_> {
    /// Whether the body's borrow check passed in the database that wrote it.
    pub(crate) fn passed(&self) -> bool {
        self.root.passed == 1
    }

    /// The body's inference result, made in this database: its ids first, against `items`
    /// ([`PartRef::items`]), then its record, read through them.
    pub(crate) fn inference<'db>(
        &self,
        db: &'db Db,
        krate: Crate,
        items: &[(IdKind, u64)],
    ) -> R<InferenceResult<'db>> {
        let words = self.root.ids.follow(self.bytes).map_err(format_error)?;
        let record = self.root.record.follow(self.bytes).map_err(format_error)?;
        let interner = DbInterner::new_with(db, krate);
        let mut table = ids::read(db, interner, items, words, self.bytes, self.root.locs)?;
        ids::relocating(&mut table, || InferenceRecord::read(record)?.to_result(interner))
            .map_err(format_error)
    }
}
