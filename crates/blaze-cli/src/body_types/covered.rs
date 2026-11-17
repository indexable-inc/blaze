//! WHICH BODIES A FILE'S CHECK COVERS: the one home of it (rule 30). `blaze check` really checks
//! (the owner's rule): every body of an outline entry is checked, and one that is not is a
//! named `TYPES_NOT_CHECKED` finding, never a pass and never left out in silence (rules 3, 19).
//!
//! An outline entry with a body stands for a ROOT ([`root_of`]), and the check covers the root
//! and every fn declared in a body it reaches, at any depth: front2's work list
//! `ProjectWorld::each_from` (project/every_body.rs, the queue `each_body` runs over the whole
//! crate), asked here for one root. The Reference: "Statements", "Item declarations" ("An item
//! declaration statement has a syntactic form identical to an item declaration within a
//! module"): a fn declared in a body is a body of the program like a module's, checked like one.
//!
//! The root of an entry is:
//! - [`Root::Written`]: the body listed at the entry's (file, chain) (front2 project_records.rs
//!   `ByChain`), its text in the file;
//! - [`Root::Replaced`]: for the crate root file's top-level `fn main` when no body is listed at
//!   its chain: the Reference, "Attribute macros": "the item is replaced by the macro's
//!   output"; D104: a blaze-edition root `fn main` is `std::main`'s input whether or not the
//!   attribute is written (blaze-scope task/lower.rs `IMPLICIT_MAIN`), and the output is the
//!   launcher `fn main() -> Exit { fn main(..) <the written body> .. }`. The Reference,
//!   "Crates and source files", main functions: the binary's `main` is the root module's; front2
//!   finds it by its row (program/run_entry.rs `root_main`: the fn `main` declared at the top
//!   of crate 0's root module). That fn, when an expansion wrote it, is the body the entry
//!   stands for; the body the user wrote is its local `main`, reached by the work list.
//!   A root `main` placed in a file but not at this chain is refused by name ([`NoRoot`]).
//!
//! WHERE A FINDING GOES ([`place`]): a body's diagnostics are relative to the text it was
//! lowered from. The root's text starts at its entry (offset 0); a local fn whose text is a
//! slice of the same file is placed by the distance of its start from the root's (front2
//! every_body.rs `offset_in`). A body a macro wrote (the launcher, the written `main` inside it,
//! anything declared there) has text the expansion rendered: where it came from in the user's
//! text is the expansion's origins (blaze-scope expansion_origins.rs), which the records do not
//! hold yet. Its findings are still printed, at the entry's start, their words saying the body
//! by its path ([`Shown`]) and that its place is not known: shown, never dropped (rule 3).
//!
//! NAMED, not fixed here: (a) [`Root::Replaced`] takes the root `main` by rule, not by the
//! call's own place; once the items record files each call's place (expansion_origins.rs
//! `CallInText`), a body is matched to the entry its call stands at, for every attribute, and
//! its findings are placed through the origin column; (b) a fn of a block the names library
//! writes as a module is also a listed body (front2 every_body.rs, KNOWN DOUBLE): it is checked
//! under its enclosing entry here; its findings are the same words at the same place, said once
//! ([`find_once`]).

use core::alloc::Allocator;
use core::fmt;

use blaze_id::ItemKey;
use blaze_ids::ContentId;
use blaze_ladder::front2::program::run_entry::{EntryRefusal, root_main};
use blaze_ladder::front2::project::every_body::Reached;
use blaze_ladder::front2::project::{BodyRef, ProjectWorld};
use blaze_ladder::front2::project_records::{ByChain, Listed};
use blaze_ladder::front2::source::{NamesSource, Records};
use blaze_own::{AVec, Alloc};
use blaze_scope::expansion_origins::OriginsView;
use blaze_scope::items_record::{ItemsRecord, MacroBodyAt};
use blaze_scope::resolve::At;
use blaze_surface::{Item, ItemIndex, ItemKind};

