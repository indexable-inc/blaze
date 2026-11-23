//! A head as the drive's `Signature` record (D114, docs/v2/plan/design/drive.md, "Check times:
//! the end state in numbers", rule 4 and piece (d)).
//!
//! THE UNIT. One `Signature` is one [`Want`] of the head store: what the checker asks to have
//! made when a body names an item, and what [`Store::ensure`] makes as one step, so the drive's
//! kind and the checker's ask are one thing (rule 30). By `by_origin::key_want`, the one place
//! a key is classified: a fn, `const` or `static` (`Want::Fn`); a struct, enum or union with its
//! variants and constructors (`Want::Adt`); a trait with its members' signatures
//! (`Want::Members`); an impl's header with its member list (`Want::Impl`); one fn of an impl
//! (`Want::MemberFn`); a type alias or trait alias (`Want::Head`).
//!
//! Its DIGEST ([`ProjectWorld::want_digest`]) is the head records (head_record.rs) of every head
//! the want makes, each by [`head_record::reader_digest`] (the record without a fn's text slot:
//! what it means, not how it is spelled), in a fixed order: a head made again equal, or only
//! respaced or commented, moves nobody. What the record form leaves out by name (head_record.rs's header: a fn's and
//! an ADT's lifetime part, an impl's const parameters) is the command's to cover beside it.
//!
//! Its PLACE ([`ProjectWorld::want_place`]): the crate, file and outline entry the want's item
//! is written at, so the command names it by (file, item chain) as it names a body. A want
//! whose item a macro wrote has no entry: [`HeadDigestError::Unplaced`], by name.
//!
//! A body's read list (reads.rs) names heads by checker key under the head kinds of
//! [`FactKind`]; [`ProjectWorld::want_of_fact`] gives each such fact its want.

use blaze_id::ItemKey as LibKey;
use blaze_ids::{ContentId, Hasher};
use blaze_own::{AVec, Alloc};
use blaze_scope::resolve::At;
use blaze_surface::{ItemIndex, ItemKind};
use blaze_typeck::ItemKey;

use super::ProjectWorld;
use crate::front2::by_origin;
use crate::front2::fill::FillError;
use crate::front2::head_record::{self, Head, HeadAt, HeadRecordError, OpaqueOrigin};
use crate::front2::heads::{HeadError, Store, Want};
use crate::front2::keys::{Keys, MemberKind, Origin, checker_of, member_key_of};
use crate::front2::project_records::BodySource;
use crate::front2::reads::{Fact, FactKind};
use crate::front2::source::NamesSource;
use crate::front2::tables::head::{FreezeError, FrozenHead};

/// The domain of a want's digest: the count of its heads, then each head record's digest.
const WANT_DOMAIN: &str = "blaze-ladder front2 signature digest 1";

/// Where a want's item is written: crate `krate` of the world (0: the program's own), its file
/// `file` (an `At::File`'s index), its outline entry `item` there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WantPlace {
    pub krate: usize,
    pub file: u32,
    pub item: ItemIndex,
}

/// Why a head could not be named or digested, each naming the key or want it is about.
#[derive(Debug)]
pub enum HeadDigestError {
    /// The fact's key is classified by no want (`by_origin`'s refusal).
    Fill { fact: Fact, error: FillError },
    /// The store refused to make the want.
    Ensure { want: Want, error: HeadError },
    /// A head of the want could not be written as a record.
    Record { want: Want, at: HeadAt, error: HeadRecordError },
    /// The want's item has no file entry: a macro wrote it, or the source has no place for it.
    Unplaced { want: Want },
    /// An impl fn's key is not a member the map filed.
    NotAMember { want: Want, key: ItemKey },
}

impl core::fmt::Display for HeadDigestError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            HeadDigestError::Fill { fact, error } => write!(f, "the read {fact:?} names no head (D114): {error}"),
            HeadDigestError::Ensure { want, error } => write!(f, "the head {want:?} could not be made (D114): {error}"),
            HeadDigestError::Record { want, at, error } => write!(f, "the head {at:?} of {want:?} is no record (D114): {error}"),
            HeadDigestError::Unplaced { want } => write!(f, "the head {want:?} has no file entry: a macro wrote it, and a `Signature` is named by its file entry (D114)"),
            HeadDigestError::NotAMember { want, key } => write!(f, "the fn {:#034x} of {want:?} is no filed member (D114)", key.0),
        }
    }
}

