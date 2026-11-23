//! D189 S3: the one supplier of each body's `BodyScope` record under `BlockScopes::Bodies`
//! ([`super::locals::BodyScopes`]), for one request.
//!
//! - A SURFACE body's record is read from the store, as the engine's `BodyScope` task wrote it
//!   ([`StoredBodyScopes`]). A surface body with no record is refused
//!   ([`NoBodyScope::NotStored`]); it is never made here instead (rule 3, no fallback).
//! - Every other body's record is made here on its first ask, by the same
//!   `blaze_scope::body_scope::body_scope_scratch` over its crate's tree record (read once per
//!   crate, rule 74), written and read back as the store would hold it (one reader of a record,
//!   rule 30), and kept for the rest of the request. The second ask answers from the memo,
//!   the refusal too. The solve's working memory is in the driver's scratch (`hook`, reset
//!   after each body: blaze-scope task/fill.rs `Scratch::run`), so a request that makes many
//!   bodies' records keeps the records, never every body's work; the hook of no driver
//!   (`Scratch::kept`) keeps it in the request's allocator.
//!
//! A body is asked by its names-library key (`Locals::library_body`): the key its
//! `HeldBody` and its blocks' modules are under. Its crate is the request's crate whose key's
//! prefix the body's key carries.

use core::cell::RefCell;

use blaze_hash::HashMap;
use blaze_id::ItemKey as LibKey;
use blaze_own::{AVec, Alloc};
use blaze_scope::body_scope::{BodyScopeRefusal, BodyScopeView, body_scope_scratch};
use blaze_scope::prepares::Prepares;
use blaze_scope::resolve::{BodyDeps, CrateInput, HeldBody, Names, ReadTree, SourceTexts, aligned};
use blaze_scope::task::ScratchHook;

use super::locals::{BodyScopes, NoBodyScope};

/// What the store holds of a body's `BodyScope` record (the driver's side: the engine's
/// `BodyScope` task writes the records of the surface bodies).
#[derive(Clone, Copy, Debug)]
pub enum Stored<'n> {
    /// Not a surface body: its record is made by the supplier.
    NotSurface,
    /// A surface body's record, placed at a multiple of `blaze_format::ALIGN`.
    Record(&'n [u8]),
    /// A surface body the store holds no record for.
    Missing,
}

/// The store's records of the surface bodies, as the driver reads them.
pub trait StoredBodyScopes<'n> {
    fn stored(&self, body: LibKey) -> Stored<'n>;
}

/// No body is a surface body: every record is made by the supplier (a request with no
/// `BodyScope` task, the tests).
pub struct NoSurface;

impl<'n> StoredBodyScopes<'n> for NoSurface {
    fn stored(&self, _body: LibKey) -> Stored<'n> {
        Stored::NotSurface
    }
}

/// What a crate's bodies are solved against, made on the first ask of one of its bodies.
struct CrateParts<'n> {
    tree: &'n ReadTree<'n>,
    deps: &'n BodyDeps<'n, 'n>,
    held: HashMap<LibKey, HeldBody<'n>, Alloc<'n>>,
}

struct Memo<'n> {
    views: HashMap<LibKey, Result<&'n BodyScopeView<'n>, NoBodyScope>, Alloc<'n>>,
    /// By crate index; `Err` when its tree record did not read.
    crates: HashMap<usize, Result<CrateParts<'n>, NoBodyScope>, Alloc<'n>>,
    refusals: AVec<'n, BodyScopeRefusal<'n>>,
}

/// The supplier of one request: its names (each crate's solve under `Bodies`, in the order of
/// `crates`), the texts, and the store's records.
pub struct Supplied<'n, S, T> {
    names: &'n Names<'n>,
    crates: &'n [CrateInput<'n>],
    sources: &'n S,
    prepares: &'n Prepares<'n>,
    stored: &'n T,
    /// The driver's scratch for one body's solve, reset after it.
    hook: ScratchHook<'n>,
    bump: blaze_own::Bump<'n>,
    by_prefix: HashMap<[u8; 8], usize, Alloc<'n>>,
    memo: RefCell<Memo<'n>>,
}

