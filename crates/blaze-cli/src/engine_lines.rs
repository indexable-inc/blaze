//! Job A sets `shown.output_only` for a stdout consumer and opens [`ViewSlot::load`].
//! After proving scope, replayability, exact owed and unchanged observations,
//! `slot.certified(cx, lines_key, shown, frame, out)` returns the tally after one bulk
//! write, or None before writing. [`ViewSlot::standing`] also handles a different
//! presentation or a caller needing fix data, loading the semantic blocks on demand.
//! On an edit, `slot.render(cx, root, store, &lines, &mut rows, shown, frame, out)`
//! returns the tally and the lines record to pass to engine_answer::save. It reuses
//! drawings in current order by their inputs, including final id width and raw source
//! bytes, and includes every placement read in the observations to seal. Stage coverage,
//! elapsed time, entered count and the unchecked-items trailer come from the live caller.
//!
//! What `engine-check` printed, kept as one record (engine.md E6, the incremental half): each
//! task's findings and notes as the printer takes them, in a block per task, sorted by name.
//!
//! A request prints the WHOLE answer through one printer ([`print_all`]): the blocks of the tasks
//! it entered, made from their results now, and the blocks of the rows that stand, copied from
//! the last request's record. So an incremental request prints what a fresh request on the same
//! text prints, and a request with nothing changed (the certificate) prints the last record's
//! blocks with no page of the answer read: one get. The record's key is in the pointer
//! (engine_answer.rs), its bytes pinned with the answer's pages.
//!
//! ```text
//! record  "BEFB" | blocks u32 | per block, by name: kind u8 | subject [32] | bytes u32 | entries
//!         "BEFC" | placement digest [32] | identical block count and entries
//! entry   0 | rule u16 | start u32 | end u32 | path bytes u32 (NO_PATH: none) | path | words u32 | words
//!         1 | class u8 | words u32 | words
//!         2 | rule u16 | start u32 | end u32 | path u32 | path | chain u64 | edition u8 | words u32 | words
//!         3 | rule u16 | first u32 | last u32 | inside u8 | inside.0 u32 | inside.1 u32 | path u32 | path | chain u64 | edition u8 | words u32 | words
//!         4 | noted u32 | noted (blaze_report::finding `encode`: subject | cause)
//!         5 | base kind u8 | base subject [32] | at u32 | units u32 | per unit: test u8 | name u32 | name
//!         6 | rule u16 | start u32 | end u32 | path u32 | path | in item u8 | chain u64 | edition u8 | words u32 | words
//! ```
//!
//! An OWN-PLACED finding (6) is a lint unit's (lints.rs): `start..end` a range of the unit's own
//! text, each child of the unit one mark there (blaze-lint `Units::own`), so no child's length is
//! stored; the unit is the item whose chain is `chain` (in item 1) or the file's (0, chain 0).
//! The printer gives each child before the range its length again from the file's outline of
//! `edition` as it is today (print.rs `Place::Own`, lints.rs `own_to_item`), so a unit's finding
//! that stands while a child above it grew is drawn where it is now. ("BEF9" and "BEFA", before
//! it: refused, rule 26.)
//!
//! A VIEW entry (5, D191) opens the block of one row of a body two or more units reach: the
//! body's base name, this row's place among the units (0: the base row) and every such unit's
//! label in rank (a test build, by its crate's name; else its crate's name). They are facts of
//! the row's own key (its plan's source holds the units' views: engine_body_plan.rs
//! `source_at_views`), so a standing row's block is never stale. The printer folds an equal
//! finding of the body's rows into one line, and says the view of a finding only some of them
//! hold ([`print_all`]). ("BEF7" and "BEF8", before it: refused, rule 26.)
//!
//! A NOTED finding (4, D164) is a body's note as a record: its subject (the body's file and
//! item) and its typed cause, the rule the cause's; no words. Its words are the table's, filled
//! when it is printed (blaze_report::finding `Finding::words`); what it counts as is its rule's
//! `Kind`; the default view groups those of a `Kind::Grouped` rule a line a cause key
//! (blaze_report::query `groups`). (7 and 8, for 5 and 6: a body's note was its words, and a
//! body under a shared cause class 6's sized fields; "BEF3" to "BEF6" records are refused so.)
//!
//! A TOKEN-PLACED finding (3, lane K1) is a body's whose place is its tokens `first..=last` in
//! its item (and with `inside` 1, the bytes `inside.0..inside.1` of the one token's lexeme;
//! both 0 when `inside` is 0): a comment or a re-format inside the body moves no token, so the
//! body's key stands and its stored finding prints where its tokens are today (print.rs
//! `Place::Tokens`).
//!
//! A finding's words are its record's message as stored (a names record's place pieces in
//! them); a finding with a path is an outline's, placed at its file's start. A PLACED finding
//! (2) is a body's: `start..end` from its item's start (rule 71), the item named by its chain
//! (work.rs `item_chain`) in the outline of `edition`, found in the file as it is when printed,
//! so a finding that stands while lines above its item moved is drawn where the item is now.
//! A block's entries are in the order its maker sorted them (a body's by start, end, rule and
//! words: rule 25). (2: the placed finding; a "BEF1" record is refused as another form, and the
//! request is answered in full.) (3: the token-placed finding, and a WAITED note's words are its
//! cause's name and its crate, not_checked/waited.rs `words`; a "BEF2" record is refused so.)
//! (5 and 6, for 3 and 4: a note of class 6, a body not checked under a refused macro call,
//! printed a line a call; "BEF3" and "BEF4" records are refused so.)
//!
//! A waited note ([`Class::Waited`]) is never printed alone: [`print_all`] groups the waiters by
//! the task their chain of waits ends at and prints, a cause, one line naming it and its reason
//! (its own failure note's words) with what hangs from it.

use core::fmt;
use core::cell::RefCell;

use blaze_drive::{Condition, Kind, Key, Name, Print};
use blaze_engine::{SMap, SSet, SVec};
use blaze_ids::ContentId;
use blaze_own::{SharedAlloc, Vec};
use bstd::Cx;
use bstd::fs::{Dir, ReadWrite};
use bstd::mem::Frame;
use bstd::out::Out;

use crate::print::RenderOutput;
use crate::{CliError, print, work};

/// The record's form.
pub(crate) const LINES_MAGIC: &[u8; 4] = b"BEFB";
/// The record's form with its placement digest (module docs).
const LINES_PLACED_MAGIC: &[u8; 4] = b"BEFC";
/// The forms before a body's note was a noted finding (module docs): refused, never migrated.
const OLDER_FORMS: [&[u8; 4]; 4] = [b"BEF3", b"BEF4", b"BEF5", b"BEF6"];
/// The forms before a body's row named its view (D191, [`VIEW`]): refused, never migrated.
const PRE_VIEW_FORMS: [&[u8; 4]; 2] = [b"BEF7", b"BEF8"];
/// The forms before a lint unit's finding was placed in its own text ([`PLACED_OWN`]): refused,
/// never migrated (their lint blocks hold ranges with children's lengths in them).
const PRE_OWN_FORMS: [&[u8; 4]; 2] = [b"BEF9", b"BEFA"];
const LINES_DOMAIN: &str = "blaze-cli engine-check lines";
/// A finding with no path of its own.
const NO_PATH: u32 = u32::MAX;
const FINDING: u8 = 0;
const NOTE: u8 = 1;
const PLACED: u8 = 2;
const PLACED_TOKENS: u8 = 3;
const NOTED: u8 = 4;
const VIEW: u8 = 5;
const PLACED_OWN: u8 = 6;

/// One unit a body's rows are at, as a [`VIEW`] entry names it: a test build or not, and its
/// crate's name.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct ViewUnit<'n> {
    pub(crate) test: bool,
    pub(crate) name: &'n str,
}

/// The view a block's row is at ([`VIEW`]): its body's base name, its place among the units
/// that reach the body's file (0: the base row), and those units' labels as stored.
#[derive(Clone, Copy)]
struct RowView<'m> {
    base: Name,
    at: u32,
    units: u32,
    labels: &'m [u8],
}

impl<'m> RowView<'m> {
    /// Each unit's label, in rank.
    fn units(&self) -> impl Iterator<Item = ViewUnit<'m>> {
        let mut reader = Reader { bytes: self.labels, at: 0 };
        (0..self.units).map_while(move |_| {
            let test = reader.byte("a view's unit flag").ok()? == 1;
            let name = core::str::from_utf8(reader.sized("a view's unit name").ok()?).ok()?;
            Some(ViewUnit { test, name })
        })
    }
}

/// The words after a finding only some of its body's rows hold (D191): "(in <views>)" naming
/// the rows that hold it, or, when the base row holds it, "(not in <views>)" naming those that
/// do not: a test build is "the test build of `<crate>`" (after "in") or "the test build"
/// (after "not in"), another unit "`<crate>`". `held`: one flag a unit, in rank.
fn view_words(view: &RowView<'_>, held: &[bool], into: &mut blaze_own::String<&Frame>) {
    use core::fmt::Write as _;
    let base_holds = held.first().copied().unwrap_or(false);
    let _ = write!(into, " ({}", if base_holds { "not in " } else { "in " });
    let mut first = true;
    for (unit, holds) in view.units().zip(held.iter().copied()) {
        if holds == base_holds {
            continue;
        }
        if !first {
            into.push_str(" and ");
        }
        first = false;
        let _ = match (unit.test, base_holds) {
            (true, false) => write!(into, "the test build of `{}`", unit.name),
            (true, true) => write!(into, "the test build"),
            (false, _) => write!(into, "`{}`", unit.name),
        };
    }
    into.push_str(")");
}

/// What a note counts as in the command's status (main.rs `close`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Class {
    /// A task that failed: printed, and the command fails.
    Failure = 0,
    /// An item the check could not check: printed, counted apart (main.rs `UNCHECKED_ITEMS`).
    Unchecked = 1,
    /// A task that waited on another's failure: a failure, counted under one line.
    Waited = 2,
    /// A head that could not be frozen: its words are its cell, head and reason
    /// (engine_head_notes.rs `words`), printed grouped by reason after the other notes
    /// (engine_head_notes.rs `print_grouped`), counted apart ([`Tally::heads`]).
    Head = 3,
    /// Said, counted as nothing: a fact of the plan, not of a check (engine_body_plan.rs: a file
    /// no unit of its crate holds under its cfg). Printed; fails nothing, leaves nothing unchecked.
    Told = 4,
    /// A body whose TYPES were checked whole and whose move and borrow check was refused
    /// (engine_bodies.rs `Said::BorrowsUnchecked`): it leaves the borrows stage not whole
    /// ([`Tally::borrows`]) and never the types stage ([`not_checked_whole`]); it fails nothing and is no item left unchecked
    /// (the closing line's "NOT checked: borrows" says it, status 3). Printed as the old path
    /// prints a `BORROWS_NOT_CHECKED` note (print.rs `finding`): one by one under `--each`, or
    /// always once the borrow check is trusted (`BORROWS_ARE_ERRORS`); counted always.
    Borrows = 5,
    // 6 was `Under`, a body not checked under a shared cause: a noted finding of a
    // `Kind::Grouped` rule now (module docs, D164), grouped by blaze_report::query.
}

impl Class {
    fn of_byte(byte: u8) -> Option<Class> {
        match byte {
            0 => Some(Class::Failure),
            1 => Some(Class::Unchecked),
            2 => Some(Class::Waited),
            3 => Some(Class::Head),
            4 => Some(Class::Told),
            5 => Some(Class::Borrows),
            _ => None,
        }
    }
}

/// Why a lines record was refused, at which byte (rule 19).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct LinesError {
    pub(crate) at: usize,
    pub(crate) what: &'static str,
}

impl fmt::Display for LinesError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the printed lines' record (engine_lines.rs) is refused at byte {}: {}", self.at, self.what)
    }
}

/// One task's block of entries inside [`Lines`]: either borrowed straight from the previous
/// request's mapped lines record (zero allocation and zero copy for standing rows) or owned in
/// the request's arena when made or appended to this request.
struct Block<'a> {
    borrowed: &'a [u8],
    owned: SVec<'a, u8>,
}

impl<'a> Block<'a> {
    fn as_slice(&self) -> &[u8] {
        if self.owned.is_empty() { self.borrowed } else { self.owned.as_slice() }
    }

    fn len(&self) -> usize {
        self.as_slice().len()
    }

    fn owned_mut(&mut self) -> &mut SVec<'a, u8> {
        if !self.borrowed.is_empty() {
            self.owned.extend_from_slice(self.borrowed);
            self.borrowed = &[];
        }
        &mut self.owned
    }
}

/// The blocks of one request, being made: each task's entries by name.
pub(crate) struct Lines<'a> {
    blocks: SMap<'a, Name, Block<'a>>,
    alloc: SharedAlloc<'a>,
}

fn word(out: &mut SVec<'_, u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn len_of(bytes: &[u8]) -> u32 {
    u32::try_from(bytes.len()).unwrap_or(u32::MAX)
}

impl<'a> Lines<'a> {
    pub(crate) fn new_in(alloc: SharedAlloc<'a>) -> Lines<'a> {
        Lines { blocks: SMap::new_in(alloc), alloc }
    }

    fn block(&mut self, name: Name) -> &mut SVec<'a, u8> {
        let alloc = self.alloc;
        self.blocks.entry(name).or_insert_with(|| Block { borrowed: &[], owned: SVec::new_in(alloc) }).owned_mut()
    }

    /// A finding of `name`'s record: `path` for an outline's (placed at the file's start).
    pub(crate) fn finding(&mut self, name: Name, rule: u16, start: u32, end: u32, path: Option<&[u8]>, words: &[u8]) {
        let block = self.block(name);
        block.push(FINDING);
        block.extend_from_slice(&rule.to_le_bytes());
        word(block, start);
        word(block, end);
        match path {
            Some(path) => {
                word(block, len_of(path));
                block.extend_from_slice(path);
            }
            None => word(block, NO_PATH),
        }
        word(block, len_of(words));
        block.extend_from_slice(words);
    }

    /// A finding of `name` (a body's) placed in an item of the file at `path`: `start..end` from
    /// the item's start, the item the one whose chain is `chain` (work.rs `item_chain`) in the
    /// file's outline of `edition`.
    #[expect(clippy::too_many_arguments, reason = "one entry's fields, each written as it is stored")]
    pub(crate) fn placed(&mut self, name: Name, rule: u16, start: u32, end: u32, path: &[u8], chain: u64, edition: blaze_surface::Edition, words: &[u8]) {
        let block = self.block(name);
        block.push(PLACED);
        block.extend_from_slice(&rule.to_le_bytes());
        word(block, start);
        word(block, end);
        word(block, len_of(path));
        block.extend_from_slice(path);
        block.extend_from_slice(&chain.to_le_bytes());
        block.push(edition.byte());
        word(block, len_of(words));
        block.extend_from_slice(words);
    }

    /// A finding of `name` (a body's) placed by its tokens `at` in an item of the file at `path`
    /// (lane K1): the item the one whose chain is `chain` in the file's outline of `edition`.
    #[expect(clippy::too_many_arguments, reason = "one entry's fields, each written as it is stored")]
    pub(crate) fn placed_tokens(&mut self, name: Name, rule: u16, at: parser::token_key::TokenPlace, path: &[u8], chain: u64, edition: blaze_surface::Edition, words: &[u8]) {
        let block = self.block(name);
        block.push(PLACED_TOKENS);
        block.extend_from_slice(&rule.to_le_bytes());
        word(block, at.first);
        word(block, at.last);
        let (inside, (from, to)) = match at.inside {
            Some(inside) => (1u8, inside),
            None => (0u8, (0, 0)),
        };
        block.push(inside);
        word(block, from);
        word(block, to);
        word(block, len_of(path));
        block.extend_from_slice(path);
        block.extend_from_slice(&chain.to_le_bytes());
        block.push(edition.byte());
        word(block, len_of(words));
        block.extend_from_slice(words);
    }

    /// A lint unit's finding of `name` (module docs, [`PLACED_OWN`]): `start..end` a range of the
    /// unit's own text, the unit the item whose chain is `chain` in the outline of `edition` of
    /// the file at `path` (`None`: the file's unit).
    #[expect(clippy::too_many_arguments, reason = "one entry's fields, each written as it is stored")]
    pub(crate) fn placed_own(&mut self, name: Name, rule: u16, start: u32, end: u32, path: &[u8], chain: Option<u64>, edition: blaze_surface::Edition, words: &[u8]) {
        let block = self.block(name);
        block.push(PLACED_OWN);
        block.extend_from_slice(&rule.to_le_bytes());
        word(block, start);
        word(block, end);
        word(block, len_of(path));
        block.extend_from_slice(path);
        block.push(u8::from(chain.is_some()));
        block.extend_from_slice(&chain.unwrap_or(0).to_le_bytes());
        block.push(edition.byte());
        word(block, len_of(words));
        block.extend_from_slice(words);
    }

    /// A note of `name`, counted as `class`.
    pub(crate) fn note(&mut self, name: Name, class: Class, words: &[u8]) {
        let block = self.block(name);
        block.push(NOTE);
        block.push(class as u8);
        word(block, len_of(words));
        block.extend_from_slice(words);
    }

    /// A noted finding of `name` (module docs): `subject` noted for `cause`, the rule the
    /// cause's, its words made only when it is printed.
    pub(crate) fn noted(&mut self, name: Name, subject: blaze_report::finding::Subject<'_>, cause: blaze_report::finding::Cause<'_>) {
        let block = self.block(name);
        block.push(NOTED);
        let at = block.len();
        word(block, 0);
        blaze_report::finding::encode(subject, cause, &mut |piece: &[u8]| block.extend_from_slice(piece));
        let len = len_of(&block[at + 4..]);
        block[at..at + 4].copy_from_slice(&len.to_le_bytes());
    }