/// Whether a fact of a body's read list is about a head: a fn's signature, an item's
/// predicates, an ADT's definition, a trait's definition, an impl's header, a container's
/// member by name, an associated item's parent, an item's attributes or const parameters, an
/// associated type's generics. The rest are names, buckets, lang items or body records.
pub const fn reads_a_head(kind: FactKind) -> bool {
    matches!(
        kind,
        FactKind::Head
            | FactKind::BorrowHead
            | FactKind::Predicates
            | FactKind::Adt
            | FactKind::BorrowAdt
            | FactKind::TraitDef
            | FactKind::ImplHeader
            | FactKind::AssocItem
            | FactKind::AssocParent
            | FactKind::Attrs
            | FactKind::ConstParams
            | FactKind::AssocGenerics
            | FactKind::TypeAlias
            | FactKind::MemberVisible
            | FactKind::MemberUnstable
    )
}

impl<'n, S: BodySource<'n>> ProjectWorld<'n, S> {
    /// The want a fact of a body's read list reads; `None` for a fact about no head
    /// ([`reads_a_head`]).
    pub fn want_of_fact(&self, fact: Fact) -> Result<Option<Want>, HeadDigestError> {
        let opaque_owner = self.opaque_fn(ItemKey(fact.key));
        if !reads_a_head(fact.kind) && fact.kind != FactKind::Coroutine && !(fact.kind == FactKind::Opaque && opaque_owner.is_some()) {
            return Ok(None);
        }
        let key = opaque_owner.unwrap_or(ItemKey(fact.key));
        let names = self.front.names;
        let keys = &self.front.keys;
        let kind = by_origin::kind_of_key(names, keys, key).map_err(|error| HeadDigestError::Fill { fact, error })?;
        let want = by_origin::key_want(names, keys, key, kind).map_err(|error| HeadDigestError::Fill { fact, error })?;
        // A trait's member signatures share one head, but a provided method's
        // hidden type and coroutines belong to that particular method's body.
        if matches!(fact.kind, FactKind::Opaque | FactKind::Coroutine)
            && let Want::Members(container) = want
        {
            return Ok(Some(Want::MemberFn(container, key)));
        }
        Ok(Some(want))
    }

    /// The parts of this checked body's hidden types that another body can read.
    pub fn hidden_facts(&self, owner: ItemKey, scratch: Alloc<'_>, put: impl FnMut(Fact, u64)) -> Result<(), blaze_typeck::CheckError> {
        self.store.hidden.published(owner, scratch, put)
    }

    /// The want whose item is the outline entry `item` of file `file` of the program's own
    /// crate: what a `Signature` task of that entry makes. `None`: the entry is no head of its
    /// own (a module, a `use`, a variant or field, a trait's member, an impl's const or type).
    pub fn want_at_entry(&self, file: u32, item: ItemIndex) -> Option<Want> {
        let names = self.front.names;
        let mut found = None;
        names.each_item(0, &mut |key, _module| {
            if found.is_some() {
                return;
            }
            let Some(place) = names.place_of(key) else { return };
            if place.at != At::File(file) {
                return;
            }
            if place.item == item {
                found = by_origin::kind_of_lib(names, key).ok().and_then(|kind| by_origin::item_want(names, kind).ok());
                return;
            }
            if names.kind_of_item(key) != Some(ItemKind::Impl) {
                return;
            }
            names.each_member(key, &mut |m| {
                if m.item == item && m.kind == ItemKind::Function && m.written_by.is_none() {
                    // Its key as COMPUTED from its container and name (keys.rs `member_key_of`),
                    // never as filed: a member is filed only once its impl's head was made in
                    // this world, so asking the filed key made the want (and the `Signature`'s
                    // out) hang on which body the job checked first (rule 25; blaze-cli
                    // order_tests.rs `reversed_order_answers_alike`, lane FH 2026-11-17). The
                    // want's making (`Store::step`) makes the impl's head first and files it.
                    found = m.name.map(|name| Want::MemberFn(key, crate::front2::keys::member_key_of(checker_of(key), crate::front2::keys::MemberKind::Fn, name)));
                }
            });
        });
        found
    }

    /// Where `want`'s item is written ([`WantPlace`]).
    pub fn want_place(&self, want: Want) -> Result<WantPlace, HeadDigestError> {
        let names = self.front.names;
        let lib = match want {
            Want::Head(k) | Want::Adt(k) | Want::Impl(k) | Want::Fn(k) | Want::Members(k) | Want::MemberFn(k, _) => k,
        };
        let place = names.place_of(lib).ok_or(HeadDigestError::Unplaced { want })?;
        let At::File(file) = place.at else { return Err(HeadDigestError::Unplaced { want }) };
        let Want::MemberFn(_, f) = want else {
            return Ok(WantPlace { krate: place.krate, file, item: place.item });
        };
        let Some(Origin::Member { name, .. }) = self.front.keys.origin(f) else {
            return Err(HeadDigestError::NotAMember { want, key: f });
        };
        let mut item = None;
        names.each_member(lib, &mut |m| {
            if m.kind == ItemKind::Function && m.name == Some(&**name) && m.written_by.is_none() {
                item = Some(m.item);
            }
        });
        let item = item.ok_or(HeadDigestError::Unplaced { want })?;
        Ok(WantPlace { krate: place.krate, file, item })
    }

    /// The written types of `want`'s fn signature that name nothing (E0412, front2
    /// type_miss.rs), each made the error type in its head: empty for a want that is no fn's,
    /// or whose head is not made ([`Self::want_digest`] makes it).
    pub fn want_type_misses(&self, want: Want) -> &[crate::front2::type_miss::HeadMiss] {
        let key = match want {
            Want::Fn(lib) => checker_of(lib),
            Want::MemberFn(_, f) => f,
            Want::Head(_) | Want::Adt(_) | Want::Impl(_) | Want::Members(_) => return &[],
        };
        self.store.type_misses.get(&key).map_or(&[], |m| m.as_slice())
    }

    /// The digest of every head `want` makes (see the module docs), made first when the store
    /// has not made it. `alloc` holds one head record's bytes at a time.
    pub fn want_digest(&mut self, want: Want, alloc: Alloc<'_>) -> Result<ContentId, HeadDigestError> {
        if !self.store.made(want) {
            self.store.ensure(self.front.names, &mut self.front.keys, want).map_err(|error| HeadDigestError::Ensure { want, error })?;
        }
        want_digest_in(self.front.names, &self.front.keys, &mut self.store, want, alloc)
    }

}

