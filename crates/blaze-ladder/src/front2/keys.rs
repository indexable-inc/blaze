//! The one map between the names library's item keys (`blaze_id::ItemKey`, 32 bytes) and the
//! checker's (`blaze_typeck::ItemKey`, 16 bytes). A checker key is the first 16 bytes of the
//! library's: its crate prefix (bytes 0..8) and 64 bits of its hash. Two library keys under
//! one checker key are refused by name (rule 21), never truncated silently.
//!
//! The library keys module-level items, variants and macro-made items; it keys no member of a
//! trait or an impl. A member's checker key is made here from its container's key, its kind
//! and its name ([`Keys::member`]), the container's crate prefix kept, and filed in the same
//! map, so one table answers "what is this checker key".

use std::alloc::Global;

use blaze_hash::HashMap;
use blaze_id::ItemKey as LibKey;
use blaze_ids::ContentId;
use blaze_typeck::ItemKey;

/// What a member of a trait or impl is: what its key is made from beside its name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
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
    origins: HashMap<ItemKey, Origin, Global>,
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
        Keys { origins: HashMap::new_in(Global), locals: HashMap::new_in(Global) }
    }

    /// Files `key` and returns its checker key; a different library key already filed under
    /// that checker key is a [`KeyClash`].
    pub fn item(&mut self, key: LibKey) -> Result<ItemKey, KeyClash> {
        let k = checker_of(key);
        self.file(k, Origin::Item(key))
    }

    /// The key of member `name` of kind `kind` of `container`, filed.
    pub fn member(&mut self, container: ItemKey, kind: MemberKind, name: &str) -> Result<ItemKey, KeyClash> {
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
    let mut bytes = std::vec::Vec::with_capacity(17 + name.len());
    bytes.extend_from_slice(&container.0.to_be_bytes());
    bytes.push(kind as u8);
    bytes.extend_from_slice(name.as_bytes());
    let id = ContentId::of("blaze-ladder front2 member key 1", &bytes);
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

impl Keys {
    fn file(&mut self, k: ItemKey, origin: Origin) -> Result<ItemKey, KeyClash> {
        match self.origins.get(&k) {
            Some(first) if *first == origin => Ok(k),
            Some(first) => Err(KeyClash { key: k, first: first.clone(), second: origin }),
            None => {
                self.origins.insert(k, origin);
                Ok(k)
            }
        }
    }

    /// What `key` stands for; `None` for a key this map never made.
    pub fn origin(&self, key: ItemKey) -> Option<&Origin> {
        self.origins.get(&key)
    }

    /// The library key of an item key; `None` for a member or an unknown key.
    pub fn library(&self, key: ItemKey) -> Option<LibKey> {
        match self.origins.get(&key)? {
            Origin::Item(k) => Some(*k),
            Origin::Member { .. } => None,
        }
    }

    /// [`Keys::library`] for code that reads its names from a source `S`: a key this map holds
    /// no row for is asked of the source ([`super::source::NamesSource::key_beginning`]). Over
    /// records the map is not filled when the front end is made (`Front2::from_records` files
    /// nothing), so an item's, a module's or a variant's library key is found from the checker
    /// key itself, by one search in its crate's records; a member's key (filed here when its
    /// container's head is made) is in no record's table and is `None`, as over the solved
    /// names. Two library keys under one checker key are refused where the source finds them
    /// (`source::SourceError::KeysBeginAlike`).
    pub fn library_in<'n, S: super::source::NamesSource<'n>>(&self, names: S, key: ItemKey) -> Option<LibKey> {
        match self.origins.get(&key) {
            Some(Origin::Item(k)) => Some(*k),
            Some(Origin::Member { .. }) => None,
            None => names.key_beginning(key.0.to_be_bytes()),
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