    /// The view `name`'s row is at ([`VIEW`], D191): the row at place `at` among `units` (the
    /// units that reach its body's file, in rank, two or more) of body `base`. Written first in
    /// the row's block, before its findings, when the row has any.
    pub(crate) fn view(&mut self, name: Name, base: Name, at: u32, units: &[ViewUnit<'_>]) {
        let block = self.block(name);
        block.push(VIEW);
        block.push(base.kind as u8);
        block.extend_from_slice(&base.subject.0);
        word(block, at);
        word(block, u32::try_from(units.len()).unwrap_or(u32::MAX));
        for unit in units.iter() {
            block.push(u8::from(unit.test));
            word(block, len_of(unit.name.as_bytes()));
            block.extend_from_slice(unit.name.as_bytes());
        }
    }

    /// A standing row's block, as the last record holds it (borrowed in place when `name` has no
    /// entries yet: zero allocation and zero copy).
    pub(crate) fn keep(&mut self, name: Name, entries: &'a [u8]) {
        let alloc = self.alloc;
        match self.blocks.get_mut(&name) {
            Some(slot) => slot.owned_mut().extend_from_slice(entries),
            None => {
                self.blocks.insert(name, Block { borrowed: entries, owned: SVec::new_in(alloc) });
            }
        }
    }

    /// Of a standing row's block as the last record holds it, the entries that are a
    /// [`Class::Told`] note (`told`), or every other entry (`!told`), in their order; the count
    /// kept. A `FileItems` block's Told note is the tail's, never its outline's (engine_check.rs
    /// `lines_of`: a target its features skip): the phase path makes it again from its walk,
    /// the ready path carries it from the last record, and the block's other entries stand with
    /// its row. Nothing is added for `name` when no entry is kept.
    pub(crate) fn keep_split(&mut self, name: Name, entries: &'a [u8], told: bool) -> Result<usize, LinesError> {
        let mut reader = Reader { bytes: entries, at: 0 };
        let mut kept = 0usize;
        let mut total = 0usize;
        while reader.at < entries.len() {
            let is_told = matches!(entry(&mut reader)?, Entry::Note { class: Class::Told, .. });
            total += 1;
            if is_told == told {
                kept += 1;
            }
        }
        if kept == 0 {
            return Ok(0);
        }
        if kept == total {
            self.keep(name, entries);
            return Ok(kept);
        }
        let mut reader = Reader { bytes: entries, at: 0 };
        while reader.at < entries.len() {
            let from = reader.at;
            let is_told = matches!(entry(&mut reader)?, Entry::Note { class: Class::Told, .. });
            if is_told == told
                && let Some(slice) = entries.get(from..reader.at)
            {
                self.block(name).extend_from_slice(slice);
            }
        }
        Ok(kept)
    }

    /// Of a standing row's block as the last record holds it, keep only entries whose finding
    /// rule equals `rule` (`matching == true`) or every other entry (`matching == false`), in
    /// order. Used to keep manifest task findings separate from tail-computed `UNUSED_DEPENDENCY`
    /// warnings across incremental runs.
    pub(crate) fn keep_split_rule(&mut self, name: Name, entries: &'a [u8], rule: u16, matching: bool) -> Result<usize, LinesError> {
        let mut reader = Reader { bytes: entries, at: 0 };
        let mut kept = 0usize;
        let mut total = 0usize;
        while reader.at < entries.len() {
            let is_rule = matches!(entry(&mut reader)?, Entry::Finding { rule: r, .. } if r == rule);
            total += 1;
            if is_rule == matching {
                kept += 1;
            }
        }
        if kept == 0 {
            return Ok(0);
        }
        if kept == total {
            self.keep(name, entries);
            return Ok(kept);
        }
        let mut reader = Reader { bytes: entries, at: 0 };
        while reader.at < entries.len() {
            let from = reader.at;
            let is_rule = matches!(entry(&mut reader)?, Entry::Finding { rule: r, .. } if r == rule);
            if is_rule == matching
                && let Some(slice) = entries.get(from..reader.at)
            {
                self.block(name).extend_from_slice(slice);
            }
        }
        Ok(kept)
    }

    /// The blocks in name order.
    pub(crate) fn blocks(&self) -> impl Iterator<Item = (Name, &[u8])> + Clone {
        self.blocks.iter().map(|(name, block)| (*name, block.as_slice()))
    }

    /// The record's bytes and its key (of its bytes: two requests that print alike write one).
    pub(crate) fn record(&self) -> (Key, SVec<'a, u8>) {
        self.encode(None)
    }

    fn record_for(&self, placement: ContentId) -> (PlacedLines, SVec<'a, u8>) {
        let (key, bytes) = self.encode(Some(placement));
        (PlacedLines { key, placement }, bytes)
    }

    fn encode(&self, placement: Option<ContentId>) -> (Key, SVec<'a, u8>) {
        let header_bytes = if placement.is_some() { 36 } else { 4 };
        let cap = header_bytes + 4 + self.blocks.values().map(|block| 1 + 32 + 4 + block.len()).sum::<usize>();
        let mut out = SVec::with_capacity_in(cap, self.alloc);
        match placement {
            Some(id) => {
                out.extend_from_slice(LINES_PLACED_MAGIC);
                out.extend_from_slice(&id.0);
            }
            None => out.extend_from_slice(LINES_MAGIC),
        }
        word(&mut out, u32::try_from(self.blocks.len()).unwrap_or(u32::MAX));
        for (name, block) in self.blocks.iter() {
            let bytes = block.as_slice();
            out.push(name.kind as u8);
            out.extend_from_slice(&name.subject.0);
            word(&mut out, len_of(bytes));
            out.extend_from_slice(bytes);
        }
        (Key(ContentId::of(LINES_DOMAIN, &out)), out)
    }

    /// Whether `self`'s blocks match the serialized `blocks u32 | per block` tail of a stored
    /// lines record byte-for-byte, without allocating or hashing.
    fn blocks_equal_encoded(&self, encoded_blocks: &[u8]) -> bool {
        let mut reader = Reader { bytes: encoded_blocks, at: 0 };
        let Ok(count) = reader.u32("the block count") else { return false };
        if usize::try_from(count).ok() != Some(self.blocks.len()) {
            return false;
        }
        for (name, block) in self.blocks.iter() {
            let Ok(kind) = reader.byte("a block's kind") else { return false };
            if Kind::from_u8(kind) != Some(name.kind) {
                return false;
            }
            let Ok(subject) = reader.take(32, "a block's subject") else { return false };
            if subject != name.subject.0 {
                return false;
            }
            let Ok(stored) = reader.sized("a block's entries") else { return false };
            if stored != block.as_slice() {
                return false;
            }
        }
        reader.at == encoded_blocks.len()
    }
}

/// A lines record read: each block by name, sorted, every entry checked once here so the
/// printer never stops half way.
pub(crate) struct Last<'m, 'a> {
    blocks: SVec<'a, (Name, &'m [u8])>,
    placement: Option<ContentId>,
}

impl<'m> Last<'m, '_> {
    /// `name`'s block, when the last request printed anything of it.
    pub(crate) fn block(&self, name: Name) -> Option<&'m [u8]> {
        self.blocks.binary_search_by_key(&name, |(at, _)| *at).ok().map(|at| self.blocks[at].1)
    }

    /// The stored refusal of the body `name` (a failure, an unchecked item or a refused borrow
    /// check), when the last request noted one: what `blaze why` says a standing refusal is
    /// (FR1: a refused body stands on its reads until one moves). A query over the block's
    /// findings: its first noted one, said as `--each` says it.
    pub(crate) fn body_refusal(&self, name: Name) -> Result<Option<Refusal<'m>>, LinesError> {
        let Some(bytes) = self.block(name) else { return Ok(None) };
        let mut reader = Reader { bytes, at: 0 };
        while reader.at < bytes.len() {
            match entry(&mut reader)? {
                Entry::Note { class: Class::Failure | Class::Unchecked | Class::Borrows, words } => {
                    return core::str::from_utf8(words)
                        .map(|words| Some(Refusal::Note(words)))
                        .map_err(|_| LinesError { at: reader.at, what: "a body's refusal note is not UTF-8" });
                }
                Entry::Noted { finding } => return Ok(Some(Refusal::Finding(finding))),
                _ => {}
            }
        }
        Ok(None)
    }

    /// The blocks in name order.
    pub(crate) fn blocks(&self) -> impl Iterator<Item = (Name, &'m [u8])> + Clone + '_ {
        self.blocks.iter().copied()
    }

    /// The blocks of `kind`'s names, in name order: one range of the sorted blocks (a name
    /// orders by its kind first), found by two binary searches, never a scan of the record.
    pub(crate) fn blocks_of(&self, kind: Kind) -> &[(Name, &'m [u8])] {
        let from = self.blocks.partition_point(|(name, _)| name.kind < kind);
        let to = self.blocks.partition_point(|(name, _)| name.kind <= kind);
        &self.blocks[from..to]
    }
}

/// Whether `block` holds a [`Class::Told`] note (engine_check.rs `ready_setup`: a target its
/// features skip has its note in the last record, which the ready path carries).
pub(crate) fn has_told(block: &[u8]) -> Result<bool, LinesError> {
    let mut reader = Reader { bytes: block, at: 0 };
    while reader.at < block.len() {
        if let Entry::Note { class: Class::Told, .. } = entry(&mut reader)? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// A cursor over a record's bytes.
struct Reader<'m> {
    bytes: &'m [u8],
    at: usize,
}

impl<'m> Reader<'m> {
    fn digest(&mut self, what: &'static str) -> Result<ContentId, LinesError> {
        let mut bytes = [0; 32];
        bytes.copy_from_slice(self.take(32, what)?);
        Ok(ContentId(bytes))
    }

    fn take(&mut self, n: usize, what: &'static str) -> Result<&'m [u8], LinesError> {
        let end = self.at.checked_add(n).filter(|end| *end <= self.bytes.len()).ok_or(LinesError { at: self.at, what })?;
        let taken = &self.bytes[self.at..end];
        self.at = end;
        Ok(taken)
    }

    fn byte(&mut self, what: &'static str) -> Result<u8, LinesError> {
        Ok(self.take(1, what)?[0])
    }

    fn u16(&mut self, what: &'static str) -> Result<u16, LinesError> {
        let b = self.take(2, what)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    fn u32(&mut self, what: &'static str) -> Result<u32, LinesError> {
        let b = self.take(4, what)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn u64(&mut self, what: &'static str) -> Result<u64, LinesError> {
        let mut b = [0u8; 8];
        b.copy_from_slice(self.take(8, what)?);
        Ok(u64::from_le_bytes(b))
    }

    fn sized(&mut self, what: &'static str) -> Result<&'m [u8], LinesError> {
        let len = self.u32(what)?;
        self.take(len as usize, what)
    }
}

/// One entry of a block.
enum Entry<'m> {
    Finding { rule: u16, start: u32, end: u32, path: Option<&'m [u8]>, words: &'m [u8] },
    Note { class: Class, words: &'m [u8] },
    /// A body's finding, placed by its item's chain (module docs).
    Placed { rule: u16, start: u32, end: u32, path: &'m str, chain: u64, edition: blaze_surface::Edition, words: &'m [u8] },
    /// A body's finding placed by its tokens in its item (module docs, lane K1).
    PlacedTokens { rule: u16, at: parser::token_key::TokenPlace, path: &'m str, chain: u64, edition: blaze_surface::Edition, words: &'m [u8] },
    /// A lint unit's finding, its range of the unit's own text (module docs, [`PLACED_OWN`]).
    PlacedOwn { rule: u16, start: u32, end: u32, path: &'m str, chain: Option<u64>, edition: blaze_surface::Edition, words: &'m [u8] },
    /// A body's note as a record (module docs, D164): read whole, every text UTF-8.
    Noted { finding: blaze_report::finding::Finding<'m> },
    /// The view the block's row is at (module docs, D191): every label checked here.
    View(RowView<'m>),
}

fn entry<'m>(reader: &mut Reader<'m>) -> Result<Entry<'m>, LinesError> {
    match reader.byte("an entry's tag")? {
        FINDING => {
            let rule = reader.u16("a finding's rule")?;
            let start = reader.u32("a finding's start")?;
            let end = reader.u32("a finding's end")?;
            let path = match reader.u32("a finding's path length")? {
                NO_PATH => None,
                len => Some(reader.take(len as usize, "a finding's path")?),
            };
            if path.is_some_and(|path| core::str::from_utf8(path).is_err()) {
                return Err(LinesError { at: reader.at, what: "a finding's path is not UTF-8" });
            }
            let words = reader.sized("a finding's words")?;
            Ok(Entry::Finding { rule, start, end, path, words })
        }
        NOTE => {
            let at = reader.at;
            let class = Class::of_byte(reader.byte("a note's class")?).ok_or(LinesError { at, what: "a note's class is none of the six" })?;
            let words = reader.sized("a note's words")?;
            if core::str::from_utf8(words).is_err() {
                return Err(LinesError { at: reader.at, what: "a note's words are not UTF-8" });
            }
            Ok(Entry::Note { class, words })
        }
        NOTED => {
            let at = reader.at + 4;
            let bytes = reader.sized("a noted finding")?;
            let finding = blaze_report::finding::decode(bytes).map_err(|why| LinesError { at: at + why.at, what: why.what })?;
            Ok(Entry::Noted { finding })
        }
        PLACED => {
            let rule = reader.u16("a placed finding's rule")?;
            let start = reader.u32("a placed finding's start")?;
            let end = reader.u32("a placed finding's end")?;
            let at = reader.at;
            let path = core::str::from_utf8(reader.sized("a placed finding's path")?).map_err(|_| LinesError { at, what: "a placed finding's path is not UTF-8" })?;
            let chain = reader.u64("a placed finding's item chain")?;
            let at = reader.at;
            let edition = blaze_surface::Edition::from_byte(reader.byte("a placed finding's edition")?).ok_or(LinesError { at, what: "a placed finding's edition byte is no edition" })?;
            let words = reader.sized("a placed finding's words")?;
            Ok(Entry::Placed { rule, start, end, path, chain, edition, words })
        }
        PLACED_TOKENS => {
            let rule = reader.u16("a token-placed finding's rule")?;
            let first = reader.u32("a token-placed finding's first token")?;
            let last = reader.u32("a token-placed finding's last token")?;
            let at = reader.at;
            let inside = reader.byte("a token-placed finding's inside flag")?;
            let from = reader.u32("a token-placed finding's inside start")?;
            let to = reader.u32("a token-placed finding's inside end")?;
            let inside = match inside {
                0 if (from, to) == (0, 0) => None,
                1 => Some((from, to)),
                _ => return Err(LinesError { at, what: "a token-placed finding's inside flag is neither 0 (with no range) nor 1" }),
            };
            let at_path = reader.at;
            let path = core::str::from_utf8(reader.sized("a token-placed finding's path")?).map_err(|_| LinesError { at: at_path, what: "a token-placed finding's path is not UTF-8" })?;
            let chain = reader.u64("a token-placed finding's item chain")?;
            let at_edition = reader.at;
            let edition = blaze_surface::Edition::from_byte(reader.byte("a token-placed finding's edition")?).ok_or(LinesError { at: at_edition, what: "a token-placed finding's edition byte is no edition" })?;
            let words = reader.sized("a token-placed finding's words")?;
            Ok(Entry::PlacedTokens { rule, at: parser::token_key::TokenPlace { first, last, inside }, path, chain, edition, words })
        }
        PLACED_OWN => {
            let rule = reader.u16("a lint unit's finding's rule")?;
            let start = reader.u32("a lint unit's finding's start")?;
            let end = reader.u32("a lint unit's finding's end")?;
            let at = reader.at;
            let path = core::str::from_utf8(reader.sized("a lint unit's finding's path")?).map_err(|_| LinesError { at, what: "a lint unit's finding's path is not UTF-8" })?;
            let at = reader.at;
            let in_item = reader.byte("a lint unit's finding's item flag")?;
            let chain = reader.u64("a lint unit's finding's item chain")?;
            let chain = match (in_item, chain) {
                (0, 0) => None,
                (1, chain) => Some(chain),
                _ => return Err(LinesError { at, what: "a lint unit's finding's item flag is neither 0 (with no chain) nor 1" }),
            };
            let at = reader.at;
            let edition = blaze_surface::Edition::from_byte(reader.byte("a lint unit's finding's edition")?).ok_or(LinesError { at, what: "a lint unit's finding's edition byte is no edition" })?;
            let words = reader.sized("a lint unit's finding's words")?;
            Ok(Entry::PlacedOwn { rule, start, end, path, chain, edition, words })
        }
        VIEW => {
            let at_kind = reader.at;
            let kind = Kind::from_u8(reader.byte("a view's base kind")?).ok_or(LinesError { at: at_kind, what: "a view's base kind is no task kind" })?;
            let base = Name::new(kind, reader.digest("a view's base subject")?);
            let at_place = reader.at;
            let at = reader.u32("a view's place")?;
            let units = reader.u32("a view's unit count")?;
            if units < 2 || at >= units {
                return Err(LinesError { at: at_place, what: "a view's place is not below its unit count, or the count is under two" });
            }
            let from = reader.at;
            for _ in 0..units {
                let at_flag = reader.at;
                if reader.byte("a view's unit flag")? > 1 {
                    return Err(LinesError { at: at_flag, what: "a view's unit flag is neither 0 nor 1" });
                }
                let at_name = reader.at;
                if core::str::from_utf8(reader.sized("a view's unit name")?).is_err() {
                    return Err(LinesError { at: at_name, what: "a view's unit name is not UTF-8" });
                }
            }
            Ok(Entry::View(RowView { base, at, units, labels: &reader.bytes[from..reader.at] }))
        }
        _ => Err(LinesError { at: reader.at - 1, what: "an entry's tag is none of finding, note, placed finding, token-placed finding, noted finding, view and own-placed finding" }),
    }
}

/// The record `bytes`, every block and entry checked.
pub(crate) fn decode<'m, 'a>(bytes: &'m [u8], alloc: SharedAlloc<'a>) -> Result<Last<'m, 'a>, LinesError> {
    let mut reader = Reader { bytes, at: 0 };
    let placement = match reader.take(4, "the form")? {
        form if form == LINES_PLACED_MAGIC => Some(reader.digest("the placement facts")?),
        form if form == LINES_MAGIC => None,
        // Before a body's note was a noted finding (D164) it was its words: refused as an older
        // form, and the request is answered in full (rule 26), as BEF1 and BEF2 were.
        form if OLDER_FORMS.iter().any(|old| form == *old) => return Err(LinesError { at: 0, what: "the form is BEF3 to BEF6, older than BEFB and BEFC (a body's note was its words, not a noted finding)" }),
        // Before a body's rows at its units' views named their view (D191): refused, and the
        // request is answered in full (rule 26).
        form if PRE_VIEW_FORMS.iter().any(|old| form == *old) => return Err(LinesError { at: 0, what: "the form is BEF7 or BEF8, older than BEFB and BEFC (a body was one row, checked under its first holder: D191)" }),
        // Before a lint unit's finding was placed in its own text: refused, and the request is
        // answered in full (rule 26).
        form if PRE_OWN_FORMS.iter().any(|old| form == *old) => return Err(LinesError { at: 0, what: "the form is BEF9 or BEFA, older than BEFB and BEFC (a lint unit's finding held its children's lengths)" }),
        _ => return Err(LinesError { at: 0, what: "the form is neither BEFB nor BEFC" }),
    };
    let count = reader.u32("the block count")?;
    let mut blocks: SVec<'a, (Name, &'m [u8])> = SVec::new_in(alloc);
    for _ in 0..count {
        let at = reader.at;
        let kind = Kind::from_u8(reader.byte("a block's kind")?).ok_or(LinesError { at, what: "a block's kind is no task kind" })?;
        let mut subject = [0; 32];
        subject.copy_from_slice(reader.take(32, "a block's subject")?);
        let name = Name::new(kind, ContentId(subject));
        if blocks.last().is_some_and(|(last, _)| *last >= name) {
            return Err(LinesError { at, what: "the blocks are not sorted by name, one each" });
        }
        let block = reader.sized("a block's entries")?;
        let mut inner = Reader { bytes: block, at: 0 };
        while inner.at < block.len() {
            entry(&mut inner).map_err(|why| LinesError { at: reader.at - block.len() + why.at, what: why.what })?;
        }
        blocks.push((name, block));
    }
    if reader.at != bytes.len() {
        return Err(LinesError { at: reader.at, what: "bytes follow the last block" });
    }
    Ok(Last { blocks, placement })
}

/// Each [`Class::Head`] note's words of `block`, in order (engine_head_notes.rs).
pub(crate) fn head_notes<'m>(block: &'m [u8], each: &mut dyn FnMut(&'m str)) -> Result<(), LinesError> {
    let mut reader = Reader { bytes: block, at: 0 };
    while reader.at < block.len() {
        if let Entry::Note { class: Class::Head, words } = entry(&mut reader)? {
            each(core::str::from_utf8(words).map_err(|_| LinesError { at: reader.at, what: "a note's words are not UTF-8" })?);
        }
    }
    Ok(())
}

/// Each noted finding of `block`, in order (D164): the records a view or a test queries
/// (tests/world_run.rs `noted`), read from the stored record, so a certificate's answer gives
/// the same findings as the request that wrote it.
pub(crate) fn noted<'m>(block: &'m [u8], each: &mut dyn FnMut(blaze_report::finding::Finding<'m>)) -> Result<(), LinesError> {
    let mut reader = Reader { bytes: block, at: 0 };
    while reader.at < block.len() {
        if let Entry::Noted { finding } = entry(&mut reader)? {
            each(finding);
        }
    }
    Ok(())
}

/// Whether `block` holds a note that says its task's item was not TYPE-checked whole: a
/// [`Class::Failure`], [`Class::Unchecked`] or [`Class::Waited`] note, or a noted finding of
/// any kind but `Kind::BorrowsUnchecked` (a body refused, failed, panicked or not checked
/// whole, engine_bodies.rs `causes`). A finding of a checked body is not, and neither is a
/// [`Class::Borrows`] note or a noted refused borrow check: its types were checked, and its borrows not being
/// whole is counted by [`print_all`] (`Tally::borrows`) over every block, standing or entered,
/// so the borrows stage is told apart from the types stage (main.rs `check_coverage`; the
/// owner's rule: never a pass for a stage that did not run, and never a fail for one that did).
/// What a standing body row says of the answer's coverage (engine_check.rs `check`).
pub(crate) fn not_checked_whole(block: &[u8]) -> Result<bool, LinesError> {
    let mut reader = Reader { bytes: block, at: 0 };
    while reader.at < block.len() {
        match entry(&mut reader)? {
            Entry::Note { class: Class::Failure | Class::Unchecked | Class::Waited, .. } => return Ok(true),
            Entry::Noted { finding } => {
                let kind = finding.kind().map_err(|_| LinesError { at: reader.at, what: "a noted finding's rule has no row in blaze_report::frame::words::WORDINGS" })?;
                if kind != blaze_report::frame::words::Kind::BorrowsUnchecked {
                    return Ok(true);
                }
            }
            _ => {}
        }
    }
    Ok(false)
}

/// What [`print_all`] counted, for the command's status.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Tally {
    pub(crate) failures: u32,
    pub(crate) unchecked: u32,
    pub(crate) waited: u32,
    /// Bodies whose types were checked and whose borrows were not ([`Class::Borrows`]): no
    /// failure and no item unchecked; the borrows stage's coverage says them.
    pub(crate) borrows: u32,
    /// Heads that could not be frozen ([`Class::Head`]).
    pub(crate) heads: u32,
    /// Findings listed (after one finding of two tasks is folded into one).
    pub(crate) listed: u32,
    /// What printing held ([`Tail`]).
    pub(crate) tail: Tail,
    pub(crate) rendering: Rendering,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) enum Rendering {
    #[default]
    Semantic,
    /// `blocks` rows (a drawing or a file's section), `reused` of them written as the last view
    /// stored them; `old_form`: the last view was an older form (BER1), refused and rebuilt
    /// whole (rule 26: reported, never migrated).
    Published { blocks: usize, reused: usize, bytes: usize, old_form: bool },
    Replayed { bytes: usize },
}

impl Rendering {
    pub(crate) fn puts(self) -> u32 {
        u32::from(matches!(self, Self::Published { .. }))
    }
}

impl fmt::Display for Rendering {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Semantic => f.write_str("render: printed from semantic blocks"),
            Self::Published { blocks, reused, bytes, old_form: false } => write!(f, "render: published {bytes} bytes in {blocks} blocks, {reused} reused"),
            Self::Published { blocks, bytes, old_form: true, .. } => write!(f, "render: published {bytes} bytes in {blocks} blocks; the last view was an older form (BER1 or BER2), refused and rebuilt whole"),
            Self::Replayed { bytes } => write!(f, "render: transferred {bytes} stored bytes in one write_all, no source reopened"),
        }
    }
}

/// The tail's memory: what [`print_all`] held at its most. The lines are STREAMED (print.rs
/// [`print::Held`]): a file's text is read into a frame emptied per file and held only while
/// its findings are placed or drawn, a line is drawn in a frame emptied per line, and nothing
/// drawn is kept. So its bound is the floor: one file's work (its text, and its outline while
/// placing) and one line, at their largest: `peak <= floor`. The lists
/// a finding needs before any is drawn (its listing, for the ids' shown lengths) are the
/// findings' own size, said apart in `listing`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Tail {
    /// The scratch frames at their most, the largest text and the largest line.
    pub(crate) held: print::Held,
    /// The bytes the findings' lists reserved (prints, places, runs, the one-of-two set, the
    /// listing and the words kept): a finding's worth each, never a text's.
    pub(crate) listing: usize,
    /// The files placed and drawn, and the lexes their token places took (print.rs `Placing`).
    pub(crate) placing: print::Placing,
    /// The file sections written out as stored (BER2, [`Sectioned`]): their files not read.
    pub(crate) replayed: u32,
}

impl Tail {
    /// The profile's line beside "answer printed": "lines: files rendered R (drawn D), replayed
    /// P, lexed L (floor: R = the files whose text or findings moved; L <= R)". A file is lexed
    /// once an edition at most, whatever its count of token places (print.rs `list_observing`),
    /// so L <= R holds for a file of one edition. P is the file sections written out as stored
    /// (BER2, [`Sectioned`]): a file whose text, findings, ids' shown lengths and leading gap
    /// stand is not read.
    pub(crate) fn lines_line<'f>(&self, frame: &'f Frame) -> bstd::mem::String<&'f Frame> {
        let Self { placing, replayed, .. } = *self;
        bstd::mem::format_in!(
            frame,
            "lines: files rendered {} (drawn {}), replayed {replayed}, lexed {} (floor: R = the files whose text or findings moved; L <= R) (engine_lines.rs `Tail`)",
            placing.placed,
            placing.drawn,
            placing.lexed,
        )
    }

    /// The profile's line: "tail: peak B bytes (floor F; ratio R): one file's work W (its text
    /// T), one line L; listing B bytes for N finding(s)". Streamed, the ratio is at most 1.
    pub(crate) fn line<'f>(&self, findings: u32, frame: &'f Frame) -> bstd::mem::String<&'f Frame> {
        let held = self.held;
        let floor = held.floor();
        // The ratio in hundredths: no float in a record's words.
        let ratio = (held.peak as u128).saturating_mul(100).checked_div(floor as u128).unwrap_or(0);
        let mut line = bstd::mem::format_in!(
            frame,
            "tail: peak {} bytes (floor {floor}; ratio {}.{:02}): one file's work {} (its text {}), one line {}; listing {} bytes for {findings} finding(s) (engine_lines.rs `Tail`)",
            held.peak,
            ratio / 100,
            ratio % 100,
            held.file,
            held.text,
            held.line,
            self.listing,
        );
        // The files' line on its own line, under the tail's ([`Tail::lines_line`]).
        line.push_str("\n");
        line.push_str(self.lines_line(frame).as_str());
        line
    }
}

