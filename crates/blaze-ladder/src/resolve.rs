//! What a written path names, read through the names library (blaze-scope's `names::path`):
//! the port of the front end onto real names (README, "For a successor lane (03:20)", step 2).
//! Nothing here is a lookup rule (rule 30): the library walks the scopes; this file only turns
//! its coarse [`Def`] into the finer kind the front end lowers by, read from the item's outline
//! entry, and finds a variant's enum and index.
//!
//! A written path's leading `crate`, `self`, `super` and `::` are read by the library's
//! `to_path`, so a path means here what it means to the library.

use std::alloc::Global;
use std::vec::Vec;

use blaze_id::{CrateItemKey, ItemKey};
use blaze_own::Alloc;
use blaze_scope::names::{self, PathError, Why, Written};
use blaze_scope::resolve::{Names, Place};
use blaze_scope::task::to_path;
use blaze_surface::{Def, DefKind, ItemDetail, Namespace};

/// The namespace a path's last segment is looked up in first; the other is tried next, as a
/// struct named in an expression (`S { .. }`) is a type and a unit struct is also a value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ns {
    Type,
    Value,
}

/// What an item is, as the front end lowers it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Module,
    Fn,
    Const,
    Static,
    Struct,
    Enum,
    Union,
    Trait,
    /// `trait A = B + C;` (the unstable `trait_alias`): it names no trait of its own; a bound
    /// that names it stands for its right-hand side (front2's `Heads::trait_alias`).
    TraitAlias,
    TypeAlias,
    /// An enum's variant: the enum and the variant's index in it.
    Variant { adt: ItemKey, index: u32 },
    Macro,
    /// A builtin type (`bool`, `u8`, `str`): its name, as the names library files it
    /// (`blaze_scope::BUILTIN_TYPES`).
    Builtin(&'static str),
    /// blaze's arbitrary-width integer (`i80`, `u24`, under `#![blaze::arbitrary_ints]`), as
    /// the names library files it (`blaze_scope::Builtin::Int`).
    WideInt { signed: bool, bits: u8 },
    /// An outline entry of a kind a path does not name as a value or a type (an extern crate
    /// names its root module, so it is [`Kind::Module`]).
    Other(ItemDetail),
}

/// A path's longest named prefix: its item, what it is, and how many segments it took (the
/// rest are the checker's: `Type::assoc`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Target {
    pub key: ItemKey,
    pub kind: Kind,
    pub consumed: u32,
}

/// Why a path names nothing the front end can lower.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResolveError {
    /// The library's answer: the segment that failed and why.
    Path(PathError),
    /// The library named an item no outline holds and no enum has as a variant: a gap between
    /// the library's records and its texts (never the program's error).
    NoText { key: ItemKey },
}

impl core::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ResolveError::Path(e) => write!(f, "{e}"),
            ResolveError::NoText { key } => write!(f, "the names library named the item {key:x}, which has no text and is no variant"),
        }
    }
}

/// The front end's view of every crate's resolved names.
#[derive(Clone, Copy)]
pub struct Resolver<'n> {
    pub names: &'n Names<'n>,
}

impl<'n> Resolver<'n> {
    /// `segments` written in `module` (with a leading `::` when `abs`), its last segment looked
    /// up in `ns` and then in the other namespace.
    pub fn path(&self, module: ItemKey, abs: bool, segments: &[&str], ns: Ns) -> Result<Target, ResolveError> {
        let krate = self.krate_key(module)?;
        let written = to_path(abs, segments, krate, Alloc::global());
        let w = Written { kind: written.kind, segments: &written.segments };
        let first = match ns {
            Ns::Type => Namespace::Types,
            Ns::Value => Namespace::Values,
        };
        let other = match ns {
            Ns::Type => Namespace::Values,
            Ns::Value => Namespace::Types,
        };
        let found = match names::path(self.names, module, first, w, Alloc::global()) {
            Err(PathError { at, why: Why::NotFound, .. }) if at + 1 == w.segments.len() => {
                names::path(self.names, module, other, w, Alloc::global())
            }
            // `E::V` asked as a value that stops at the enum `E`: a struct-like variant is in
            // the type namespace only (the Reference, "Namespaces"; "Struct expressions": its
            // path names the variant), so the whole path is asked there; a value answer that
            // is not a variant stays as it was (`E::assoc_fn` is type-relative).
            Ok(f) if ns == Ns::Value && f.def.kind == DefKind::Enum && f.consumed + 1 == w.segments.len() => {
                match names::path(self.names, module, Namespace::Types, w, Alloc::global()) {
                    Ok(t) if t.consumed == w.segments.len() && self.names.place(t.def.key).is_none() => Ok(t),
                    _ => Ok(f),
                }
            }
            r => r,
        }
        .map_err(ResolveError::Path)?;
        // The segments `to_path` read as the path's head (`crate`, `self`, `super`) count as
        // consumed in the path as written.
        let head = (segments.len() - w.segments.len()) as u32;
        let consumed = found.consumed as u32 + head;
        // A variant is no item of a module: its enum is the prefix one segment shorter.
        if self.names.place(found.def.key).is_none() && found.def.kind != DefKind::Module && found.consumed >= 2 {
            let prefix = Written { kind: w.kind, segments: &w.segments[..found.consumed - 1] };
            if let Ok(e) = names::path(self.names, module, Namespace::Types, prefix, Alloc::global()) {
                if e.def.kind == DefKind::Enum {
                    if let Some(index) = self.variant_index(e.def.key, found.def.key) {
                        return Ok(Target { key: found.def.key, kind: Kind::Variant { adt: e.def.key, index }, consumed });
                    }
                }
            }
        }
        Ok(Target { key: found.def.key, kind: self.kind_of(found.def)?, consumed })
    }