/// The digest of every head `want` made in `store` ([`want_heads`]; see the module docs): the
/// count, then each head's [`head_record::reader_digest`] in that order. `want` must be made.
/// Each head's lifetime part is made first (lifetimes.rs `Store::make_lives`): it is in what
/// the head means (head_record.rs form 10), so an edit of a lifetime bound alone moves it.
pub fn want_digest_in<'n, N: crate::front2::source::NamesSource<'n>>(names: N, keys: &crate::front2::keys::Keys, store: &mut crate::front2::heads::Store<'n>, want: Want, alloc: Alloc<'_>) -> Result<ContentId, HeadDigestError> {
    let heads = want_heads(store, want, alloc);
    store.make_lives(names, keys, &heads);
    let store: &crate::front2::heads::Store<'n> = store;
    let mut digests: AVec<'_, ContentId> = AVec::new_in(alloc);
    for at in heads.iter().copied() {
        let head = head_record::from_store(store, at).map_err(|error| HeadDigestError::Record { want, at, error })?;
        // What the head means, not how it is spelled: a reformat moves no reader.
        let origin = |key| opaque_origin(names, keys, store, key);
        digests.push(head_record::reader_digest_with_origins(head, store.tys.view(), alloc, &origin)
            .map_err(|error| HeadDigestError::Record { want, at, error })?);
    }
    Ok(digest_of_heads(digests.len(), digests.iter().copied()))
}

/// A want's digest from its `count` heads' reader digests, in [`want_heads`]'s order: the one
/// formula a `Signature`'s out and a frozen head's meaning (front2/tables/head.rs) share.
pub fn digest_of_heads(count: usize, digests: impl IntoIterator<Item = ContentId>) -> ContentId {
    let mut h = Hasher::new(WANT_DOMAIN);
    h.update(&(count as u64).to_le_bytes());
    for d in digests {
        h.update(&d.0);
    }
    h.finish()
}

