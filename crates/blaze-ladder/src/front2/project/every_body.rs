//! EVERY body of the program's own crate, each checked once ([`ProjectWorld::each_body`]): the
//! whole-project check (`blaze check` over records) asks this, never [`list_bodies`] alone.
//!
//! The Reference, "Statements", "Item declarations": "An item declaration statement has a
//! syntactic form identical to an item declaration within a module"; "Items": "Items may be
//! declared in a block". A fn declared in a body is a body of the program like a module's, and
//! is checked like one. "Attribute macros": the item is replaced by the macro's output, so a
//! body an expansion wrote is checked too. D104 is the case that asks: the root's written
//! `fn main` is `std::main`'s input (blaze-std entry.rs `main!`), whose expansion is the
//! launcher `fn main() -> Exit { fn main(..) <the written body> .. }`: the body the user wrote
//! is a LOCAL fn of a body a macro wrote, and before this module no whole check reached it.
//!
//! Where each body comes from (one home each, no second walker, rule 30):
//! - the module items, members of impls and traits, at any depth of expansion (attribute,
//!   derive or fn-like): [`list_bodies`] over the names source's item rows. An item a macro
//!   wrote already has its key from its call's key, its kind's tag, its name and its ordinal
//!   (`blaze_scope::task::key_of`, "the `index`th entry of `parent` (a module, a call or an
//!   item)"): keys from text, before the work (rules 21, 65);
//! - the fns, consts and statics declared in a body (each an item with a body; "fns" below
//!   says all three): that body's lowering keeps them (front2 locals.rs
//!   `Locals::keep`, keyed by nested_exports.rs `local_key` from the enclosing body's key, the
//!   kind's tag, the name and the ordinal, with `LibKey::INNER`). They are known only once
//!   the enclosing body is lowered, so the list is a WORK LIST: each body checked appends the
//!   fns its lowering kept, which are checked in turn (their own local fns after them, at any
//!   depth, no recursion).
//!
//! KNOWN DOUBLE, named, not hidden: a fn of a block the names library writes as a module (a
//! block holding a type, impl, trait, `mod` or statement macro call) is an item of the
//! library too, so [`list_bodies`] lists it under the block module's key, and its enclosing
//! body keeps it under its local key (the one a call in the body resolves, locals.rs
//! `Locals::find`): it is checked twice, with the same findings (the command says a finding
//! once per place and words). Leaving one out needs the block module's identity from the
//! names source and an enclosing body that is always reached (a `const`'s block is not a fn
//! body): the next piece, never a skip by guess. A local fn of an `extern` block declares no
//! body and is not a body here. A body whose lowering is refused keeps no local fn: its own
//! verdict is that refusal (never a pass), and the fns inside it are not reached; the count
//! of bodies reached says so to a caller that compares.
//!
//! Order (deterministic, rule 25): the listed bodies in [`BodyRef`] order, then the local fns
//! in the order checked bodies kept them (each body's rows in the lowering's order, a function
//! of its text alone).

use blaze_own::{AVec, Alloc};
use blaze_typeck::ItemKey;

use super::super::keys::checker_of;
use super::super::locals::LocalKind;
use super::super::project_records::{BodySource, Listed, list_bodies};
use super::{BodyAt, BodyRef, ProjectWorld};

/// The offset of `text` in `base` when `text` is a slice of it (the same bytes, not a copy);
/// `None` else. The one home of "where in its file is this text" for [`list_bodies`] and
/// [`ProjectWorld::each_body`].
pub fn offset_in(base: &str, text: &str) -> Option<u32> {
    let (b, t) = (base.as_ptr() as usize, text.as_ptr() as usize);
    if t >= b && t + text.len() <= b + base.len() { u32::try_from(t - b).ok() } else { None }
}

/// One body as [`ProjectWorld::each_body`] reached it, with what its check gave.
#[derive(Debug)]
pub struct Reached<'n, T> {
    /// The body, where it is written (`at`: `None` when a macro wrote its text) and its entry
    /// in its file's outline (`None` for a body a macro wrote and for every local fn: a file's
    /// outline lists module-level items only).
    pub listed: Listed<'n>,
    /// For a fn declared in a body: that body, and the fn's name as written. `None` for a
    /// body [`list_bodies`] listed.
    pub within: Option<(BodyRef<'n>, &'n str)>,
    /// What the check gave.
    pub got: T,
}

