//! The project world's two source-independent pieces (drive.md, "End state: the whole pipeline
//! on the command", piece 5 and the gap row "One body identity"):
//!
//! - THE LIST of the program's own bodies ([`list_bodies`]), asked of any names source
//!   ([`NamesSource`]: the solved names or each crate's records) through the same questions,
//!   so `ProjectWorld::bodies` answers the same over both;
//! - ONE BODY IDENTITY ([`ByChain`]): a body found by (its file, its item chain). The command
//!   names a body by its file and the chain of its outline entry
//!   (`blaze_file_items::item_chain`, the chain's one home); the front end names it by a
//!   [`BodyRef`]. The index is built once per world ([`index_by_chain`]): each file that holds
//!   a body is outlined once (over the solved names the solve's own outline is used, nothing
//!   is outlined again), each body's chain is computed from its outline entry, and the rows
//!   are sorted by (file, chain) so [`ByChain::get`] is one binary search.
//!
//! Two bodies under one (file, chain) is a typed refusal ([`ChainError::Collision`], rule 21:
//! the fix is then the chain, never the lookup), as is an entry that is not the fn the names
//! source said it is. A body a macro wrote has no entry in a file's outline: it is listed with
//! no place and is not in the index (the command names such a body by its call's expansion,
//! which this identity does not cover yet: [`ByChain::unplaced`] counts them). It IS checked:
//! the whole-project check asks `ProjectWorld::each_body` (project/every_body.rs), which
//! reaches every listed body and, through each body's lowering, the fns declared in it (D104's
//! written `main` is one: a local fn of std::main's launcher).
//!
//! The chain function is a parameter of [`index_by_chain`] (rule 78): the world passes
//! `blaze_file_items::item_chain`; tests/front2_project_records.rs passes a chain without the
//! enclosing entries' part and asserts the collision of two members of one name is refused.

use blaze_file_items::Outline;
use blaze_id::ItemKey as LibKey;
use blaze_ids::ContentId;
use blaze_own::{AVec, Alloc};
use blaze_scope::resolve::{At, Names};
use blaze_surface::{Item, ItemIndex, ItemKind};

use super::keys::MemberKind;
use super::project::{BodyAt, BodyRef};
use super::source::{self, NamesSource};

/// A names source that can also give the program's own files: what a body's place and chain
/// are read from.
pub trait BodySource<'n>: NamesSource<'n> {
    /// The whole text of file `file` (an [`At::File`]'s index) of the program's own crate
    /// (crate 0), as it is now.
    fn own_file(self, file: u32) -> Option<&'n str>;

    /// Calls `each` once with the text of file `file` of the program's own crate and its
    /// outline; `false`, `each` not called, when there is no such text. An outline made for
    /// the call is made in `alloc`.
    fn with_outline(self, file: u32, alloc: Alloc<'_>, each: &mut dyn FnMut(&str, &Outline<'_>)) -> bool;
}

impl<'n> BodySource<'n> for &'n Names<'n> {
    fn own_file(self, file: u32) -> Option<&'n str> {
        self.outline_at(0, At::File(file)).map(|(text, _)| text)
    }

    fn with_outline(self, file: u32, _alloc: Alloc<'_>, each: &mut dyn FnMut(&str, &Outline<'_>)) -> bool {
        // The solve's own outline of the file: nothing is outlined twice.
        match self.outline_at(0, At::File(file)) {
            Some((text, outline)) => {
                each(text, outline);
                true
            }
            None => false,
        }
    }
}

impl<'n> BodySource<'n> for &'n source::Records<'n> {
    fn own_file(self, file: u32) -> Option<&'n str> {
        // The file as it is now: every body listed from it was read at item grain
        // (source/item_grain.rs), so after a body edit the file's other bodies still list.
        self.file_now(0, file)
    }

    fn with_outline(self, file: u32, alloc: Alloc<'_>, each: &mut dyn FnMut(&str, &Outline<'_>)) -> bool {
        let Some(text) = self.file_now(0, file) else { return false };
        let Some(edition) = self.edition_of(0) else { return false };
        // The outline today's item rows' `outline` indexes are in: the same function over the
        // file as it is now (each listed item proved at item grain) and edition.
        let outline = blaze_file_items::outline(text, edition, alloc);
        each(text, &outline);
        true
    }
}

/// One listed body: what it is, where its text starts, and its entry in its file's outline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Listed<'n> {
    pub body: BodyRef<'n>,
    /// Its file and the start of the text it is lowered from; `None` when a macro wrote it.
    pub at: Option<BodyAt>,
    /// Its file and its entry in that file's outline; `None` when a macro wrote it.
    pub entry: Option<(u32, ItemIndex)>,
}