/// What makes two findings one when printed: their file, their item (its chain, or its start),
/// rule, range and words.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Seen<'p> {
    path: Option<&'p str>,
    at_item: (u8, u64),
    rule: u16,
    start: u32,
    end: u32,
    /// A token-placed finding's range inside its tokens: two that differ only there are two.
    inside: Option<(u32, u32)>,
    words: &'p [u8],
    /// The row, where a body's rows at their views are not folded ([`ViewFold`]); `None` else.
    row: Option<Name>,
}

/// WHETHER ONE BODY'S ROWS AT ITS UNITS' VIEWS PRINT AN EQUAL FINDING ONCE (D191, rule 78: the
/// one seam): [`FoldViews`], the command's: an equal finding (file, item, rule, range, words) of
/// a body's rows is one line, and one counted; a finding only some rows hold names them.
/// `EachRow` (tests only): each row's finding is its own line, the control that the fold is what
/// makes one line of two.
pub(crate) trait ViewFold {
    const FOLDS: bool;
}

/// The command's: a body's rows fold.
pub(crate) struct FoldViews;

impl ViewFold for FoldViews {
    const FOLDS: bool = true;
}

/// The planted control (tests only): no fold.
#[cfg(test)]
pub(crate) struct EachRow;

#[cfg(test)]
impl ViewFold for EachRow {
    const FOLDS: bool = false;
}

/// [`print_all`] with no fold of a body's rows ([`EachRow`]): what the fold's test compares.
#[cfg(test)]
pub(crate) fn print_all_each_row<'f, 'b, O: print::RenderOutput>(
    cx: &Cx,
    root: &Dir<ReadWrite>,
    blocks: impl Iterator<Item = (Name, &'b [u8])>,
    shown: &mut print::Shown<'f>,
    alloc: SharedAlloc<'_>,
    frame: &'f Frame,
    out: &O,
) -> Result<Tally, CliError> {
    print_with(cx, root, blocks, shown, alloc, frame, out, &mut Whole, EachRow)
}

/// Every block printed as `check` prints (main.rs `draw`): each note as it comes, then the
/// findings listed and drawn a file at a time; a names record's unresolved path, include outside
/// its package and failed call placed by their message's pieces; one finding two tasks hold
/// printed once, the second task named with it. The waited notes are grouped under the task
/// their chain of waits ends at, a line a cause, after the findings ([`waited_lines`]). The
/// lists are in `alloc`, the request's arena: given back with it.
pub(crate) fn print_all<'f, 'b, O: print::RenderOutput>(
    cx: &Cx,
    root: &Dir<ReadWrite>,
    blocks: impl Iterator<Item = (Name, &'b [u8])>,
    shown: &mut print::Shown<'f>,
    alloc: SharedAlloc<'_>,
    frame: &'f Frame,
    out: &O,
) -> Result<Tally, CliError> {
    print_with(cx, root, blocks, shown, alloc, frame, out, &mut Whole, FoldViews)
}

/// The findings of a request's files, by file: `runs` are (path, first print, past the last),
/// each file's findings neighbours, in the order they are printed.
struct Found<'l, 'p, 'f> {
    prints: &'l [Print<'p>],
    wanted: &'l [print::Wanted<'f>],
    runs: &'l [(Option<&'f str>, usize, usize)],
}

/// How [`print_with`] places and draws the files' findings (rule 78: the strategy is a type at
/// this one seam): [`Whole`], every file read, placed and drawn; or [`Sectioned`], a file's
/// stored section (BER2) replayed where its key stands and the rest as [`Whole`] does it. The
/// two print equal bytes and leave equal counts (`a_stored_file_section_replays_what_a_fresh_check_prints`).
trait Files<'f, O: print::RenderOutput> {
    /// Places every finding of `found` (`shown.listed` from its length now), sets the ids'
    /// shown lengths over all of them, and prints the runs in order.
    #[expect(clippy::too_many_arguments, reason = "the printer's inputs, each a different one")]
    fn print(&mut self, cx: &Cx, root: &Dir<ReadWrite>, found: &Found<'_, '_, 'f>, scratch: &mut print::Scratch, shown: &mut print::Shown<'f>, frame: &'f Frame, out: &O) -> Result<(), CliError>;

    /// The file sections replayed, not drawn ([`Tail::replayed`]).
    fn replayed(&self) -> u32 {
        0
    }
}

/// Every file read, placed and drawn: the fresh printer.
struct Whole;

impl<'f, O: print::RenderOutput> Files<'f, O> for Whole {
    fn print(&mut self, cx: &Cx, root: &Dir<ReadWrite>, found: &Found<'_, '_, 'f>, scratch: &mut print::Scratch, shown: &mut print::Shown<'f>, frame: &'f Frame, out: &O) -> Result<(), CliError> {
        let first = shown.listed.len();
        print::list_observing(cx, root, found.prints, found.wanted, scratch, frame, &mut shown.listed, |path, bytes| out.source(path, bytes))?;
        crate::fix::lengths(&mut shown.listed[first..], frame);
        for &(_, from, past) in found.runs.iter() {
            by_place(&mut shown.listed[first + from..first + past]);
        }
        for &(path, from, past) in found.runs.iter() {
            print::file_findings(cx, root, path, first + from..first + past, shown, scratch, out)?;
        }
        Ok(())
    }
}