use crate::names::expansion_texts::ExpansionTexts;
use crate::work::Record;

/// The body an outline entry stands for: where the bodies its check covers start.
#[derive(Clone, Copy, Debug)]
pub(super) enum Root<'n> {
    /// The entry's own body, listed at its (file, chain).
    Written(Listed<'n>),
    /// The root `fn main` an attribute took: crate 0's root `main`, which its expansion wrote.
    Replaced(Listed<'n>),
    /// A body an expansion at the entry wrote at module level ([`WrittenBy`]): a free fn, or a
    /// fn of an impl or trait the expansion wrote. `by`: the macro's path as written at the
    /// call that wrote it (its origins' `macro_name`).
    Expanded { listed: Listed<'n>, by: &'n str },
}

impl<'n> Root<'n> {
    pub(super) fn listed(self) -> Listed<'n> {
        match self {
            Root::Written(l) | Root::Replaced(l) | Root::Expanded { listed: l, .. } => l,
        }
    }

    /// The first segment of a covered body's path ([`Shown`]): the entry's name, or for a
    /// replaced `main` the expansion that wrote it. An expanded root's is [`Expanded`]'s words,
    /// made by the caller (they hold the body's name).
    pub(super) fn label<'t>(self, name: &'t str) -> &'t str {
        match self {
            Root::Written(_) => name,
            Root::Replaced(_) => "{the expansion that took `main`}",
            Root::Expanded { .. } => "{an expansion at this item}",
        }
    }
}

/// An expanded root's label ([`Root::label`]): the expansion by its macro, then the body by
/// its name, `{the expansion of `write_f!`}::f`; a member, `{..}::clone`. What a finding in
/// expanded code names, at the call (module doc, "WHERE A FINDING GOES").
pub(super) struct Expanded<'n> {
    pub(super) records: &'n Records<'n>,
    pub(super) root: Root<'n>,
}

impl fmt::Display for Expanded<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Root::Expanded { listed, by } = self.root else { return f.write_str(self.root.label("")) };
        let by = by.trim_start_matches("::").trim_end_matches('!');
        let name = match listed.body {
            BodyRef::Free(key) => NamesSource::name_of(self.records, key).unwrap_or("_"),
            BodyRef::Member { name, .. } => name,
        };
        write!(f, "{{the expansion of `{by}!`}}::{name}")
    }
}

/// A body of the owner's crate that a macro wrote at module level, and the file entry its call
/// (or its outermost caller) is at: the entry whose check covers it (D122: a macro-written
/// item's place is its expansion's). `top` is `Err` when the call's origins do not say it.
#[derive(Clone, Copy, Debug)]
pub(super) struct WrittenBy<'n> {
    pub(super) listed: Listed<'n>,
    /// The call whose expansion wrote it (the innermost).
    pub(super) call: ItemKey,
    /// That call's macro, as written.
    pub(super) by: &'n str,
    pub(super) top: Result<(u32, ItemIndex), &'static str>,
}

/// The key whose place says where a listed body is written: a free fn's own, a member's
/// container's (an impl or trait a macro wrote writes its members).
fn placed_by(body: BodyRef<'_>) -> ItemKey {
    match body {
        BodyRef::Free(key) => key,
        BodyRef::Member { container, .. } => container,
    }
}

