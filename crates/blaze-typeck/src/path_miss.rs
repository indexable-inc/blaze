//! Why a written path names nothing: the world's answer to [`crate::World::resolve_path`] when
//! the path does not resolve, carried into E0425 as [`crate::Because::Path`].
//!
//! Rule 19 (errors name the thing): before this answer the world said only `None`, and the
//! checker's E0425 named the path's LAST segment at the whole path's range, so `Rc::new`
//! failing read the same whether `Rc` was not in scope or `new` was not on it. The answer now
//! says which written segment failed, why, and what the segments before it named.
//!
//! The Reference, "Paths", "Path qualifiers" and "Paths in expressions": a path is resolved
//! segment by segment, each segment in the namespace the one before it names; the first one
//! in the scope of the body. So a failure is always at one segment, after a resolved prefix.

use crate::hash::{ContentHash, StableHasher};
use crate::ids::ItemKey;
use crate::world::{Def, Holed, PathRes};

/// The world's answer to a written path: what it names, or where and why it names nothing.
pub type PathAnswer = Result<PathRes, PathMiss>;

/// Where a written path stopped naming anything.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PathMiss {
    /// The written segment at index `segment` (0: the first, as written in the body, before
    /// any `use` or glob the world reads it through) named nothing, for `why`; `after` is what
    /// the segments before it named (`None` when `segment` is the first looked up: in the
    /// body's scope, or at the crate's root after `::`; and for `MissWhy::NotAValue`, which
    /// carries the item the whole path named).
    At { segment: u32, after: Option<PathPrefix>, why: MissWhy },
    /// A world that answers whole paths only (the old front end's and the test worlds' that
    /// do not walk segments) said the path names nothing and not which step failed. front2's
    /// world never answers this; it is also the stand-in a missing read takes (a run with a
    /// hole is never an answer, so it reaches no finding).
    Whole,
}

/// What a path's resolved prefix named.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PathPrefix {
    /// A module (a crate's root included): the failing segment is not among its items.
    Module(ItemKey),
    /// A definition the checker knows (a type, a trait, a fn): the failing segment is not
    /// an associated item or variant of it the world or the checker found.
    Def(Def),
    /// An item of another kind the world names by key (an alias, an enum's variant, a fn):
    /// nothing follows it.
    Item(ItemKey),
    /// A module of bstd, reached through a written first segment `std` that names bstd: the
    /// failing segment is not among its items, and the finding says what `std` named. Under a
    /// blaze edition `std` is bstd and Rust's `std` has no name (blaze-unit-graph implicit.rs:
    /// "bstd; Rust's `std` none"); `edition` is that edition's manifest name (`blaze-2026.11`).
    Bstd { module: ItemKey, edition: &'static str },
}

/// Why the segment named nothing: each a FACT about the program, so each is an error. The
/// names library's own reasons, without their payloads (the payloads are the library's keys,
/// not the program's), plus the checker's. A miss the world does not KNOW (a scope it could not
/// read whole: a crate not read, a refused macro call that may have bound the name, a glob
/// lookup past its bound) is no answer at all: the world's gap, and the body is not checked
/// (blaze-ladder front2 world_path_miss.rs).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MissWhy {
    /// No item, value or variant of that name where the segment was looked up, every scope it
    /// could be in read whole.
    NotFound,
    /// An item of that name exists there but is not visible from the body.
    Private,
    /// Two glob imports give the name and neither wins.
    Ambiguous,
    /// The prefix names an item with no members (a fn, a const), so nothing follows it.
    NotAModule,
    /// `super` past the crate's root.
    SuperPastRoot,
    /// The whole path names an item that is no value (a module, a macro, a trait alias): the
    /// item named.
    NotAValue(ItemKey),
    /// The segment is a `use` binding, or reached through a glob, whose own path resolves
    /// only part way: the binding names no item.
    ThroughUse,
    /// Not found in a module one of whose macro calls was refused: the name may be one the
    /// call would have made. Not the program's error but the refusal's consequence: blaze-cli
    /// body_types.rs files the body as not checked under the refused call (`MACRO_REFUSED`).
    /// `call` is the library's key of that call, which the world's words join to the call's
    /// path and reason; `why` is why it was refused.
    Refused { call: blaze_id::ItemKey, why: Refusal },
}

/// Why a macro call was refused (the names library's `RefusedWhy`, mirrored: this crate does
/// not read the library's types).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Refusal {
    /// Its expansion failed (no rule matched, `compile_error!`, a proc macro's error): the
    /// crate's names record holds the call's path and the reason.
    Expansion,
    /// An `env!` of a variable this build could not answer.
    UnknownEnv,
    /// `env!("OUT_DIR")` of a package whose build script's outputs are missing.
    BuildScriptOutputs,
}

impl ContentHash for PathMiss {
    fn hash_into(&self, h: &mut StableHasher) {
        match *self {
            PathMiss::At { segment, after, why } => {
                h.tag(0);
                h.u32(segment);
                match after {
                    None => h.tag(0),
                    Some(PathPrefix::Module(k)) => {
                        h.tag(1);
                        k.hash_into(h);
                    }
                    Some(PathPrefix::Def(d)) => {
                        h.tag(2);
                        d.hash_into(h);
                    }
                    Some(PathPrefix::Item(k)) => {
                        h.tag(3);
                        k.hash_into(h);
                    }
                    Some(PathPrefix::Bstd { module, edition }) => {
                        h.tag(4);
                        module.hash_into(h);
                        // The name's bytes, ended by 0xff (no byte of UTF-8 text).
                        *h = h.bytes(edition.as_bytes());
                        h.tag(0xff);
                    }
                }
                why.hash_into(h);
            }
            PathMiss::Whole => h.tag(1),
        }
    }
}

impl ContentHash for MissWhy {
    fn hash_into(&self, h: &mut StableHasher) {
        match *self {
            MissWhy::NotFound => h.tag(0),
            MissWhy::Private => h.tag(1),
            MissWhy::Ambiguous => h.tag(2),
            MissWhy::NotAModule => h.tag(3),
            MissWhy::SuperPastRoot => h.tag(4),
            // Tags 5 and 6 were `NotRead` and `Unbounded`: no miss now (the world's gap),
            // never reused.
            MissWhy::Refused { call, why } => {
                h.tag(7);
                *h = h.bytes(&call.0.0);
                h.tag(match why {
                    Refusal::Expansion => 0,
                    Refusal::UnknownEnv => 1,
                    Refusal::BuildScriptOutputs => 2,
                });
            }
            MissWhy::NotAValue(k) => {
                h.tag(8);
                k.hash_into(h);
            }
            MissWhy::ThroughUse => h.tag(9),
        }
    }
}

impl ContentHash for PathAnswer {
    fn hash_into(&self, h: &mut StableHasher) {
        match self {
            Ok(r) => {
                h.tag(0);
                r.hash_into(h);
            }
            Err(m) => {
                h.tag(1);
                m.hash_into(h);
            }
        }
    }
}

impl Holed for PathAnswer {
    fn hole() -> Self {
        Err(PathMiss::Whole)
    }
}

impl PathMiss {
    /// The miss of a world that walks no segments and found no prefix: the first segment.
    pub const FIRST: PathMiss = PathMiss::At { segment: 0, after: None, why: MissWhy::NotFound };

    /// The index of the written segment that failed, when the world located it.
    pub const fn segment(self) -> Option<u32> {
        match self {
            PathMiss::At { segment, .. } => Some(segment),
            PathMiss::Whole => None,
        }
    }
}