/// The owner of a term is independent of the head currently being written. In
/// particular a field's substituted alias and a trait method's return type can
/// belong to cells other than the field's ADT or the method's checker key.
pub fn opaque_origin<'n, N: NamesSource<'n>>(names: N, keys: &Keys, store: &Store<'n>, key: ItemKey) -> Result<OpaqueOrigin, HeadRecordError> {
    let &(owner, index) = store.opaques.get(&key).ok_or(HeadRecordError::MissingOpaqueOrigin { key })?;
    let want = crate::front2::fill::want_of_key(names, keys, owner)
        .map_err(|error| HeadRecordError::OpaqueOwner { key, owner, error })?;
    Ok(OpaqueOrigin { want, owner, index })
}

/// Bind the serialized checker owner to the named cell in this names snapshot.
/// A trait member is keyed by its container, kind and name. Its library item
/// key is a different identity and cannot be substituted for the checker key.
fn register_owner<'n, N: NamesSource<'n>>(names: N, keys: &Keys, key: ItemKey, origin: OpaqueOrigin) -> Result<(), FreezeError> {
    let want = origin.want;
    let lib = want.key();
    let invalid = || FreezeError::Record { want, at: HeadAt::Opaque(key), error: HeadRecordError::InvalidOpaqueOrigin { key, origin } };
    let clash = |error| FreezeError::Head { want, error: HeadError::Key(error) };
    let member = match (want, names.kind_of_item(lib)) {
        (Want::Head(_), Some(ItemKind::TypeAlias)) | (Want::Fn(_), Some(ItemKind::Function | ItemKind::Const | ItemKind::Static)) => None,
        (Want::Members(_), Some(ItemKind::Trait)) | (Want::MemberFn(..), Some(ItemKind::Impl)) => {
            let mut found = None;
            let mut ambiguous = false;
            names.each_member(lib, &mut |m| {
                let Some(name) = m.name else { return };
                let kind = match m.kind {
                    ItemKind::Function => MemberKind::Fn,
                    ItemKind::Const if matches!(want, Want::Members(_)) => MemberKind::Const,
                    _ => return,
                };
                if member_key_of(checker_of(lib), kind, name) == origin.owner {
                    ambiguous |= found.replace((kind, name)).is_some();
                }
            });
            if ambiguous {
                return Err(invalid());
            }
            Some(found.ok_or_else(invalid)?)
        }
        _ => return Err(invalid()),
    };
    keys.item(lib).map_err(clash)?;
    if let Some((kind, name)) = member {
        keys.member(checker_of(lib), kind, name).map_err(clash)?;
    }
    Ok(())
}

/// Read all entries before publishing their opaque references to the consumer.
/// References point at a definition. Only the defining cell supplies predicates.
pub fn import_with_opaque_origins<'n, 'a, N: NamesSource<'n>>(
    names: N, keys: &Keys, store: &Store<'n>, frozen: &FrozenHead<'_>,
    text_of: &mut dyn FnMut(HeadAt) -> Option<&'n str>, alloc: Alloc<'static>, out: Alloc<'a>,
) -> Result<AVec<'a, (HeadAt, Head<'n>)>, FreezeError> {
    let boxed = import_with_opaque_origins_boxed(names, keys, store, frozen, text_of, alloc, out)?;
    let mut heads = AVec::with_capacity_in(boxed.len(), out);
    for (at, head) in boxed {
        heads.push((at, blaze_own::Box::into_inner(head)));
    }
    Ok(heads)
}

/// As [`import_with_opaque_origins`], keeping each decoded [`Head`] boxed in `alloc` so importing
/// a frozen head uses minimal stack space.
pub fn import_with_opaque_origins_boxed<'n, 'a, N: NamesSource<'n>>(
    names: N, keys: &Keys, store: &Store<'n>, frozen: &FrozenHead<'_>,
    text_of: &mut dyn FnMut(HeadAt) -> Option<&'n str>, alloc: Alloc<'static>, out: Alloc<'a>,
) -> Result<AVec<'a, (HeadAt, blaze_own::Box<Head<'n>, Alloc<'static>>)>, FreezeError> {
    let want = frozen.want();
    let mut heads = AVec::new_in(out);
    let mut origins = AVec::new_in(alloc);
    for entry in frozen.heads() {
        let rec = |error| FreezeError::Record { want, at: entry.at, error };
        let text = match entry.at { HeadAt::Fn(_) => text_of(entry.at), _ => None };
        let head = head_record::decode_with_origins_boxed(entry.record, text, &store.tys, alloc, &mut |key, origin| {
            origins.push((key, origin, false));
            Ok(())
        }).map_err(rec)?;
        match (entry.at, &*head) {
            (HeadAt::Opaque(at), Head::Opaque { key, fn_, index, preds, .. }) if at == *key => {
                let origin = OpaqueOrigin { want, owner: *fn_, index: *index };
                origin.check(*key).map_err(rec)?;
                if preds.is_none() {
                    return Err(rec(HeadRecordError::MissingOpaqueRow { key: *key, origin }));
                }
                origins.push((*key, origin, true));
            }
            (HeadAt::Opaque(_), _) | (_, Head::Opaque { .. }) =>
                return Err(rec(HeadRecordError::Shape { what: "the opaque row disagrees with its frozen directory entry" })),
            _ => {}
        }
        heads.push((entry.at, head));
    }
    verify_opaque_origins(names, keys, store, want, &mut origins)?;
    Ok(heads)
}

