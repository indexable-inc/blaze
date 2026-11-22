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

use blaze_id::{CrateItemKey, ItemKey};
use blaze_ids::ContentId;
use blaze_ladder::front2::program::run_entry::{EntryRefusal, root_main};
use blaze_ladder::front2::project::every_body::Reached;
use blaze_ladder::front2::project::{BodyRef, ProjectWorld};
use blaze_ladder::front2::project_records::{ByChain, Listed};
use blaze_ladder::front2::source::{NamesSource, Records};
use blaze_own::{AVec, Alloc};
use blaze_scope::expansion_origins::CallView;
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

/// The label of a body a macro wrote at module level ([`Expanded`]'s words), for a check of it
/// outside this module (engine_bodies.rs).
pub(crate) fn written_label<'n>(records: &'n Records<'n>, written: &WrittenBy<'n>) -> impl fmt::Display + 'n {
    Expanded { records, root: Root::Expanded { listed: written.listed, by: written.by } }
}

/// What a finding of a body a macro wrote says after "in `<the body's path>`, ": its text's place
/// in yours is not known, so it is shown at the item's start (module doc, "WHERE A FINDING
/// GOES"). The one wording, for `check` and `engine-check`.
pub(crate) const WRITTEN_UNPLACED: &str = "a body a macro wrote (where its text came from in yours is not in the records yet: blaze-scope expansion_origins.rs), shown at the item's start: ";

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
pub(crate) struct WrittenBy<'n> {
    pub(crate) listed: Listed<'n>,
    /// The call whose expansion wrote it (the innermost).
    pub(crate) call: ItemKey,
    /// That call's macro, as written.
    pub(crate) by: &'n str,
    pub(crate) top: Result<(u32, ItemIndex), &'static str>,
}

/// The key whose place says where a listed body is written: a free fn's own, a member's
/// container's (an impl or trait a macro wrote writes its members).
fn placed_by(body: BodyRef<'_>) -> ItemKey {
    match body {
        BodyRef::Free(key) => key,
        BodyRef::Member { container, .. } => container,
    }
}

/// Each listed body's block text key ([`block_key`]) beside the body's own key ([`body_key`]),
/// sorted by the block key: the keys a walk up from a body of a block's module stops at
/// ([`walk_up`]). THE ONE STOP LIST of [`written_by`] and [`in_blocks`]: a body whose walk stops
/// at one is a block's, and only [`in_blocks`] holds it, so the two lists are apart and each
/// body is checked once (rule 77). A block's module text of a body a macro wrote keeps its
/// writer's origins (blaze-scope resolve.rs `make_blocks`), so a walk that did not stop there
/// went on to the writer's file entry and listed the block's bodies a second time, as written
/// bodies (`{the expansion of `std::main!`}::MANIFEST` beside `{a block of `main`}::MANIFEST`,
/// 2026-11-21).
fn block_stops<'n>(records: &'n Records<'n>, krate: CrateItemKey, listed: &[Listed<'n>], alloc: Alloc<'n>) -> AVec<'n, (ItemKey, ItemKey)> {
    let mut blocks: AVec<'n, (ItemKey, ItemKey)> = AVec::with_capacity_in(listed.len(), alloc);
    for l in listed.iter() {
        if let Some(key) = body_key(records, l.body) {
            blocks.push((block_key(krate, key), key));
        }
    }
    blocks.sort_unstable();
    blocks
}

/// Where [`walk_up`] ended: at the file entry the outermost call is at, or at the first key of
/// the chain (`call` itself included) that `stop` names, before its origins are read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Up {
    File((u32, ItemIndex)),
    Stopped(ItemKey),
}

