//! D189 step 4: a body's block scopes solved as the BODY'S OWN, by this cell core, over the
//! body's own block tree, the outermost blocks' parent its module in the crate's FINISHED names
//! ([`solve_body_in`]). The tree source is the core's [`Outside`] seam (rule 78): a one-group solve
//! answers every module outside its members from its group's record (switch.rs
//! `OutsideRecords`); a body solve answers every module of the crate from its finished scope
//! ([`FinishedScopes`]). The members of a body solve are the modules the body's blocks make
//! (resolve.rs `make_blocks` over the one body): their texts are read and solved here, every
//! other module of the crate is read only where a block reads it, and nothing of the crate is
//! solved again.
//!
//! A block's textual scope goes out of the body at the body's place in its module's sequence
//! (Reference `macro.decl.scope.textual`): the crate solve counts that place once
//! ([`Cells::textual_len_at`], kept beside the body: `crate::resolve::HeldBody::cut`), and the
//! body solve reads the module's finished sequence cut there ([`Cells::body_cuts`]), as it reads
//! a `mod` child's cut by its mark.

#![blaze::sans_io]

use core::cell::RefCell;

use blaze_hash::HashMap;
use blaze_id::{ContentId, ItemKey};
use blaze_own::{AVec, Alloc};
use blaze_surface::ScopeBuilder;

use super::edit::MemberRefusal;
use super::switch::{MOST_ROUNDS, Unwritten};
use super::{Answered, BuiltDeps, Cells, DepAnswer, EnumNames, Filled, FragId, ModId, Outside, Slot};
use crate::facts::ScopeFacts;
use crate::resolve::Built;
use crate::scope::Resolved;
use crate::{Def, DefKind, Namespace};

/// The domain of the digest a finished scope stands under in a body solve ([`Answered::Record`]
/// and the textual cell's): the module's key alone. A body solve writes no group, so no record
/// compares it; what the body read of a finished scope is in its record's read list
/// (`crate::body_scope`), digested from the scope's bytes.
const FINISHED_DOMAIN: &str = "blaze-scope finished scope 1";

/// The crate's finished scopes as a body solve's tree source ([`Outside`]): every module of the
/// crate answered from its scope as the crate solve wrote it, each module the solve read noted.
pub(crate) struct FinishedScopes<'r, 'a> {
    done: &'r HashMap<ItemKey, Resolved<'a, &'a str>, Alloc<'a>>,
    /// Each module's chain up to the root, itself first (`CrateNames::chains`): how a module
    /// no text of the body solve names is reached ([`Outside::chain`]). `None` only under the
    /// planted bug ([`crate::resolve::ReachPlant::Unplaced`]).
    chains: Option<&'r HashMap<ItemKey, AVec<'a, ItemKey>, Alloc<'a>>>,
    read: RefCell<AVec<'a, ItemKey>>,
}

impl<'r, 'a> FinishedScopes<'r, 'a> {
    pub(crate) fn new(
        done: &'r HashMap<ItemKey, Resolved<'a, &'a str>, Alloc<'a>>,
        chains: Option<&'r HashMap<ItemKey, AVec<'a, ItemKey>, Alloc<'a>>>,
        alloc: Alloc<'a>,
    ) -> FinishedScopes<'r, 'a> {
        FinishedScopes { done, chains, read: RefCell::new(AVec::new_in(alloc)) }
    }

    /// Every module whose finished scope the solve read, sorted, without repeats.
    pub(crate) fn read(self) -> AVec<'a, ItemKey> {
        let mut read = self.read.into_inner();
        read.sort_unstable();
        read.dedup();
        read
    }
}

impl<'a> Outside<&'a str> for FinishedScopes<'_, 'a> {
    fn fill(&self, key: ItemKey, into: &mut Filled<'_, &'a str>) -> Result<Option<ContentId>, MemberRefusal> {
        let Some(r) = self.done.get(&key) else { return Ok(None) };
        self.read.borrow_mut().push(key);
        let s = &r.scope;
        into.bindings.extend(s.bindings.iter().copied());
        into.globs.extend(s.globs.iter().copied());
        into.defs.extend(s.textual.iter().copied());
        into.marks.extend(s.marks.iter().copied());
        // The `#[macro_export]`s an expansion of the module wrote are bindings of the finished
        // root already (task/exports.rs `publish_expanded_exports` ran before the names were
        // finished): read there, never refused here as a one-group solve must.
        into.macro_exports = false;
        let digest = ContentId::of(FINISHED_DOMAIN, &key.0.0);
        into.textual = Some(digest);
        Ok(Some(digest))
    }

    /// Every module of the crate with a finished scope is placed: a glob of the body (or of a
    /// finished module the body reads) whose source an expansion outside the body wrote is
    /// reached through its ancestors, never refused as a one-group solve's `GlobSource`
    /// (tempo, 21 Nov 04:10: all 1,790 refused bodies were that refusal).
    fn chain(&self, key: ItemKey) -> Option<&[ItemKey]> {
        if !self.done.contains_key(&key) {
            return None;
        }
        self.chains?.get(&key).map(|c| &c[..])
    }
}