/// [`print_all`] with the files printed by `files`.
/// `_fold`: whether a body's rows at its units' views fold ([`ViewFold`]).
#[expect(clippy::too_many_arguments, reason = "print_all's inputs and the two strategies")]
fn print_with<'f, 'b, O: print::RenderOutput, F: Files<'f, O>, VF: ViewFold>(
    cx: &Cx,
    root: &Dir<ReadWrite>,
    blocks: impl Iterator<Item = (Name, &'b [u8])>,
    shown: &mut print::Shown<'f>,
    alloc: SharedAlloc<'_>,
    frame: &'f Frame,
    out: &O,
    files: &mut F,
    _fold: VF,
) -> Result<Tally, CliError> {
    let erased = blaze_own::Erased::new(frame);
    let listed = erased.handle();
    let frame_at = frame.used();
    let listed_at = shown.listed.len();
    // One a finding or a file: the project's size, so in the request's arena, never the frame.
    let mut prints: SVec<'_, Print<'_>> = SVec::new_in(alloc);
    let mut wanted: SVec<'_, print::Wanted<'f>> = SVec::new_in(alloc);
    // (path, first print, past the last): each file's findings are neighbours.
    let mut runs: SVec<'_, (Option<&'f str>, usize, usize)> = SVec::new_in(alloc);
    let mut seen: SMap<'_, Seen<'f>, usize> = SMap::new_in(alloc);
    // The waiters, and the failures' own words by task: what a waiter's cause line names.
    let mut waits: Vec<crate::not_checked::waited::Wait<'f>, &'f Frame> = Vec::new_in(frame);
    let mut failed: SMap<'_, Name, &'f str> = SMap::new_in(alloc);
    // The noted findings of a `Kind::Grouped` rule (a body under a refused call or the plan's
    // refusal), printed a line a cause key ([`grouped_lines`]) unless `--each`.
    let mut grouped: Vec<blaze_report::finding::Finding<'f>, &'f Frame> = Vec::new_in(frame);
    let mut tally = Tally::default();
    let refused = |why: LinesError| CliError::Text { what: why.what };
    // D191, A BODY'S ROWS AT ITS UNITS' VIEWS: each body's view entry (its units' labels), by its
    // base name; each printed finding's rows (print, body, place among the units); and each note
    // of a body said once, by (body, its entry's bytes): a body is one item, whatever number of
    // its rows say the same.
    let mut views_of: SMap<'_, Name, RowView<'b>> = SMap::new_in(alloc);
    let mut held_by: SVec<'_, (usize, Name, u32)> = SVec::new_in(alloc);
    let mut said_once: SSet<'_, (Name, &'b [u8])> = SSet::new_in(alloc);
    // D191, A BODY IS COUNTED ONCE: the bodies (by base name) already counted not checked, and
    // those already counted with their borrows refused. Rows of one body whose notes differ in
    // bytes (each unit's item keys are its own) still print each note, but the summary's "could
    // not check N items" and "N bodies ... had the move and borrow check refused" count bodies,
    // never rows. A block with no view entry is a body one unit holds: counted as before.
    let mut unchecked_bodies: SSet<'_, Name> = SSet::new_in(alloc);
    let mut borrows_bodies: SSet<'_, Name> = SSet::new_in(alloc);
    fn first_of_body(bodies: &mut SSet<'_, Name>, view: Option<RowView<'_>>) -> bool {
        view.is_none_or(|at| bodies.insert(at.base))
    }
    for (name, block) in blocks {
        let mut reader = Reader { bytes: block, at: 0 };
        // A body is one item not checked, whatever number of notes say why.
        let mut counted = false;
        // The view this block's row is at (its first entry, when its body has views).
        let mut view: Option<RowView<'b>> = None;
        while reader.at < block.len() {
            let from = reader.at;
            let parsed = entry(&mut reader).map_err(refused)?;
            if let Entry::View(at) = parsed {
                views_of.entry(at.base).or_insert(at);
                view = Some(at);
                continue;
            }
            // A note of a row at a view that another row of its body said already: said once
            // (the fold, [`ViewFold`]).
            if VF::FOLDS
                && matches!(parsed, Entry::Noted { .. } | Entry::Note { .. })
                && let Some(at) = view
                && !said_once.insert((at.base, &block[from..reader.at]))
            {
                continue;
            }
            // A finding's rule, range and message, and where it is drawn: its file, its place and
            // the words drawn (the message but for its place pieces).
            // The printer reads a finding's words from its `Wanted` (print.rs `list`), never its
            // whole message: no second copy is made of it (the tail's memory).
            let (rule, start, end, _message, path, place, words): (u16, u32, u32, &[u8], Option<&'f str>, print::Place, &[u8]) = match parsed {
                Entry::View(_) => continue,
                // A body's note as a record: what it counts as and whether it is drawn alone are
                // its rule's `Kind` (D164); its words the table's, made here.
                Entry::Noted { finding } => {
                    use blaze_report::frame::words::Kind as Noted;
                    let kind = finding.kind().map_err(|_| CliError::Text { what: "a noted finding's rule has no row in blaze_report::frame::words::WORDINGS (engine_lines.rs `print_with`)" })?;
                    match kind {
                        Noted::Failure => {
                            tally.failures += 1;
                            let words = said(&finding, frame)?;
                            print::note(cx, words, shown, frame, out)?;
                            // A task's first failure is what its waiters' line names.
                            if !failed.contains_key(&name) {
                                failed.insert(name, words);
                            }
                        }
                        // An item not checked: counted once a body, whatever number of findings
                        // say why; a grouped one drawn a line a cause after the notes, each
                        // under `--each`.
                        Noted::Unchecked | Noted::Grouped => {
                            if !counted {
                                counted = true;
                                if first_of_body(&mut unchecked_bodies, view) {
                                    tally.unchecked += 1;
                                }
                            }
                            if kind == Noted::Grouped && !shown.each {
                                // Its texts into the frame: the block's bytes are not kept.
                                grouped.push(finding.map(&mut |text: &str| work::held_str(text, frame)).map_err(|why| CliError::Text { what: why.what })?);
                            } else {
                                print::note(cx, said(&finding, frame)?, shown, frame, out)?;
                            }
                        }
                        // As a `Class::Borrows` note.
                        Noted::BorrowsUnchecked => {
                            if first_of_body(&mut borrows_bodies, view) {
                                tally.borrows += 1;
                                shown.borrows_unchecked += 1;
                            }
                            if shown.each || crate::body_types::BORROWS_ARE_ERRORS {
                                print::note(cx, said(&finding, frame)?, shown, frame, out)?;
                            }
                        }
                        Noted::Error | Noted::Warning | Noted::Borrows | Noted::Waited | Noted::Told => {
                            return Err(CliError::Text { what: "a noted finding's rule is of a kind no body's note is (engine_lines.rs `print_with`, D164)" });
                        }
                    }
                    continue;
                }
                Entry::Note { class, words } => {
                    // Checked UTF-8 at the decode, and made from a `str` at the write.
                    let words = core::str::from_utf8(words).map_err(|_| CliError::Text { what: "a note's words are not UTF-8" })?;
                    match class {
                        Class::Failure => {
                            tally.failures += 1;
                            print::note(cx, words, shown, frame, out)?;
                            // A task's first failure is what its waiters' line names.
                            if !failed.contains_key(&name) {
                                failed.insert(name, work::held_str(words, frame));
                            }
                        }
                        Class::Unchecked => {
                            if view.is_none() || (!counted && first_of_body(&mut unchecked_bodies, view)) {
                                tally.unchecked += 1;
                            }
                            counted = true;
                            print::note(cx, words, shown, frame, out)?;
                        }
                        // Grouped under its cause, printed after the findings.
                        Class::Waited => {
                            tally.failures += 1;
                            tally.waited += 1;
                            let (on, label) = crate::not_checked::waited::read(words.as_bytes())?;
                            waits.push(crate::not_checked::waited::Wait { name, on, label: work::held_str(label, frame), file: None });
                        }
                        // Printed grouped by reason (engine_head_notes.rs `print_grouped`).
                        Class::Head => tally.heads += 1,
                        Class::Told => print::note(cx, words, shown, frame, out)?,
                        // As the old path's `BORROWS_NOT_CHECKED` (print.rs `finding`): counted;
                        // drawn under `--each`, or always once borrows are errors.
                        Class::Borrows => {
                            if first_of_body(&mut borrows_bodies, view) {
                                tally.borrows += 1;
                                shown.borrows_unchecked += 1;
                            }
                            if shown.each || crate::body_types::BORROWS_ARE_ERRORS {
                                print::note(cx, words, shown, frame, out)?;
                            }
                        }
                    }
                    continue;
                }
                Entry::Finding { rule, start, end, path, words: message } => {
                    let (path, place, words): (Option<&'f str>, print::Place, &[u8]) = match path {
                        Some(path) => (Some(work::held_str(core::str::from_utf8(path).map_err(|_| CliError::Text { what: "a path is not UTF-8" })?, frame)), print::Place::At(0), message),
                        None => {
                            let placed = crate::names::placed(rule) && message.contains(&0x1F);
                            match placed {
                                true => match crate::names::place_pieces(message)? {
                                    (Some((path, chain, edition)), words) => (Some(work::held_str(path, frame)), print::Place::Chain(chain, edition), words),
                                    (None, words) => (None, print::Place::At(0), words),
                                },
                                false => (None, print::Place::At(0), message),
                            }
                        }
                    };
                    (rule, start, end, message, path, place, words)
                }
                // A body's finding: in its item, found by its chain in the file as it is now.
                Entry::Placed { rule, start, end, path, chain, edition, words } => (rule, start, end, words, Some(work::held_str(path, frame)), print::Place::Chain(chain, edition), words),
                // A body's finding at token places (lane K1, rule 71): its bytes are found in its
                // item's text today (print.rs `list`), so a comment above it moves no key. Its
                // first and last tokens stand as its range in what makes two findings one.
                Entry::PlacedTokens { rule, at, path, chain, edition, words } => (rule, at.first, at.last, words, Some(work::held_str(path, frame)), print::Place::Tokens(chain, edition, at), words),
                // A lint unit's finding: its own range, given its children's lengths from the
                // file as it is now (print.rs `Place::Own`), so a child that grew moves no key.
                Entry::PlacedOwn { rule, start, end, path, chain, edition, words } => (rule, start, end, words, Some(work::held_str(path, frame)), print::Place::Own(chain, edition), words),
            };
            let kept: &'f [u8] = frame.alloc_slice_copy(words);
            let at = prints.len();
            let at_item = match place {
                print::Place::Chain(chain, _) => (1, chain),
                print::Place::At(start) => (0, u64::from(start)),
                print::Place::Counted => (2, 0),
                print::Place::Tokens(chain, ..) => (3, chain),
                print::Place::Own(Some(chain), _) => (4, chain),
                print::Place::Own(None, _) => (5, 0),
            };
            let inside = match place {
                print::Place::Tokens(_, _, at) => at.inside,
                _ => None,
            };
            // Unfolded (the control), a row at a view is its own line.
            let row = match (VF::FOLDS, view) {
                (false, Some(_)) => Some(name),
                _ => None,
            };
            let what = Seen { path, at_item, rule, start, end, inside, words: kept, row };
            if let Some(&first) = seen.get(&what) {
                if let Some(row_view) = view {
                    held_by.push((first, row_view.base, row_view.at));
                }
                if prints[first].name != name && !prints[first].also.contains(&name) {
                    prints[first].also.push(name);
                }
                continue;
            }
            seen.insert(what, at);
            if let Some(row_view) = view {
                held_by.push((at, row_view.base, row_view.at));
            }
            prints.push(Print { name, also: Vec::new_in(listed), condition: Condition::ANY, file: None, place: 0, rule, start, end, message: Vec::new_in(listed) });
            wanted.push(print::Wanted { path, place, words: kept });
            match runs.last_mut() {
                Some(run) if run.0 == path && run.2 == at => run.2 = at + 1,
                _ => runs.push((path, at, at + 1)),
            }
        }
    }
    // D191: a finding only some of its body's rows hold says which, after its message
    // ([`view_words`]); one all of them hold says nothing more. A finding of rows of two bodies
    // (two bodies' equal findings in one item) names none: no one view is its. Unfolded (the
    // control), each row's line stands alone and names nothing.
    held_by.sort_unstable();
    for one in held_by.chunk_by(|a, b| a.0 == b.0).filter(|_| VF::FOLDS) {
        let (print_at, base, _) = one[0];
        let Some(view) = views_of.get(&base).copied() else { continue };
        if one.iter().any(|row| row.1 != base) {
            continue;
        }
        let mut held: Vec<bool, &'f Frame> = Vec::new_in(frame);
        held.resize(view.units as usize, false);
        for row in one.iter() {
            if let Some(flag) = held.get_mut(row.2 as usize) {
                *flag = true;
            }
        }
        if held.iter().all(|flag| *flag) {
            continue;
        }
        let Some(wanted_one) = wanted.get_mut(print_at) else { continue };
        let message = core::str::from_utf8(wanted_one.words).map_err(|_| CliError::Text { what: "a finding's message is not UTF-8" })?;
        let mut words = blaze_own::format_in!(frame, "{message}");
        view_words(&view, &held, &mut words);
        wanted_one.words = work::held_str(words.as_str(), frame).as_bytes();
    }
    // FILES IN PATH ORDER: the findings came in their tasks' name order (blocks by name), so
    // one file's findings of two bodies could be two runs with another file's between; each
    // file is one run now, the fileless first, a file's in the order they came (the printer
    // then draws a run by place: [`by_place`]).
    by_file(&mut prints, &mut wanted, &mut runs, alloc);
    // Where the bodies' own notes are, before the findings.
    grouped_lines(cx, &mut grouped, shown, frame, out)?;
    let mut scratch = print::Scratch::new(work::Sizes::USUAL.scratch)?;
    files.print(cx, root, &Found { prints: prints.as_slice(), wanted: wanted.as_slice(), runs: runs.as_slice() }, &mut scratch, shown, frame, out)?;
    waited_lines(cx, &mut waits, &failed, shown, &mut scratch, frame, out)?;
    tally.listed = u32::try_from(prints.len()).unwrap_or(u32::MAX);
    // What the lists reserved: their capacities, the set's entries, the listing's rows, and
    // the frame's bytes past where this call began (the words kept, the listing's growth).
    let size = core::mem::size_of::<usize>();
    let set = seen.len().saturating_mul(core::mem::size_of::<Seen<'_>>().saturating_add(size));
    let lists = prints.capacity().saturating_mul(core::mem::size_of::<Print<'_>>())
        .saturating_add(wanted.capacity().saturating_mul(core::mem::size_of::<print::Wanted<'_>>()))
        .saturating_add(runs.capacity().saturating_mul(core::mem::size_of::<(Option<&str>, usize, usize)>()))
        .saturating_add(set)
        .saturating_add(failed.len().saturating_mul(core::mem::size_of::<(Name, &str)>()));
    let in_frame = frame.used().saturating_sub(frame_at).max(shown.listed.len().saturating_sub(listed_at).saturating_mul(core::mem::size_of::<crate::fix::Listed<'_>>()));
    tally.tail = Tail { held: scratch.held(), listing: lists.saturating_add(in_frame), placing: scratch.placing(), replayed: files.replayed() };
    Ok(tally)
}

/// The findings `prints` (with their `wanted`, one each) put in path order, the fileless
/// first and each path's in the order they came, and `runs` made again: one a path. Moved in
/// place, cycle by cycle (no second list of the project's findings).
fn by_file<'f>(prints: &mut SVec<'_, Print<'_>>, wanted: &mut SVec<'_, print::Wanted<'f>>, runs: &mut SVec<'_, (Option<&'f str>, usize, usize)>, alloc: SharedAlloc<'_>) {
    let n = prints.len();
    let mut order: SVec<'_, usize> = SVec::new_in(alloc);
    order.extend(0..n);
    // Unique keys: in place, no buffer (the stable sort's is the ambient heap's).
    order.sort_unstable_by_key(|&i| (wanted[i].path, i));
    // Position `k` takes the finding at `order[k]`: each cycle of the permutation by swaps.
    let mut moved: SVec<'_, bool> = SVec::new_in(alloc);
    moved.resize(n, false);
    for start in 0..n {
        if moved[start] {
            continue;
        }
        let mut k = start;
        loop {
            moved[k] = true;
            let from = order[k];
            if from == start {
                break;
            }
            prints.swap(k, from);
            wanted.swap(k, from);
            k = from;
        }
    }
    runs.clear();
    for (at, one) in wanted.iter().enumerate() {
        match runs.last_mut() {
            Some(run) if run.0 == one.path => run.2 = at + 1,
            _ => runs.push((one.path, at, at + 1)),
        }
    }
}

/// A file run's findings in the order they are drawn: by their place in the file as it was
/// read (file-relative range), the unplaced first, then by rule and words (in place, no
/// buffer: the stable sort's is the ambient heap's). Both strategies draw so ([`Files`]), and
/// a stored section's ids are in this order.
fn by_place(run: &mut [crate::fix::Listed<'_>]) {
    run.sort_unstable_by_key(|one| (one.path.is_some(), one.start, one.end, one.rule, one.words));
}

/// A noted finding's words, as `--each` says it, in the frame.
fn said<'f>(finding: &blaze_report::finding::Finding<'_>, frame: &'f Frame) -> Result<&'f str, CliError> {
    let mut words = bstd::mem::format_in!(frame, "");
    finding.words(&mut words).map_err(|_| CliError::Text { what: "a noted finding's words could not be said (engine_lines.rs `said`)" })?;
    Ok(work::held_str(words.as_str(), frame))
}

/// A body's stored refusal ([`Last::body_refusal`]), said as the check printed it under `--each`.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Refusal<'m> {
    /// A failure, unchecked or borrows note's words as stored (a note of no noted finding: a
    /// lint unit's, a plan's).
    Note(&'m str),
    /// A body's noted finding: its words are its rule's (blaze_report::finding).
    Finding(blaze_report::finding::Finding<'m>),
}

impl fmt::Display for Refusal<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refusal::Note(words) => f.write_str(words),
            Refusal::Finding(finding) => finding.words(f).map_err(|_| fmt::Error),
        }
    }
}

/// The noted findings of a `Kind::Grouped` rule, one line a cause key (the default view;
/// `--each` printed each where it was read): the query's groups (blaze_report::query `groups`,
/// the one home of grouping), each said by its line ("2 bodies not checked because <the
/// cause>: `here` (src/lib.rs), `there` (src/lib.rs)", the first `SHOWN` named and "(and N
/// more)" past them). A cause is a refused macro call's failure, or the plan's refusal of a
/// member's bodies (its check scope cut at a crate with no names record). On tempo one refused
/// `sol!` leaves hundreds of readers unchecked, and one failed crate every body of the members
/// above it: one line, not hundreds of near-equal ones. A body held by two units is named once.
fn grouped_lines<'f>(cx: &Cx, grouped: &mut Vec<blaze_report::finding::Finding<'f>, &'f Frame>, shown: &mut print::Shown<'f>, frame: &'f Frame, out: &impl print::RenderOutput) -> Result<(), CliError> {
    blaze_report::query::sort(&mut grouped[..]);
    for group in blaze_report::query::groups(&grouped[..]) {
        let mut words = bstd::mem::format_in!(frame, "");
        group.line(&mut words).map_err(|_| CliError::Text { what: "a not-checked line's words (engine_lines.rs `grouped_lines`)" })?;
        print::note(cx, words.as_str(), shown, frame, out)?;
    }
    Ok(())
}

/// The waiters, a line a cause (not_checked/waited.rs `groups`): "N task(s) not run: each waits
/// on a task that failed: <the cause's own failure>", once a cause, and when crates hang from
/// it, what (`waited::line`). A cause whose failure is not among these lines (its row stands
/// with no block, or the machine's own) is named by its task. Causes in name order (rule 25).
fn waited_lines<'f>(
    cx: &Cx,
    waits: &mut Vec<crate::not_checked::waited::Wait<'f>, &'f Frame>,
    failed: &SMap<'_, Name, &'f str>,
    shown: &mut print::Shown<'f>,
    scratch: &mut print::Scratch,
    frame: &'f Frame,
    out: &impl print::RenderOutput,
) -> Result<(), CliError> {
    if waits.is_empty() {
        return Ok(());
    }
    let groups = crate::not_checked::waited::groups(waits, frame)?;
    for group in groups.iter() {
        let one = scratch.one();
        let reason = failed.get(&group.cause).copied();
        shown.behind.add(group.items);
        print::note_from(cx, shown, frame, out, || {
            let mut h = blaze_ids::Hasher::new("blaze-cli rendered wait group 1");
            h.update(&[group.cause.kind as u8]).update(&group.cause.subject.0)
                .opt_part(reason.map(str::as_bytes)).update(&group.tasks.to_le_bytes())
                .update(&group.items.to_le_bytes()).update(&(group.files.len() as u64).to_le_bytes())
                .update(&(group.crates.len() as u64).to_le_bytes());
            for name in group.crates.iter().take(crate::not_checked::CRATES_SHOWN) {
                h.part(name.as_bytes());
            }
            h.finish()
        }, || {
            let mut words = match reason {
                Some(reason) => bstd::mem::format_in!(one, "{} task(s) not run: each waits on a task that failed: {reason}", group.tasks),
                None => bstd::mem::format_in!(one, "{} task(s) not run: each waits on `{}`, which failed (its failure is not among these lines)", group.tasks, group.cause),
            };
            if !group.crates.is_empty() {
                words.push_str("; ");
                words.push_str(crate::not_checked::waited::line(group, one)?.as_str());
            }
            Ok(words)
        })?;
    }
    Ok(())
}

/// Exact file bytes behind placement. BEF4 binds the semantic blocks to these facts.
/// Stamps, machine names and the insertion order of the sweep are absent. A certified
/// request already has that key in the pointer and never builds this table. Only paths
/// the printer actually opened contribute, including an unsuccessful item-chain lookup.
struct Sources<'f> {
    files: RefCell<blaze_own::BTreeMap<&'f str, ContentId, &'f Frame>>,
    frame: &'f Frame,
}

const MOST_BLOCKS: usize = 1 << 22;
const MOST_RENDER_BYTES: usize = 1 << 30;

impl<'f> Sources<'f> {
    fn new(frame: &'f Frame) -> Self {
        Self { files: RefCell::new(blaze_own::BTreeMap::new_in(frame)), frame }
    }

    fn observe(&self, path: &str, bytes: &[u8]) -> Result<(), CliError> {
        self.observe_id(path, ContentId::of(crate::sweep::FILE_DOMAIN, bytes))
    }

    /// A file placed by its digest alone: a replayed section's (BER2), whose digest is the
    /// sweep row's (`FILE_DOMAIN`, [`texts_of`]). The witness is then the one placing the file
    /// would have made, so the semantic record's placement digest is a fresh check's.
    fn observe_id(&self, path: &str, id: ContentId) -> Result<(), CliError> {
        let mut files = self.files.borrow_mut();
        if let Some(was) = files.get(path) {
            return if *was == id { Ok(()) } else { Err(print::placement_changed(path)) };
        }
        if files.len() == MOST_BLOCKS {
            return Err(CliError::Text { what: "too many source paths in the rendered answer" });
        }
        files.insert(work::held_str(path, self.frame), id);
        Ok(())
    }

    fn digest(&self) -> ContentId {
        let mut hash = blaze_ids::Hasher::new("blaze-cli presentation placement 1");
        for (path, id) in self.files.borrow().iter() {
            hash.part(path.as_bytes()).update(&id.0);
        }
        hash.finish()
    }

    /// Every placement dependency must stand under the same certificate as the answer.
    /// A draw-only source is an ordinary exact-path file slot (MODULE clear). It does
    /// not replace a module slot or manufacture an outline task.
    fn cover(&self, rows: &mut crate::sweep::Rows<'_>) -> Result<(), CliError> {
        use crate::sweep::{ABSENT, AT_MOD_RS, BOTH, LeafKind};
        let files = self.files.borrow();
        if files.is_empty() {
            return Ok(());
        }
        // Same newest (kind, slot) that Leaves::bytes_in seals. Consider only slots
        // that can name a read. A superseded x/mod.rs row must never witness that file.
        let mut candidates = blaze_own::BTreeSet::new_in(self.frame);
        for &path in files.keys() {
            candidates.insert(path);
            if let Some(stem) = path.strip_suffix("/mod.rs") {
                candidates.insert(stem);
                let slot = bstd::mem::format_in!(self.frame, "{stem}.rs");
                candidates.insert(work::held_str(slot.as_str(), self.frame));
            }
        }
        let mut seen = blaze_own::BTreeSet::new_in(self.frame);
        let mut covered = blaze_own::BTreeSet::new_in(self.frame);
        for row in rows.rows.iter().rev() {
            if !matches!(row.kind, LeafKind::Slot | LeafKind::Manifest | LeafKind::MacroFile) {
                continue;
            }
            let slot = rows.path(row);
            if !candidates.contains(slot) || !seen.insert((row.kind, slot)) {
                continue;
            }
            if seen.len() > MOST_BLOCKS {
                return Err(CliError::Text { what: "too many file slots in the placement witness" });
            }
            if files.contains_key(slot) && row.flags & (ABSENT | BOTH | AT_MOD_RS) != 0 {
                return Err(print::placement_unobserved(slot));
            }
            if row.flags & (ABSENT | BOTH) != 0 {
                continue;
            }
            let physical = (row.flags & AT_MOD_RS != 0).then(|| crate::sweep::real_path(slot, row.flags, self.frame));
            let path = match &physical {
                None => slot,
                Some(bytes) => core::str::from_utf8(bytes).map_err(|_| CliError::Text { what: "a rendered answer's module path is not UTF-8" })?,
            };
            if let Some((name, expected)) = files.get_key_value(path) {
                if *expected != row.id {
                    return Err(print::placement_changed(path));
                }
                covered.insert(*name);
            }
        }
        drop(seen);
        for (&path, &id) in files.iter() {
            if !covered.contains(path) {
                rows.push(LeafKind::Slot, 0, path, None, id);
            }
        }
        Ok(())
    }
}

struct ListingOutput<'s, 'f> {
    sources: &'s Sources<'f>,
    out: &'s Out,
}

impl print::RenderOutput for ListingOutput<'_, '_> {
    fn source(&self, path: &str, bytes: &[u8]) -> Result<(), CliError> {
        self.sources.observe(path, bytes)
    }

    fn block<'a>(&self, cx: &Cx, key: impl FnOnce() -> ContentId, draw: impl FnOnce() -> Result<blaze_own::AString<'a>, CliError>) -> Result<(), CliError> {
        print::RenderOutput::block(self.out, cx, key, draw)
    }
}

