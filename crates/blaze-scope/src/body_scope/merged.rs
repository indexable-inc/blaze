//! D189 step 5, S2: the crate's MERGED impl table under `BlockScopes::Bodies`. The crate solve
//! holds no impl written inside a body (Names solves module-level items only), so a crate's
//! items record alone would hide from every dependent an impl a SURFACE body writes
//! (`fn f() { impl Tr for X { type A = Local; } }`, read downstream through `<X as Tr>::A`).
//! The merged table is the crate's module-level impl rows with each surface body's rows from its
//! `BodyScope` record, as ONE sorted table:
//!
//! - Its rows: [`ImplRow`] (impl key, module, filing through the body's blocks, [`BodyWorld`]),
//!   with the header and the impl's whole text the dependents read.
//! - Its key ([`merged_key`]): the crate's names key, then each surface body's IMPL ROWS DIGEST
//!   ([`impl_rows_digest`], held in the record, version 3) in (file, body ordinal) order. An edit
//!   to a body's statements that moves no impl, no local type an impl may name, and no filing
//!   leaves every digest, so the key, so the table, so every dependent's read of it unchanged.
//!   A record's other bytes (its read list, its blocks' scopes) are NOT in the key: keying by the
//!   record's whole bytes would move the table on every statement edit (the world test's planted
//!   bug, tests/front2_body_scope_merged.rs).
//!
//! Who reads it (the step-6 switch, not applied here): the items record's impl section and
//! bucket index (crate::items_record `encode_with`, buckets.rs `bucket_digest`), which the heads
//! stage and every dependent's method and trait lookup read: under `Bodies` `encode_with` takes
//! the merged rows beside `names.tables.impls`, and the items record's key takes
//! [`merged_key`] in place of the names key alone.

#![blaze::sans_io]

use blaze_id::ItemKey;
use blaze_ids::{ContentId, Hasher};
use blaze_own::{AVec, Alloc};

use super::{BodyScope, BodyWorld, ImplRow, LocalHead};
use crate::items_record::buckets::Filing;
use crate::items_record::{ItemsRecordError, impl_filing};
use crate::resolve::CrateNames;
use crate::surface::BlockScopes;

/// The merged table's format version: in its key, so a change of what a row holds is a new key.
pub const MERGED_VERSION: u32 = 1;

const ROWS_DOMAIN: &str = "blaze-scope body impl rows 1";
const KEY_DOMAIN: &str = "blaze-scope merged impl table 1";

/// The impl rows of one body: each impl its blocks write, filed through the body's own blocks
/// on top of the crate's finished names ([`BodyWorld`]), sorted by impl key.
pub fn body_rows<'o>(c: &CrateNames<'_>, body: &BodyScope<'_>, alloc: Alloc<'o>) -> Result<AVec<'o, ImplRow>, ItemsRecordError> {
    let world = BodyWorld { krate: c, body };
    let mut rows: AVec<'o, ImplRow> = AVec::with_capacity_in(body.solved.tables.impls.len(), alloc);
    for i in body.solved.tables.impls.iter() {
        rows.push(ImplRow { key: i.key, module: i.module, filing: impl_filing(&world, c.key, c.edition, i, alloc)? });
    }
    rows.sort_unstable_by_key(|r| r.key);
    Ok(rows)
}

