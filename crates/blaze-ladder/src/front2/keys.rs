//! The one map between the names library's item keys (`blaze_id::ItemKey`, 32 bytes) and the
//! checker's (`blaze_typeck::ItemKey`, 16 bytes). A checker key is the first 16 bytes of the
//! library's: its crate prefix (bytes 0..8) and 64 bits of its hash. Two library keys under
//! one checker key are refused by name (rule 21), never truncated silently.
//!
//! The library keeps a trait or impl's members in a separate table. A member's
//! checker identity comes from its container, kind and name ([`Keys::member`]).
//! Its source identity is its member-row key ([`Keys::source_member`]).

use std::alloc::Global;

use blaze_hash::HashMap;
use blaze_id::ItemKey as LibKey;
use blaze_ids::ContentId;
use blaze_typeck::ItemKey;

/// What a member of a trait or impl is: what its key is made from beside its name. Ordered (in
/// the order written) so a body that names one sorts (`project::BodyRef`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MemberKind {
    Fn,
    Const,
    Type,
    /// A struct's constructor (a tuple struct's fn, a unit struct's value).
    Ctor,
    /// An anonymous constant a head writes (an array length, a discriminant, a const argument,
    /// a pattern type's range end that is an expression): a member of the MODULE it is written
    /// in, named by its type and its text ([`super::anon`]). It has no associated parent.
    Anon,
}

/// The key of the anonymous constant `name` of `module` ([`super::anon::Ask::name`]): a
/// function of the module's key and the name alone, so the type builder computes it with no
/// [`Keys`] at hand; [`Keys::member`] with [`MemberKind::Anon`] files the same key.
pub fn anon_const_key(module: ItemKey, name: &str) -> ItemKey {
    member_key(module, MemberKind::Anon, name)
}

/// What a checker key stands for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Origin {
    /// An item the library keys (a module-level item, a variant, a macro-made item).
    Item(LibKey),
    /// A member of the trait or impl `container`.
    Member { container: ItemKey, kind: MemberKind, name: Box<str, Global> },
}

/// Two origins under one checker key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyClash {
    pub key: ItemKey,
    pub first: Origin,
    pub second: Origin,
}

impl core::fmt::Display for KeyClash {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "two items share the checker key {:#034x} (rule 21): {:?} and {:?}", self.key.0, self.first, self.second)
    }
}

/// The map from checker keys to what they stand for.
pub struct Keys {
    /// Filed through `&self` ([`super::rows::Rows`]): a head imported during a body's check
    /// files its keys while the check holds the map.
    origins: super::rows::Rows<ItemKey, (Origin, Option<ItemKey>)>,
    /// The kind of each item declared in a body ([`Keys::local`]): the names library keys no
    /// such item and holds no outline entry for it, so what it is is told here, beside its
    /// origin, and never guessed from a missing entry.
    locals: HashMap<ItemKey, super::locals::LocalKind, Global>,
}

/// The checker key of a library key: its first 16 bytes.
pub fn checker_of(key: LibKey) -> ItemKey {
    let b = key.0.0;
    let mut first = [0u8; 16];
    first.copy_from_slice(&b[..16]);
    ItemKey(u128::from_be_bytes(first))
}

impl Keys {
    pub fn new() -> Keys {
        Keys { origins: super::rows::Rows::new_in(Global), locals: HashMap::new_in(Global) }
    }

    /// Files `key` and returns its checker key; a different library key already filed under
    /// that checker key is a [`KeyClash`].
    pub fn item(&self, key: LibKey) -> Result<ItemKey, KeyClash> {
        let k = checker_of(key);
        self.file(k, Origin::Item(key))
    }

    /// The key of member `name` of kind `kind` of `container`, filed.
    pub fn member(&self, container: ItemKey, kind: MemberKind, name: &str) -> Result<ItemKey, KeyClash> {
        let k = member_key(container, kind, name);
        self.file(k, Origin::Member { container, kind, name: Box::from(name) })
    }

    /// The member `name` of `container` if it was filed (its container's head was made).
    pub fn filed_member(&self, container: ItemKey, kind: MemberKind, name: &str) -> Option<ItemKey> {
        let k = member_key(container, kind, name);
        self.origins.contains_key(&k).then_some(k)
    }