/// THE RENDERED ANSWER, form BER3 (engine.md D149, piece I3): stdout as the last request
/// printed it, in rows whose concatenation is stdout's order, so the certificate writes the
/// whole stream with one write and skips the index in one slice operation.
///
/// ```text
/// record  "BER3" | lines key [32] | presentation [32] | 16 u32 (tally 6, counts 6, behind 2,
///         types, borrows) | fix ids sized | index sized | stream sized
/// row     0 (a drawing) | its input key [32] | bytes u32
///         1 (a file's section) | key [32] | bytes u32 | gap u8 | drew u8 | failures errors
///         warnings notes fixable types borrows (7 u32) | ids u32 | per id: full [25] | shown u8 | fix u8
/// ```
///
/// A FILE'S SECTION is the bytes one run of a file's findings drew (print.rs `file_findings`),
/// stored as one row: its key is the presentation, the path, the file's text digest (the sweep
/// row's: [`texts_of`]), and the digest of its findings' rows ([`findings_digest`]); beside it,
/// what drawing it added to the printer's state (the counts, the file counted, the types and
/// borrows not checked), whether a blank line led it ([`print::frame_gap`] when it began), and
/// each listed id whole with its shown length and whether it carries a fix. On an edit
/// ([`Sectioned`]) a section whose key is found is written out as stored, its file not read,
/// when its gap is today's and its ids' shown lengths are what the lengths over every file's
/// ids give today (recomputed from the stored whole ids, no file read); so the key of D149
/// (presentation, path, text, findings, shown lengths) is the row's key with those two checked
/// beside it. The text digest is in the key because `file:line:col` exists only here (rule 71):
/// a body edit that moves a line moves the text and the file is drawn again. A file holding a
/// finding that adds to the printer's lists (an item not checked, a task not run:
/// [`print::adds_to_lists`]) or whose text digest no sweep row gives is always drawn, its
/// drawings rows of their own (0). A drawing's row is reused by its input key ([`Capture`]).
/// A BER1 or BER2 record is an older form: refused, reported (`Rendering::Published::old_form`) and
/// rebuilt whole, never migrated (rule 26).
/// BER3 (for BER2): bodies under a refused macro call print one line a call (a noted finding of
/// a `Kind::Grouped` rule since D164, whose default view prints the same bytes; the lines key
/// in the record moved with the BEF7 form, so no stored stream stands for a new record), so
/// a BER2 stream, which printed a note a body, is an older form too.
const RENDER_FORM: &[u8; 4] = b"BER3";
const OLD_RENDER_FORMS: [&[u8; 4]; 2] = [b"BER1", b"BER2"];
/// A row of one drawing.
const ROW_BLOCK: u8 = 0;
/// A row of one file's section.
const ROW_FILE: u8 = 1;
const SECTION_DOMAIN: &str = "blaze-cli rendered file section 1";
const FINDINGS_DOMAIN: &str = "blaze-cli rendered file findings 1";

#[derive(Clone, Copy)]
struct Rendered<'m> {
    lines: Key,
    presentation: ContentId,
    tally: Tally,
    counts: blaze_report::frame::Counts,
    behind: crate::not_checked::Behind,
    types: u32,
    borrows: u32,
    fixes: &'m str,
    index: &'m [u8],
    stream: &'m [u8],
    /// An older form's record (BER1): matches nothing, stands in for nothing.
    old_form: bool,
}

/// What drawing a file's section added to the printer's state (print.rs `finding`): counts
/// add, so a replay adds them again whatever came before.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
struct Deltas {
    failures: u32,
    errors: u32,
    warnings: u32,
    notes: u32,
    fixable: u32,
    types: u32,
    borrows: u32,
}

/// The printer's additive state before a section is drawn.
#[derive(Clone, Copy)]
struct Before {
    counts: blaze_report::frame::Counts,
    types: u32,
    borrows: u32,
}

impl Before {
    fn of(shown: &print::Shown<'_>) -> Before {
        Before { counts: shown.counts, types: shown.types_unchecked, borrows: shown.borrows_unchecked }
    }
}

impl Deltas {
    /// What `shown` gained since `before`. The files count is not here: [`Shown::file`] says
    /// it (a file counted once whichever section drew in it first).
    fn since(before: Before, shown: &print::Shown<'_>) -> Deltas {
        let (was, now) = (before.counts, shown.counts);
        Deltas {
            failures: now.failures.saturating_sub(was.failures),
            errors: now.errors.saturating_sub(was.errors),
            warnings: now.warnings.saturating_sub(was.warnings),
            notes: now.notes.saturating_sub(was.notes),
            fixable: now.fixable.saturating_sub(was.fixable),
            types: shown.types_unchecked.saturating_sub(before.types),
            borrows: shown.borrows_unchecked.saturating_sub(before.borrows),
        }
    }

    fn add_to(self, shown: &mut print::Shown<'_>) {
        let counts = &mut shown.counts;
        counts.failures = counts.failures.saturating_add(self.failures);
        counts.errors = counts.errors.saturating_add(self.errors);
        counts.warnings = counts.warnings.saturating_add(self.warnings);
        counts.notes = counts.notes.saturating_add(self.notes);
        counts.fixable = counts.fixable.saturating_add(self.fixable);
        shown.types_unchecked = shown.types_unchecked.saturating_add(self.types);
        shown.borrows_unchecked = shown.borrows_unchecked.saturating_add(self.borrows);
    }

    fn words(self) -> [u32; 7] {
        [self.failures, self.errors, self.warnings, self.notes, self.fixable, self.types, self.borrows]
    }
}

/// A stored file section, read ([`Rendered::previous`]): its ids are `ids` of [`Previous::ids`].
#[derive(Clone, Copy)]
struct FileRow<'m> {
    gap: bool,
    drew: bool,
    deltas: Deltas,
    ids: (usize, usize),
    bytes: &'m [u8],
}

/// The last view's rows by key, read once on an edit: the drawings, the file sections, and
/// every section's ids (whole, shown length, carries a fix), each checked here once.
struct Previous<'m, 'a> {
    blocks: SMap<'a, ContentId, &'m [u8]>,
    files: SMap<'a, ContentId, FileRow<'m>>,
    ids: SVec<'a, (crate::fix::Id, bool)>,
}

impl<'m, 'a> Previous<'m, 'a> {
    fn none(alloc: SharedAlloc<'a>) -> Self {
        Previous { blocks: SMap::new_in(alloc), files: SMap::new_in(alloc), ids: SVec::new_in(alloc) }
    }
}

/// `row`'s ids.
fn ids_of<'i>(ids: &'i [(crate::fix::Id, bool)], row: &FileRow<'_>) -> &'i [(crate::fix::Id, bool)] {
    &ids[row.ids.0..row.ids.1]
}

impl<'m> Rendered<'m> {
    /// A BER1 record: an older form, refused and rebuilt (rule 26).
    fn older() -> Self {
        Rendered {
            lines: Key(ContentId([0; 32])),
            presentation: ContentId([0; 32]),
            tally: Tally::default(),
            counts: blaze_report::frame::Counts::default(),
            behind: crate::not_checked::Behind::default(),
            types: 0,
            borrows: 0,
            fixes: "",
            index: &[],
            stream: &[],
            old_form: true,
        }
    }

    fn open(bytes: &'m [u8]) -> Result<Self, LinesError> {
        let mut r = Reader { bytes, at: 0 };
        match r.take(4, "the rendered answer's form")? {
            form if form == RENDER_FORM => {}
            form if OLD_RENDER_FORMS.iter().any(|old| form == *old) => return Ok(Rendered::older()),
            _ => return Err(LinesError { at: 0, what: "the rendered answer is neither BER3 nor an older BER1 or BER2" }),
        }
        let lines = Key(r.digest("the semantic lines and placement key")?);
        let presentation = r.digest("the rendered presentation key")?;
        let mut n = || r.u32("the rendered answer's tally");
        // Tail is this request's scratch usage. A bulk transfer uses no printer scratch.
        let tally = Tally { failures: n()?, unchecked: n()?, waited: n()?, borrows: n()?, heads: n()?, listed: n()?, ..Tally::default() };
        let counts = blaze_report::frame::Counts { failures: n()?, errors: n()?, warnings: n()?, notes: n()?, files: n()?, fixable: n()? };
        let behind = crate::not_checked::Behind { items: n()?, failures: n()? };
        let (types, borrows) = (n()?, n()?);
        let fix_bytes = r.sized("the rendered fix ids")?;
        if fix_bytes.len() > MOST_BLOCKS * (crate::fix::FULL + 1) {
            return Err(LinesError { at: r.at, what: "the rendered fix list exceeds its byte cap" });
        }
        let fixes = core::str::from_utf8(fix_bytes).map_err(|_| LinesError { at: r.at, what: "invalid UTF-8 in the rendered fix ids" })?;
        if !fixes.is_empty() && fixes.split(' ').enumerate().any(|(i, id)| {
            i >= MOST_BLOCKS || !(crate::fix::SHORT..=crate::fix::FULL).contains(&id.len()) || !id.bytes().all(|b| crate::fix::ALPHABET.contains(&b))
        }) {
            return Err(LinesError { at: r.at, what: "invalid id in the rendered answer's fix list" });
        }
        let index = r.sized("the rendered row index")?;
        if index.len() > MOST_RENDER_BYTES {
            return Err(LinesError { at: r.at, what: "the rendered row index exceeds its byte cap" });
        }
        let stream = r.sized("the rendered stream")?;
        if stream.len() > MOST_RENDER_BYTES || r.at != bytes.len() {
            return Err(LinesError { at: r.at, what: "the rendered stream is oversized or has trailing bytes" });
        }
        Ok(Self { lines, presentation, tally, counts, behind, types, borrows, fixes, index, stream, old_form: false })
    }

    /// The rows by key, every length, flag and id checked here once (an edit's read; the
    /// certificate never parses the index).
    fn previous<'a>(&self, alloc: SharedAlloc<'a>) -> Result<Previous<'m, 'a>, LinesError> {
        let mut previous = Previous::none(alloc);
        let mut index = Reader { bytes: self.index, at: 0 };
        let mut stream = Reader { bytes: self.stream, at: 0 };
        let mut rows = 0usize;
        while index.at < self.index.len() {
            rows += 1;
            if rows > MOST_BLOCKS {
                return Err(LinesError { at: index.at, what: "too many rows in the rendered answer" });
            }
            let at = index.at;
            let tag = index.byte("a rendered row's tag")?;
            let key = index.digest("a rendered row's key")?;
            let len = index.u32("a rendered row's length")? as usize;
            match tag {
                ROW_BLOCK => {
                    let block = stream.take(len, "a block outside the rendered stream")?;
                    if previous.blocks.insert(key, block).is_some_and(|was| was != block) {
                        return Err(LinesError { at: index.at, what: "two different rendered blocks have the same input key" });
                    }
                }
                ROW_FILE => {
                    let flag = |byte: u8, at: usize, what: &'static str| match byte {
                        0 => Ok(false),
                        1 => Ok(true),
                        _ => Err(LinesError { at, what }),
                    };
                    let gap = flag(index.byte("a file section's gap")?, index.at, "a file section's gap is neither 0 nor 1")?;
                    let drew = flag(index.byte("a file section's file mark")?, index.at, "a file section's file mark is neither 0 nor 1")?;
                    let mut n = || index.u32("a file section's counts");
                    let deltas = Deltas { failures: n()?, errors: n()?, warnings: n()?, notes: n()?, fixable: n()?, types: n()?, borrows: n()? };
                    let count = index.u32("a file section's id count")? as usize;
                    if count > MOST_BLOCKS || previous.ids.len() + count > MOST_BLOCKS {
                        return Err(LinesError { at: index.at, what: "too many ids in the rendered answer's sections" });
                    }
                    let from = previous.ids.len();
                    for _ in 0..count {
                        let mut full = [0u8; crate::fix::FULL];
                        full.copy_from_slice(index.take(crate::fix::FULL, "a file section's id")?);
                        let shown = index.byte("a file section's id length")?;
                        let id = crate::fix::Id::of_parts(full, shown).ok_or(LinesError { at: index.at, what: "a file section's id is not of the id alphabet or its shown length is outside SHORT..=FULL" })?;
                        let fix = flag(index.byte("a file section's fix mark")?, index.at, "a file section's fix mark is neither 0 nor 1")?;
                        previous.ids.push((id, fix));
                    }
                    let bytes = stream.take(len, "a file section outside the rendered stream")?;
                    let row = FileRow { gap, drew, deltas, ids: (from, previous.ids.len()), bytes };
                    if previous.files.insert(key, row).is_some() {
                        return Err(LinesError { at, what: "two file sections of the rendered answer have the same key" });
                    }
                }
                _ => return Err(LinesError { at, what: "a rendered row's tag is neither a drawing (0) nor a file's section (1)" }),
            }
        }
        if stream.at != self.stream.len() {
            return Err(LinesError { at: stream.at, what: "the row index does not cover the rendered stream" });
        }
        Ok(previous)
    }

    fn matches(&self, lines: Key, presentation: ContentId) -> bool {
        !self.old_form && self.lines == lines && self.presentation == presentation
    }

    fn replay<'f>(&self, shown: &mut print::Shown<'f>, frame: &'f Frame, write_all: impl FnOnce(&[u8]) -> Result<(), CliError>) -> Result<Tally, CliError> {
        write_all(self.stream)?;
        shown.counts = self.counts;
        shown.behind = self.behind;
        shown.types_unchecked = self.types;
        shown.borrows_unchecked = self.borrows;
        shown.replay_fixes.extend(self.fixes.split_ascii_whitespace().map(|id| work::held_str(id, frame)));
        Ok(Tally { rendering: Rendering::Replayed { bytes: self.stream.len() }, ..self.tally })
    }
}

struct Drawn<'a> {
    index: SVec<'a, u8>,
    stream: SVec<'a, u8>,
    rows: usize,
    reused: usize,
}

impl<'a> Drawn<'a> {
    fn new_in(alloc: SharedAlloc<'a>) -> Self {
        Drawn { index: SVec::new_in(alloc), stream: SVec::new_in(alloc), rows: 0, reused: 0 }
    }

    /// Room for one more row of `bytes` (and `index` bytes of its index row).
    fn room(&self, bytes: usize, index: usize) -> Result<(), CliError> {
        if self.rows == MOST_BLOCKS || bytes > MOST_RENDER_BYTES - self.stream.len() || index > MOST_RENDER_BYTES - self.index.len() {
            return Err(CliError::Text { what: "the ordered rendering exceeds the rendered answer's row or byte cap" });
        }
        Ok(())
    }

    /// One drawing's row.
    fn append(&mut self, key: ContentId, bytes: &[u8]) -> Result<(), CliError> {
        self.room(bytes.len(), 1 + 32 + 4)?;
        self.index.push(ROW_BLOCK);
        self.index.extend_from_slice(&key.0);
        word(&mut self.index, bytes.len() as u32);
        self.stream.extend_from_slice(bytes);
        self.rows += 1;
        Ok(())
    }

    /// One file section's row (module docs at [`RENDER_FORM`]).
    fn append_file(&mut self, key: ContentId, gap: bool, drew: bool, deltas: Deltas, ids: &[(crate::fix::Id, bool)], bytes: &[u8]) -> Result<(), CliError> {
        let index = 1 + 32 + 4 + 2 + 7 * 4 + 4 + ids.len().saturating_mul(crate::fix::FULL + 2);
        self.room(bytes.len(), index)?;
        if ids.len() > MOST_BLOCKS {
            return Err(CliError::Text { what: "a file section holds more ids than the rendered answer's cap" });
        }
        self.index.push(ROW_FILE);
        self.index.extend_from_slice(&key.0);
        word(&mut self.index, bytes.len() as u32);
        self.index.push(u8::from(gap));
        self.index.push(u8::from(drew));
        for n in deltas.words() {
            word(&mut self.index, n);
        }
        word(&mut self.index, ids.len() as u32);
        for &(id, fix) in ids {
            let (full, shown) = id.parts();
            self.index.extend_from_slice(&full);
            self.index.push(shown);
            self.index.push(u8::from(fix));
        }
        self.stream.extend_from_slice(bytes);
        self.rows += 1;
        Ok(())
    }
}

struct Capture<'m, 'a, 's, 'f> {
    /// The last view's drawings by input key.
    previous: SMap<'a, ContentId, &'m [u8]>,
    sources: &'s Sources<'f>,
    drawn: RefCell<Drawn<'a>>,
    /// While one file's section is drawn ([`Sectioned`]): its drawings' bytes, one row for all.
    section: RefCell<Option<SVec<'a, u8>>>,
    alloc: SharedAlloc<'a>,
}