/// The crate's enums as a body solve reads them, each enum read noted (its variants are in the
/// body record's read list).
pub(crate) struct LoggedEnums<'r, 'a> {
    inner: &'r dyn EnumNames<&'a str>,
    read: RefCell<AVec<'a, ItemKey>>,
}

impl<'r, 'a> LoggedEnums<'r, 'a> {
    pub(crate) fn new(inner: &'r dyn EnumNames<&'a str>, alloc: Alloc<'a>) -> LoggedEnums<'r, 'a> {
        LoggedEnums { inner, read: RefCell::new(AVec::new_in(alloc)) }
    }

    /// Every enum the solve asked of, sorted, without repeats.
    pub(crate) fn read(self) -> AVec<'a, ItemKey> {
        let mut read = self.read.into_inner();
        read.sort_unstable();
        read.dedup();
        read
    }
}

impl<'a> EnumNames<&'a str> for LoggedEnums<'_, 'a> {
    fn holds(&self, of: ItemKey) -> bool {
        self.read.borrow_mut().push(of);
        self.inner.holds(of)
    }

    fn variant(&self, of: ItemKey, ns: Namespace, name: &'a str) -> DepAnswer {
        self.read.borrow_mut().push(of);
        self.inner.variant(of, ns, name)
    }
}

/// What one body's blocks are, for [`solve_body_in`]: the modules its blocks made (the solve's
/// members), where each outermost block's body is written, and each outermost block's cut of
/// its module's textual sequence.
pub(crate) struct BodyTree<'r, 'a> {
    /// Every module the body's blocks made so far (and the expansions in them), for the rounds.
    pub(crate) made: &'r dyn Fn(&mut AVec<'a, ItemKey>),
    /// Whether a module is one the body's blocks made: read from its text.
    pub(crate) members: &'r dyn Fn(ItemKey) -> bool,
    /// A block module's body place (`BodySrc::place`), for a walk out of it.
    pub(crate) places: &'r dyn Fn(ItemKey) -> Option<(ItemKey, u32)>,
    /// Each outermost block module of the body, with the length of its module's finished
    /// textual sequence at the body's place.
    pub(crate) cuts: &'r [(ItemKey, u32)],
    /// The body's module and its ancestors (`CrateNames::chains`), from the root down to the
    /// first module an expansion made (that one and its subtree come with `made`): the modules
    /// the solve makes at its load ([`Cells::load_body`]); every other module of the crate is
    /// made when a lookup reaches it.
    pub(crate) chain: &'r [ItemKey],
    /// The crate's own standard prelude and its ancestors (core's and std's
    /// `prelude::rust_20xx`), itself first; empty when the prelude is a dependency's.
    pub(crate) prelude: &'r [ItemKey],
    /// The planted cause (`crate::resolve::ReachPlant::WholeTree`): the load makes the crate's
    /// whole tree from the root, as before [`Cells::load_body`]. `false` in every real solve.
    pub(crate) whole_tree: bool,
}