/// Walks the call `call` of a crate whose expansion texts are `texts` up to the file entry it
/// is at: its own origins' calling text when that is a file, else its caller's. `each` is
/// given each call's macro path as written, innermost first. Bounded by the crate's count of
/// calls: a chain longer than it is a loop, refused by name. The same walk as heads.rs
/// `top_of` (lane R8's), which keeps no names: one of them goes when the two lanes are joined.
pub(super) fn callers<'n>(texts: &ExpansionTexts<'n>, call: ItemKey, each: &mut dyn FnMut(&'n str)) -> Result<(u32, ItemIndex), &'static str> {
    let mut at = call;
    for _ in 0..=texts.len() {
        let bytes = match texts.origins(at) {
            Ok(Some(bytes)) => bytes,
            Ok(None) => return Err("a macro call's expansion holds no text in its crate's record (names/expansion_texts.rs)"),
            Err(_) => return Err("a macro call's expansion origins run past their section (names/expansion_texts.rs)"),
        };
        if bytes.is_empty() {
            return Err("the expansion that wrote it keeps no origins (blaze-scope expansion_origins.rs `Rewritten`), so the entry its call is at is not known");
        }
        let view = OriginsView::open(bytes).map_err(|_| "a macro call's expansion origins do not open (blaze-scope expansion_origins.rs)")?;
        each(view.macro_name);
        match view.text {
            At::File(file) => return Ok((file, view.item)),
            At::Expansion(up) => at = up,
        }
    }
    Err("a macro call's origins name its callers in a loop (body_types/covered.rs `callers`)")
}

/// Every listed body of crate 0 a macro wrote at module level, each with the entry its call is
/// at ([`WrittenBy`]): by call, each call's chain walked once. A fn of an impl written in a
/// file that a macro call inside the impl block wrote is not here (its container's place is
/// the file): NAMED, not covered yet (heads.rs says the same of its head).
pub(super) fn written_by<'n>(records: &'n Records<'n>, texts: &ExpansionTexts<'n>, listed: &[Listed<'n>], alloc: Alloc<'n>) -> AVec<'n, WrittenBy<'n>> {
    let mut calls: AVec<'n, (ItemKey, Listed<'n>)> = AVec::new_in(alloc);
    for l in listed.iter().filter(|l| l.entry.is_none()) {
        if let Some(At::Expansion(call)) = NamesSource::place_of(records, placed_by(l.body)).map(|p| p.at) {
            calls.push((call, *l));
        }
    }
    calls.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(a.1.body.cmp(&b.1.body)));
    let mut out: AVec<'n, WrittenBy<'n>> = AVec::with_capacity_in(calls.len(), alloc);
    let mut last: Option<(ItemKey, &'n str, Result<(u32, ItemIndex), &'static str>)> = None;
    for (call, l) in calls.iter().copied() {
        let (by, top) = match last {
            Some((was, by, top)) if was == call => (by, top),
            _ => {
                let mut first: Option<&'n str> = None;
                let top = callers(texts, call, &mut |name| {
                    first.get_or_insert(name);
                });
                let by = first.unwrap_or("?");
                last = Some((call, by, top));
                (by, top)
            }
        };
        out.push(WrittenBy { listed: l, call, by, top });
    }
    out
}

/// The files of crate 0 that define a declarative macro by a name `called` holds (a path's
/// last segment), each given to `each` (once a definition: the caller's read list keeps one
/// read a name), for the `Surface` reads of an entry whose
/// expansions they wrote: an edit of such a macro's body in another file moves that file's
/// `Surface` (it holds a macro's tokens: blaze-file-items `Outline::surface`), and with it the
/// entry's check. By name, not by resolution: a macro of that name the call did not use costs
/// a rerun, never a stale check. A macro defined in an expansion, or in another crate, is not
/// here: another crate's is under its `CrateScope` and `MacroModule` reads.
pub(super) fn rules_files(items: &ItemsRecord<'_>, called: &[&str], each: &mut dyn FnMut(u32)) {
    for i in 0..items.macros() {
        let Ok(Some(row)) = items.macro_at(i) else { continue };
        if !called.iter().any(|c| last_segment(c) == row.name) {
            continue;
        }
        let span = match row.body {
            MacroBodyAt::Rules(span) | MacroBodyAt::Macro2 { body: span, .. } => span,
            _ => None,
        };
        if let Some(At::File(file)) = span.map(|s| s.at) {
            each(file);
        }
    }
}

