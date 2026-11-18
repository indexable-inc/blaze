//! The front end's world for the signature reader (blaze-flow-lower sig/alias.rs `SigWorld`,
//! lane B11): a written type path in a head's text is RESOLVED through the names, as the type
//! check resolves it, before its lifetimes are read.
//!
//! THE RULE (the Reference, "Type aliases": "A type alias defines a new name for an existing
//! type"; "Paths in types"; "Preludes"): a path written in an item's text means what the names
//! library says it means from the item's module (the standard prelude, the extern prelude and
//! the builtin types included: `Paths::path` walks them, the type check's own resolver,
//! front2 types.rs `resolve_ty`). So:
//! - a struct, enum or union is `PathIs::Adt(checker_of(key))`;
//! - a type alias is `PathIs::Alias` with its whole item text, and a token ([`AliasScope`]) that
//!   brings the reader back here to resolve the alias's own paths from the ALIAS's module;
//! - a builtin type, and a name that is one of the item's own type parameters (a parameter
//!   shadows an item of its name), is `Unresolved` with no item behind it: the reader takes it
//!   as the node it names when the word spells that node (sig/alias.rs `builtin_word`);
//! - anything else that resolves (a trait, a module, a fn, a variant, a path that names an item
//!   and then more segments) is `Other`; a path that does not resolve is `Unresolved`.
//!
//! READS (rule 21). Every answer is noted ([`AskedPaths`]), the path as written, where it was
//! read from, and what it resolved to (an ADT by key, an alias by key and text), and every
//! ADT's lifetime count as `asked.note` does (borrow_reads.rs `AskedAdts`): the head's row
//! (borrow_reads.rs `head_read`) holds both, so an alias whose text changes, or a path that
//! comes to resolve elsewhere, moves the row of every head that read it.
//!
//! LIMIT (UNSURE): a path an expansion wrote with `$crate` resolves through `crate` of the
//! item's own crate here (no mark is read: `Paths::path_with`'s `dollar` is `None`): such a
//! path in another crate's macro output may not resolve, and the head is then refused by name
//! (`SigRefusal::Path { why: Unresolved }`), never guessed.

use core::cell::RefCell;

use blaze_flow_lower::sig::{AliasScope, PathIs, SigWorld};
use blaze_id::ItemKey as LibKey;
use blaze_own::{AVec, Alloc};
use blaze_typeck as tc;
use blaze_typeck::StableHasher;

use crate::front2::borrow_reads::AskedAdts;
use crate::front2::keys::{Keys, checker_of};
use crate::front2::locals::Locals;
use crate::front2::source::NamesSource;
use crate::resolve::{Kind, Ns, Paths};

/// The domain of one noted path answer.
const PATH_DOMAIN: &str = "blaze-ladder front2 borrow sig world path v1";

/// The most aliases one reading may open (each its own scope token): past it, `Unresolved`
/// (the reader then refuses the head by name: rule 18, a table has a cap).
pub const MOST_ALIASES: usize = 256;

/// The path answers a reading took, each as one digest (rule 21).
pub struct AskedPaths<'a> {
    asked: RefCell<AVec<'a, u64>>,
    alloc: Alloc<'a>,
}

impl<'a> AskedPaths<'a> {
    pub fn new_in(alloc: Alloc<'a>) -> AskedPaths<'a> {
        AskedPaths { asked: RefCell::new(AVec::new_in(alloc)), alloc }
    }

    fn note(&self, digest: u64) {
        self.asked.borrow_mut().push(digest);
    }

    /// Every answer noted, sorted, each once; the recorder is left empty for the next reading.
    pub fn take_sorted(&self) -> AVec<'a, u64> {
        let mut out = core::mem::replace(&mut *self.asked.borrow_mut(), AVec::new_in(self.alloc));
        out.sort_unstable();
        out.dedup();
        out
    }
}

/// An item's text and the module it resolves its paths from: a local item's from its row
/// (front2 locals.rs), a library item's from the names (lifetimes.rs `item_text`'s rule, one
/// rule: the module is `home_of`). `None`: no text (the caller then answers nothing).
pub fn text_and_module<'n, S: NamesSource<'n>>(names: S, locals: &Locals<'n>, lib: LibKey) -> Option<(&'n str, LibKey)> {
    match locals.head_of(lib) {
        Some(Ok((_, text, module))) => Some((text, module)),
        Some(Err(_)) => None,
        None => Some((names.text_of(lib)?.text, names.home_of(lib)?)),
    }
}

/// The world of one reading: an item's text read from `module`, its type parameters' names
/// `params` (they shadow items), each alias opened in `scopes` (the token is the index).
pub struct FrontWorld<'r, 'n, 'a, S> {
    names: S,
    keys: &'r Keys,
    locals: &'r Locals<'n>,
    module: LibKey,
    params: &'r [tc::Name],
    adts: &'r AskedAdts<'a>,
    paths: &'r AskedPaths<'a>,
    /// Per alias opened: its module, where its own text resolves.
    scopes: RefCell<AVec<'a, LibKey>>,
}