/// The one walk of [`callers`], stopping at the first key `stop` names: a block's module text
/// keeps no origins (blaze-scope resolve.rs `make_blocks`: `Rewritten`), so a walk that is to
/// end at the body whose block it is stops at that block's key ([`in_blocks`]) instead of
/// reading its empty origins. `each` is given each call's key and its call part (where it stands,
/// its macro as written), innermost first.
pub(crate) fn walk_up<'n>(texts: &ExpansionTexts<'n>, call: ItemKey, each: &mut dyn FnMut(ItemKey, CallView<'n>), stop: &dyn Fn(ItemKey) -> bool) -> Result<Up, &'static str> {
    let mut at = call;
    for _ in 0..=texts.len() {
        if stop(at) {
            return Ok(Up::Stopped(at));
        }
        // The call part alone, from the crate's directory: the walk reads no expansion record.
        let view = match texts.call(at) {
            Ok(Some(Some(view))) => view,
            Ok(None) => return Err("a macro call's expansion holds no text in its crate's record (names/expansion_texts.rs)"),
            // The texts blaze-scope writes as `Rewritten` (resolve.rs): a call's statements read
            // as items and a block's module text, both inside a body; and a text whose column
            // could not be made. The lowering knows the call's entry (`CallSrc::place`): the
            // piece owed is blaze-scope writing the call's part for them too.
            Ok(Some(None)) => {
                return Err("the expansion that wrote it keeps no origins: a call's statements read as items or a block's module text (blaze-scope expansion_origins.rs `Rewritten`), or a column that could not be made, and blaze-scope writes no calling entry without its column, so the entry its call is at is not known");
            }
            Err(_) => return Err("a macro call's origins do not read from its crate's expansions directory (names/expansion_texts.rs, blaze-scope expansion_records.rs)"),
        };
        each(at, view);
        match view.text {
            At::File(file) => return Ok(Up::File((file, view.item))),
            At::Expansion(up) => at = up,
        }
    }
    Err("a macro call's origins name its callers in a loop (body_types/covered.rs `callers`)")
}

/// Each body of a crate whose block is written as a module, by that module text's key
/// ([`block_key`]), sorted: the text the body is written in and its entry there (an item's own
/// place; a member's container's text, or the call in the container's block that wrote it). What
/// [`written_path`] steps through when a walk reaches a block's module text, which keeps no
/// origins (blaze-scope resolve.rs `make_blocks`).
pub(crate) type BlockHomes = [(ItemKey, At, ItemIndex)];

/// The domain of a written item's steps ([`WrittenPath::steps`]).
const WRITTEN_STEPS_DOMAIN: &str = "blaze-cli written path steps 1";

/// Where a text a macro (or a body's block) wrote is written, walked up to the file entry it
/// stands at ([`written_path`]).
#[derive(Clone, Copy)]
pub(crate) struct WrittenPath<'n> {
    /// The file and the file's entry the outermost call (or body) is at.
    pub(crate) top: (u32, ItemIndex),
    /// The way down from that entry, each step relative to the text it is in, never to the file
    /// (rule 71): the item's entry in the innermost text; each call's entry in its calling text
    /// (left out for the outermost, whose entry the caller makes relative to the entry that
    /// covers it), its input's start from its entry and its macro's path as written; each block
    /// step's body entry in its text. A function of the entry's text and the expansions' texts
    /// alone, so two items of one entry have two `steps` (two derives at one struct: their
    /// macros' paths; two calls in one expansion: their entries).
    pub(crate) steps: ContentId,
    /// The innermost call's macro, as written; `None` when only blocks were stepped through.
    pub(crate) by: Option<&'n str>,
}