/// A macro path's last segment: `write_f` of `crate::write_f`, `::std::main`'s `main`.
fn last_segment(path: &str) -> &str {
    let path = path.trim_end_matches('!').trim();
    path.rsplit("::").next().unwrap_or(path).trim()
}

/// Why an entry with a fn body has no root: each a `TYPES_NOT_CHECKED` note naming the item.
#[derive(Debug)]
pub(super) enum NoRoot {
    /// No body is listed at the entry's chain, and it is not the root's `fn main`.
    NoBody,
    /// The entry is the root file's top-level `fn main`, no body is listed at its chain, and
    /// front2 finds no root `main` (run_entry.rs `root_main`).
    NoRootMain(EntryRefusal),
    /// The root `main` front2 finds has no place in the records.
    RootMainUnplaced,
    /// The root `main` front2 finds is written in a file, yet not listed at this entry's chain:
    /// the chain index and the item rows disagree.
    RootMainElsewhere { file: u32 },
}

impl NoRoot {
    /// The fixed phrase that groups the items with this reason (not_checked/items.rs).
    pub(super) fn cause(&self) -> &'static str {
        match self {
            NoRoot::NoBody => "front2 lists no body for it",
            NoRoot::NoRootMain(_) => "the root's `fn main` was replaced by an attribute and front2 finds no root `main`",
            NoRoot::RootMainUnplaced => "the root's `fn main` was replaced by an attribute and the root `main` has no place in the records",
            NoRoot::RootMainElsewhere { .. } => "the root's `fn main` was replaced by an attribute and the root `main` is listed at another item",
        }
    }
}

impl fmt::Display for NoRoot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NoRoot::NoBody => write!(f, "front2 lists no body at this item (rule: blaze-cli body_types/covered.rs)"),
            NoRoot::NoRootMain(why) => write!(f, "this is the root's `fn main`, which an attribute replaced, and the expansion's `main` is not found: {why} (rule: blaze-cli body_types/covered.rs)"),
            NoRoot::RootMainUnplaced => write!(f, "this is the root's `fn main`, which an attribute replaced; the root `main` has no place in the records (rule: blaze-cli body_types/covered.rs)"),
            NoRoot::RootMainElsewhere { file } => write!(
                f,
                "this is the root's `fn main`, but the root `main` the records list is written in file {file} and not at this item (rule: blaze-cli body_types/covered.rs)"
            ),
        }
    }
}

/// The root of the entry `item` of file `file` (the owner's file index; `chain` its
/// `item_chain`), over `text`. `mine`: the listed bodies whose entry is in this file, which
/// `by_chain` indexes. `root_file`: this file is the owner's crate root.
#[expect(clippy::too_many_arguments, reason = "the entry and the owner's indexes of it, each one handle")]
pub(super) fn root_of<'n>(
    records: &'n Records<'n>,
    by_chain: &ByChain<'n>,
    mine: &[Listed<'n>],
    file: u32,
    chain: ContentId,
    item: &Item,
    text: &str,
    root_file: bool,
) -> Result<Root<'n>, NoRoot> {
    if let Some(body) = by_chain.get(file, chain) {
        return match mine.iter().find(|l| l.body == body) {
            Some(l) => Ok(Root::Written(*l)),
            // `by_chain` is made from `mine` (body_types.rs `checker`): never apart.
            None => Err(NoRoot::NoBody),
        };
    }
    let root_main_entry = root_file && item.parent.is_none() && matches!(item.kind(), ItemKind::Function) && crate::item_name::shown_name(item, text) == "main";
    if !root_main_entry {
        return Err(NoRoot::NoBody);
    }
    let key = root_main(records).map_err(NoRoot::NoRootMain)?;
    match NamesSource::place_of(records, key).map(|p| p.at) {
        Some(At::Expansion(_)) => Ok(Root::Replaced(Listed { body: BodyRef::Free(key), at: None, entry: None })),
        Some(At::File(file)) => Err(NoRoot::RootMainElsewhere { file }),
        None => Err(NoRoot::RootMainUnplaced),
    }
}

