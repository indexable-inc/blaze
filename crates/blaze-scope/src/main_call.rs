//! The root's `main` is missing because an attribute macro took `fn main` as its input, and the
//! call did not write a `main` (Class P: a hand-written `#[bstd::main]`; also D104's implicit
//! `#[::std::main]`). The Reference, "Procedural Macros", "Attribute macros": "The item to which
//! the attribute is applied is replaced by the zero or more items in the returned TokenStream":
//! so the written `fn main` is no item of the crate, and what stands in its place is whatever
//! the call wrote. A lookup of `main` that misses there must name that call and why it wrote no
//! `main` (rules 19 and 54), never "the crate root has no `main`".
//!
//! Three causes, one per [`MainCallWhy`] variant:
//! - the attribute's path names no attribute macro (blaze-surface `ScopeError::Macro` in the
//!   root's scope): the call never ran;
//! - the call was refused ([`blaze_surface::ScopeError::Refused`]; a call whose expansion FAILED
//!   is refused too, and [`crate::failed_call::FailedCall`] names it with the matcher's reason:
//!   ask it first);
//! - the call expanded and its output declares no `main`.
//!
//! One home: the facts are the scope's errors and the lowering's call table; this only joins
//! them to the root's `fn main`. Positions stay item-relative in every record (rule 71): the
//! call's [`Loc`] is kept for a printer, never written.

use core::fmt;

use blaze_id::ItemKey;
use blaze_surface::{ItemDetail, RefusedWhy, ScopeError, Unresolved};

use crate::resolve::Names;
use crate::task::{CallSrc, Loc};

/// Why the attribute call over the root's `fn main` wrote no `main`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MainCallWhy {
    /// The attribute's path does not resolve: its segment `segment` (0-based), and why.
    Unresolved { segment: u32, why: Unresolved },
    /// The call was refused (an `env!` this build could not answer, a definition not held, ..).
    Refused(RefusedWhy),
    /// The call expanded, and its output declares no `main`.
    WroteNoMain,
}

/// The attribute call that took the root's `fn main`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MainCall<'n> {
    /// The call's key and the attribute's path as written (`bstd::main`), or task/lower.rs
    /// `IMPLICIT_MAIN` for the implicit call.
    pub call: ItemKey,
    pub path: &'n str,
    /// The call no attribute in the text names (D104).
    pub implicit: bool,
    /// Where the call stands, for a printer only.
    pub at: Option<Loc>,
    pub why: MainCallWhy,
}

impl<'n> MainCall<'n> {
    /// The attribute call over crate `krate`'s root `fn main` (a fn item of the root module, in
    /// a root file, outside any other item), when the lookup of `main` missed: `None` when no
    /// attribute call took one (the crate has no `fn main` at all: front2's `NoMain`). Of
    /// several (impossible for one item: the outermost attribute is the call, the rest are its
    /// input), the smallest key, so the answer is the same in any map order (rule 25).
    pub fn of(names: &'n Names<'n>, krate: usize) -> Option<MainCall<'n>> {
        let c = names.crates().get(krate)?;
        let mut found: Option<(ItemKey, &CallSrc<'n>)> = None;
        for (call, src) in c.tables.calls.iter() {
            if src.module != c.root || src.owner.is_some() || src.depth != 0 || !takes_root_main(c, src) {
                continue;
            }
            if found.is_none_or(|(least, _)| *call < least) {
                found = Some((*call, src));
            }
        }
        let (call, src) = found?;
        let errors = c.done.get(&c.root).map_or(&[][..], |r| &r.scope.errors[..]);
        let why = errors
            .iter()
            .find_map(|error| match error {
                ScopeError::Macro { call: k, segment, why, .. } if *k == call => Some(MainCallWhy::Unresolved { segment: *segment, why: *why }),
                ScopeError::Refused { call: k, why, .. } if *k == call => Some(MainCallWhy::Refused(*why)),
                _ => None,
            })
            .unwrap_or(MainCallWhy::WroteNoMain);
        Some(MainCall { call, path: src.name, implicit: src.implicit, at: Some(src.loc), why })
    }
}

/// Whether `src` is an attribute call whose input is a top-level `fn main` of its file.
fn takes_root_main(c: &crate::resolve::CrateNames<'_>, src: &CallSrc<'_>) -> bool {
    if !matches!(src.kind, blaze_surface::CallShape::Attr) {
        return false;
    }
    let Some(file) = c.files.get(src.loc.file as usize) else { return false };
    let at = src.loc.offset;
    let item = file.outline.items().iter().filter(|item| item.range.start <= at && at < item.range.end).min_by_key(|item| item.range.end - item.range.start);
    // `Item::name` is entry-relative (blaze-surface file_items.rs `Item::text`), read as
    // lowering reads it (lower.rs `item`: `unraw(item.text(text, r))`).
    item.is_some_and(|item| {
        item.parent.is_none()
            && matches!(item.detail, ItemDetail::Fn { .. })
            && item.name.is_some_and(|name| crate::task::unraw(item.text(file.text, name)) == "main")
    })
}

impl fmt::Display for MainCall<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let MainCall { call, path, implicit, why, .. } = *self;
        let attr = if implicit { " (implicit, D104)" } else { "" };
        write!(f, "`main` is not written: `fn main` is the input of the attribute `#[{path}]`{attr}, and the item is replaced by what that call writes (the Reference, \"Attribute macros\"): ")?;
        match why {
            MainCallWhy::Unresolved { segment, why } => {
                let name = path.split("::").filter(|s| !s.is_empty()).nth(segment as usize).unwrap_or("");
                write!(f, "its path does not resolve (segment {segment}, `{name}`: {why:?}), so the call never ran")?;
            }
            MainCallWhy::Refused(why) => write!(f, "the call was refused: {}", why.text())?,
            MainCallWhy::WroteNoMain => write!(f, "the call expanded and its output declares no `main`")?,
        }
        write!(f, " (macro call {call:x})")
    }
}
