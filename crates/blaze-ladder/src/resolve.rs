//! What a written path names, read through the names library (blaze-scope's `names::path`):
//! the port of the front end onto real names (README, "For a successor lane (03:20)", step 2).
//! Nothing here is a lookup rule (rule 30): the library walks the scopes; this file only turns
//! its coarse [`Def`] into the finer kind the front end lowers by, read from the item's outline
//! entry, and finds a variant's enum and index.
//!
//! A written path's leading `crate`, `self`, `super` and `::` are read by the library's
//! `to_path`, so a path means here what it means to the library.


use blaze_id::{CrateItemKey, ItemKey};
use blaze_own::Alloc;
use blaze_scope::names::{PathError, Unheld, Why, Written};
use blaze_scope::resolve::Names;
use blaze_scope::task::{to_path, to_path_head};
use blaze_surface::{Def, DefKind, ItemDetail, ItemKind, Namespace};

use crate::front2::source::NamesSource;

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

/// The front end's view of the names of every crate, over any source of them
/// ([`NamesSource`]: the solved names, or each crate's records): a written path's meaning as
/// the front end lowers by it. `Paths { names }` over a `&Names` is [`Resolver`].
#[derive(Clone, Copy)]
pub struct Paths<S> {
    pub names: S,
}

/// The front end's view of every crate's resolved names: [`Paths`] over the solved names (the
/// name every function not yet switched to a source says).
pub type Resolver<'n> = Paths<&'n Names<'n>>;

impl<'n, S: NamesSource<'n>> Paths<S> {
    /// `segments` written in `module` (with a leading `::` when `abs`), its last segment looked
    /// up in `ns` and then in the other namespace.
    pub fn path(&self, module: ItemKey, abs: bool, segments: &[&str], ns: Ns) -> Result<Target, ResolveError> {
        self.path_with(module, None, abs, segments, ns)
    }