#[inline(never)]
fn verify_opaque_origins<'n, N: NamesSource<'n>>(
    names: N,
    keys: &Keys,
    store: &Store<'n>,
    want: Want,
    origins: &mut AVec<'_, (ItemKey, OpaqueOrigin, bool)>,
) -> Result<(), FreezeError> {
    origins.sort_unstable_by_key(|&(key, _, _)| key);
    // A damaged producer might omit both the row and every use in its own type.
    // A promise already imported from another cell still binds that producer.
    if !matches!(want, Want::Adt(_) | Want::Impl(_)) {
        let container = checker_of(want.key());
        let missing = store.opaques.iter().filter_map(|(&key, &(owner, index))| {
            if store.preds.contains_key(&key) {
                return None;
            }
            let ours = match want {
                Want::Head(_) | Want::Fn(_) => owner == container,
                Want::MemberFn(_, member) => owner == member,
                Want::Members(_) => matches!(keys.origin(owner), Some(Origin::Member { container: k, .. }) if *k == container),
                Want::Adt(_) | Want::Impl(_) => false,
            };
            (ours && origins.binary_search_by_key(&key, |r| r.0).is_err()).then_some((key, owner, index))
        }).min_by_key(|r| r.0);
        if let Some((key, owner, index)) = missing {
            let origin = OpaqueOrigin { want, owner, index };
            return Err(FreezeError::Record { want, at: HeadAt::Opaque(key), error: HeadRecordError::MissingOpaqueRow { key, origin } });
        }
    }
    for group in origins.chunk_by(|a, b| a.0 == b.0) {
        let (key, origin, _) = group[0];
        let rec = |error| FreezeError::Record { want, at: HeadAt::Opaque(key), error };
        let mut rows = 0;
        for &(_, other, is_row) in group {
            if origin != other {
                return Err(rec(HeadRecordError::ConflictingOpaqueOrigin { key, first: origin, second: other }));
            }
            rows += usize::from(is_row);
        }
        if rows > 1 {
            return Err(rec(HeadRecordError::Shape { what: "two opaque definition rows have the same key" }));
        }
        if (origin.want == want && rows == 0) || (store.made(origin.want) && !store.preds.contains_key(&key)) {
            return Err(rec(HeadRecordError::MissingOpaqueRow { key, origin }));
        }
        if store.opaques.contains_key(&key) {
            let first = opaque_origin(names, keys, store, key).map_err(rec)?;
            if first != origin {
                return Err(rec(HeadRecordError::ConflictingOpaqueOrigin { key, first, second: origin }));
            }
        } else {
            register_owner(names, keys, key, origin)?;
        }
    }
    origins.dedup_by_key(|row| row.0);
    for &(key, origin, _) in origins.iter() {
        if !store.opaques.contains_key(&key) {
            store.opaques.insert(key, (origin.owner, origin.index));
        }
    }
    Ok(())
}