impl<'a> Capture<'_, 'a, '_, '_> {
    fn open_section(&self) {
        *self.section.borrow_mut() = Some(SVec::new_in(self.alloc));
    }

    /// The section's bytes; empty when none was open.
    fn close_section(&self) -> SVec<'a, u8> {
        self.section.borrow_mut().take().unwrap_or_else(|| SVec::new_in(self.alloc))
    }
}

impl print::RenderOutput for Capture<'_, '_, '_, '_> {
    fn source(&self, path: &str, bytes: &[u8]) -> Result<(), CliError> {
        self.sources.observe(path, bytes)
    }

    fn block<'a>(&self, _cx: &Cx, key: impl FnOnce() -> ContentId, draw: impl FnOnce() -> Result<blaze_own::AString<'a>, CliError>) -> Result<(), CliError> {
        // Inside a file's section: drawn now, its bytes the section's (the file was read).
        if self.section.borrow().is_some() {
            let drawn = draw()?;
            let mut section = self.section.borrow_mut();
            let Some(section) = section.as_mut() else { return Err(CliError::Text { what: "a file section closed while one of its drawings was made (engine_lines.rs `Capture`)" }) };
            if drawn.as_bytes().len() > MOST_RENDER_BYTES.saturating_sub(section.len()) {
                return Err(CliError::Text { what: "a file section exceeds the rendered answer's byte cap" });
            }
            section.extend_from_slice(drawn.as_bytes());
            return Ok(());
        }
        let key = key();
        match self.previous.get(&key) {
            Some(bytes) => {
                let mut drawn = self.drawn.borrow_mut();
                drawn.append(key, bytes)?;
                drawn.reused += 1;
                Ok(())
            }
            None => self.drawn.borrow_mut().append(key, draw()?.as_bytes()),
        }
    }
}

/// Each file's text digest as this command's sweep saw it: the newest row of each slot (a
/// source slot, a manifest, a member proc-macro file), at the path it was found at (`x/mod.rs`
/// for a module there), the same rows [`Sources::cover`] checks the placement against. A path
/// two rows give different digests is `None`, and so is one no row gives (absent, at both
/// places): uncertain, its file is drawn, never replayed (a replayed old line is the worst
/// outcome).
type Texts<'f> = blaze_own::BTreeMap<&'f str, Option<ContentId>, &'f Frame>;

fn texts_of<'f>(rows: &crate::sweep::Rows<'_>, frame: &'f Frame) -> Result<Texts<'f>, CliError> {
    use crate::sweep::{ABSENT, BOTH, LeafKind};
    let mut texts: Texts<'f> = blaze_own::BTreeMap::new_in(frame);
    let mut seen = blaze_own::BTreeSet::new_in(frame);
    for row in rows.rows.iter().rev() {
        if !matches!(row.kind, LeafKind::Slot | LeafKind::Manifest | LeafKind::MacroFile) {
            continue;
        }
        let slot = rows.path(row);
        // The newest row of a slot is its state; an older one is superseded.
        if !seen.insert((row.kind, slot)) {
            continue;
        }
        if seen.len() > MOST_BLOCKS {
            return Err(CliError::Text { what: "too many file slots for the rendered answer's text digests" });
        }
        if row.flags & (ABSENT | BOTH) != 0 {
            continue;
        }
        let physical = crate::sweep::real_path(slot, row.flags, frame);
        let path = core::str::from_utf8(physical.into_bump_slice()).map_err(|_| CliError::Text { what: "a sweep row's path is not UTF-8" })?;
        match texts.get(path).copied() {
            None => {
                texts.insert(path, Some(row.id));
            }
            Some(Some(id)) if id == row.id => {}
            // Two rows, two digests: not known which the printer would read.
            Some(_) => {
                texts.insert(path, None);
            }
        }
    }
    Ok(texts)
}

/// Extracts the source file path from the next entry in `reader`, skipping `NOTE`, `NOTED`, and
/// `VIEW` entries without `blaze_report::finding::decode` or UTF-8 validation overhead while still
/// checking tags and byte bounds.
fn next_entry_path<'m>(reader: &mut Reader<'m>) -> Result<Option<&'m str>, LinesError> {
    let tag = reader.byte("an entry's tag")?;
    match tag {
        NOTE => {
            let at = reader.at;
            Class::of_byte(reader.byte("a note's class")?).ok_or(LinesError { at, what: "a note's class is none of the six" })?;
            let _ = reader.sized("a note's words")?;
            Ok(None)
        }
        NOTED => {
            let _ = reader.sized("a noted finding")?;
            Ok(None)
        }
        VIEW => {
            let at_kind = reader.at;
            Kind::from_u8(reader.byte("a view's base kind")?).ok_or(LinesError { at: at_kind, what: "a view's base kind is no task kind" })?;
            let _ = reader.take(32, "a view's base subject")?;
            let at_place = reader.at;
            let at = reader.u32("a view's place")?;
            let units = reader.u32("a view's unit count")?;
            if units < 2 || at >= units {
                return Err(LinesError { at: at_place, what: "a view's place is not below its unit count, or the count is under two" });
            }
            for _ in 0..units {
                let at_flag = reader.at;
                if reader.byte("a view's unit flag")? > 1 {
                    return Err(LinesError { at: at_flag, what: "a view's unit flag is neither 0 nor 1" });
                }
                let _ = reader.sized("a view's unit name")?;
            }
            Ok(None)
        }
        _ => {
            reader.at -= 1;
            match entry(reader)? {
                Entry::Finding { path: Some(path), .. } => core::str::from_utf8(path)
                    .map(Some)
                    .map_err(|_| LinesError { at: reader.at, what: "a finding's path is not UTF-8" }),
                Entry::Finding { rule, path: None, words, .. } => {
                    if crate::names::placed(rule) && words.contains(&0x1F) {
                        match crate::names::place_pieces(words) {
                            Ok((Some((path, _, _)), _)) => Ok(Some(path)),
                            Ok((None, _)) => Ok(None),
                            Err(_) => Err(LinesError { at: reader.at, what: "a finding's place pieces are malformed" }),
                        }
                    } else {
                        Ok(None)
                    }
                }
                Entry::Placed { path, .. } | Entry::PlacedTokens { path, .. } | Entry::PlacedOwn { path, .. } => Ok(Some(path)),
                Entry::Note { .. } | Entry::Noted { .. } | Entry::View(_) => Ok(None),
            }
        }
    }
}

/// Observes every file path referenced by finding entries in `lines` using its unambiguous sweep
/// digest from `texts` (populated lazily on the first referenced path). Returns `Ok(false)`
/// (forcing a fallthrough to full rendering) if any entry fails to parse or any referenced path
/// lacks a single unambiguous sweep digest.
fn observe_standing_paths<'f>(
    lines: &Lines<'_>,
    rows: &crate::sweep::Rows<'_>,
    texts: &mut Option<Texts<'f>>,
    sources: &Sources<'f>,
    frame: &'f Frame,
) -> Result<bool, CliError> {
    for (_, block) in lines.blocks() {
        let mut reader = Reader { bytes: block, at: 0 };
        while reader.at < block.len() {
            let Ok(path) = next_entry_path(&mut reader) else { return Ok(false) };
            if let Some(path) = path {
                let map = match texts {
                    Some(map) => map,
                    None => texts.insert(texts_of(rows, frame)?),
                };
                let Some(id) = map.get(path).copied().flatten() else {
                    return Ok(false);
                };
                sources.observe_id(path, id)?;
            }
        }
    }
    Ok(true)
}

#[cfg(test)]
std::thread_local! {
    /// The planted control of a section's key ([`with_a_key_blind_to_text`]).
    static BLIND_TO_TEXT: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
}

/// `run` with a file section's key made without its text digest (rule 23's control): a body
/// edit that moves a line then replays the old line number, and the test that compares with a
/// fresh check catches it.
#[cfg(test)]
pub(crate) fn with_a_key_blind_to_text<R>(run: impl FnOnce() -> R) -> R {
    struct Back;
    impl Drop for Back {
        fn drop(&mut self) {
            BLIND_TO_TEXT.with(|blind| blind.set(false));
        }
    }
    BLIND_TO_TEXT.with(|blind| blind.set(true));
    let _back = Back;
    run()
}

/// Whether a section's key holds its file's text digest: always, but under the control.
fn key_reads_text() -> bool {
    #[cfg(test)]
    if BLIND_TO_TEXT.with(core::cell::Cell::get) {
        return false;
    }
    true
}

/// A file section's key (module docs at [`RENDER_FORM`]).
fn section_key(presentation: ContentId, path: &str, text: ContentId, findings: ContentId) -> ContentId {
    let mut h = blaze_ids::Hasher::new(SECTION_DOMAIN);
    h.update(&presentation.0).part(path.as_bytes());
    if key_reads_text() {
        h.update(&text.0);
    }
    h.update(&findings.0);
    h.finish()
}

/// The digest of a run's findings' rows: each one's task, rule, range, place and words, all a
/// finding's listing and drawing read of it beside its file's text (print.rs `placed_in`,
/// `unplaced`, `finding`).
fn findings_digest(prints: &[Print<'_>], wanted: &[print::Wanted<'_>]) -> ContentId {
    let mut h = blaze_ids::Hasher::new(FINDINGS_DOMAIN);
    h.update(&(prints.len() as u64).to_le_bytes());
    for (finding, wanted) in prints.iter().zip(wanted.iter()) {
        h.update(&[finding.name.kind as u8]).update(&finding.name.subject.0).update(&finding.rule.to_le_bytes())
            .update(&finding.start.to_le_bytes()).update(&finding.end.to_le_bytes());
        match wanted.place {
            print::Place::At(start) => h.update(&[0]).update(&start.to_le_bytes()),
            print::Place::Chain(chain, edition) => h.update(&[1]).update(&chain.to_le_bytes()).update(&[edition.byte()]),
            print::Place::Tokens(chain, edition, at) => {
                let (inside, (from, to)) = match at.inside {
                    Some(inside) => (1u8, inside),
                    None => (0u8, (0, 0)),
                };
                h.update(&[2]).update(&chain.to_le_bytes()).update(&[edition.byte()]).update(&at.first.to_le_bytes())
                    .update(&at.last.to_le_bytes()).update(&[inside]).update(&from.to_le_bytes()).update(&to.to_le_bytes())
            }
            print::Place::Counted => h.update(&[3]),
            print::Place::Own(chain, edition) => h.update(&[4, u8::from(chain.is_some())]).update(&chain.unwrap_or(0).to_le_bytes()).update(&[edition.byte()]),
        };
        h.opt_part(wanted.path.map(str::as_bytes)).part(wanted.words);
    }
    h.finish()
}

/// How one run of a file's findings is printed on an edit ([`Sectioned`]).
#[derive(Clone, Copy)]
enum Plan<'m, 'f> {
    /// Drawn, its drawings rows of their own: a run of no file, or of a file whose text digest
    /// is uncertain or that holds a finding adding to the printer's lists.
    Drawn,
    /// Drawn and stored as one section under its key.
    Section(ContentId),
    /// The last view's section of the same key: written out as stored when its gap and its
    /// ids' shown lengths stand (`at`: its ids' first place in the lengths' list), else drawn.
    Stored { key: ContentId, path: &'f str, row: FileRow<'m>, text: ContentId, at: usize },
}

/// A stored id standing in the lengths' list: only its id is read there (fix.rs `lengths`).
fn stand_in<'f>(id: crate::fix::Id) -> crate::fix::Listed<'f> {
    crate::fix::Listed {
        path: None,
        about: Name::new(Kind::BodyCheck, ContentId([0; 32])),
        read: ContentId([0; 32]),
        area: blaze_report::frame::words::Area::Cli,
        rule: 0,
        words: &[],
        item: 0,
        start: 0,
        end: 0,
        id,
        fix: None,
    }
}

/// The files of an edited answer: a file's stored section written out where it stands (its
/// file not read), the rest read, placed and drawn as [`Whole`] does, each stored as a section
/// for the next request (module docs at [`RENDER_FORM`]).
struct Sectioned<'p, 'm, 'a, 'f> {
    files: &'p SMap<'a, ContentId, FileRow<'m>>,
    ids: &'p [(crate::fix::Id, bool)],
    /// None: no sweep rows at hand (a new view of a standing answer); every file is drawn.
    texts: Option<&'p Texts<'f>>,
    presentation: ContentId,
    alloc: SharedAlloc<'a>,
    replayed: u32,
}

impl<'p, 'm, 'a, 'f> Sectioned<'p, 'm, 'a, 'f> {
    /// The run `which` of `path` drawn as one section and stored under `key`.
    #[expect(clippy::too_many_arguments, reason = "one section's inputs, each a different one")]
    fn draw_section(&mut self, cx: &Cx, root: &Dir<ReadWrite>, key: ContentId, path: &'f str, which: core::ops::Range<usize>, scratch: &mut print::Scratch, shown: &mut print::Shown<'f>, out: &Capture<'m, 'a, '_, 'f>) -> Result<(), CliError> {
        let before = Before::of(shown);
        let gap = print::frame_gap(shown);
        out.open_section();
        let drawn = print::file_findings(cx, root, Some(path), which.clone(), shown, scratch, out);
        let bytes = out.close_section();
        drawn?;
        let deltas = Deltas::since(before, shown);
        let mut ids: SVec<'a, (crate::fix::Id, bool)> = SVec::new_in(self.alloc);
        ids.extend(shown.listed[which].iter().filter(|listed| listed.id.is_shown()).map(|listed| (listed.id, listed.fix.is_some())));
        out.drawn.borrow_mut().append_file(key, gap, shown.drew_in(path), deltas, ids.as_slice(), bytes.as_slice())
    }

    /// A stored section written out: its bytes, and what its drawing added to the printer.
    fn replay(&mut self, key: ContentId, path: &'f str, row: FileRow<'m>, text: ContentId, shown: &mut print::Shown<'f>, out: &Capture<'m, 'a, '_, 'f>) -> Result<(), CliError> {
        // The witness the file's placing would have made: the sweep's digest of its bytes.
        out.sources.observe_id(path, text)?;
        let ids = ids_of(self.ids, &row);
        row.deltas.add_to(shown);
        if row.drew {
            shown.file(path);
        }
        for &(id, fix) in ids {
            if fix {
                shown.fixes.push(id);
            }
        }
        let mut drawn = out.drawn.borrow_mut();
        drawn.append_file(key, row.gap, row.drew, row.deltas, ids, row.bytes)?;
        drawn.reused += 1;
        self.replayed = self.replayed.saturating_add(1);
        Ok(())
    }
}

impl<'p, 'm, 'a, 's, 'f> Files<'f, Capture<'m, 'a, 's, 'f>> for Sectioned<'p, 'm, 'a, 'f> {
    fn print(&mut self, cx: &Cx, root: &Dir<ReadWrite>, found: &Found<'_, '_, 'f>, scratch: &mut print::Scratch, shown: &mut print::Shown<'f>, frame: &'f Frame, out: &Capture<'m, 'a, 's, 'f>) -> Result<(), CliError> {
        let Found { prints, wanted, runs } = *found;
        let alloc = self.alloc;
        // Each run's plan, and the paths read now: each a file a run of which is not stored.
        let mut plans: SVec<'a, Plan<'m, 'f>> = SVec::new_in(alloc);
        let mut read: SSet<'a, &'f str> = SSet::new_in(alloc);
        for &(path, from, past) in runs {
            let plan = match path {
                None => Plan::Drawn,
                Some(path) => match self.texts.and_then(|texts| texts.get(path).copied().flatten()) {
                    None => Plan::Drawn,
                    Some(_) if prints[from..past].iter().any(print::adds_to_lists) => Plan::Drawn,
                    Some(text) => {
                        let key = section_key(self.presentation, path, text, findings_digest(&prints[from..past], &wanted[from..past]));
                        match self.files.get(&key) {
                            Some(row) => Plan::Stored { key, path, row: *row, text, at: 0 },
                            None => Plan::Section(key),
                        }
                    }
                },
            };
            if let (Some(path), false) = (path, matches!(plan, Plan::Stored { .. })) {
                read.insert(path);
            }
            plans.push(plan);
        }
        // A file read anyway is drawn whole: its stored runs are drawn with it.
        for plan in plans.iter_mut() {
            if let Plan::Stored { key, path, .. } = *plan
                && read.contains(path)
            {
                *plan = Plan::Section(key);
            }
        }
        let first = shown.listed.len();
        print::list_some(cx, root, prints, wanted, scratch, frame, &mut shown.listed, |path, bytes| out.source(path, bytes), |path| path.is_none_or(|path| read.contains(path)))?;
        // Each run placed now in its drawing order, as [`Whole`] draws it.
        for &(path, from, past) in runs.iter() {
            if path.is_none_or(|path| read.contains(path)) {
                by_place(&mut shown.listed[first + from..first + past]);
            }
        }
        // The ids' shown lengths over every file's ids: the listed, and the stored whole ids of
        // the files not read, each at the shortest length (fix.rs `lengths`, D70).
        let listed = shown.listed.len() - first;
        let mut ids: SVec<'a, crate::fix::Listed<'f>> = SVec::new_in(alloc);
        ids.extend(shown.listed[first..].iter().copied());
        for plan in plans.iter_mut() {
            if let Plan::Stored { row, at, .. } = plan {
                *at = ids.len();
                for &(id, _) in ids_of(self.ids, row) {
                    let (full, _) = id.parts();
                    let shortest = crate::fix::Id::of_parts(full, crate::fix::SHORT as u8).ok_or(CliError::Text { what: "a stored id is not of the id alphabet (engine_lines.rs BER2)" })?;
                    ids.push(stand_in(shortest));
                }
            }
        }
        crate::fix::lengths(&mut ids[..], frame);
        for (at, one) in ids[..listed].iter().enumerate() {
            shown.listed[first + at].id = one.id;
        }
        for (n, &(path, from, past)) in runs.iter().enumerate() {
            let which = first + from..first + past;
            match plans[n] {
                Plan::Drawn => print::file_findings(cx, root, path, which, shown, scratch, out)?,
                Plan::Section(key) => {
                    let Some(path) = path else { return Err(CliError::Text { what: "a file section's run has no file (engine_lines.rs `Sectioned`)" }) };
                    self.draw_section(cx, root, key, path, which, scratch, shown, out)?;
                }
                Plan::Stored { key, path, row, text, at } => {
                    let stored = ids_of(self.ids, &row);
                    let lengths_stand = stored.iter().zip(ids[at..at + stored.len()].iter()).all(|(&(was, _), now)| was == now.id);
                    if lengths_stand && row.gap == print::frame_gap(shown) && !read.contains(path) {
                        self.replay(key, path, row, text, shown, out)?;
                        continue;
                    }
                    // The section does not stand: its file read and placed now, once, and each
                    // stored run of it given its ids' lengths of today.
                    if !read.contains(path) {
                        print::place_listed(cx, root, prints, wanted, scratch, frame, &mut shown.listed[first..], |path, bytes| out.source(path, bytes), |at_path| at_path == Some(path))?;
                        read.insert(path);
                        for (m, &(of, from, past)) in runs.iter().enumerate() {
                            if of != Some(path) {
                                continue;
                            }
                            let Plan::Stored { row, at, .. } = plans[m] else { continue };
                            by_place(&mut shown.listed[first + from..first + past]);
                            take_lengths(&mut shown.listed[first + from..first + past], ids_of(self.ids, &row), &ids[at..], path)?;
                        }
                    }
                    self.draw_section(cx, root, key, path, which, scratch, shown, out)?;
                }
            }
        }
        Ok(())
    }