    /// [`Self::path`], a first segment `$crate` (or the `crate` an item text writes for it)
    /// naming `dollar` (the crate of its token's
    /// mark: blaze-lower `mark_site::site_of`'s `dollar_crate`); `None`: the crate `module`
    /// is in, the only answer a path of no expansion has.
    pub fn path_with(&self, module: ItemKey, dollar: Option<CrateItemKey>, abs: bool, segments: &[&str], ns: Ns) -> Result<Target, ResolveError> {
        let krate = match dollar {
            Some(k) => k,
            None => self.krate_key(module)?,
        };
        // The head read in place: the written segments after it are borrowed, never copied (a
        // copy per path in the process's heap before, 2026-11-18).
        let (mut kind, skip) = to_path_head(abs, segments, None, krate);
        // token-mark-2: an item text writes `crate` for another crate's `$crate`; its mark's crate
        // is `dollar` (blaze-lower `site_of` via blaze-scope `crate_roots::root_by_mark`).
        if let (Some(k), false, Some(&"crate")) = (dollar, abs, segments.first()) {
            kind = blaze_surface::PathKind::DollarCrate(k);
        }
        let w = Written { kind, segments: &segments[skip..] };
        // The segments `to_path` read as the path's head (`crate`, `self`, `super`): counted in
        // the path as WRITTEN, in a refusal's segment as in `consumed` below, so `at` indexes
        // `segments`. (The library's `at` indexes what follows the head: `crate::a::f` with no
        // `f` is its segment 1, the written path's 2; read as the written one, the prefix before
        // it was `crate` alone, a path of no segments, and an E0425 became a refusal `Empty`.)
        let head = (segments.len() - w.segments.len()) as u32;
        let first = match ns {
            Ns::Type => Namespace::Types,
            Ns::Value => Namespace::Values,
        };
        let other = match ns {
            Ns::Type => Namespace::Values,
            Ns::Value => Namespace::Types,
        };
        let found = match self.names.walk(module, first, w) {
            Err(PathError { at, why: Why::NotFound, .. }) if at + 1 == w.segments.len() => {
                self.names.walk(module, other, w)
            }
            // `E::V` asked as a value that stops at the enum `E`: a struct-like variant is in
            // the type namespace only (the Reference, "Namespaces"; "Struct expressions": its
            // path names the variant), so the whole path is asked there; a value answer that
            // is not a variant stays as it was (`E::assoc_fn` is type-relative).
            Ok(f) if ns == Ns::Value && f.def.kind == DefKind::Enum && f.consumed + 1 == w.segments.len() => {
                match self.names.walk(module, Namespace::Types, w) {
                    Ok(t) if t.consumed == w.segments.len() && !self.names.has_text(t.def.key) => Ok(t),
                    _ => Ok(f),
                }
            }
            r => r,
        }
        .map_err(|e| ResolveError::Path(PathError { at: e.at + head as usize, ..e }))?;
        let consumed = found.consumed as u32 + head;
        // A variant is no item of a module: its enum is the prefix one segment shorter.
        if !self.names.has_text(found.def.key) && found.def.kind != DefKind::Module && found.consumed >= 2 {
            let prefix = Written { kind: w.kind, segments: &w.segments[..found.consumed - 1] };
            if let Ok(e) = self.names.walk(module, Namespace::Types, prefix) {
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
        // The entry's kind decides (asked without its detail's data, which a crate's records
        // do not hold for every kind); only a kind a path does not name carries its detail.
        let Some(kind) = self.names.kind_of_item(def.key) else {
            // A variant reached with no enum before it (`Ok` through the prelude's `use`): its
            // enum is the row of its crate's `enums` table that holds it.
            return match self.variant_of(def.key) {
                Some((adt, index)) => Ok(Kind::Variant { adt, index }),
                None => Err(ResolveError::NoText { key: def.key }),
            };
        };
        Ok(match kind {
            ItemKind::Function => Kind::Fn,
            ItemKind::Const => Kind::Const,
            ItemKind::Static => Kind::Static,
            ItemKind::Struct => Kind::Struct,
            ItemKind::Enum => Kind::Enum,
            ItemKind::Union => Kind::Union,
            ItemKind::Trait => Kind::Trait,
            ItemKind::TraitAlias => Kind::TraitAlias,
            ItemKind::TypeAlias => Kind::TypeAlias,
            ItemKind::Module | ItemKind::ExternCrate => Kind::Module,
            ItemKind::MacroRules | ItemKind::Macro2 => Kind::Macro,
            _ => Kind::Other(self.detail(def.key).ok_or(ResolveError::NoText { key: def.key })?),
        })
    }

    /// The outline entry of an item with text.
    pub fn detail(&self, key: ItemKey) -> Option<ItemDetail> {
        self.names.detail_of(key)
    }

    /// The enum `variant` is in and its index there, from its crate's `enums` table.
    pub fn variant_of(&self, variant: ItemKey) -> Option<(ItemKey, u32)> {
        self.names.variant_of(variant)
    }

    /// The trait or impl `member` (a member's library key) is of, and its name, from its
    /// crate's `members` table.
    pub fn member_of(&self, member: ItemKey) -> Option<(ItemKey, &'n str)> {
        self.names.member_owner(member)
    }

    /// The index of `variant` among `adt`'s variants.
    pub fn variant_index(&self, adt: ItemKey, variant: ItemKey) -> Option<u32> {
        let vs = self.names.variants(adt)?;
        vs.iter().position(|v| v.key == variant).map(|i| i as u32)
    }

    /// The traits method resolution may use in `module`, sorted.
    /// They are held in `alloc`, the caller's.
    pub fn traits_in_scope<'b>(&self, module: ItemKey, alloc: Alloc<'b>) -> Result<blaze_own::AVec<'b, ItemKey>, ResolveError> {
        self.names.traits_in_scope(module, alloc).map_err(ResolveError::Path)
    }

    /// The definition the macro path `segments` written in `module` names, as the names
    /// library holds it (`None`: a definition with no source record).
    /// `alloc` is the caller's scratch for the path as written.
    pub fn macro_body(&self, module: ItemKey, abs: bool, segments: &[&str], alloc: Alloc<'_>) -> Result<Option<blaze_scope::task::MacroBody<'n>>, ResolveError> {
        let krate = self.krate_key(module)?;
        let written = to_path(abs, segments, krate, alloc);
        let w = Written { kind: written.kind, segments: &written.segments };
        self.names.macro_body_of(module, w).map_err(ResolveError::Path)
    }

    /// The crate key of the crate `module` is in; a module of no crate the names hold is the
    /// library's `NotRead`.
    fn krate_key(&self, module: ItemKey) -> Result<CrateItemKey, ResolveError> {
        let not_read = ResolveError::Path(PathError { module, at: 0, why: Why::NotRead { module, layer: Unheld::Step } });
        match self.names.crate_of(module) {
            Some(c) => self.names.crate_key(c).ok_or(not_read),
            None => Err(not_read),
        }
    }
}