impl<'r, 'n, 'a, S: NamesSource<'n>> FrontWorld<'r, 'n, 'a, S> {
    #[expect(clippy::too_many_arguments, reason = "the names, the key map, the local items, the reading's module and parameters, and its two recorders")]
    pub fn new(
        names: S,
        keys: &'r Keys,
        locals: &'r Locals<'n>,
        module: LibKey,
        params: &'r [tc::Name],
        adts: &'r AskedAdts<'a>,
        paths: &'r AskedPaths<'a>,
        alloc: Alloc<'a>,
    ) -> FrontWorld<'r, 'n, 'a, S> {
        FrontWorld { names, keys, locals, module, params, adts, paths, scopes: RefCell::new(AVec::new_in(alloc)) }
    }

    fn text_and_module(&self, lib: LibKey) -> Option<(&'n str, LibKey)> {
        text_and_module(self.names, self.locals, lib)
    }

    fn resolve(&self, from: Option<AliasScope>, absolute: bool, segments: &[&str]) -> (PathIs<'n>, Option<LibKey>) {
        let module = match from {
            None => self.module,
            Some(AliasScope(i)) => match self.scopes.borrow().get(i as usize) {
                Some(&m) => m,
                None => return (PathIs::Unresolved, None),
            },
        };
        // The item's own type parameters shadow every item of their name (the Reference,
        // "Generic parameters": a parameter is in scope in the item's whole signature).
        if from.is_none() && !absolute && segments.len() == 1 && self.params.contains(&tc::Name::of(segments[0])) {
            return (PathIs::Unresolved, None);
        }
        let Ok(found) = (Paths { names: self.names }).path(module, absolute, segments, Ns::Type) else {
            return (PathIs::Unresolved, None);
        };
        // A path that names an item and then more (`S::Assoc`) is type-relative: not a type
        // path the reader lines up.
        if found.consumed as usize != segments.len() {
            return (PathIs::Other, None);
        }
        match found.kind {
            Kind::Struct | Kind::Enum | Kind::Union => (PathIs::Adt(checker_of(found.key)), Some(found.key)),
            Kind::TypeAlias => {
                let Some((text, alias_module)) = self.text_and_module(found.key) else { return (PathIs::Unresolved, Some(found.key)) };
                let mut scopes = self.scopes.borrow_mut();
                if scopes.len() >= MOST_ALIASES {
                    return (PathIs::Unresolved, Some(found.key));
                }
                scopes.push(alias_module);
                (PathIs::Alias { scope: AliasScope((scopes.len() - 1) as u128), text }, Some(found.key))
            }
            // A builtin type has no item: the reader takes it by its spelling.
            Kind::Builtin(_) | Kind::WideInt { .. } => (PathIs::Unresolved, None),
            _ => (PathIs::Other, Some(found.key)),
        }
    }
}

impl<'n, S: NamesSource<'n>> SigWorld for FrontWorld<'_, 'n, '_, S> {
    fn adt_lifetimes(&self, adt: tc::ItemKey) -> Option<u32> {
        // Mounted under borrows.rs (`#[path]`): its one count of an ADT's lifetimes.
        let lifetimes = super::adt_head_lifetimes(self.keys, self.names, adt);
        self.adts.note(adt, lifetimes);
        Some(lifetimes)
    }

    fn type_path(&self, from: Option<AliasScope>, absolute: bool, segments: &[&str]) -> PathIs<'_> {
        let (is, item) = self.resolve(from, absolute, segments);
        // The row of this answer (rule 21): where it was read (the alias's module for an
        // alias's own text), the path, and what it named.
        let mut h = StableHasher::new(PATH_DOMAIN);
        let module = match from {
            None => Some(self.module),
            Some(AliasScope(i)) => self.scopes.borrow().get(i as usize).copied(),
        };
        match module {
            Some(m) => {
                h.tag(1);
                h = h.bytes(&m.0.0);
            }
            None => h.tag(0),
        }
        h.tag(u8::from(absolute));
        h.u32(segments.len() as u32);
        for s in segments {
            h.u32(s.len() as u32);
            h = h.bytes(s.as_bytes());
        }
        match is {
            PathIs::Adt(k) => {
                h.tag(0);
                h.u128(k.0);
            }
            PathIs::Alias { text, .. } => {
                h.tag(1);
                h.u32(text.len() as u32);
                h = h.bytes(text.as_bytes());
            }
            PathIs::NotAlias => h.tag(2),
            PathIs::Other => h.tag(3),
            PathIs::Unresolved => h.tag(4),
        }
        if let Some(k) = item {
            h = h.bytes(&k.0.0);
        }
        self.paths.note(h.finish());
        is
    }
}