    /// Files the item `key` declared in a body (front2 locals.rs `Locals::keep`; its key is
    /// derived by nested_exports.rs `local_key`) with what kind of item it is, and returns its
    /// checker key. Its origin is [`Origin::Item`] like a module item's; its kind is read by
    /// [`Keys::local_kind`] (front2 by_origin.rs `kind_of_key`), since the names library holds
    /// no outline entry for it. The kind is a part of the key's derivation (its tag), so one
    /// key is never filed with two kinds.
    pub fn local(&mut self, key: LibKey, kind: super::locals::LocalKind) -> Result<ItemKey, KeyClash> {
        let k = self.item(key)?;
        self.locals.insert(k, kind);
        Ok(k)
    }

    /// The kind of the body-local item `key` if [`Keys::local`] filed it; `None` for every
    /// other key.
    pub fn local_kind(&self, key: ItemKey) -> Option<super::locals::LocalKind> {
        self.locals.get(&key).copied()
    }
}

/// The key [`Keys::member`] files member `name` of kind `kind` of `container` under, computed
/// without filing it: what a reader that holds the member's library row (its container and its
/// name) asks the key map by, before the container's head is made (the lazy program's search,
/// front2/program/lazy/locate.rs; a trait fn's lang item, lazy/front_end.rs `lang_item`).
pub fn member_key_of(container: ItemKey, kind: MemberKind, name: &str) -> ItemKey {
    member_key(container, kind, name)
}

/// The checker key library item `lib` is asked by: a member of a trait or an impl the library
/// keys (a `#[lang]` member: `Future::poll`, `IntoFuture::into_future`, `Coroutine::resume`,
/// `Future::Output`) is filed by its container's head under its MEMBER key ([`Keys::member`],
/// heads.rs), never under its library key's first 16 bytes, so it is asked by that; any other
/// item by [`checker_of`]. A member whose kind no head keys is asked by [`checker_of`] (and
/// found under no head, refused where it is read).
pub fn asked_key<'n, S: super::source::NamesSource<'n>>(names: S, lib: LibKey) -> ItemKey {
    let Some((owner, name)) = names.member_owner(lib) else { return checker_of(lib) };
    let mut kind = None;
    names.each_member(owner, &mut |m| {
        if m.key == lib {
            kind = match m.kind {
                blaze_surface::ItemKind::Function => Some(MemberKind::Fn),
                blaze_surface::ItemKind::Const => Some(MemberKind::Const),
                blaze_surface::ItemKind::TypeAlias => Some(MemberKind::Type),
                _ => None,
            };
        }
    });
    match kind {
        Some(kind) => member_key(checker_of(owner), kind, name),
        None => checker_of(lib),
    }
}

/// A member's key: a function of its container, kind and name alone.
fn member_key(container: ItemKey, kind: MemberKind, name: &str) -> ItemKey {
    // The bytes `ContentId::of` hashed from one buffer, fed in three updates: BLAKE3's output is
    // a function of the bytes fed, not of how they are split, so the key is unchanged.
    let id = blaze_ids::Hasher::new("blaze-ladder front2 member key 1").update(&container.0.to_be_bytes()).update(&[kind as u8]).update(name.as_bytes()).finish();
    // The container's crate prefix, then 64 bits of the member's hash.
    let mut first = [0u8; 16];
    first[..8].copy_from_slice(&container.0.to_be_bytes()[..8]);
    first[8..].copy_from_slice(&id.0[..8]);
    ItemKey(u128::from_be_bytes(first))
}

/// The key of the opaque type of the `index`th `impl Trait` in fn `fn_`'s return type (the
/// order of [`super::types::return_impls`]; an `async fn`'s own `impl Future` is index 0, see
/// `heads.rs` `fn_head`): a function of the fn's key and the index alone, so it does not
/// depend on the order heads are built in. The fn's crate prefix is kept.
pub fn opaque_key(fn_: ItemKey, index: u32) -> ItemKey {
    let mut bytes = [0u8; 20];
    bytes[..16].copy_from_slice(&fn_.0.to_be_bytes());
    bytes[16..].copy_from_slice(&index.to_be_bytes());
    let id = ContentId::of("blaze-ladder front2 opaque key 1", &bytes);
    let mut first = [0u8; 16];
    first[..8].copy_from_slice(&fn_.0.to_be_bytes()[..8]);
    first[8..].copy_from_slice(&id.0[..8]);
    ItemKey(u128::from_be_bytes(first))
}