impl<'n, S: BodySource<'n>> ProjectWorld<'n, S> {
    /// Every body of the program's own crate, each checked once by `check` (the world's
    /// [`ProjectWorld::check`], or `check_borrows`: the strategy is the caller's, rule 78), in
    /// the module doc's order. The list and what it holds live in `alloc`.
    pub fn each_body<'a, T>(&mut self, alloc: Alloc<'a>, check: impl FnMut(&mut ProjectWorld<'n, S>, BodyRef<'n>) -> T) -> AVec<'a, Reached<'n, T>> {
        let listed = list_bodies(self.front.names, alloc);
        self.each_from(&listed, alloc, check)
    }

    /// [`ProjectWorld::each_body`]'s work list from `roots` instead of every listed body: each
    /// root checked, then each fn a checked body declares, at any depth. The command's check of
    /// one outline entry asks this with the entry's one root (blaze-cli body_types/covered.rs).
    pub fn each_from<'a, T>(&mut self, roots: &[Listed<'n>], alloc: Alloc<'a>, mut check: impl FnMut(&mut ProjectWorld<'n, S>, BodyRef<'n>) -> T) -> AVec<'a, Reached<'n, T>> {
        let mut work: AVec<'a, (Listed<'n>, Option<(BodyRef<'n>, &'n str)>)> = AVec::with_capacity_in(roots.len(), alloc);
        for l in roots.iter() {
            work.push((*l, None));
        }
        let mut out: AVec<'a, Reached<'n, T>> = AVec::with_capacity_in(work.len(), alloc);
        // A queue over `work`: each body checked appends its local fns after the rest.
        let mut next = 0usize;
        while next < work.len() {
            let (l, within) = work[next];
            next += 1;
            let got = check(self, l.body);
            out.push(Reached { listed: l, within, got });
            let Some(owner) = self.owner_of(l.body) else { continue };
            // The rows live in the command's frame (`'n`): no borrow of the store is held.
            let rows = match self.store.locals.body(owner) {
                Some(kept) => kept.rows,
                None => continue,
            };
            for row in rows.iter() {
                // Each local item with a body: a fn's, a const's or a static's initializer (the
                // Reference, "Block expressions": an item in a block is an item; its initializer
                // is checked as a module-level one is). Before (tempo t1935d, 7 lines: chainspec
                // `mainnet::IDENTITY`, clap's `augment_args::DEFAULT_VALUE`, `thread_local!`'s
                // `__RUST_STD_INTERNAL_VAL`), only fns were queued: a local static's initializer
                // was never checked, and no covers listed it ("the body ids hold nothing for").
                if !matches!(row.kind, LocalKind::Fn | LocalKind::Const | LocalKind::Static) {
                    continue;
                }
                let ck = checker_of(row.key);
                // A fn or static of an `extern` block in a body: a declaration, no body.
                if self.store.locals.foreign(ck).is_some() {
                    continue;
                }
                // Written in its file when its kept text is a slice of the file's (its
                // enclosing body's text is); a copy (an expansion wrote it) has no place.
                let at = match (l.at, self.store.locals.head(ck)) {
                    (Some(outer), Some(head)) => self.front.names.own_file(outer.file).and_then(|file| offset_in(file, head.text)).map(|start| BodyAt { file: outer.file, start }),
                    _ => None,
                };
                work.push((Listed { body: BodyRef::Free(row.key), at, entry: None }, Some((l.body, row.name))));
            }
        }
        out
    }

    /// The checker key the body `body`'s local items are kept under (locals.rs `Enclosing::
    /// owner`, body.rs `Body::owner`): a free fn's own key, a member's filed key. `None` for
    /// a member whose container's head was never made (its check was refused before it, and
    /// it kept nothing).
    fn owner_of(&self, body: BodyRef<'n>) -> Option<ItemKey> {
        match body {
            BodyRef::Free(lib) => Some(checker_of(lib)),
            BodyRef::Member { container, kind, name, .. } => self.front.keys.filed_member(checker_of(container), kind, name),
        }
    }
}