impl<'n, S: SourceTexts<'n>, T: StoredBodyScopes<'n>> Supplied<'n, S, T> {
    /// `names` must be the solve of `crates`, crate `k` of one at index `k` of the other.
    /// `hook` is the driver's scratch for each body's solve (`Par::call_scratch`), never asked
    /// inside its own use: a body's solve asks the supplier nothing.
    pub fn new(names: &'n Names<'n>, crates: &'n [CrateInput<'n>], sources: &'n S, prepares: &'n Prepares<'n>, stored: &'n T, hook: ScratchHook<'n>, bump: blaze_own::Bump<'n>) -> Supplied<'n, S, T> {
        let alloc = bump.alloc();
        let mut by_prefix = HashMap::new_in(alloc);
        for (k, c) in crates.iter().enumerate() {
            by_prefix.insert(c.key.prefix(), k);
        }
        let memo = Memo { views: HashMap::new_in(alloc), crates: HashMap::new_in(alloc), refusals: AVec::new_in(alloc) };
        Supplied { names, crates, sources, prepares, stored, hook, bump, by_prefix, memo: RefCell::new(memo) }
    }

    /// The bodies `body_scope` refused so far, with its words (rule 19), in the order asked.
    pub fn refusals(&self) -> usize {
        self.memo.borrow().refusals.len()
    }

    /// Each refusal so far, its words written by `each`.
    pub fn each_refusal(&self, each: &mut dyn FnMut(&BodyScopeRefusal<'n>)) {
        for r in self.memo.borrow().refusals.iter() {
            each(r);
        }
    }

    /// How many bodies' records were asked (made, read or refused).
    pub fn asked(&self) -> usize {
        self.memo.borrow().views.len()
    }

    fn keep(&self, view: BodyScopeView<'n>) -> &'n BodyScopeView<'n> {
        self.bump.alloc_val(view)
    }

    /// Crate `krate`'s tree record, dependencies and held bodies, made once.
    fn parts(&self, krate: usize) -> Result<(), NoBodyScope> {
        if let Some(known) = self.memo.borrow().crates.get(&krate) {
            return known.as_ref().map(|_| ()).map_err(|e| *e);
        }
        let alloc = self.bump.alloc();
        let c = *self.names.crates().get(krate).ok_or(NoBodyScope::NotHeld)?;
        let made = match ReadTree::of(c, self.bump) {
            Ok(tree) => {
                let tree: &'n ReadTree<'n> = self.bump.alloc_val(tree);
                let deps: &'n BodyDeps<'n, 'n> = self.bump.alloc_val(BodyDeps::of_names(self.names, krate, alloc));
                let mut held = HashMap::new_in(alloc);
                for h in c.bodies.iter() {
                    held.insert(h.src.key, *h);
                }
                Ok(CrateParts { tree, deps, held })
            }
            Err(_) => Err(NoBodyScope::NoTreeRecord),
        };
        let answer = made.as_ref().map(|_| ()).map_err(|e| *e);
        self.memo.borrow_mut().crates.insert(krate, made);
        answer
    }

    /// The record of `body`, read from the store or made here.
    fn make(&self, body: LibKey) -> Result<&'n BodyScopeView<'n>, NoBodyScope> {
        let alloc = self.bump.alloc();
        match self.stored.stored(body) {
            Stored::Record(bytes) => return BodyScopeView::read(bytes, alloc).map(|v| self.keep(v)).map_err(|_| NoBodyScope::Unreadable),
            Stored::Missing => return Err(NoBodyScope::NotStored),
            Stored::NotSurface => {}
        }
        let krate = *self.by_prefix.get(&body.crate_prefix()).ok_or(NoBodyScope::NotHeld)?;
        self.parts(krate)?;
        let (tree, deps, held) = {
            let memo = self.memo.borrow();
            let Some(Ok(p)) = memo.crates.get(&krate) else { return Err(NoBodyScope::NotHeld) };
            (p.tree, p.deps, p.held.get(&body).copied())
        };
        let held = held.ok_or(NoBodyScope::NotHeld)?;
        let input = self.crates.get(krate).ok_or(NoBodyScope::NotHeld)?;
        match body_scope_scratch(tree, deps, input, krate, held, self.sources, self.prepares, self.hook, self.bump) {
            Ok(scope) => {
                let bytes = scope.write(alloc).map_err(|_| NoBodyScope::Unwritable)?;
                let bytes = aligned(&bytes, self.bump);
                BodyScopeView::read(bytes, alloc).map(|v| self.keep(v)).map_err(|_| NoBodyScope::Unreadable)
            }
            Err(refusal) => {
                self.memo.borrow_mut().refusals.push(refusal);
                Err(NoBodyScope::Refused)
            }
        }
    }
}

impl<'n, S: SourceTexts<'n>, T: StoredBodyScopes<'n>> BodyScopes for Supplied<'n, S, T> {
    fn of(&self, body: LibKey) -> Result<&BodyScopeView<'_>, NoBodyScope> {
        // `&'n BodyScopeView<'n>` is a `&BodyScopeView<'_>` of `&self`'s life: the view is
        // covariant (blaze-scope view.rs `covariant`).
        if let Some(&known) = self.memo.borrow().views.get(&body) {
            return known;
        }
        let made = self.make(body);
        self.memo.borrow_mut().views.insert(body, made);
        made
    }
}