/// The opaque type an owner row `(owner, index)` names (the inverse of the seam's
/// `opaque_of`): [`opaque_key`] of `owner` at `index` without tait.rs's `ALIAS_OPAQUE` bit,
/// which an alias's row sets over the same numbering (tait.rs `alias_opaque_preds`). A code
/// entry's read list names an opaque's question by its row (code_reads.rs
/// `ReadKind::OpaqueHidden`).
pub fn opaque_of_row(owner: ItemKey, index: u32) -> ItemKey {
    opaque_key(owner, index & !super::tait::ALIAS_OPAQUE)
}

impl Keys {
    fn file(&self, k: ItemKey, origin: Origin) -> Result<ItemKey, KeyClash> {
        self.file_with_body(k, origin, None)
    }

    /// A written local declaration has a lexical source identity even when body
    /// expansion contributes other declarations to the checker's scope. Keep its
    /// live body for warm placement. The records reader derives the lexical key
    /// from today's declarations without expanding or checking any body.
    pub(super) fn file_source_local(&self, local: LibKey, live: ItemKey) -> Result<ItemKey, KeyClash> {
        self.file_with_body(checker_of(local), Origin::Item(local), Some(live))
    }

    pub(super) fn source_local_body(&self, key: ItemKey) -> Option<ItemKey> {
        self.origins.get(&key)?.1
    }

    fn file_with_body(&self, k: ItemKey, origin: Origin, live: Option<ItemKey>) -> Result<ItemKey, KeyClash> {
        match self.origins.get(&k) {
            Some((first, body)) if *first == origin && *body == live => Ok(k),
            Some((first, _)) => Err(KeyClash { key: k, first: first.clone(), second: origin }),
            None => {
                self.origins.insert(k, (origin, live));
                Ok(k)
            }
        }
    }

    /// What `key` stands for; `None` for a key this map never made.
    pub fn origin(&self, key: ItemKey) -> Option<&Origin> {
        self.origins.get(&key).map(|(origin, _)| origin)
    }

    /// The library key of an item key; `None` for a member or a key that is no item's. The ONE
    /// way to ask it (a map-only form was removed 2026-11-17: over records it answered `None`
    /// for every item whose head no body had made yet, and the lazy program refused
    /// `core::error::Error`'s vtable for it). A key this map holds
    /// no row for is asked of the source ([`super::source::NamesSource::key_beginning`]). Over
    /// records the map is not filled when the front end is made (`Front2::from_records` files
    /// nothing), so an item's, a module's or a variant's library key is found from the checker
    /// key itself, by one search in its crate's records; a member's key (filed here when its
    /// container's head is made) is in no record's table and is `None`, as over the solved
    /// names. Two library keys under one checker key are refused where the source finds them
    /// (`source::SourceError::KeysBeginAlike`).
    pub fn library_in<'n, S: super::source::NamesSource<'n>>(&self, names: S, key: ItemKey) -> Option<LibKey> {
        match self.origin(key) {
            Some(Origin::Item(k)) => Some(*k),
            Some(Origin::Member { .. }) => None,
            None => names.key_beginning(key.0.to_be_bytes()),
        }
    }

    /// The member row whose text was actually lowered. The checker keys a method by
    /// container, kind and name. Its library row supplies the stable identity of its
    /// source, including for a method produced by a macro in the container.
    pub(super) fn source_member<'n, S: super::source::NamesSource<'n>>(
        &self, names: S, key: ItemKey, text: &str,
    ) -> Option<(LibKey, super::source::MemberOf<'n>)> {
        let Origin::Member { container, kind, name } = self.origin(key)? else { return None };
        let kind = match kind {
            MemberKind::Fn => blaze_surface::ItemKind::Function,
            MemberKind::Const => blaze_surface::ItemKind::Const,
            _ => return None,
        };
        let owner = self.library_in(names, *container)?;
        let mut first = None;
        let mut exact = None;
        let mut count = 0;
        let mut matches = 0;
        names.each_member(owner, &mut |m| {
            if m.kind == kind && m.name == Some(name.as_ref()) {
                names.read_member_body(m.key, m.written_by);
                first.get_or_insert(m);
                count += 1;
                if m.text == text {
                    exact = Some(m);
                    matches += 1;
                }
            }
        });
        match (count, matches) {
            (1, _) => Some((owner, first?)),
            (_, 1) => Some((owner, exact?)),
            _ => None,
        }
    }

    pub fn len(&self) -> usize {
        self.origins.len()
    }
}

impl Default for Keys {
    fn default() -> Keys {
        Keys::new()
    }
}