/// Every body `root` covers, each checked once by `check`, in the work list's order (the root
/// first; each fn declared in a checked body after the rest, at any depth).
pub(super) fn each_covered<'n, 'a, T>(
    world: &mut ProjectWorld<'n, &'n Records<'n>>,
    root: Root<'n>,
    alloc: Alloc<'a>,
    check: impl FnMut(&mut ProjectWorld<'n, &'n Records<'n>>, BodyRef<'n>) -> T,
) -> AVec<'a, Reached<'n, T>> {
    world.each_from(&[root.listed()], alloc, check)
}

/// Where in its entry a body's findings are: the offset of its text from the entry's start, or
/// `None` when a macro wrote its text (module doc, "WHERE A FINDING GOES").
pub(super) fn place<T>(root: Root<'_>, reached: &Reached<'_, T>) -> Option<u32> {
    let Root::Written(top) = root else { return None };
    if reached.within.is_none() {
        return Some(0);
    }
    match (top.at, reached.listed.at) {
        (Some(top), Some(at)) if top.file == at.file => at.start.checked_sub(top.start),
        _ => None,
    }
}

/// A covered body's path for a reader: the root's label, then each enclosing fn's name down to
/// it (`outer::mid::deep`; `{the expansion that took `main`}::main` for the body the user wrote
/// in a replaced `main`). No allocation: each level is found by walking up.
pub(super) struct Shown<'r, 'n, T> {
    pub(super) reached: &'r [Reached<'n, T>],
    pub(super) at: usize,
    pub(super) root: &'r str,
}

impl<T> Shown<'_, '_, T> {
    /// The body `i` was declared in: its index, earlier in the list (the work list puts a body
    /// before the fns its check kept).
    fn parent(&self, i: usize) -> Option<usize> {
        let (outer, _) = self.reached.get(i)?.within?;
        self.reached.get(..i)?.iter().position(|r| r.listed.body == outer)
    }

    fn up(&self, mut i: usize, steps: usize) -> usize {
        for _ in 0..steps {
            match self.parent(i) {
                Some(p) => i = p,
                None => break,
            }
        }
        i
    }
}

impl<T> fmt::Display for Shown<'_, '_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut depth = 0usize;
        let mut top = self.at;
        while let Some(p) = self.parent(top) {
            depth += 1;
            top = p;
        }
        f.write_str(self.root)?;
        // The top's own name when it is a local fn (never for `each_from`'s list, whose top is
        // the root): said, not hidden.
        if let Some((_, name)) = self.reached.get(top).and_then(|r| r.within) {
            write!(f, "::{name}")?;
        }
        for level in (0..depth).rev() {
            let i = self.up(self.at, level);
            if let Some((_, name)) = self.reached.get(i).and_then(|r| r.within) {
                write!(f, "::{name}")?;
            }
        }
        Ok(())
    }
}

/// Files the finding `code` + `prefix` + `text` of `rule` at `range` into `made`, once: two
/// owners of the file (a library and its test unit), or a body reached twice (module doc, (b)),
/// that agree on it say it once.
pub(super) fn find_once<A: Allocator + Copy>(made: &mut Record<A>, rule: u16, range: (u32, u32), code: &str, prefix: &str, text: &str) {
    let said = made.findings.iter().any(|&(r, start, end, from, to)| {
        let words = &made.messages[from..to];
        r == rule
            && (start, end) == range
            && words.len() == code.len() + 1 + prefix.len() + text.len()
            && words.starts_with(code.as_bytes())
            && words[code.len() + 1..].starts_with(prefix.as_bytes())
            && words.ends_with(text.as_bytes())
    });
    if !said {
        made.find(rule, range, &[code, "\x1f", prefix, text]);
    }
}