/// THE ONE WALK of a written item up to its file entry (heads and bodies alike take [`walk_up`]
/// for each run of calls): from the text keyed `from` (a call's expansion, or a block's module
/// text) and, for an item, its entry `item` in that text, through every call ([`walk_up`], the
/// directory alone: no expansion record is read) and every block's body ([`BlockHomes`]).
/// `each_call` is given every call stepped through, innermost first: the calls whose expansion
/// records a head task of the entry reads (engine_heads.rs `heads_of`). Bounded by the crate's
/// calls and blocks: past them it is a loop, refused by name (rule 18).
pub(crate) fn written_path<'n>(
    texts: &ExpansionTexts<'n>,
    blocks: &BlockHomes,
    from: ItemKey,
    item: Option<ItemIndex>,
    each_call: &mut dyn FnMut(ItemKey),
) -> Result<WrittenPath<'n>, &'static str> {
    let mut h = blaze_ids::Hasher::new(WRITTEN_STEPS_DOMAIN);
    match item {
        Some(entry) => h.update(&[1]).update(&entry.0.to_le_bytes()),
        None => h.update(&[0]),
    };
    let stop = |at: ItemKey| blocks.binary_search_by(|b| b.0.cmp(&at)).is_ok();
    let mut by: Option<&'n str> = None;
    let mut at = from;
    for _ in 0..=texts.len().saturating_add(blocks.len()) {
        let up = walk_up(
            texts,
            at,
            &mut |call, view| {
                each_call(call);
                by.get_or_insert(view.macro_name);
                h.update(&[2]);
                // The outermost call's entry is a file's: the caller makes it relative.
                if let At::Expansion(_) = view.text {
                    h.update(&view.item.0.to_le_bytes());
                }
                h.update(&view.input_start.to_le_bytes()).part(view.macro_name.as_bytes());
            },
            &stop,
        )?;
        match up {
            Up::File(top) => return Ok(WrittenPath { top, steps: h.finish(), by }),
            Up::Stopped(block) => {
                let Ok(home) = blocks.binary_search_by(|b| b.0.cmp(&block)) else {
                    return Err("a walk stopped at a block no body of the crate has (body_types/covered.rs `written_path`)");
                };
                let (_, text, body) = blocks[home];
                h.update(&[3]);
                match text {
                    At::File(file) => return Ok(WrittenPath { top: (file, body), steps: h.finish(), by }),
                    At::Expansion(call) => {
                        h.update(&body.0.to_le_bytes());
                        at = call;
                    }
                }
            }
        }
    }
    Err("a written item's walk names its calls and blocks in a loop (body_types/covered.rs `written_path`)")
}

/// Every listed body of crate 0 a macro wrote at module level, each with the entry its call is
/// at ([`WrittenBy`]): by call, each call's chain walked once ([`walk_up`]), innermost macro
/// first. A body whose walk stops at a listed body's block ([`block_stops`]) is that block's
/// and [`in_blocks`]'s alone: not here. A fn of an impl written in a file that a macro call
/// inside the impl block wrote is not here (its container's place is the file): NAMED, not
/// covered yet (heads.rs says the same of its head). With no crate key there is no stop list:
/// [`in_blocks`] is then refused (`NoCrateKey`), and every entry with it (engine_bodies.rs
/// `groups`), so no body is checked twice.
pub(crate) fn written_by<'n>(records: &'n Records<'n>, texts: &ExpansionTexts<'n>, listed: &[Listed<'n>], alloc: Alloc<'n>) -> AVec<'n, WrittenBy<'n>> {
    let blocks = match NamesSource::crate_key(records, 0) {
        Some(krate) => block_stops(records, krate, listed, alloc),
        None => AVec::new_in(alloc),
    };
    let stop = |at: ItemKey| blocks.binary_search_by(|b| b.0.cmp(&at)).is_ok();
    let mut calls: AVec<'n, (ItemKey, Listed<'n>)> = AVec::new_in(alloc);
    for l in listed.iter().filter(|l| l.entry.is_none()) {
        // From the items row alone (front2 source.rs `Records::text_at`): `place_of` reads the
        // text first, and a body whose expansion record is missing then dropped out of its
        // call's task, which passed clean with nothing checked; its check now reads the text
        // and is refused naming the record.
        if let Some(At::Expansion(call)) = records.text_at(placed_by(l.body)) {
            calls.push((call, *l));
        }
    }
    calls.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(a.1.body.cmp(&b.1.body)));
    let mut out: AVec<'n, WrittenBy<'n>> = AVec::with_capacity_in(calls.len(), alloc);
    let mut last: Option<(ItemKey, &'n str, Result<Up, &'static str>)> = None;
    for (call, l) in calls.iter().copied() {
        let (by, up) = match last {
            Some((was, by, up)) if was == call => (by, up),
            _ => {
                let mut first: Option<&'n str> = None;
                let up = walk_up(
                    texts,
                    call,
                    &mut |_, view| {
                        first.get_or_insert(view.macro_name);
                    },
                    &stop,
                );
                let by = first.unwrap_or("?");
                last = Some((call, by, up));
                (by, up)
            }
        };
        let top = match up {
            Ok(Up::File(top)) => Ok(top),
            // A block's body: [`in_blocks`] lists it, under the body whose block it is.
            Ok(Up::Stopped(_)) => continue,
            Err(why) => Err(why),
        };
        out.push(WrittenBy { listed: l, call, by, top });
    }
    out
}