/// The digest of what a dependent can read of a body's impls: each impl's key, module, filing
/// (its bucket key's bytes, or why it is not filed), header, whole text and each member's key and
/// text (an expansion's members too), in key order; then each local type an impl may name ([`LocalHead`]: a superset, rule 3) with its
/// whole text. Nothing of the body's statements is in it.
pub fn impl_rows_digest(body: &BodyScope<'_>, rows: &[ImplRow]) -> ContentId {
    let mut h = Hasher::new(ROWS_DOMAIN);
    h.update(&count(rows.len()));
    for r in rows {
        h.update(&r.key.0.0).update(&r.module.0.0);
        match r.filing {
            Filing::At(bucket) => {
                let b = bucket.bytes();
                h.update(&[1]).update(&count(b.len())).update(b);
            }
            Filing::Not(why) => {
                h.update(&[0, why as u8]);
            }
        }
        let header = body.impls.iter().find(|i| i.key == r.key).map_or("", |i| i.header);
        h.update(&count(header.len())).update(header.as_bytes());
        text_into(&mut h, body, r.key);
        // Its members as the solve joined them (an expansion's members included), in order.
        let members = body.solved.tables.members.get(&r.key).map_or(&[][..], |m| &m[..]);
        h.update(&count(members.len()));
        for m in members {
            h.update(&m.key.0.0).update(&count(m.text.len())).update(m.text.as_bytes());
        }
    }
    h.update(&count(body.locals.len()));
    for LocalHead { key, module, name } in body.locals.iter() {
        h.update(&key.0.0).update(&module.0.0).update(&count(name.len())).update(name.as_bytes());
        text_into(&mut h, body, *key);
    }
    h.finish()
}

/// `key`'s whole text as the body's solve holds it, or a marker that it holds none.
fn text_into(h: &mut Hasher, body: &BodyScope<'_>, key: ItemKey) {
    match body.solved.tables.texts.get(&key) {
        Some((t, _)) => {
            h.update(&[1]).update(&count(t.text.len())).update(t.text.as_bytes());
        }
        None => {
            h.update(&[0]);
        }
    }
}

/// One surface body's place in the crate and its rows digest: what the merged key is made of.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct SurfaceBody {
    /// The crate file its text is in (`CrateNames::files` index; a body an expansion wrote is
    /// at the file of the outermost call) and its ordinal among that file's surface bodies.
    pub file: u32,
    pub ordinal: u32,
    /// Its record's [`BodyScope::impl_rows`].
    pub impl_rows: ContentId,
}

/// The merged table's key (rules 21, 65): the version, the strategy's byte, the crate's names
/// key (every module-level impl and every name a filing reads outside a body), then each
/// surface body's `impl_rows` in (file, ordinal) order. `bodies` must be sorted that way; the
/// caller's surface pass gives them so (surface_names.rs, one record per file, rows in order).
pub fn merged_key(names_key: ContentId, bodies: &[SurfaceBody]) -> ContentId {
    let mut h = Hasher::new(KEY_DOMAIN);
    h.update(&MERGED_VERSION.to_le_bytes()).update(&[BlockScopes::Bodies.key_byte()]).update(&names_key.0).update(&count(bodies.len()));
    for b in bodies {
        h.update(&b.file.to_le_bytes()).update(&b.ordinal.to_le_bytes()).update(&b.impl_rows.0);
    }
    h.finish()
}

/// The merged rows: the crate's module-level rows (`module_rows`, as `modules_impl_table` makes
/// them from the crate's own tables, which hold no body's impl under `Bodies`) and each surface
/// body's rows, sorted by impl key. Two rows under one key is a typed refusal (rule 21: a body's
/// impl key is derived under its body's key, so none can equal another's).
pub fn merged_rows<'o>(module_rows: &[ImplRow], bodies: &[&[ImplRow]], alloc: Alloc<'o>) -> Result<AVec<'o, ImplRow>, MergeError> {
    let mut rows: AVec<'o, ImplRow> = AVec::with_capacity_in(module_rows.len() + bodies.iter().map(|b| b.len()).sum::<usize>(), alloc);
    rows.extend_from_slice(module_rows);
    for b in bodies {
        rows.extend_from_slice(b);
    }
    rows.sort_unstable_by_key(|r| r.key);
    if let Some(w) = rows.windows(2).find(|w| w[0].key == w[1].key) {
        return Err(MergeError::TwoRows { key: w[0].key });
    }
    Ok(rows)
}

/// Why rows do not merge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MergeError {
    /// Two rows hold impl `key`.
    TwoRows { key: ItemKey },
}

fn count(n: usize) -> [u8; 8] {
    u64::try_from(n).unwrap_or(u64::MAX).to_le_bytes()
}