    fn replayed(&self) -> u32 {
        self.replayed
    }
}

/// A late-placed run's ids given today's shown lengths: its listed ids must be its stored
/// ones, in order, with the same fix marks (the key's promise; else two values under one key,
/// a typed refusal: rule 21), and each takes the length the lengths' list gave its stand-in.
fn take_lengths(listed: &mut [crate::fix::Listed<'_>], stored: &[(crate::fix::Id, bool)], today: &[crate::fix::Listed<'_>], path: &str) -> Result<(), CliError> {
    let mut at = 0;
    for one in listed.iter_mut().filter(|one| one.id.is_shown()) {
        let (Some(&(was, fix)), Some(now)) = (stored.get(at), today.get(at)) else { return Err(print::section_key_missed(path)) };
        if was.parts().0 != one.id.parts().0 || fix != one.fix.is_some() {
            return Err(print::section_key_missed(path));
        }
        one.id = now.id;
        at += 1;
    }
    if at != stored.len() {
        return Err(print::section_key_missed(path));
    }
    Ok(())
}

/// The bounded, checkout-local root for this view. At most three per answer root.
/// Its one immutable record contains the semantic+placement key, options, trailer inputs,
/// block index and stdout. Changing either root before a crash can only produce a miss.
pub(crate) struct ViewSlot<'m, 'f> {
    pub(crate) name: blaze_own::String<&'f Frame>,
    tools: blaze_engine::Tools,
    previous: Option<Rendered<'m>>,
}

fn malformed(error: LinesError) -> CliError {
    CliError::Draw(print::DrawError::Rendered { at: error.at, what: error.what })
}

impl<'m, 'f> ViewSlot<'m, 'f> {
    pub(crate) fn load(
        lent: &'m crate::store::CommandLent, answer: &str, fresh: bool,
        shown: &print::Shown<'_>, tools: &blaze_engine::Tools, frame: &'f Frame,
    ) -> Result<Self, CliError> {
        let local = crate::store::PlainPlace::new(lent, false);
        let view = match shown.view {
            blaze_report::frame::plain::View::Person => "person",
            blaze_report::frame::plain::View::Agent => "agent",
            blaze_report::frame::plain::View::Json => "json",
        };
        let name = bstd::mem::format_in!(frame, "{answer}.render.{view}");
        let previous = if !fresh && shown.ready_to_replay() && let Some(keys) = local.root(name.as_str())? {
            let bytes = <[u8; 32]>::try_from(keys.bytes).map_err(|_| CliError::Text { what: "a rendered answer root must name exactly one record" })?;
            let key = Key(ContentId(bytes));
            let held = local.get(key)?.ok_or(CliError::Draw(print::DrawError::MissingRender(key)))?;
            Some(Rendered::open(held.bytes).map_err(malformed)?)
        } else {
            None
        };
        Ok(Self { name, tools: *tools, previous })
    }

    /// Only after scope, owed, replayability and the observation certificate have stood.
    /// A miss (including a caller requiring the listing) writes no byte.
    pub(crate) fn certified(&self, cx: &Cx, lines: Key, shown: &mut print::Shown<'f>, frame: &'f Frame, out: &Out) -> Result<Option<Tally>, CliError> {
        let hit = self.previous.filter(|old| shown.ready_to_replay() && old.matches(lines, shown.presentation(&self.tools)));
        hit.map(|old| old.replay(shown, frame, |bytes| Ok(out.write_all(cx, bytes)?))).transpose()
    }

    /// Entry point for the existing page-form certificate. A matching stream never
    /// opens the semantic lines record. A damaged semantic record takes the command's
    /// ordinary named Unreadable path before any bytes have been transferred.
    pub(crate) fn standing<D: blaze_core::Driver, S: crate::store::Sharing>(
        &self, cx: &Cx, root: &Dir<ReadWrite>, store: &mut crate::store::CommandStore<'m, '_, D, S>,
        lines: Key, shown: &mut print::Shown<'f>, alloc: SharedAlloc<'_>, frame: &'f Frame, out: &Out,
    ) -> Result<Result<Tally, crate::engine_answer::Unreadable>, CliError> {
        if let Some(tally) = self.certified(cx, lines, shown, frame, out)? {
            return Ok(Ok(tally));
        }
        match crate::engine_answer::load_lines(cx, store, lines, alloc)? {
            Ok(last) => Ok(Ok(self.new_view(cx, root, store, lines, &last, shown, alloc, frame, out)?)),
            Err(why) => Ok(Err(why)),
        }
    }

    /// A new view, a listing for fixes, and a legacy BEF3 record can all be served
    /// from semantics without planning a task. A BEF3 key alone cannot certify placement.
    pub(crate) fn new_view<D: blaze_core::Driver, S: crate::store::Sharing>(
        &self, cx: &Cx, root: &Dir<ReadWrite>, store: &mut crate::store::CommandStore<'m, '_, D, S>,
        lines: Key, last: &Last<'_, '_>, shown: &mut print::Shown<'f>, alloc: SharedAlloc<'_>, frame: &'f Frame, out: &Out,
    ) -> Result<Tally, CliError> {
        let Some(placement) = last.placement.filter(|_| shown.ready_to_replay()) else {
            let tally = print_all(cx, root, last.blocks(), shown, alloc, frame, out)?;
            crate::engine_head_notes::print_grouped(cx, last.blocks(), shown, frame, out)?;
            return Ok(tally);
        };
        let sources = Sources::new(frame);
        let (tally, pending) = self.draw(cx, root, last.blocks(), &sources, shown, alloc, frame)?;
        self.publish(cx, store, PlacedLines { key: lines, placement }, pending, out)?;
        Ok(tally)
    }

    /// For an edit, prepare both records from the same semantic blocks and raw source
    /// witness. Publish the rendering before the answer's existing final store commit.
    pub(crate) fn render<'a, D: blaze_core::Driver, S: crate::store::Sharing>(
        &self, cx: &Cx, root: &Dir<ReadWrite>, store: &mut crate::store::CommandStore<'m, '_, D, S>,
        lines: &Lines<'a>, rows: &mut crate::sweep::Rows<'_>, shown: &mut print::Shown<'f>, frame: &'f Frame, out: &Out,
    ) -> Result<(Tally, (Key, SVec<'a, u8>)), CliError> {
        let sources = Sources::new(frame);
        if !shown.ready_to_replay() {
            let sink = ListingOutput { sources: &sources, out };
            let tally = print_all(cx, root, lines.blocks(), shown, lines.alloc, frame, &sink)?;
            crate::engine_head_notes::print_grouped(cx, lines.blocks(), shown, frame, out)?;
            sources.cover(rows)?;
            let (binding, bytes) = lines.record_for(sources.digest());
            return Ok((tally, (binding.key, bytes)));
        }
        // Each file's text digest from this command's sweep, built lazily only if a finding path
        // is observed (skipping the full sweep-row scan on standing replays with zero file paths).
        let mut texts: Option<Texts<'f>> = None;
        if let Some(replayed) = self.try_replay_standing(cx, store, lines, &mut texts, rows, shown, frame, out)? {
            return Ok(replayed);
        }
        let texts = match texts {
            Some(map) => map,
            None => texts_of(rows, frame)?,
        };
        let (tally, pending) = self.draw_with(cx, root, lines.blocks(), &sources, shown, lines.alloc, frame, Some(&texts))?;
        sources.cover(rows)?;
        let (binding, bytes) = lines.record_for(pending.placement);
        self.publish(cx, store, binding, pending, out)?;
        Ok((tally, (binding.key, bytes)))
    }

    /// Zero-rewrite fast path when every semantic block and every observed file's text digest
    /// match the previous rendered view: skips re-rendering, re-encoding, re-hashing, and
    /// re-putting both the `BEFC` lines record and the `BER3` view record, transferring the cached
    /// stdout in one bulk write and returning empty bytes so `engine_answer::save` reuses the
    /// already-stored `lines` key.
    #[expect(clippy::too_many_arguments, reason = "render's fast path takes the same handles as render")]
    fn try_replay_standing<'a, D: blaze_core::Driver, S: crate::store::Sharing>(
        &self,
        cx: &Cx,
        store: &mut crate::store::CommandStore<'m, '_, D, S>,
        lines: &Lines<'a>,
        texts: &mut Option<Texts<'f>>,
        rows: &mut crate::sweep::Rows<'_>,
        shown: &mut print::Shown<'f>,
        frame: &'f Frame,
        out: &Out,
    ) -> Result<Option<(Tally, (Key, SVec<'a, u8>))>, CliError> {
        const RENEW_SENTINEL: blaze_toolchain::ToolchainId = blaze_toolchain::ToolchainId(*b"blaze-renew-no-toolchain-root\0\0\0");
        if !key_reads_text() {
            return Ok(None);
        }
        let presentation = shown.presentation(&self.tools);
        let Some(old) = self.previous.filter(|old| !old.old_form && old.presentation == presentation) else {
            return Ok(None);
        };
        let Some(held) = crate::store::Store::get(store, cx, old.lines)? else {
            return Ok(None);
        };
        if !held.bytes.starts_with(LINES_PLACED_MAGIC) || held.bytes.len() < 40 {
            return Ok(None);
        }
        if !lines.blocks_equal_encoded(&held.bytes[36..]) {
            return Ok(None);
        }
        let mut old_placement = [0u8; 32];
        old_placement.copy_from_slice(&held.bytes[4..36]);
        let fast_sources = Sources::new(frame);
        if !observe_standing_paths(lines, rows, texts, &fast_sources, frame)? {
            return Ok(None);
        }
        if fast_sources.digest() != ContentId(old_placement) {
            return Ok(None);
        }
        fast_sources.cover(rows)?;
        let tally = old.replay(shown, frame, |bytes| Ok(out.write_all(cx, bytes)?))?;
        if !self.name.is_empty() {
            store.renew(&[self.name.as_str()], RENEW_SENTINEL)?;
        }
        Ok(Some((tally, (old.lines, SVec::new_in(lines.alloc)))))
    }

    /// Prepare the current answer's immutable rendering. The fresh printer
    /// owns ordering, grouping, fix-id widths and counts, including when a drawing stands.
    fn draw<'a, 'b>(
        &self, cx: &Cx, root: &Dir<ReadWrite>, blocks: impl Iterator<Item = (Name, &'b [u8])> + Clone,
        sources: &Sources<'f>, shown: &mut print::Shown<'f>, alloc: SharedAlloc<'a>, frame: &'f Frame,
    ) -> Result<(Tally, PendingRender<'a>), CliError> {
        self.draw_with(cx, root, blocks, sources, shown, alloc, frame, None)
    }

    /// [`ViewSlot::draw`] with each file's text digest (`texts`, [`texts_of`]): a stored file
    /// section whose key stands is written out with its file not read ([`Sectioned`]).
    #[expect(clippy::too_many_arguments, reason = "draw's inputs and the sweep's digests")]
    fn draw_with<'a, 'b>(
        &self, cx: &Cx, root: &Dir<ReadWrite>, blocks: impl Iterator<Item = (Name, &'b [u8])> + Clone,
        sources: &Sources<'f>, shown: &mut print::Shown<'f>, alloc: SharedAlloc<'a>, frame: &'f Frame,
        texts: Option<&Texts<'f>>,
    ) -> Result<(Tally, PendingRender<'a>), CliError> {
        let presentation = shown.presentation(&self.tools);
        let old_form = self.previous.is_some_and(|old| old.old_form);
        let previous = match self.previous.filter(|old| !old.old_form && old.presentation == presentation) {
            Some(old) => old.previous(alloc).map_err(malformed)?,
            None => Previous::none(alloc),
        };
        let Previous { blocks: drawings, files, ids } = previous;
        let sink = Capture { previous: drawings, sources, drawn: RefCell::new(Drawn::new_in(alloc)), section: RefCell::new(None), alloc };
        let mut sections = Sectioned { files: &files, ids: ids.as_slice(), texts, presentation, alloc, replayed: 0 };
        let mut tally = print_with(cx, root, blocks.clone(), shown, alloc, frame, &sink, &mut sections, FoldViews)?;
        crate::engine_head_notes::print_grouped(cx, blocks, shown, frame, &sink)?;
        let drawn = sink.drawn.into_inner();
        tally.rendering = Rendering::Published { blocks: drawn.rows, reused: drawn.reused, bytes: drawn.stream.len(), old_form };
        let mut header = SVec::new_in(alloc);
        header.extend_from_slice(RENDER_FORM);
        header.extend_from_slice(&[0; 32]); // filled when the semantic record gets its key
        header.extend_from_slice(&presentation.0);
        let counts = shown.counts;
        for n in [tally.failures, tally.unchecked, tally.waited, tally.borrows, tally.heads, tally.listed,
            counts.failures, counts.errors, counts.warnings, counts.notes, counts.files, counts.fixable,
            shown.behind.items, shown.behind.failures, shown.types_unchecked, shown.borrows_unchecked] {
            word(&mut header, n);
        }
        let fix_len = header.len();
        word(&mut header, 0);
        let mut ids = blaze_engine::SSet::new_in(alloc);
        for id in &shown.fixes {
            if ids.insert(id.as_str()) {
                if ids.len() > MOST_BLOCKS {
                    return Err(CliError::Text { what: "too many fix ids for the rendered answer" });
                }
                if ids.len() > 1 {
                    header.push(b' ');
                }
                header.extend_from_slice(id.as_str().as_bytes());
            }
        }
        let len = (header.len() - fix_len - 4) as u32;
        header[fix_len..fix_len + 4].copy_from_slice(&len.to_le_bytes());
        // The index's length (its rows are checked when an edit reads them: `previous`).
        word(&mut header, len_of(&drawn.index));
        Ok((tally, PendingRender { header, drawn, placement: sources.digest() }))
    }

    fn publish<D: blaze_core::Driver, S: crate::store::Sharing>(
        &self, cx: &Cx, store: &mut crate::store::CommandStore<'m, '_, D, S>, lines: PlacedLines, pending: PendingRender<'_>, out: &Out,
    ) -> Result<(), CliError> {
        use crate::store::Store as _;
        pending.deliver(lines, |bytes| Ok(out.write_all(cx, bytes)?), |effect| match effect {
            RenderWrite::Put(key, parts) => store.put_parts(cx, key, parts),
            RenderWrite::Pin(key) => store.pin(self.name.as_str(), &[key]),
        })
    }
}

#[derive(Clone, Copy)]
struct PlacedLines {
    key: Key,
    placement: ContentId,
}

struct PendingRender<'a> {
    header: SVec<'a, u8>,
    drawn: Drawn<'a>,
    placement: ContentId,
}

enum RenderWrite<'b> {
    Put(Key, &'b [&'b [u8]]),
    Pin(Key),
}

impl PendingRender<'_> {
    fn deliver(mut self, lines: PlacedLines, write_all: impl FnOnce(&[u8]) -> Result<(), CliError>, mut apply: impl FnMut(RenderWrite<'_>) -> Result<(), CliError>) -> Result<(), CliError> {
        if self.placement != lines.placement {
            return Err(CliError::Text { what: "the rendered blocks and semantic answer used different placement facts" });
        }
        write_all(&self.drawn.stream)?;
        self.header[4..36].copy_from_slice(&lines.key.0.0);
        let size = (self.drawn.stream.len() as u32).to_le_bytes();
        let parts: [&[u8]; 4] = [&self.header, &self.drawn.index, &size, &self.drawn.stream];
        let mut hash = blaze_ids::Hasher::new("blaze-cli local rendered answer 1");
        for part in parts {
            hash.update(part);
        }
        let key = Key(hash.finish());
        apply(RenderWrite::Put(key, &parts))?;
        apply(RenderWrite::Pin(key))
    }
}

#[cfg(test)]
#[path = "tests/engine_render_replay_tests.rs"]
mod replay_tests;

#[cfg(test)]
#[path = "tests/engine_lines_replay_tests.rs"]
mod lines_replay_tests;

/// A FILE'S RENDERED LINES (engine.md D149, piece I3): on an edit, a file whose text and
/// findings stand is written out from its stored section and not read; a file whose text moved
/// is drawn alone. What is right is a fresh check's output in a new home (rule 26): the same
/// stdout bytes, the same counts, the same stored record.
#[cfg(test)]
mod section_tests {
    use super::*;
    use blaze_report::frame::Caps;
    use blaze_report::frame::plain::View;
    use bstd::fs::{Content, Durable};
    use bstd::io::{Io, Limits, SimWorld};
    use bstd::mem::TASK_FRAME;
    use bstd::path::Path;
    use bstd::test::BUDGET;
    use std::vec::Vec as Bytes;

    const EDITION: blaze_surface::Edition = crate::fix::ITEM_GRAMMAR;
    const PARSER_EDITION: parser::Edition = parser::Edition::Edition2024;
    /// One type error in `first`.
    const A_RS: &str = "fn first() {\n    let x: u32 = true;\n}\n";
    /// `A_RS` with its body edited: a comment line inside `first` above its error. No token
    /// moved, so the body's finding stands as it was made in `A_RS`; its line is one lower.
    const A_RS_EDITED: &str = "fn first() {\n    // the error is below\n    let x: u32 = true;\n}\n";
    /// One type error, in a second file the edit does not touch.
    const B_RS: &str = "/// The one.\npub fn only() -> u8 {\n    'c'\n}\n";

    fn tools() -> blaze_engine::Tools {
        blaze_engine::Tools([blaze_drive::Tool(ContentId([5; 32])); blaze_drive::key::KINDS])
    }

    /// A finding's token place: the bytes of `needle` in `text` as a body check makes it, and
    /// its item's chain.
    fn place_of(text: &str, needle: &str, frame: &Frame) -> (parser::token_key::TokenPlace, u64) {
        let at = text.find(needle).expect("the needle is in the text");
        let (item, _, chain) = crate::fix::item_of(text, at as u32, Some(EDITION), frame);
        let item_text = &text[item as usize..];
        let from = at - item as usize;
        let place = parser::token_key::TokenPlace::from_bytes(item_text, PARSER_EDITION, from..from + needle.len()).expect("the needle is whole tokens");
        (place, chain)
    }

    /// `path:line:column` of `needle` in `text`, both from 1: where a plain view prints it.
    fn line_col(path: &str, text: &str, needle: &str) -> std::string::String {
        let at = text.find(needle).expect("the needle is in the text");
        let line = text[..at].matches('\n').count() + 1;
        let column = at - text[..at].rfind('\n').map_or(0, |newline| newline + 1) + 1;
        format!("{path}:{line}:{column}")
    }