/// The key a listed body is checked by (`check_body` finds a body by it): a free fn's own, a
/// member's own key among its container's members. `None`: the records list no such member.
pub(crate) fn body_key<'n>(records: &'n Records<'n>, body: BodyRef<'n>) -> Option<ItemKey> {
    match body {
        BodyRef::Free(key) => Some(key),
        BodyRef::Member { container, name, .. } => {
            let mut key = None;
            NamesSource::each_member(records, container, &mut |m| {
                if m.name == Some(name) {
                    key = Some(m.key);
                }
            });
            key
        }
    }
}

/// What a finding of a body of a block's module says after "in `<the body's path>`, ": its
/// module text keeps no origins, so it is shown at its entry's start, said so. The one wording.
pub(crate) const BLOCK_UNPLACED: &str = "a body of a block's module (its module text keeps no origins yet: blaze-scope resolve.rs `make_blocks`), shown at the item's start: ";

/// The key of the text a body's blocks are written in as modules: blaze-scope blocks.rs
/// `block_modules`'s fragment `(body, "blocks", "", 0)`, the `call` resolve.rs `make_blocks`
/// places every item of the body's block modules at (`At::Expansion(call)`). A function of the
/// body's key alone, so the walk from a block's item reaches its body forward, by key, never by
/// a path's prefix or a second parse.
pub(crate) fn block_key(krate: CrateItemKey, body: ItemKey) -> ItemKey {
    blaze_scope::task::key_of(krate, body, "blocks", "", 0)
}

/// A body of crate 0 written in a block's module (blaze-scope blocks.rs: a body's block that
/// holds an impl, a type, a `mod` or a statement call is an unnamed module; serde's derive is
/// `const _: () = { extern crate serde as _serde; impl _serde::Serialize for T { .. } };`), and
/// the body whose block it is.
#[derive(Clone, Copy, Debug)]
pub(crate) struct InBlock<'n> {
    pub(crate) listed: Listed<'n>,
    /// Its own key ([`body_key`]).
    pub(crate) key: ItemKey,
    /// The body whose block it is written in.
    pub(crate) within: ItemKey,
    /// `within`, walked up through bodies of blocks to the first that is no block's: an entry's
    /// own body or a body a macro wrote at module level, whose task checks it.
    pub(crate) owner: ItemKey,
}

/// Why [`in_blocks`] could not tell a crate's bodies of blocks apart: typed, so a stored row
/// (engine_member_rows.rs) keeps the cause by its [`InBlocksError::code`] and says the same
/// words when read back ([`InBlocksError::words`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InBlocksError {
    /// The crate has no key in its own records.
    NoCrateKey,
    /// A body of a block's module has no key of its own in its crate's records.
    NoBodyKey,
    /// Bodies of blocks' modules name each other as the body whose block they are in.
    Loop,
}

impl InBlocksError {
    /// Every cause, in [`InBlocksError::code`] order.
    pub(crate) const ALL: [InBlocksError; 3] = [InBlocksError::NoCrateKey, InBlocksError::NoBodyKey, InBlocksError::Loop];

    /// What the refusal of a body says (rule 19: the thing and the rule's home).
    pub(crate) const fn words(self) -> &'static str {
        match self {
            InBlocksError::NoCrateKey => "its crate has no key in its own records (front2 source.rs `crate_key`)",
            InBlocksError::NoBodyKey => "a body of a block's module has no key of its own in its crate's records (body_types/covered.rs `in_blocks`)",
            InBlocksError::Loop => "bodies of blocks' modules name each other as the body whose block they are in, in a loop (body_types/covered.rs `in_blocks`)",
        }
    }

    /// Its stable byte in a stored row: its index in [`InBlocksError::ALL`].
    pub(crate) const fn code(self) -> u8 {
        match self {
            InBlocksError::NoCrateKey => 0,
            InBlocksError::NoBodyKey => 1,
            InBlocksError::Loop => 2,
        }
    }

    /// The cause of the stored byte `code`; `None` for a byte no cause has.
    pub(crate) fn of_code(code: u8) -> Option<InBlocksError> {
        InBlocksError::ALL.get(usize::from(code)).copied()
    }
}