/// Which bodies a listing holds (rule 78: one listing, the choice a parameter).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Listing {
    /// Fns only: what the whole-program walks (`each_body`, the run path) and their tests read.
    Fns,
    /// Fns, and each `const` and `static` initializer: free items and `const` members of impls
    /// and traits (the Reference, "Constant items", "Static items", "Associated constants").
    /// What `blaze check` checks an outline entry's own body by (blaze-cli body_types.rs).
    /// A `const` or `static` with no initializer (a trait's `const N: u8;`, an `extern`
    /// block's `static`) is listed too and refused as `NoBody` if checked: its outline entry
    /// has no body, so the command never asks for it.
    FnsAndInitializers,
}

/// Every body of the program's own crate (crate 0) over `names`, sorted by [`BodyRef`]: each
/// free fn with a body, then each fn member of each impl and trait. A free fn's row that is
/// also filed as a member is one body, the member.
pub fn list_bodies<'n, 'a, S: BodySource<'n>>(names: S, alloc: Alloc<'a>) -> AVec<'a, Listed<'n>> {
    list_with(names, Listing::Fns, alloc)
}

/// [`list_bodies`] holding what `listing` names.
pub fn list_with<'n, 'a, S: BodySource<'n>>(names: S, listing: Listing, alloc: Alloc<'a>) -> AVec<'a, Listed<'n>> {
    let initializers = listing == Listing::FnsAndInitializers;
    let mut items: AVec<'a, LibKey> = AVec::new_in(alloc);
    names.each_item(0, &mut |key, _module| items.push(key));
    // Each file's text, asked once: a body's start is its text's offset in it.
    let mut files: AVec<'a, (u32, Option<&'n str>)> = AVec::new_in(alloc);
    let mut start_in = |file: u32, text: &str| -> Option<u32> {
        let base = match files.iter().find(|(f, _)| *f == file) {
            Some((_, base)) => *base,
            None => {
                let base = names.own_file(file);
                files.push((file, base));
                base
            }
        }?;
        super::project::every_body::offset_in(base, text)
    };
    let mut out: AVec<'a, Listed<'n>> = AVec::new_in(alloc);
    let mut members: AVec<'a, LibKey> = AVec::new_in(alloc);
    for &key in items.iter() {
        let Some(kind) = names.kind_of_item(key) else { continue };
        let value = matches!(kind, ItemKind::Const | ItemKind::Static);
        match kind {
            ItemKind::Function | ItemKind::Const | ItemKind::Static if !value || initializers => {
                if !names.has_text(key) {
                    continue;
                }
                let Some(it) = names.text_of(key) else { continue };
                // A fn's declaration with no body; a value's text always ends in `;`.
                if !value && it.text.trim_end().ends_with(';') {
                    continue;
                }
                let place = names.place_of(key);
                let file = place.and_then(|p| match p.at {
                    At::File(file) => Some((file, p.item)),
                    At::Expansion(_) => None,
                });
                let at = file.and_then(|(file, _)| start_in(file, it.text).map(|start| BodyAt { file, start }));
                // A text outside its file's (a macro wrote it) has no entry in the file's outline.
                let entry = if at.is_some() { file } else { None };
                out.push(Listed { body: BodyRef::Free(key), at, entry });
            }
            ItemKind::Impl | ItemKind::Trait => {
                let file = names.place_of(key).and_then(|p| match p.at {
                    At::File(file) => Some(file),
                    At::Expansion(_) => None,
                });
                let is_trait = kind == ItemKind::Trait;
                names.each_member(key, &mut |m| {
                    members.push(m.key);
                    let member = match (m.kind, m.name) {
                        (ItemKind::Function, Some(name)) => (MemberKind::Fn, name),
                        (ItemKind::Const, Some(name)) if initializers => (MemberKind::Const, name),
                        _ => return,
                    };
                    let (kind, name) = member;
                    let at = file.and_then(|file| start_in(file, m.text).map(|start| BodyAt { file, start }));
                    let entry = match (at, m.written_by) {
                        (Some(at), None) => Some((at.file, m.item)),
                        _ => None,
                    };
                    out.push(Listed { body: BodyRef::Member { container: key, is_trait, kind, name }, at, entry });
                });
            }
            _ => {}
        }
    }
    members.sort_unstable();
    out.retain(|l| !matches!(l.body, BodyRef::Free(k) if members.binary_search(&k).is_ok()));
    out.sort_unstable_by(|a, b| a.body.cmp(&b.body));
    out
}