/// The heads `want` makes in `store` (the one list: a `Signature`'s digest above and a frozen
/// head, front2/tables/head.rs), in a fixed order with no map order in it: the item's own first,
/// then the heads made with it, each group sorted by key:
/// - a fn (free or of an impl) or type alias: its owned opaques;
/// - an ADT: its constructors, then its field defaults' anonymous constants (RFC 3681);
/// - a trait's members: its fns and associated constants that have a head or were refused alone
///   (`Store::fn_gaps`: head_record.rs `FnRefused`, its why), then those fns' opaques;
/// - an impl: its associated constants.
///
/// A trait's associated types' item bounds (filed in `Store::preds` under each type's key) are
/// no head of their own kind yet (`HeadAt`), so not listed.
pub fn want_heads<'a>(store: &crate::front2::heads::Store<'_>, want: Want, alloc: Alloc<'a>) -> AVec<'a, HeadAt> {
    let mut out: AVec<'a, HeadAt> = AVec::new_in(alloc);
    // Each opaque made by one of these owners, sorted.
    let opaques_of = |owners: &[ItemKey], out: &mut AVec<'a, HeadAt>| {
        let mut found: AVec<'a, ItemKey> = AVec::new_in(alloc);
        found.extend(store.opaques.iter().filter(|(_, (owner, _))| owners.contains(owner)).map(|(k, _)| *k));
        found.sort_unstable();
        out.extend(found.iter().map(|k| HeadAt::Opaque(*k)));
    };
    match want {
        Want::Fn(lib) => {
            out.push(HeadAt::Fn(checker_of(lib)));
            opaques_of(&[checker_of(lib)], &mut out);
        }
        Want::MemberFn(_, f) => {
            out.push(HeadAt::Fn(f));
            opaques_of(&[f], &mut out);
        }
        Want::Impl(lib) => {
            let ck = checker_of(lib);
            out.push(HeadAt::Impl(ck));
            if let Some(head) = store.impls.get(&ck) {
                let mut consts: AVec<'a, ItemKey> = AVec::new_in(alloc);
                consts.extend(head.consts.iter().map(|(_, k)| *k).filter(|k| store.fns.contains_key(k)));
                consts.sort_unstable();
                out.extend(consts.iter().map(|k| HeadAt::Fn(*k)));
            }
        }
        Want::Adt(lib) => {
            out.push(HeadAt::Adt(lib));
            let adt = checker_of(lib);
            // Each variant's ctor fn (a tuple variant's or tuple struct's), by the row's own
            // variant keys: a struct's is its `MemberKind::Ctor` key, an enum variant's the
            // first 16 bytes of its library key (heads.rs `Store::adt`), which no key origin
            // names, so a search by origin left every enum's ctors out of the record and its
            // seeded readers found no `Ok` (lane FL, the cost test's body `res`).
            let mut ctors: AVec<'a, ItemKey> = AVec::new_in(alloc);
            if let Some(row) = store.adts.get(&adt) {
                ctors.extend(row.variants.iter().map(|v| v.key).filter(|k| store.ctors.contains_key(k)));
            }
            ctors.sort_unstable();
            out.extend(ctors.iter().map(|k| HeadAt::Ctor(*k)));
            let mut defaults: AVec<'a, ItemKey> = AVec::new_in(alloc);
            if let Some(d) = store.field_defaults.get(&adt) {
                defaults.extend(d.iter().flatten().copied().filter(|k| store.fns.contains_key(k)));
            }
            defaults.sort_unstable();
            out.extend(defaults.iter().map(|k| HeadAt::Fn(*k)));
        }
        Want::Members(lib) => {
            out.push(HeadAt::Trait(lib));
            if let Some(head) = store.maps.traits.get(&lib) {
                let mut fns: AVec<'a, ItemKey> = AVec::new_in(alloc);
                let held = |k: &ItemKey| store.fns.contains_key(k) || store.fn_gaps.contains_key(k);
                fns.extend(head.fns.iter().map(|(_, k)| *k).filter(held));
                if let Some(extra) = store.trait_defs.get(&checker_of(lib)) {
                    fns.extend(extra.consts.iter().map(|(_, k)| *k).filter(held));
                }
                fns.sort_unstable();
                fns.dedup();
                out.extend(fns.iter().map(|k| HeadAt::Fn(*k)));
                opaques_of(&fns, &mut out);
            }
        }
        Want::Head(lib) => {
            if store.maps.aliases.contains_key(&lib) {
                out.push(HeadAt::Alias(lib));
                opaques_of(&[checker_of(lib)], &mut out);
            } else if store.maps.trait_aliases.contains_key(&lib) {
                out.push(HeadAt::TraitAlias(lib));
            } else if store.maps.traits.contains_key(&lib) {
                out.push(HeadAt::Trait(lib));
            } else {
                out.push(HeadAt::Adt(lib));
            }
        }
    }
    out
}

/// A want's library key: the item it is of (an impl fn's: its impl's).
pub fn want_lib(want: Want) -> LibKey {
    match want {
        Want::Head(k) | Want::Adt(k) | Want::Impl(k) | Want::Fn(k) | Want::Members(k) | Want::MemberFn(k, _) => k,
    }
}