/// Every listed body of crate 0 written in a block's module ([`InBlock`]) but its free fns,
/// consts and statics (each its body's local item, checked by that body's check: front2
/// every_body.rs `each_from`), sorted by (owner, key), each once.
/// A body is a block's when the walk up from where it is placed (its own place, or its
/// container's: [`placed_by`]) through the calls that wrote it stops at a listed body's
/// [`block_key`] ([`block_stops`], [`walk_up`]). A body whose walk ends in a file, or
/// cannot be walked, is no block's and is not here ([`written_by`] says where it is).
pub(crate) fn in_blocks<'n>(records: &'n Records<'n>, texts: &ExpansionTexts<'n>, listed: &[Listed<'n>], alloc: Alloc<'n>) -> Result<AVec<'n, InBlock<'n>>, InBlocksError> {
    let krate = NamesSource::crate_key(records, 0).ok_or(InBlocksError::NoCrateKey)?;
    let blocks = block_stops(records, krate, listed, alloc);
    let stop = |at: ItemKey| blocks.binary_search_by(|b| b.0.cmp(&at)).is_ok();
    // Each body of a block: (its key, the body whose block it is in, it).
    let mut links: AVec<'n, (ItemKey, ItemKey, Listed<'n>)> = AVec::new_in(alloc);
    for l in listed.iter().filter(|l| l.entry.is_none()) {
        // A free fn of a block's module is a local fn of the body whose block it is: that
        // body's check checks it, at any depth, named `<body>::<fn>` (front2 tables/task.rs
        // `Said::within`, every_body.rs `each_from`). Listed here too it was checked twice, and
        // its finding printed under two names (`{a block of `main`}::main` beside `{the
        // expansion of `std::main!`}::main::main`, the fn D104's `main!` writes inside the
        // launcher, 2026-11-21): one body, one check (rule 77). A block's `const` and `static`
        // initializers are its body's locals too (front2 every_body.rs `each_from` queues
        // every local fn, const and static: before, only fns, so a static of a block that is
        // no module, chainspec's `mainnet::IDENTITY`, was checked by no task, tempo t1935d):
        // not here either. A block's impl and trait members are no body's locals: here.
        if let BodyRef::Free(key) = l.body
            && matches!(NamesSource::kind_of_item(records, key), Some(ItemKind::Function | ItemKind::Const | ItemKind::Static))
        {
            continue;
        }
        let Some(At::Expansion(call)) = records.text_at(placed_by(l.body)) else { continue };
        let Ok(Up::Stopped(block)) = walk_up(texts, call, &mut |_, _| {}, &stop) else { continue };
        let Ok(at) = blocks.binary_search_by(|b| b.0.cmp(&block)) else { continue };
        let key = body_key(records, l.body).ok_or(InBlocksError::NoBodyKey)?;
        links.push((key, blocks[at].1, *l));
    }
    links.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    links.dedup_by(|a, b| a.0 == b.0);
    let mut out: AVec<'n, InBlock<'n>> = AVec::with_capacity_in(links.len(), alloc);
    for &(key, within, listed) in links.iter() {
        // Up through the bodies of blocks, at most one step a body of a block (rule 18).
        let mut owner = within;
        let mut steps = 0usize;
        while let Ok(at) = links.binary_search_by(|x| x.0.cmp(&owner)) {
            owner = links[at].1;
            steps += 1;
            if steps > links.len() {
                return Err(InBlocksError::Loop);
            }
        }
        out.push(InBlock { listed, key, within, owner });
    }
    out.sort_unstable_by(|a, b| a.owner.cmp(&b.owner).then(a.key.cmp(&b.key)));
    Ok(out)
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