/// Why the bodies could not be indexed by chain. Each names the file (its index in the
/// program's own crate) and the entry or the bodies it is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChainError<'n> {
    /// The names source has no text for a file a body is placed in.
    NoFile { file: u32, body: BodyRef<'n> },
    /// The file's outline has no entry at the index the names source gave for the body.
    NoEntry { file: u32, entry: u32, body: BodyRef<'n> },
    /// The entry at the body's index is not of the body's kind (a fn's, a `const`'s or a
    /// `static`'s): the outline is not the one the names source's indexes were made in.
    NotItsKind { file: u32, entry: u32, found: ItemKind, body: BodyRef<'n> },
    /// Two bodies of one file have one chain (rule 21, keys-cover-every-input): the chain
    /// misses what tells them apart.
    Collision { file: u32, chain: ContentId, first: BodyRef<'n>, second: BodyRef<'n> },
}

impl core::fmt::Display for ChainError<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ChainError::NoFile { file, body } => write!(f, "file {file} of the program's crate has no text, and the body {body:?} is placed in it"),
            ChainError::NoEntry { file, entry, body } => write!(f, "file {file} of the program's crate: its outline has no entry {entry}, where the body {body:?} is placed"),
            ChainError::NotItsKind { file, entry, found, body } => write!(f, "file {file} of the program's crate: entry {entry} of its outline is {found:?}, not the item of the body {body:?}"),
            ChainError::Collision { file, chain, first, second } => {
                write!(f, "file {file} of the program's crate: the bodies {first:?} and {second:?} have one item chain {chain:?} (rule keys-cover-every-input)")
            }
        }
    }
}

/// How an entry's chain is computed: `blaze_file_items::item_chain`, or a test's planted bug.
pub type ChainOf<'c> = &'c dyn Fn(&Outline<'_>, &Item, &str) -> ContentId;

/// The program's own bodies by (file, chain), sorted: a lookup is one binary search.
#[derive(Debug)]
pub struct ByChain<'n> {
    rows: AVec<'n, (u32, ContentId, BodyRef<'n>)>,
    unplaced: u32,
}

impl<'n> ByChain<'n> {
    /// The body of file `file` (its index in the program's own crate) whose outline entry has
    /// the chain `chain`.
    pub fn get(&self, file: u32, chain: ContentId) -> Option<BodyRef<'n>> {
        let at = self.rows.binary_search_by(|row| (row.0, &row.1.0).cmp(&(file, &chain.0))).ok()?;
        Some(self.rows[at].2)
    }

    /// Every indexed body with its file and chain, sorted by (file, chain).
    pub fn rows(&self) -> &[(u32, ContentId, BodyRef<'n>)] {
        &self.rows
    }

    /// How many listed bodies have no entry in a file's outline (a macro wrote them) and so
    /// are not in the index.
    pub fn unplaced(&self) -> u32 {
        self.unplaced
    }
}

/// The index of `listed` (from [`list_bodies`] over the same `names`) by (file, chain), each
/// chain computed by `chain` from the body's entry in its file's outline. Each file is
/// outlined at most once; the rows and any outline made live in `alloc`.
pub fn index_by_chain<'n, S: BodySource<'n>>(names: S, listed: &[Listed<'n>], chain: ChainOf<'_>, alloc: Alloc<'n>) -> Result<ByChain<'n>, ChainError<'n>> {
    let mut placed: AVec<'n, (u32, u32, BodyRef<'n>)> = AVec::new_in(alloc);
    let mut unplaced = 0u32;
    for l in listed {
        match l.entry {
            Some((file, entry)) => placed.push((file, entry.0, l.body)),
            None => unplaced += 1,
        }
    }
    placed.sort_unstable();
    let mut rows: AVec<'n, (u32, ContentId, BodyRef<'n>)> = AVec::new_in(alloc);
    let mut from = 0;
    while from < placed.len() {
        let file = placed[from].0;
        let mut to = from;
        while to < placed.len() && placed[to].0 == file {
            to += 1;
        }
        let run = &placed[from..to];
        let mut refused: Option<ChainError<'n>> = None;
        let outlined = names.with_outline(file, alloc, &mut |text, outline| {
            for &(_, entry, body) in run {
                let Some(item) = outline.items().get(entry as usize) else {
                    refused = Some(ChainError::NoEntry { file, entry, body });
                    return;
                };
                let fits = match body {
                    BodyRef::Free(_) => matches!(item.kind(), ItemKind::Function | ItemKind::Const | ItemKind::Static),
                    BodyRef::Member { kind: MemberKind::Fn, .. } => item.kind() == ItemKind::Function,
                    BodyRef::Member { kind: MemberKind::Const, .. } => item.kind() == ItemKind::Const,
                    BodyRef::Member { .. } => false,
                };
                if !fits {
                    refused = Some(ChainError::NotItsKind { file, entry, found: item.kind(), body });
                    return;
                }
                rows.push((file, chain(outline, item, text), body));
            }
        });
        if !outlined {
            return Err(ChainError::NoFile { file, body: run[0].2 });
        }
        if let Some(error) = refused {
            return Err(error);
        }
        from = to;
    }
    rows.sort_unstable_by(|a, b| (a.0, &a.1.0, a.2).cmp(&(b.0, &b.1.0, b.2)));
    for pair in rows.windows(2) {
        if pair[0].0 == pair[1].0 && pair[0].1 == pair[1].1 {
            return Err(ChainError::Collision { file: pair[0].0, chain: pair[0].1, first: pair[0].2, second: pair[1].2 });
        }
    }
    Ok(ByChain { rows, unplaced })
}