/// Solves the blocks of one body over the crate's finished names: `facts` answer the crate's
/// walked modules (their `mod` lines) and the blocks' texts; `finished` every module of the crate
/// a block reads; `built` the dependencies (every read noted in its `asked`); `root` the crate's
/// root. Each member's scope, by key; a member that left an import or call outside what the
/// core follows, or a read the finished names cannot answer, refuses the body (rule 3).
///
/// The cell core's working memory is in `work`, and only the scopes are written in `alloc`
/// (`work` may be `alloc`). The core makes the root and the body's module chain
/// ([`Cells::load_body`], `tree.chain`) and every other module of the crate only when a
/// lookup reaches it; its `Outside` copies each finished scope a block reads into its cells.
/// (It made EVERY module of the crate from the tree record's `mod` lines before, each body
/// again: bodies × modules, tempo's `core` outgrowing its 4 GiB frame on 21 Nov 05:33.) In a
/// scratch the driver resets after each body (`Par::call_scratch`, crate::task::Scratch::run),
/// a body's work is gone once its scopes are written (rules 11, 18). Nothing the core holds is
/// in the answer: the scopes are written by [`Cells::scope`] into `alloc`, their names the
/// facts' (`&'a str`).
#[expect(clippy::too_many_arguments, reason = "the body solve's inputs, each named")]
pub(crate) fn solve_body_in<'a, F: ScopeFacts<Name = &'a str>>(
    facts: &F,
    built: &Built<'_, 'a>,
    enums: &dyn EnumNames<&'a str>,
    finished: &FinishedScopes<'_, 'a>,
    root: ItemKey,
    tree: BodyTree<'_, 'a>,
    expand: &dyn Fn(ItemKey, Def),
    work: Alloc<'_>,
    alloc: Alloc<'a>,
) -> Result<BodyCells<'a>, Unwritten> {
    let deps = BuiltDeps { built, alloc: work, asks: true, through: core::cell::Cell::new(None) };
    let mut c = if tree.whole_tree {
        Cells::load_reading(facts, root, Some(&deps), super::CellPlant::None, tree.members, work)
    } else {
        Cells::load_body(facts, root, tree.chain, tree.prelude, Some(&deps), work)
    }
    .map_err(Unwritten::Load)?;
    c.enums = Some(enums);
    c.outside = Some(finished);
    for &(block, len) in tree.cuts {
        c.body_cuts.insert(block, len);
    }
    c.pend(0..c.modules.len() as ModId);
    c.fill_macro_use_children();
    c.run(facts, expand, &[]).map_err(Unwritten::Run)?;
    // The body's blocks, then the modules their expansions make, round by round.
    let mut rounds = 0u32;
    loop {
        let mut extra: AVec<'a, ItemKey> = AVec::new_in(alloc);
        (tree.made)(&mut extra);
        extra.retain(|k| !c.by_key.contains_key(k));
        if extra.is_empty() {
            break;
        }
        rounds += 1;
        if rounds > MOST_ROUNDS {
            return Err(Unwritten::Rounds);
        }
        extra.sort_unstable();
        extra.dedup();
        let (_, outside) = c.load_more_reading(facts, &extra, tree.places, tree.members).map_err(Unwritten::Load)?;
        c.pend(outside.iter().copied());
        c.fill_macro_use_children();
        c.run(facts, expand, &[]).map_err(Unwritten::Run)?;
    }
    if let Some(r) = c.member_refusal {
        return Err(Unwritten::Member(r));
    }
    let ix = c.write_index(work);
    let mut keys: AVec<'_, ItemKey> = AVec::new_in(work);
    keys.extend(c.modules.iter().filter(|m| m.answered == Answered::Text && !m.foreign && !m.unread && m.text != FragId::MAX).map(|m| m.key));
    keys.sort_unstable();
    let mut out: AVec<'a, (ItemKey, ScopeBuilder<'a, &'a str>)> = AVec::with_capacity_in(keys.len(), alloc);
    for &k in keys.iter() {
        let Some((scope, unknown)) = c.scope(&ix, k, alloc) else { continue };
        if unknown > 0 {
            return Err(Unwritten::Unknown { module: k, count: unknown, why: c.first_unknown(&ix, k) });
        }
        out.push((k, scope));
    }
    // The crate's modules the solve made beside its own (no text read here): the chain, its
    // `#[macro_use]` closure, and what its lookups reached. A dependency's are not counted.
    let made_outside = c.modules.iter().filter(|m| !m.foreign && m.text == FragId::MAX).count();
    Ok(BodyCells { scopes: out, made_outside })
}

/// What [`solve_body_in`] answers: each member's scope, by key, and how many of the crate's
/// modules outside the body it made (the profile's "modules made for body solves": a body's
/// chain and reads, never its crate's tree).
pub(crate) struct BodyCells<'a> {
    pub(crate) scopes: AVec<'a, (ItemKey, ScopeBuilder<'a, &'a str>)>,
    pub(crate) made_outside: usize,
}

impl<N: crate::Name> Cells<'_, N> {
    /// The length of a module's textual sequence (its scope's `textual`, as write.rs `scope`
    /// flattens it) before the place `pos` of the fragment `owner` (the module's own text, or a
    /// call's expansion in it, by the call's key): what a body written there sees of the
    /// module's `macro_rules!` (`BodySrc::place`, read as [`Cells::line_of`] reads a block's:
    /// every entry at or before `pos` is before the body). `None` when the fragment is not read
    /// here, or a `#[macro_use]` child before the place has no sequence here.
    pub(crate) fn textual_len_at(&self, owner: ItemKey, pos: u32) -> Option<u32> {
        let target = match self.by_key.get(&owner) {
            Some(&m) => self.modules[m as usize].text,
            None => {
                let &j = self.call_ids.get(&owner)?;
                self.items[j as usize].expansion?
            }
        };
        if target == FragId::MAX {
            return None;
        }
        let module = self.frags[target as usize].module;
        let start = self.modules[module as usize].text;
        if start == FragId::MAX {
            return None;
        }
        let mut count: u32 = 0;
        let mut stack: AVec<'_, (FragId, usize)> = AVec::new_in(self.alloc);
        stack.push((start, 0));
        while let Some(&(f, si)) = stack.last() {
            let next = self.frags[f as usize].slots.get(si).copied();
            if f == target && next.is_none_or(|(at, _)| at > pos) {
                return Some(count);
            }
            let Some((_, slot)) = next else {
                stack.pop();
                continue;
            };
            if let Some(top) = stack.last_mut() {
                top.1 += 1;
            }
            match slot {
                Slot::Rules(_, def, _) => {
                    if matches!(def.kind, DefKind::Macro(_)) {
                        count += 1;
                    }
                }
                Slot::Mod(_, false) => {}
                Slot::Mod(child, true) => {
                    let c = &self.modules[child as usize];
                    if c.foreign || c.unread {
                        continue;
                    }
                    if c.text != FragId::MAX {
                        stack.push((c.text, 0));
                    } else {
                        let r = self.recorded_textual.get(&child)?;
                        count += u32::try_from(r.defs.len()).ok()?;
                    }
                }
                Slot::Call(j) => {
                    if let Some(e) = self.items[j as usize].expansion {
                        stack.push((e, 0));
                    }
                }
            }
        }
        None
    }
}