    /// The finer kind of a definition the library returned.
    pub fn kind_of(&self, def: Def) -> Result<Kind, ResolveError> {
        match def.kind {
            DefKind::Module => return Ok(Kind::Module),
            DefKind::Enum => return Ok(Kind::Enum),
            DefKind::Trait => return Ok(Kind::Trait),
            DefKind::Macro(_) => return Ok(Kind::Macro),
            DefKind::Other => {}
        }
        if blaze_scope::is_builtin_type(def.key) {
            return match blaze_scope::builtin_of(def.key) {
                Some(blaze_scope::Builtin::Named(n)) => Ok(Kind::Builtin(n)),
                Some(blaze_scope::Builtin::Int { signed, bits }) => Ok(Kind::WideInt { signed, bits }),
                None => Err(ResolveError::NoText { key: def.key }),
            };
        }
        let Some(detail) = self.detail(def.key) else {
            // A variant reached with no enum before it (`Ok` through the prelude's `use`): its
            // enum is the row of its crate's `enums` table that holds it.
            return match self.variant_of(def.key) {
                Some((adt, index)) => Ok(Kind::Variant { adt, index }),
                None => Err(ResolveError::NoText { key: def.key }),
            };
        };
        Ok(match detail {
            ItemDetail::Fn { .. } => Kind::Fn,
            ItemDetail::Const { .. } => Kind::Const,
            ItemDetail::Static { .. } => Kind::Static,
            ItemDetail::Struct => Kind::Struct,
            ItemDetail::Enum => Kind::Enum,
            ItemDetail::Union => Kind::Union,
            ItemDetail::Trait => Kind::Trait,
            ItemDetail::TraitAlias => Kind::TraitAlias,
            ItemDetail::TypeAlias => Kind::TypeAlias,
            ItemDetail::Mod { .. } | ItemDetail::ExternCrate { .. } => Kind::Module,
            ItemDetail::MacroRules { .. } | ItemDetail::MacroDef => Kind::Macro,
            other => Kind::Other(other),
        })
    }

    /// The outline entry of an item with text.
    pub fn detail(&self, key: ItemKey) -> Option<ItemDetail> {
        let Place { krate, at, item } = self.names.place(key)?;
        let (_, outline) = self.names.outline_at(krate, at)?;
        Some(outline.item(item).detail)
    }

    /// The enum `variant` is in and its index there, from its crate's `enums` table.
    pub fn variant_of(&self, variant: ItemKey) -> Option<(ItemKey, u32)> {
        let c = self.names.crate_of(variant)?;
        self.names.krate(c).tables.enums.iter().find_map(|(e, vs)| vs.iter().position(|v| v.key == variant).map(|i| (*e, i as u32)))
    }

    /// The trait or impl `member` (a member's library key) is of, and its name, from its
    /// crate's `members` table.
    pub fn member_of(&self, member: ItemKey) -> Option<(ItemKey, &'n str)> {
        let c = self.names.crate_of(member)?;
        self.names.krate(c).tables.members.iter().find_map(|(owner, ms)| ms.iter().find(|m| m.key == member).and_then(|m| m.name.map(|n| (*owner, n))))
    }

    /// The index of `variant` among `adt`'s variants.
    pub fn variant_index(&self, adt: ItemKey, variant: ItemKey) -> Option<u32> {
        let c = self.names.crate_of(adt)?;
        let vs = self.names.krate(c).tables.enums.get(&adt)?;
        vs.iter().position(|v| v.key == variant).map(|i| i as u32)
    }

    /// The traits method resolution may use in `module`, sorted.
    pub fn traits_in_scope(&self, module: ItemKey) -> Result<Vec<ItemKey, Global>, ResolveError> {
        let found = names::traits_in_scope(self.names, module, Alloc::global()).map_err(ResolveError::Path)?;
        let mut out = Vec::with_capacity_in(found.len(), Global);
        out.extend_from_slice(&found);
        Ok(out)
    }

    /// The crate key of the crate `module` is in; a module of no crate the names hold is the
    /// library's `NotRead`.
    /// The definition the macro path `segments` written in `module` names, as the names
    /// library holds it (`None`: a definition with no source record).
    pub fn macro_body(&self, module: ItemKey, abs: bool, segments: &[&str]) -> Result<Option<blaze_scope::task::MacroBody<'n>>, ResolveError> {
        let krate = self.krate_key(module)?;
        let written = to_path(abs, segments, krate, Alloc::global());
        let w = Written { kind: written.kind, segments: &written.segments };
        let found = self.names.item_at(module, Namespace::Macros, w, Alloc::global()).map_err(ResolveError::Path)?;
        Ok(found.macro_src.map(|s| s.body))
    }

    fn krate_key(&self, module: ItemKey) -> Result<CrateItemKey, ResolveError> {
        match self.names.crate_of(module) {
            Some(c) => Ok(self.names.krate(c).key),
            None => Err(ResolveError::Path(PathError { module, at: 0, why: Why::NotRead { module } })),
        }
    }
}
