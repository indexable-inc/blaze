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
use std::boxed::Box;

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
        Keys { origins: HashMap::new_in(Global) }
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

    pub fn len(&self) -> usize {
        self.origins.len()
    }
}

impl Default for Keys {
    fn default() -> Keys {
        Keys::new()
    }
}