    /// What one edit-path request printed and stored.
    struct Pass {
        out: Bytes<u8>,
        record: Bytes<u8>,
        tally: Tally,
        counts: blaze_report::frame::Counts,
        fixes: usize,
        types: u32,
        borrows: u32,
    }

    /// One request over the files `now` (the two findings made in `A_RS` and `B_RS`, as the
    /// standing bodies hold them), in the agent's view, through the edit path's renderer
    /// (`ViewSlot::render`'s `draw_with` with the sweep's digests), the last view `previous`.
    fn pass(io: &Io, now: &[(&str, &str)], previous: Option<&[u8]>) -> Pass {
        let cx = io.cx(BUDGET, TASK_FRAME).expect("request context");
        let root = io.need_fs().expect("filesystem").root();
        let out = io.need_out().expect("stdout");
        let frame = cx.frame();
        let alloc = blaze_own::testing::heap().shared();
        let printed_before = io.script(|host| host.out.len()).expect("simulated stdout");
        let mut rows = crate::sweep::Rows::new_in(frame);
        for &(path, text) in now {
            root.publish(&cx, &Path::new(path).unwrap(), Content::bytes(text.as_bytes()), Durable::No).unwrap();
            rows.push(crate::sweep::LeafKind::Slot, 0, path, None, ContentId::of(crate::sweep::FILE_DOMAIN, text.as_bytes()));
        }
        let mut lines = Lines::new_in(alloc);
        for (n, (path, made_in, needle)) in [("a.rs", A_RS, "true"), ("b.rs", B_RS, "'c'")].into_iter().enumerate() {
            let (at, chain) = place_of(made_in, needle, frame);
            lines.placed_tokens(Name::new(Kind::BodyCheck, ContentId([n as u8 + 1; 32])), crate::work::SYNTAX, at, path.as_bytes(), chain, EDITION, b"expected an expression");
        }
        let mut shown = print::Shown::new(Caps::LINES, View::Agent, "/ws", frame);
        shown.output_only = true;
        let slot = ViewSlot { name: blaze_own::String::new_in(frame), tools: tools(), previous: previous.map(|bytes| Rendered::open(bytes).expect("the last view opens")) };
        let sources = Sources::new(frame);
        let texts = texts_of(&rows, frame).unwrap();
        let (tally, pending) = slot.draw_with(&cx, &root, lines.blocks(), &sources, &mut shown, alloc, frame, Some(&texts)).unwrap();
        sources.cover(&mut rows).unwrap();
        let (binding, _) = lines.record_for(pending.placement);
        let mut record = Bytes::new();
        pending.deliver(binding, |bytes| Ok(out.write_all(&cx, bytes)?), |effect| {
            if let RenderWrite::Put(_, parts) = effect {
                record = parts.concat();
            }
            Ok(())
        }).unwrap();
        let out = io.script(|host| host.out[printed_before..].to_vec()).expect("simulated stdout");
        Pass { out, record, tally, counts: shown.counts, fixes: shown.fixes.len(), types: shown.types_unchecked, borrows: shown.borrows_unchecked }
    }

    /// Two files each with a finding; a body of one edited (a comment above its error), then
    /// reverted. Each edit renders the edited file alone and replays the other, and prints what a
    /// fresh check of the same text in a new home prints. `blind`: the planted key without the
    /// text digest, on the edit path's requests only (the fresh oracle is never planted).
    fn edit_and_revert(blind: bool) -> Result<(), &'static str> {
        let run = |io: &Io, now: &[(&str, &str)], previous: Option<&[u8]>| if blind { with_a_key_blind_to_text(|| pass(io, now, previous)) } else { pass(io, now, previous) };
        let home = Io::simulated(SimWorld::new(31, Limits::USUAL));
        let first = run(&home, &[("a.rs", A_RS), ("b.rs", B_RS)], None);
        assert_eq!((first.tally.tail.placing.placed, first.tally.tail.replayed), (2, 0), "a first request reads and draws both files");
        let edited = run(&home, &[("a.rs", A_RS_EDITED), ("b.rs", B_RS)], Some(&first.record));
        let fresh = pass(&Io::simulated(SimWorld::new(32, Limits::USUAL)), &[("a.rs", A_RS_EDITED), ("b.rs", B_RS)], None);
        let said = core::str::from_utf8(&fresh.out).unwrap();
        assert!(said.contains(&line_col("a.rs", A_RS_EDITED, "true")), "the oracle prints the error at its line today: {said}");
        assert!(said.contains(&line_col("b.rs", B_RS, "'c'")), "{said}");
        if edited.out != fresh.out {
            return Err("an old line number was replayed: the edit's lines are not a fresh check's");
        }
        assert_eq!((edited.tally.tail.placing.placed, edited.tally.tail.replayed), (1, 1), "I3: rendered 1 (the edited file), replayed 1 (the other, not read)");
        assert!(edited.counts == fresh.counts, "a replayed section adds the counts its drawing added");
        assert_eq!((edited.fixes, edited.types, edited.borrows), (fresh.fixes, fresh.types, fresh.borrows));
        assert_eq!(edited.record, fresh.record, "rule 26: the stored view after an edit equals a fresh one in bytes");
        let reverted = run(&home, &[("a.rs", A_RS), ("b.rs", B_RS)], Some(&edited.record));
        if reverted.out != first.out {
            return Err("an old line number was replayed: the revert's lines are not the first request's");
        }
        assert_eq!((reverted.tally.tail.placing.placed, reverted.tally.tail.replayed), (1, 1), "the revert renders the reverted file alone");
        assert_eq!(reverted.record, first.record, "the revert's stored view is the first request's");
        let unmoved = run(&home, &[("a.rs", A_RS), ("b.rs", B_RS)], Some(&reverted.record));
        assert_eq!((unmoved.tally.tail.placing.placed, unmoved.tally.tail.replayed, unmoved.tally.tail.placing.drawn), (0, 2, 0), "nothing moved: no file read, both sections replayed");
        assert_eq!(unmoved.out, first.out);
        Ok(())
    }

    #[test]
    fn a_stored_file_section_replays_what_a_fresh_check_prints() {
        assert_eq!(edit_and_revert(false), Ok(()));
    }

    /// CONTROL (rule 23): a section key that ignores the file's text digest replays the edited
    /// file's old section, its error at the old line, and the comparison with a fresh check
    /// catches it.
    #[test]
    fn a_key_blind_to_the_text_prints_an_old_line_and_is_caught() {
        assert_eq!(edit_and_revert(true), Err("an old line number was replayed: the edit's lines are not a fresh check's"));
    }

    /// A text digest two sweep rows disagree on is uncertain: its file is drawn, never replayed.
    #[test]
    fn a_path_two_rows_disagree_on_has_no_text_digest() {
        let io = Io::simulated(SimWorld::new(33, Limits::USUAL));
        let cx = io.cx(BUDGET, TASK_FRAME).unwrap();
        let frame = cx.frame();
        let mut rows = crate::sweep::Rows::new_in(frame);
        let (one, two) = (ContentId::of(crate::sweep::FILE_DOMAIN, A_RS.as_bytes()), ContentId::of(crate::sweep::FILE_DOMAIN, B_RS.as_bytes()));
        rows.push(crate::sweep::LeafKind::Slot, 0, "a.rs", None, one);
        rows.push(crate::sweep::LeafKind::Manifest, 0, "a.rs", None, two);
        rows.push(crate::sweep::LeafKind::Slot, 0, "b.rs", None, one);
        // A newer row of the same slot supersedes the older: b.rs is `two`.
        rows.push(crate::sweep::LeafKind::Slot, 0, "b.rs", None, two);
        rows.push(crate::sweep::LeafKind::Slot, crate::sweep::ABSENT, "c.rs", None, ContentId([0; 32]));
        let texts = texts_of(&rows, frame).unwrap();
        assert_eq!(texts.get("a.rs").copied(), Some(None), "two kinds of row, two digests: uncertain");
        assert_eq!(texts.get("b.rs").copied(), Some(Some(two)), "the newest row of a slot is its state");
        assert_eq!(texts.get("c.rs").copied(), None, "an absent slot gives no digest");
    }

    /// An older form's view (BER1) is refused as a stand-in, reported and rebuilt (rule 26).
    #[test]
    fn an_older_view_form_is_rebuilt_not_read() {
        let old = Rendered::open(b"BER1 and whatever followed").expect("an older form opens as older");
        assert!(old.old_form && !old.matches(old.lines, old.presentation), "an older view stands in for nothing");
        let before_under = Rendered::open(b"BER2 and whatever followed").expect("BER2 opens as older");
        assert!(before_under.old_form, "a BER2 view, a note a body under a refused call, is an older form");
        assert!(Rendered::open(b"BERX").is_err(), "an unknown form is refused by name");
    }

    /// Zero-rewrite fast path (`ViewSlot::try_replay_standing`): when findings, notes, and
    /// observed files' text digests are unchanged on an edit, `ViewSlot::render` reuses the prior
    /// `lines` key with 0 bytes returned and 0 CAS puts, producing byte-for-byte identical stdout;
    /// moving a line in a file with a finding or adding/removing a finding or note invalidates the
    /// fast path and emits the exact updated diagnostic.
    #[test]
    fn unchanged_findings_reuse_prior_lines_with_zero_puts_and_changes_invalidate() {
        use crate::store::Store as _;
        use blaze_cas::plain::Settings;
        let io = Io::simulated(SimWorld::new(37, Limits::USUAL));
        let cx = io.cx(BUDGET, TASK_FRAME).expect("root context");
        let fs = io.need_fs().expect("filesystem");
        let root = fs.root();
        let home = crate::store::Home::of(&cx, &fs, Some("/h")).expect("home");
        let clock = io.need_clock().expect("clock");
        let out = io.need_out().expect("stdout");
        let err = io.need_err().expect("stderr");
        let heap = blaze_own::testing::heap();
        let alloc = heap.shared();
        let frame = cx.frame();
        let erased = blaze_own::Erased::new(frame);
        let lent = home.open_store(&Settings::default(), crate::store::Facts { boot: Some(1), nonce: 37 }, heap.alloc()).expect("store");
        let mut store = crate::store::CommandStore::new(&cx, &lent, false, 0, None, clock.clone(), &err, erased.handle()).expect("command store");
        let answer_name = "engine.zero_rewrite.test";

        let make_rows = |files: &[(&str, &str)]| {
            let mut rows = crate::sweep::Rows::new_in(frame);
            for &(path, text) in files {
                root.publish(&cx, &Path::new(path).unwrap(), Content::bytes(text.as_bytes()), Durable::No).unwrap();
                rows.push(crate::sweep::LeafKind::Slot, 0, path, None, ContentId::of(crate::sweep::FILE_DOMAIN, text.as_bytes()));
            }
            rows
        };
        let make_lines = |include_b: bool, note: Option<&[u8]>| {
            let mut lines = Lines::new_in(alloc);
            let (at_a, chain_a) = place_of(A_RS, "true", frame);
            lines.placed_tokens(Name::new(Kind::BodyCheck, ContentId([1; 32])), crate::work::SYNTAX, at_a, b"a.rs", chain_a, EDITION, b"expected an expression");
            if include_b {
                let (at_b, chain_b) = place_of(B_RS, "'c'", frame);
                lines.placed_tokens(Name::new(Kind::BodyCheck, ContentId([2; 32])), crate::work::SYNTAX, at_b, b"b.rs", chain_b, EDITION, b"expected an expression");
            }
            if let Some(words) = note {
                lines.note(Name::file(b"a.rs"), Class::Unchecked, words);
            }
            lines
        };

        // 1. Cold render: publishes both lines and rendered view records.
        let mut rows1 = make_rows(&[("a.rs", A_RS), ("b.rs", B_RS), ("clean.rs", "fn clean() {}\n")]);
        let lines1 = make_lines(true, None);
        let mut shown1 = print::Shown::new(Caps::LINES, View::Agent, "/ws", frame);
        shown1.output_only = true;
        let out_at1 = io.script(|h| h.out.len()).unwrap();
        let slot1 = ViewSlot::load(&lent, answer_name, false, &shown1, &tools(), frame).unwrap();
        let (tally1, (key1, bytes1)) = slot1.render(&cx, &root, &mut store, &lines1, &mut rows1, &mut shown1, frame, &out).unwrap();
        let stdout1 = io.script(|h| h.out[out_at1..].to_vec()).unwrap();
        assert!(!bytes1.is_empty(), "cold render encodes the BEFC lines record");
        assert!(matches!(tally1.rendering, Rendering::Published { .. }), "cold render publishes a new view");
        store.put(&cx, key1, &bytes1).unwrap();
        store.pin("engine.zero_rewrite.test.lines", &[key1]).unwrap();
        lent.commit().unwrap();

        // 2. Edit a clean file (no findings in `clean.rs`, findings & observed files unchanged):
        //    zero-rewrite fast path hits -> same lines key, empty bytes, 0 CAS puts, identical stdout!
        let mut rows2 = make_rows(&[("a.rs", A_RS), ("b.rs", B_RS), ("clean.rs", "fn clean() { let _ = 1; }\n")]);
        let lines2 = make_lines(true, None);
        let mut shown2 = print::Shown::new(Caps::LINES, View::Agent, "/ws", frame);
        shown2.output_only = true;
        let copied_before = lent.put_tally().copied;
        let out_at2 = io.script(|h| h.out.len()).unwrap();
        let slot2 = ViewSlot::load(&lent, answer_name, false, &shown2, &tools(), frame).unwrap();
        let (tally2, (key2, bytes2)) = slot2.render(&cx, &root, &mut store, &lines2, &mut rows2, &mut shown2, frame, &out).unwrap();
        let stdout2 = io.script(|h| h.out[out_at2..].to_vec()).unwrap();
        assert_eq!(key2, key1, "unchanged findings reuse the exact prior lines key");
        assert!(bytes2.is_empty(), "zero-rewrite fast path returns 0 bytes to put");
        assert_eq!(lent.put_tally().copied, copied_before, "zero-rewrite fast path copies 0 records into CAS");
        assert_eq!(lent.uncommitted_bytes(), 0, "zero-rewrite fast path leaves 0 uncommitted bytes");
        assert_eq!(stdout2, stdout1, "replayed stdout matches byte-for-byte");
        assert!(matches!(tally2.rendering, Rendering::Replayed { .. }), "fast path reports Rendering::Replayed");
        assert_eq!((tally2.listed, shown2.counts.errors), (tally1.listed, shown1.counts.errors));

        // 3. Edit `a.rs` above its error (semantic blocks identical, placement digest moved):
        //    must invalidate fast path and output the updated line number!
        let mut rows3 = make_rows(&[("a.rs", A_RS_EDITED), ("b.rs", B_RS), ("clean.rs", "fn clean() { let _ = 1; }\n")]);
        let lines3 = make_lines(true, None);
        let mut shown3 = print::Shown::new(Caps::LINES, View::Agent, "/ws", frame);
        shown3.output_only = true;
        let out_at3 = io.script(|h| h.out.len()).unwrap();
        let slot3 = ViewSlot::load(&lent, answer_name, false, &shown3, &tools(), frame).unwrap();
        let (tally3, (key3, bytes3)) = slot3.render(&cx, &root, &mut store, &lines3, &mut rows3, &mut shown3, frame, &out).unwrap();
        let stdout3 = io.script(|h| h.out[out_at3..].to_vec()).unwrap();
        let text3 = core::str::from_utf8(&stdout3).unwrap();
        assert_ne!(key3, key1, "moved line placement changes the BEFC lines key");
        assert!(!bytes3.is_empty(), "invalidated placement re-encodes lines");
        assert!(matches!(tally3.rendering, Rendering::Published { .. }), "moved placement publishes new view");
        assert!(text3.contains(&line_col("a.rs", A_RS_EDITED, "true")), "outputs updated line number: {text3}");
        assert!(!text3.contains(&line_col("a.rs", A_RS, "true")), "does not replay stale line number: {text3}");
        store.put(&cx, key3, &bytes3).unwrap();
        store.pin("engine.zero_rewrite.test.lines", &[key3]).unwrap();
        lent.commit().unwrap();

        // 4. Introducing a note invalidates the fast path and outputs the new note.
        let mut rows4 = make_rows(&[("a.rs", A_RS_EDITED), ("b.rs", B_RS), ("clean.rs", "fn clean() { let _ = 1; }\n")]);
        let lines4 = make_lines(true, Some(b"new unchecked note"));
        let mut shown4 = print::Shown::new(Caps::LINES, View::Agent, "/ws", frame);
        shown4.output_only = true;
        let out_at4 = io.script(|h| h.out.len()).unwrap();
        let slot4 = ViewSlot::load(&lent, answer_name, false, &shown4, &tools(), frame).unwrap();
        let (tally4, (key4, bytes4)) = slot4.render(&cx, &root, &mut store, &lines4, &mut rows4, &mut shown4, frame, &out).unwrap();
        let stdout4 = io.script(|h| h.out[out_at4..].to_vec()).unwrap();
        let text4 = core::str::from_utf8(&stdout4).unwrap();
        assert_ne!(key4, key3, "added note changes the lines key");
        assert!(!bytes4.is_empty(), "added note re-encodes lines");
        assert_eq!(tally4.unchecked, 1, "added note is counted");
        assert!(text4.contains("new unchecked note"), "added note is printed: {text4}");
        store.put(&cx, key4, &bytes4).unwrap();
        store.pin("engine.zero_rewrite.test.lines", &[key4]).unwrap();
        lent.commit().unwrap();

        // 5. Removing a finding (`b.rs`) invalidates the fast path and drops `b.rs` from output.
        let mut rows5 = make_rows(&[("a.rs", A_RS_EDITED), ("b.rs", B_RS), ("clean.rs", "fn clean() { let _ = 1; }\n")]);
        let lines5 = make_lines(false, Some(b"new unchecked note"));
        let mut shown5 = print::Shown::new(Caps::LINES, View::Agent, "/ws", frame);
        shown5.output_only = true;
        let out_at5 = io.script(|h| h.out.len()).unwrap();
        let slot5 = ViewSlot::load(&lent, answer_name, false, &shown5, &tools(), frame).unwrap();
        let (tally5, (key5, bytes5)) = slot5.render(&cx, &root, &mut store, &lines5, &mut rows5, &mut shown5, frame, &out).unwrap();
        let stdout5 = io.script(|h| h.out[out_at5..].to_vec()).unwrap();
        let text5 = core::str::from_utf8(&stdout5).unwrap();
        assert_ne!(key5, key4, "removed finding changes the lines key");
        assert!(!bytes5.is_empty(), "removed finding re-encodes lines");
        assert_eq!(tally5.listed, 1, "only a.rs finding remains");
        assert!(!text5.contains("b.rs"), "removed finding is no longer printed: {text5}");
    }
}
