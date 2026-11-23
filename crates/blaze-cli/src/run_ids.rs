//! Every body has a key before any check (run.md, "The cut to the lazy parent (D124)", piece I;
//! stage 5 of "End state (D113)"): the command's [`BodyIds`] for the run path's record walk
//! (blaze-ladder body_ids.rs `Walk`), so a function instance's code record is found by the
//! instance's symbol, its fn's [`BodyId`] and the crate facts of the items its name holds, with
//! nothing checked, resolved or read of a body.
//!
//! THE GRAIN of a body is its crate's, told by the checker key's first 8 bytes (its crate's
//! prefix: blaze-id `ItemKey::crate_prefix`, front2 keys.rs `checker_of`; a member's, an opaque
//! type's and a body-local item's key keep their container's prefix) and the crate list the
//! `UnitGraph` record holds (blaze-world crate_list.rs: the TOOLCHAIN flag, the root's tree and
//! the tree table's origin). Three typed cases ([`Grain`], asked by [`RunIds::grain`]; a
//! workspace body's check as the gate left it by [`RunIds::checked`]):
//!
//! 1. TOOLCHAIN (core, alloc, std, proc_macro, bstd and the rest of the pinned shelf, either
//!    side): key = H(the check's producers, the toolchain's content id, the crate's key, the
//!    item); reads = ONE fact, the toolchain ([`toolchain_fact`]), whose digest today is the
//!    content id. A coarse proof (AGENTS.md: "a coarse proof covers all under it"): the manifest
//!    names every pack of the cut by digest, so no input of such a body's check is outside it,
//!    and no `BodyCheck` record is needed for it.
//! 2. LOCKED (a crate whose root is in a fetched pack, `PackOrigin::Source`): key = H(the
//!    check's producers, the crate's key, the item); reads = ONE fact, the crate's
//!    ([`locked_fact`]), whose digest is a Merkle digest over its dependency closure: per crate
//!    its key, its pack's store key and files' BLAKE3 (`TreeRow`), then each dependency's
//!    digest in the list's order, a toolchain crate's being the toolchain's id. The crate key
//!    (blaze-world list.rs `unit_crate_key`) already holds its dependencies' keys, features,
//!    cfg, edition, root and a correction's row, but NOT its package's content (the unit's
//!    list key is name, version and the lock's `source` line, blaze-scope units.rs `unit_key`;
//!    a registry checksum is in neither): the pack digests are what make the one read cover
//!    the closure's text (rule 21).
//! 3. WORKSPACE (a crate whose root is in the checkout, tree 0): key = the key the engine's
//!    `BodyCheck` task row holds; reads = ONE fact, the check's coarse proof ([`body_fact`] of
//!    its name), whose digest folds every (fact, digest the check saw) of the row's reads in the
//!    row's order, and whose digest TODAY folds the same facts' digests today ([`Proof`]). The
//!    two are equal exactly when every read of the check stands, so the walk's one comparison
//!    (body_ids.rs `Walk::one`) is the engine's standing decision, and the code key hashes one
//!    digest, not the list. The proof is read BY NEED, at the gate of the call that steps the
//!    body ([`RunIds::runnable`], from the stored answer in place: [`BodyReads`]), once a check
//!    ([`Slot`]); the table holds no read list (the 16 GiB `RunIds::new` request on tempo,
//!    2026-11-21, was every workspace body's read list copied three times). No record valid
//!    today: `reads: &[]` (the walk makes that instance unkeyed, body_ids.rs
//!    `Unkeyed::NoRecord`), and [`RunIds::missing`] says why nothing was found; a key is never
//!    invented. A proof that cannot be read (no row, a refused page, past [`Most::reads`]) is
//!    [`NotRunnable::Unread`] at that call: that call is refused by name, the rest run.
//!
//! A drive read `(Name, out)` is the 32-byte fact [`fact_of`] (the name hashed) with the out as
//! its digest. [`BodyIds::crate_fact`] of a workspace crate's item is its `CrateScope` read
//! today (the crate's exports); of a toolchain item the toolchain fact; of a locked item the
//! crate's fact.
//!
//! THE TABLE IS A RECORD (the FORM below): written once per walk at the command's certificate
//! ([`table_key`]: the certificate, the toolchain and every kind's tool) through the one store
//! put ([`RunIds::put`], engine_start.rs `body_ids`), and read in place when the certificate
//! stands ([`RunIds::from_record`]). A forward walk knows only what it reached; what it did not
//! reach is carried from the last answer's table ([`RunIds::new`], `previous`).
//!
//! COST: [`RunIds::new`] reads the crate list (one row a crate, O(crates + dependency edges)
//! hashes for the locked closure digests) and, per workspace body the walk reached, its row's
//! key and verdict (its facts record); one it did not reach is a copy of `previous`'s row. It
//! solves no names and reads no read list. A hit costs one get and one pass over the bytes
//! that checks them. Every answer after it is a binary search over the record's bytes:
//! O(log crates) for the grain, O(log bodies) for a workspace body, O(log facts) for `today`;
//! a workspace body's proof is O(its reads) point reads of the stored answer, once a check,
//! and only for a body a call steps.
//!
//! THE JOIN (checker key to the body's `BodyCheck` name, its entry's text digest and its file,
//! a [`WorkspaceBody`]) is the answer's own: the engine's `BodyCheck` task files the checker key
//! of every fn its check covered (covered.rs `each_covered`; engine_body_facts.rs), and
//! [`workspace_of`] reads them from the units down ([`Answer::body`]'s `covers`). No second join here (rule 30). A fn a macro wrote inside a body is keyed by the
//! covering body's check; one a macro wrote at module level by the check of the entry its call
//! stands at (`workspace_of`'s doc). A constructor is keyed by its ADT's head task
//! (`Signature`), whose record files the same facts after its runs (`workspace_of`'s doc).
//!
//! EACH UNIT'S ROWS (D191, test.md "The order as built"): a body in a file several units hold
//! (a library and its test build; a lib and a bin that both `mod` one file) is checked by every
//! holder, the first in rank under the plain name (the base row), each other under the name at
//! its view (blaze-drive view.rs `at_view`). The walk asks, for each body part of a file a
//! unit's module tree reaches, the row at that unit's view ([`UnitRoot`]). A row is taken under
//! the unit it was CHECKED under, which its covered keys state (their first 8 bytes are that
//! unit's crate prefix): a base row of another unit lands under that unit's prefix and no call
//! through this unit's keys finds it, so it is never used for this unit ([`NotRunnable::NoRow`]
//! says "no row at this unit's view"); a view row whose keys name another unit is refused
//! ([`RunIdsError::ViewOfAnotherUnit`]). No read-through to the base while keys differ.
//!
//! OPEN, said rather than guessed: (b)
//! A workspace crate's fact is its `CrateScope` out, which covers its exports and not its
//! dependencies' (body_ids.rs OPEN (a)). (c) A cell a shape reads that the request did not
//! look up this run is [`Missing::Cells`] (`Today::cell`). (d) A body under a cfg condition
//! (drive `at_condition`): the walk asks the base `BodyCheck` name a `FileItems` record lists;
//! a check the request settled only under a fork's name is not known by it, so it is carried
//! from the last table or refused [`RunIdsError::NotWalked`], never guessed.

#![allow(dead_code, reason = "lane P wires RunIds into the parent's loop (run.md D124, piece P)")]

use core::fmt;
use core::num::NonZeroU32;

use blaze_drive::key::ABSENT;
use blaze_drive::record::{Stored, open};
use blaze_drive::view::{UnitView, ViewKey, at_view};
use blaze_drive::{Cell, Key, Kind, Name, Tool, WireError};
use blaze_id::CrateItemKey;
use blaze_ids::{ContentId, Hasher};
use blaze_ladder::body_ids::{BodyId, BodyIds, Read};
use blaze_ladder::code_records::CodeClass;
use blaze_own::{AVec, Alloc, Bump, Vec};
use blaze_toolchain::ToolchainId;
use blaze_typeck::ItemKey;
use blaze_world::PackOrigin;
use blaze_world::crate_list::{CrateGraph, CrateList};
use bstd::cx::Cx;

use crate::CliError;
use crate::store::Store;

#[cfg(test)]
#[path = "run_ids_tests.rs"]
mod tests;

const TOOLCHAIN_FACT_DOMAIN: &str = "blaze-cli run ids toolchain fact 1";
const TOOLCHAIN_BODY_DOMAIN: &str = "blaze-cli run ids toolchain body 1";
const LOCKED_FACT_DOMAIN: &str = "blaze-cli run ids locked crate fact 1";
const LOCKED_DIGEST_DOMAIN: &str = "blaze-cli run ids locked crate digest 1";
const LOCKED_BODY_DOMAIN: &str = "blaze-cli run ids locked body 1";
const DRIVE_FACT_DOMAIN: &str = "blaze-cli run ids drive read 1";
const DRIVE_CELL_FACT_DOMAIN: &str = "blaze-cli run ids drive cell read 1";
const PRODUCERS_DOMAIN: &str = "blaze-cli run ids check producers 1";
const BODY_PROOF_FACT_DOMAIN: &str = "blaze-cli run ids body proof fact 1";
const BODY_PROOF_DIGEST_DOMAIN: &str = "blaze-cli run ids body proof digest 1";

/// The kinds whose producers a body's check runs: the checker, the heads it reads, the names it
/// resolves through, the macros it expands and the proc-macro modules it calls. A toolchain or
/// locked body's key holds each one's tool, so a changed checker is another key (rule 21), as
/// it is for a workspace body through its `BodyCheck` task key.
const CHECK_KINDS: [Kind; 5] = [Kind::BodyCheck, Kind::Signature, Kind::CrateScope, Kind::Expansion, Kind::MacroModule];

/// The one fact a toolchain body reads: the toolchain. Its digest today is the toolchain's id.
pub(crate) fn toolchain_fact() -> [u8; 32] {
    ContentId::of(TOOLCHAIN_FACT_DOMAIN, b"the pinned toolchain").0
}

/// The fact of the locked crate `key`: its name; its digest is the closure digest.
pub(crate) fn locked_fact(key: CrateItemKey) -> [u8; 32] {
    ContentId::of(LOCKED_FACT_DOMAIN, &key.0.0).0
}

/// A drive read's fact: its name (kind and subject) as 32 bytes.
pub(crate) fn fact_of(name: Name) -> [u8; 32] {
    let mut h = Hasher::new(DRIVE_FACT_DOMAIN);
    h.update(&[name.kind as u8]).id(&name.subject);
    h.finish().0
}

/// The checker key's crate prefix: its first 8 bytes as front2 keys.rs `checker_of` made it.
fn prefix_of(item: ItemKey) -> [u8; 8] {
    let b = item.0.to_be_bytes();
    [b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]
}

/// Where a crate's text is, as the crate list places it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Place {
    /// A toolchain crate, its root in a pack on the toolchain's shelf.
    Toolchain,
    /// Its root is in the checkout (tree 0).
    Checkout,
    /// Its root is in a fetched package's pack: the sources store's key and its files' BLAKE3.
    Fetched { source: ContentId, files: [u8; 32] },
    /// None of those: `why`.
    Unplaced { why: &'static str },
}

/// What [`RunIds::new`] reads of the crates: the crate list, or a test's table.
pub(crate) trait CrateTable<'n> {
    fn len(&self) -> usize;
    fn key(&self, at: usize) -> CrateItemKey;
    fn name(&self, at: usize) -> &'n str;
    fn place(&self, at: usize) -> Place;
    /// Each dependency's row, in the crate's own order.
    fn each_dep(&self, at: usize, each: &mut dyn FnMut(usize));
}

/// The tree of a world path `/<k>/...`.
fn tree_of(root: &str) -> Option<usize> {
    root.strip_prefix('/')?.split('/').next()?.parse().ok()
}

impl<'n> CrateTable<'n> for CrateList<'n> {
    fn len(&self) -> usize {
        CrateGraph::len(self)
    }
    fn key(&self, at: usize) -> CrateItemKey {
        CrateList::key(self, at)
    }
    fn name(&self, at: usize) -> &'n str {
        CrateGraph::name(self, at)
    }
    fn place(&self, at: usize) -> Place {
        if self.member(at) || self.root(at).starts_with("/0/") {
            return Place::Checkout;
        }
        let Some(k) = tree_of(self.root(at)) else { return Place::Unplaced { why: "its root names no tree (`/<k>/...`)" } };
        let Some(tree) = self.tree(k) else { return Place::Unplaced { why: "its root's tree is not in the crate list's tree table" } };
        match (CrateGraph::toolchain(self, at), tree.origin) {
            (true, PackOrigin::Shelf { .. }) => Place::Toolchain,
            (false, PackOrigin::Source { key }) => Place::Fetched { source: key, files: tree.files },
            (true, PackOrigin::Source { .. }) => Place::Unplaced { why: "a toolchain crate whose pack is a fetched record, not the shelf's" },
            (false, PackOrigin::Shelf { .. }) => Place::Unplaced { why: "a crate not of the toolchain whose pack is on the toolchain's shelf" },
        }
    }
    fn each_dep(&self, at: usize, each: &mut dyn FnMut(usize)) {
        for dep in CrateGraph::deps(self, at) {
            each(dep);
        }
    }
}

/// One workspace body the walk may ask for, as the command names it: the checker key, its
/// `BodyCheck` task's name (work.rs `body_name`), its outline entry's text digest (`Item::hash`,
/// what body_types.rs `body_source` is made of) and its file's `FileItems` name (whose `Owners`
/// the body's key folds in: body_types.rs `body_check_key`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct WorkspaceBody {
    pub item: ItemKey,
    pub check: Name,
    pub text: ContentId,
    pub file: Name,
}

/// What a drive name's record is TODAY, as a reader of it sees it (its out): the one question
/// [`RunIds::new`] asks of the command's standing answer. `None`: the command does not know.
pub(crate) trait Today {
    fn out(&self, name: Name) -> Option<ContentId>;
    /// What cell `cell` of `name`'s record holds today ([`blaze_drive::Seen::digest`]), when the
    /// command looked it up in the record `name` stands at now. `None`: not known.
    fn cell(&self, name: Name, cell: Cell) -> Option<ContentId>;
}

/// Today's outs as a list sorted by name, one each, checked at [`SortedToday::new`]: a test's.
#[derive(Clone, Copy, Debug)]
pub(crate) struct SortedToday<'t>(&'t [(Name, ContentId)]);

impl<'t> SortedToday<'t> {
    pub(crate) const EMPTY: SortedToday<'static> = SortedToday(&[]);

    pub(crate) fn new<'n>(outs: &'t [(Name, ContentId)]) -> Result<SortedToday<'t>, RunIdsError<'n>> {
        match outs.windows(2).position(|pair| pair[0].0 >= pair[1].0) {
            Some(at) => Err(RunIdsError::TodayUnsorted { at: at + 1 }),
            None => Ok(SortedToday(outs)),
        }
    }
}

impl Today for SortedToday<'_> {
    fn out(&self, name: Name) -> Option<ContentId> {
        self.0.binary_search_by_key(&name, |(n, _)| *n).ok().map(|at| self.0[at].1)
    }
    /// A test's list holds no cell.
    fn cell(&self, _name: Name, _cell: Cell) -> Option<ContentId> {
        None
    }
}

/// What [`workspace_of`] is handed of one record of the answer.
pub(crate) enum Seen<'b> {
    /// One part the record lists.
    Part(blaze_drive::Part),
    /// Its payload (handed once, after its parts).
    Payload(&'b [u8]),
}

/// What [`Answer::body`] knows of one `BodyCheck` task, besides the list in [`BodyScratch`]. Its
/// reads are not here: they are read by need ([`BodyReads`]), never copied per body.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct BodyFacts {
    /// What [`BodyId::key`] is made from (the engine: the task's row key).
    pub key: ContentId,
    /// The check's verdict today ([`Checked`]).
    pub checked: Checked,
    /// The body's entry text digest (what `WorkspaceBody::text` holds).
    pub text: ContentId,
    /// The body's file's `FileItems` name (what `WorkspaceBody::file` holds).
    pub file: Name,
}

/// The list [`Answer::body`] fills, owned by the caller so one serves every body (rule 11).
pub(crate) struct BodyScratch<'a> {
    /// The checker keys the check covered (the join, [`workspace_of`]); empty when the answer was
    /// asked with the body's text and file already known, or when the check filed a failure.
    pub(crate) covers: AVec<'a, ItemKey>,
}

impl<'a> BodyScratch<'a> {
    pub(crate) fn new(alloc: Alloc<'a>) -> BodyScratch<'a> {
        BodyScratch { covers: Vec::new_in(alloc) }
    }

    pub(crate) fn clear(&mut self) {
        self.covers.clear();
    }
}

// --- A workspace body's reads, BY NEED (the module doc, case 3) ---

/// The one fact a workspace body's code key reads: its check's coarse proof, named by the
/// check's `BodyCheck` name (a base row's or a unit's view's, D191). Its digest is [`Proof`]'s.
pub(crate) fn body_fact(check: Name) -> [u8; 32] {
    let mut h = Hasher::new(BODY_PROOF_FACT_DOMAIN);
    h.update(&[check.kind as u8]).id(&check.subject);
    h.finish().0
}

/// A workspace body's coarse proof: the ONE read its code key holds (`read`: [`body_fact`] of
/// its check, with the digest folded over what the check saw) and that fact's digest today
/// (`today`: the same fold over today's digests). Equal exactly when every read of the check
/// stands today (up to a hash collision): the engine's standing decision, as one comparison.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Proof {
    pub(crate) read: Read,
    pub(crate) today: [u8; 32],
    /// How many reads the row lists (folded into both digests; the profile's count).
    pub(crate) reads: u32,
}

/// The fold that makes a [`Proof`] from a row's reads, one at a time in the row's order (the
/// engine's stored order, deterministic for a row: rule 25), with nothing kept but two running
/// hashes: O(1) memory whatever the row's length.
pub(crate) struct ProofFold {
    check: Name,
    saw: Hasher,
    now: Hasher,
    reads: u32,
}

impl ProofFold {
    pub(crate) fn new(check: Name) -> ProofFold {
        let start = || {
            let mut h = Hasher::new(BODY_PROOF_DIGEST_DOMAIN);
            h.update(&[check.kind as u8]).id(&check.subject);
            h
        };
        ProofFold { check, saw: start(), now: start(), reads: 0 }
    }

    /// One read: its fact, the digest the check saw and the digest the fact has today.
    pub(crate) fn read(&mut self, fact: &[u8; 32], saw: &ContentId, now: &ContentId) {
        self.saw.update(fact).id(saw);
        self.now.update(fact).id(now);
        self.reads = self.reads.saturating_add(1);
    }

    /// The proof, the count folded last (a list and its prefix are two digests).
    pub(crate) fn finish(mut self) -> Proof {
        let count = u64::from(self.reads).to_le_bytes();
        self.saw.update(&count);
        self.now.update(&count);
        Proof { read: Read { fact: body_fact(self.check), digest: self.saw.finish().0 }, today: self.now.finish().0, reads: self.reads }
    }
}

/// Why a workspace body's proof could not be read (rule 19: each names the check): the call
/// that needed it is refused by name ([`NotRunnable::Unread`]); the other calls run.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Unread {
    /// The stored answer has no row of `check` today (dropped, or never planned).
    NoRow { check: Name },
    /// The stored answer refused the page holding `check`'s row or one of its reads.
    Refused { check: Name },
    /// `check`'s row lists `count` reads, more than the `most` a proof folds ([`Most::reads`]).
    TooManyReads { check: Name, count: usize, most: u32 },
    /// The table was handed no source of reads ([`RunIds::reading`] not called): never a guess.
    NoSource { check: Name },
}

impl fmt::Display for Unread {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let rule = "(blaze-cli run_ids.rs `BodyReads`, read by need at the call)";
        match self {
            Unread::NoRow { check } => write!(f, "the stored answer holds no row of {check} today, so its reads cannot be checked {rule}"),
            Unread::Refused { check } => write!(f, "the stored answer refused a page holding {check}'s row or reads {rule}"),
            Unread::TooManyReads { check, count, most } => write!(f, "{check}'s row lists {count} reads, more than the {most} a proof folds {rule}"),
            Unread::NoSource { check } => write!(f, "the body id table was handed no stored answer to read {check}'s reads from {rule}"),
        }
    }
}

impl core::error::Error for Unread {}

/// Where a workspace body's proof is read from, by its check's name (sans-IO, rule 14): the
/// engine's stored answer in place (engine_run_answer.rs, `blaze_engine::Answer`), or a test's.
/// `most`: the most reads one proof folds ([`Unread::TooManyReads`] past it).
pub(crate) trait BodyReads {
    fn proof(&self, check: Name, most: u32) -> Result<Proof, Unread>;
}

/// The caps of the body id table (rule 18), each a typed error naming what passed it: never an
/// allocator's panic.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Most {
    /// Workspace body rows one table holds ([`RunIdsError::TooManyBodies`]).
    pub(crate) bodies: u32,
    /// Reads one body's proof folds ([`Unread::TooManyReads`]).
    pub(crate) reads: u32,
}

impl Most {
    /// Far above any checkout (tempo: tens of thousands of bodies; a body reads hundreds to
    /// thousands of cells), at or below the FORM's own row cap.
    pub(crate) const USUAL: Most = Most { bodies: 1 << 22, reads: 1 << 20 };
}

/// What the table read by need, counted where it is read: kept by the command beside the table
/// (the table may be handed away to the parent), for the profile's line ([`IdsLine`]). The
/// command thread's alone (test_drive.rs: "The command thread owns the front end").
#[derive(Default, Debug)]
pub(crate) struct Asked {
    bodies: core::cell::Cell<u32>,
    reads: core::cell::Cell<u64>,
}

impl Asked {
    /// The checks whose proof was read (each once).
    pub(crate) fn bodies(&self) -> u32 {
        self.bodies.get()
    }

    /// The reads those proofs folded.
    pub(crate) fn reads(&self) -> u64 {
        self.reads.get()
    }
}

/// The source a table reads its workspace bodies' proofs from, by need ([`RunIds::reading`]).
#[derive(Clone, Copy)]
pub(crate) struct ReadsFrom<'a> {
    source: &'a dyn BodyReads,
    asked: &'a Asked,
    most: Most,
}

impl<'a> ReadsFrom<'a> {
    pub(crate) fn new(source: &'a dyn BodyReads, asked: &'a Asked, most: Most) -> ReadsFrom<'a> {
        ReadsFrom { source, asked, most }
    }
}

/// When the table reads its workspace bodies' proofs (rule 78: the strategy is a type at one
/// seam, [`RunIds::reading`]). [`Production`] is [`ByNeed`]; [`Eager`] (tests only) reads every
/// row's at once, the control the by-need tests are compared against (equal keys and reads).
pub(crate) trait Reading {
    /// What is read when the source is handed over.
    fn settle(ids: &RunIds<'_>) -> Result<(), Unread>;
}

/// Nothing up front: a body's proof is read at the first call that steps it.
pub(crate) struct ByNeed;

impl Reading for ByNeed {
    fn settle(_ids: &RunIds<'_>) -> Result<(), Unread> {
        Ok(())
    }
}

/// Every row's proof at once (each check once): the old eager table's reads, as a control.
#[cfg(test)]
pub(crate) struct Eager;

#[cfg(test)]
impl Reading for Eager {
    fn settle(ids: &RunIds<'_>) -> Result<(), Unread> {
        ids.settle_every()
    }
}

/// The strategy the command runs.
pub(crate) type Production = ByNeed;

/// The profile's line of the body ids (rule 16): `run ids: N workspace rows known, M body rows
/// read by need, 0 read rows held (0 by design), table B bytes; S ms`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct IdsLine {
    /// The table's workspace body rows.
    pub(crate) rows: u32,
    /// The checks whose proof a call read ([`Asked::bodies`]).
    pub(crate) read: u32,
    /// The reads those proofs folded ([`Asked::reads`]).
    pub(crate) folded: u64,
    /// The table record's bytes ([`RunIds::table_bytes`]).
    pub(crate) bytes: u64,
    /// The time the table took to read or build, in ns.
    pub(crate) ns: u64,
}

impl fmt::Display for IdsLine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "run ids: {} workspace rows known, {} body rows read by need ({} reads folded), 0 read rows held (0 by design), table {} bytes; {} ms",
            self.rows,
            self.read,
            self.folded,
            self.bytes,
            self.ns / 1_000_000
        )
    }
}

/// One check's proof, read at most once (a memo of the command's one thread; an error is kept
/// too: a refused read is said again, never retried, rule 3). Sorted by `fact`, one each.
pub(crate) struct Slot {
    fact: [u8; 32],
    check: Name,
    proof: core::cell::OnceCell<Result<Proof, Unread>>,
}

/// The slots of BODY_ROW-wide `bodies`: one a distinct check, sorted by its fact, in `bump`.
/// O(bodies log bodies) hashes and compares; nothing of a body is read.
fn slots_of<'a>(bodies: &[u8], bump: Bump<'a>) -> &'a [Slot] {
    let mut slots: Vec<Slot, Bump<'a>> = Vec::with_capacity_in(bodies.len() / BODY_ROW, bump);
    for b in bodies.chunks_exact(BODY_ROW) {
        let check = row_check(b);
        slots.push(Slot { fact: body_fact(check), check, proof: core::cell::OnceCell::new() });
    }
    slots.sort_unstable_by(|a, b| a.fact.cmp(&b.fact));
    slots.dedup_by_key(|slot| slot.fact);
    slots.into_bump_slice()
}

/// The answer's records as the walk reads them, sans-IO (rule 14): the engine's stored answer
/// (engine_run_answer.rs `EngineAnswer`), or a test's. It is also [`Today`]: the outs and cells
/// the keys are checked against are the answer's own.
pub(crate) trait Answer: Today {
    /// Hands `seen` each part, then the payload, of the record `name` stands at today;
    /// `Ok(false)`: the command does not know `name`'s record today (a forward walk did not
    /// reach it).
    fn open(&mut self, cx: &Cx, name: Name, seen: &mut dyn FnMut(Seen<'_>) -> Result<(), RunIdsError<'static>>) -> Result<bool, RunIdsError<'static>>;

    /// Whether the check planned NO body task of the file `file` (a `FileItems` name), by the
    /// answer's own word: the file is reached only from a target its features skip (a bin whose
    /// `required-features` are off: engine_check.rs `PLACE_SKIPPED`). Its outline lists body
    /// parts, and no row of the answer is any of them, so [`workspace_of`] does not walk it: no
    /// unit of the crate list holds its bodies, and no program calls them. `false` when the
    /// answer does not say so: the walk then opens the file, and a body part of it with no row
    /// is refused by name ([`RunIdsError::NotWalked`]), never skipped (rule 3).
    fn skipped(&self, file: Name) -> bool;

    /// The facts of the workspace body whose `BodyCheck` task is `check`, hiding how its record
    /// is found: its key, its verdict and the checker keys it covers (into `out.covers`); its
    /// reads are not asked here ([`BodyReads`], by need at the call). `tool` is the `BodyCheck` kind's tool. `known` is the
    /// body's text digest and file when the caller already has them (a row of the walk or of the
    /// last table); with `None` the answer finds them itself. `Ok(None)`: the command does not
    /// know `check` today. `out` is cleared first.
    fn body(
        &mut self,
        cx: &Cx,
        check: Name,
        tool: Tool,
        known: Option<(ContentId, Name)>,
        out: &mut BodyScratch<'_>,
    ) -> Result<Option<BodyFacts>, RunIdsError<'static>>;
}

/// The record of `name` at store key `at`, opened whole and handed to `seen` part by part, then
/// its payload ([`Answer::open`] of every answer whose rows name a record key). A key the store
/// does not hold: never a guess ([`RunIdsError::AnswerGone`]).
pub(crate) fn open_record(
    cx: &Cx,
    store: &mut dyn Store<'_>,
    name: Name,
    at: Key,
    seen: &mut dyn FnMut(Seen<'_>) -> Result<(), RunIdsError<'static>>,
) -> Result<bool, RunIdsError<'static>> {
    let held = store.get(cx, at).map_err(|why| RunIdsError::AnswerStore { name, why })?;
    // A row the answer stands at whose record the store does not hold: never a guess.
    let Some(held) = held else { return Err(RunIdsError::AnswerGone { name }) };
    match open(held.bytes).map_err(|why| RunIdsError::AnswerRecord { name, why })? {
        Stored::Whole(whole) => {
            for part in whole.parts() {
                seen(Seen::Part(part))?;
            }
            seen(Seen::Payload(whole.payload))?;
            Ok(true)
        }
        Stored::Shape(_) => Err(RunIdsError::AnswerShape { name }),
    }
}

/// The workspace bodies of the answer, each a [`WorkspaceBody`], and the tree walked to find
/// them (the edges [`RunIds`] keeps, so the next forward walk crosses what it did not reach).
pub(crate) struct Walked<'a> {
    pub(crate) bodies: AVec<'a, WorkspaceBody>,
    /// Sorted, one each ([`Edge`]'s order is the FORM's row order).
    pub(crate) edges: AVec<'a, Edge>,
}

/// One edge of the walked tree (finding 12): a record the walk crossed, or a part its record
/// lists. The FORM writes [`Edge::Walked`] as the row `(name, name)` and a part as `(parent,
/// child)`; read decodes each row into one of these once ([`RunIds::read`]), so no reader tells
/// the mark from a part by comparing names again.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Edge {
    /// `name`'s record was walked: its parts are the [`Edge::Part`] rows under it.
    Walked(Name),
    /// `parent`'s record lists `child` as a part (`parent != child`: [`Edge::of_pair`]).
    Part { parent: Name, child: Name },
}

impl Edge {
    /// The edge as the FORM's row pair: `(name, name)` for the mark.
    pub(crate) fn pair(self) -> (Name, Name) {
        match self {
            Edge::Walked(name) => (name, name),
            Edge::Part { parent, child } => (parent, child),
        }
    }

    /// The edge a row pair is: equal names are the mark.
    fn of_pair((parent, child): (Name, Name)) -> Edge {
        match parent == child {
            true => Edge::Walked(parent),
            false => Edge::Part { parent, child },
        }
    }

    /// The record the row is under: the walked name, or the part's parent.
    fn parent(self) -> Name {
        self.pair().0
    }
}

/// By the row pair: the order of the FORM's edge rows, so a sorted `[Edge]` lays sorted bytes.
impl Ord for Edge {
    fn cmp(&self, other: &Edge) -> core::cmp::Ordering {
        self.pair().cmp(&other.pair())
    }
}

impl PartialOrd for Edge {
    fn partial_cmp(&self, other: &Edge) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// The most records the walk crosses (rule 18): far above any checkout's units and files.
const MOST_WALKED: usize = 1 << 22;

/// One unit of the checkout as the walk asks its rows (D191): its crate root's `FileItems`
/// name, its view (blaze-drive view.rs `ViewKey::of_unit` of its crate key: the one spelling)
/// and its crate key's prefix, which every key its checks cover begins with. Ordered by root,
/// so the units of one root (a library and its test build) are one run.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) struct UnitRoot {
    pub(crate) root: Name,
    pub(crate) view: ViewKey,
    pub(crate) prefix: [u8; 8],
}

/// Every member unit of `list` (its root in the checkout, tree 0) as a [`UnitRoot`], in the
/// list's order, in `alloc`. A member whose root is not a checkout path (`/0/...`) is refused by
/// name ([`RunIdsError::UnitRootOutside`]), never given another file.
pub(crate) fn unit_roots<'n, 'a>(list: &CrateList<'n>, alloc: Alloc<'a>) -> Result<AVec<'a, UnitRoot>, RunIdsError<'n>> {
    let mut units: AVec<'a, UnitRoot> = Vec::new_in(alloc);
    for at in 0..CrateGraph::len(list) {
        if !list.member(at) {
            continue;
        }
        let root = list.root(at);
        let Some(path) = root.strip_prefix("/0/") else { return Err(RunIdsError::UnitRootOutside { krate: CrateGraph::name(list, at), root }) };
        let key = list.key(at);
        units.push(UnitRoot { root: Name::file(path.as_bytes()), view: crate::engine_body_plan::unit_view(list, at), prefix: key.prefix() });
    }
    Ok(units)
}

/// The parts `edges` (sorted) list under the walked record `parent`.
fn parts_of(edges: &[Edge], parent: Name) -> impl Iterator<Item = Name> + '_ {
    let from = edges.partition_point(|edge| edge.parent() < parent);
    let to = edges.partition_point(|edge| edge.parent() <= parent);
    edges[from..to].iter().filter_map(|edge| match *edge {
        Edge::Part { child, .. } => Some(child),
        Edge::Walked(_) => None,
    })
}

/// Each name a unit's row of a body is at, with that unit's prefix (D191): for every unit, every
/// `BodyCheck` part of every file its module tree reaches in the walked tree `edges` (sorted),
/// cfg-blind (a unit whose cfg cuts a `mod` reaches its file and holds none of its bodies: its
/// asks find no row), the name at the unit's view. The base holder's row is the plain name, which
/// the walk reads already; its ask at its view finds nothing. Sorted, one each, in `alloc`.
///
/// COST: per distinct root, one pass over the files its tree reaches (each a binary search of
/// the edges) and one name per (body, unit of that root): O(units x bodies of their files).
fn views_of<'a>(edges: &[Edge], units: &[UnitRoot], alloc: Alloc<'a>) -> AVec<'a, (Name, [u8; 8])> {
    let mut asks: AVec<'a, (Name, [u8; 8])> = Vec::new_in(alloc);
    let mut by_root: AVec<'a, UnitRoot> = Vec::with_capacity_in(units.len(), alloc);
    by_root.extend_from_slice(units);
    by_root.sort_unstable();
    let mut reached: AVec<'a, Name> = Vec::new_in(alloc);
    let mut stack: AVec<'a, Name> = Vec::new_in(alloc);
    let mut at = 0;
    while at < by_root.len() {
        let root = by_root[at].root;
        let end = at + by_root[at..].partition_point(|unit| unit.root == root);
        let holders = &by_root[at..end];
        reached.clear();
        stack.clear();
        stack.push(root);
        while let Some(file) = stack.pop() {
            let Err(slot) = reached.binary_search(&file) else { continue };
            reached.insert_shift(slot, file);
            for child in parts_of(edges, file) {
                match child.kind {
                    Kind::FileItems => stack.push(child),
                    Kind::BodyCheck => asks.extend(holders.iter().map(|unit| (at_view(child, UnitView::At(unit.view)), unit.prefix))),
                    _ => {}
                }
            }
        }
        at = end;
    }
    asks.sort_unstable();
    asks.dedup();
    asks
}

/// THE JOIN (the coordinator's decision, 2026-11-17: "the BodyCheck record lists the checker
/// keys it covered"): each workspace body from the answer alone. From the root `Units` down:
/// each member's `Units`, its crate root's `FileItems`, each `mod x;`'s `FileItems` (a data
/// file's part is not followed), and each `FileItems` record's `BodyCheck` parts. A `BodyCheck`
/// task names its file, its entry's text and every checker key its check covered
/// ([`Answer::body`]: engine_body_facts.rs): one row per key, all under that one check's name.
///
/// So a fn a macro wrote INSIDE a body (the launcher's `main` and the written `main` in it, a fn
/// a macro call in a body declares) is keyed by the covering body's `BodyCheck`. A fn a macro
/// wrote at MODULE level (tempo-hardfork's tests, which `tempo_hardfork!` writes in a
/// `#[cfg(test)] mod tests`) is in no outline entry of its own: the task of the entry its call
/// stands at checks it (engine_bodies.rs, from body_types/covered.rs `written_by`, D122), so it
/// is in that task's covers and keyed by that `BodyCheck`. Only the unit whose cfg writes it
/// has it: a test fn such an expansion writes is covered by the test build's row of the call's
/// entry, at its view (D191). One whose call's walk ends at no file entry is checked by no task
/// (engine_bodies.rs `uncovered`, in the profile): it gets no row, [`RunIds::checked`] and
/// `body_check` say `None` for it, and the parent refuses it by name (never keyed by a guess).
///
/// A CONSTRUCTOR (a tuple struct's or a tuple variant's, called or passed as a fn: `Wrap(1)`,
/// `.map(E::V)`) has no body and no `BodyCheck`: it runs under its ADT's HEAD TASK, the
/// `FileItems` record's `Signature` part of the ADT's entry, whose record ends in run facts of
/// the body form (engine_heads.rs `ctor_facts`: covers are each holding unit's constructor
/// keys, the class `Untyped` when an ADT's head did not freeze), read by [`Answer::body`] as a
/// body's are. Its row is keyed and proved as a body's: the head row's key, its proof the head
/// row's reads folded by need ([`BodyReads`]). A head task refused whole keeps no record, so its
/// constructors get no row and a call is `NoRow` by name; a body-local ADT has no head task and
/// its constructor no row either (OPEN). A head task of a fn or a trait covers nothing. Views
/// are not asked for head tasks: one head row holds every unit's cells and constructors, each
/// key under its own unit's prefix.
///
/// A record the command does not know today (a forward walk did not reach it): a `Units` or
/// `FileItems` record's parts are `previous`'s edges (it stands as the last answer has it,
/// `RunIds::new`'s INVARIANT); a `BodyCheck`'s rows are `previous`'s rows of that check. Not in
/// `previous` either: [`RunIdsError::NotWalked`] names it. A check that filed a failure covers
/// nothing. A file the answer says the check planned no body task of ([`Answer::skipped`]: a
/// features-skipped target's, tempo's `localnet` bin, seen 2026-11-21 as `NotWalked` of one of
/// its `BodyCheck` parts) is not walked.
///
/// THEN EACH UNIT'S VIEW (D191): for each of `units` (the checkout's member units, by root:
/// [`unit_roots`]) and each body part of a file its tree reaches, the row at its view
/// ([`views_of`]). Found: its covers are rows under that view's name, each key checked to be of
/// that unit (else [`RunIdsError::ViewOfAnotherUnit`]). Not known today: `previous`'s rows of
/// that name, if it walked one. Neither: the unit has no row of its own there (it is the base
/// holder, whose plain row is read above, or it does not hold the body, or its row was not
/// made): nothing is taken, and a call through its keys is refused by name at the call
/// ([`NotRunnable::NoRow`]). Never the base row in its place: that row's keys are another
/// unit's. With `units` empty only plain rows are read (the rule before D191: the planted
/// control of the tests).
///
/// COST: one store get per record the walk knows today, breadth first, each level sorted once
/// (O(names log names)); a record it does not know is a binary search of `previous`'s edges.
/// The views: one point read of the answer per (body, unit reaching its file) ([`views_of`]),
/// a store get for each that has a row.
pub(crate) fn workspace_of<'a>(cx: &Cx, answer: &mut dyn Answer, tool: Tool, units: &[UnitRoot], previous: Option<&RunIds<'_>>, alloc: Alloc<'a>) -> Result<Walked<'a>, RunIdsError<'static>> {
    let _cx = cx.span("run ids: the workspace bodies of the answer");
    let mut edges: AVec<'a, Edge> = Vec::new_in(alloc);
    let mut visited: AVec<'a, Name> = Vec::new_in(alloc);
    let mut frontier: AVec<'a, Name> = Vec::new_in(alloc);
    let mut next: AVec<'a, Name> = Vec::new_in(alloc);
    let mut checks: AVec<'a, Name> = Vec::new_in(alloc);
    frontier.push(Name::units());
    while !frontier.is_empty() {
        frontier.sort_unstable();
        frontier.dedup();
        frontier.retain(|name| visited.binary_search(name).is_err());
        // A file the check planned no body task of (a features-skipped target's, `Answer::skipped`)
        // stays a part of its parent (its edge is in) and is not walked: none of its body parts
        // has a row, and none is a body of the crate list's units.
        frontier.retain(|name| !(name.kind == Kind::FileItems && answer.skipped(*name)));
        if visited.len() + frontier.len() > MOST_WALKED {
            return Err(RunIdsError::TooManyWalked { most: MOST_WALKED });
        }
        for parent in frontier.iter().copied() {
            edges.push(Edge::Walked(parent));
            // A record that lists itself as a part is the walked mark again (one row), never a
            // part: a `Units` that named itself would loop, and `visited` already holds it.
            let mut child = |name: Name, edges: &mut AVec<'a, Edge>| match name.kind {
                Kind::Units | Kind::FileItems => {
                    edges.push(Edge::of_pair((parent, name)));
                    next.push(name);
                }
                // A head task is a check too: its covers are its ADTs' constructors (the doc).
                Kind::BodyCheck | Kind::Signature => {
                    edges.push(Edge::of_pair((parent, name)));
                    checks.push(name);
                }
                _ => {}
            };
            let known = answer.open(cx, parent, &mut |seen| {
                if let Seen::Part(part) = seen {
                    // A data file (`include_str!`) is bytes, not Rust: no body is in it.
                    if !matches!(part.locate, blaze_drive::Locate::File { edition: blaze_drive::record::DATA_FILE }) {
                        child(part.name, &mut edges);
                    }
                }
                Ok(())
            })?;
            if !known {
                let Some(children) = previous.and_then(|p| p.children(parent)) else { return Err(RunIdsError::NotWalked { name: parent }) };
                for name in children {
                    child(name, &mut edges);
                }
            }
        }
        visited.extend(frontier.iter().copied());
        visited.sort_unstable();
        core::mem::swap(&mut frontier, &mut next);
        next.clear();
    }
    edges.sort_unstable();
    edges.dedup();
    checks.sort_unstable();
    checks.dedup();

    // Each check's rows: its payload's keys, or `previous`'s rows of it.
    let mut bodies: AVec<'a, WorkspaceBody> = Vec::new_in(alloc);
    let mut carried: Option<AVec<'a, WorkspaceBody>> = None;
    let mut scratch = BodyScratch::new(alloc);
    for check in checks.iter().copied() {
        if let Some(held) = answer.body(cx, check, tool, None, &mut scratch)? {
            for item in scratch.covers.iter().copied() {
                bodies.push(WorkspaceBody { item, check, text: held.text, file: held.file });
            }
            continue;
        }
        let Some(previous) = previous else { return Err(RunIdsError::NotWalked { name: check }) };
        // `previous`'s rows by check, made once, on the first check this walk did not reach.
        let by_check = carried.get_or_insert_with(|| {
            let mut rows = previous.workspace_rows(alloc);
            rows.sort_unstable_by_key(|row| (row.check, row.item));
            rows
        });
        let from = by_check.partition_point(|row| row.check < check);
        let to = by_check.partition_point(|row| row.check <= check);
        bodies.extend_from_slice(&by_check[from..to]);
    }

    // Each unit's rows at its view (D191; the doc's second half).
    for (view, prefix) in views_of(&edges, units, alloc).iter().copied() {
        if let Some(held) = answer.body(cx, view, tool, None, &mut scratch)? {
            for item in scratch.covers.iter().copied() {
                let covered = prefix_of(item);
                if covered != prefix {
                    return Err(RunIdsError::ViewOfAnotherUnit { check: view, unit: prefix, covered });
                }
                bodies.push(WorkspaceBody { item, check: view, text: held.text, file: held.file });
            }
            continue;
        }
        // Not known today: the last table's rows under this view's name stand, if it has any.
        let Some(previous) = previous else { continue };
        let by_check = carried.get_or_insert_with(|| {
            let mut rows = previous.workspace_rows(alloc);
            rows.sort_unstable_by_key(|row| (row.check, row.item));
            rows
        });
        let from = by_check.partition_point(|row| row.check < view);
        let to = by_check.partition_point(|row| row.check <= view);
        bodies.extend_from_slice(&by_check[from..to]);
    }
    Ok(Walked { bodies, edges })
}

/// Why a workspace body has no record valid today (its `BodyId` has no reads).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Missing {
    /// The store holds nothing at its task key: never checked under this text and checker.
    NoRecord,
    /// Its shape reads `name`, which the command has no digest for today.
    NotToday { name: Name },
    /// Its shape's reads with today's digests name no record: checked, never against these facts.
    NoValue,
    /// Its shape reads a cell (cells.rs) the command did not look up today (`Today::cell`), or a
    /// record at its bare task key holds cell reads (no writer stores one so).
    Cells,
}

/// Which grain an item's body is keyed at (the module doc's three cases), for the parent's loop.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Grain {
    /// A crate of the pinned toolchain: keyed by the toolchain's id.
    Toolchain,
    /// A locked dependency (a fetched package): keyed by its crate's closure digest.
    Dependency,
    /// A crate of the checkout: keyed by its `BodyCheck` task and record.
    Workspace,
}

/// What the store holds of a workspace body's check, today (asked at the call). THE GATE'S
/// RULE (D151), its one home [`Checked::runs`]: a body runs when its types were checked whole
/// and it has no error (`Clean`, or `Typed`: its borrow check refused or not run, which never
/// changes what a body does and whose findings are notes until `BORROWS_ARE_ERRORS`). Anything
/// else is a typed stop of the parent naming the class, never a second check.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Checked {
    /// Its record valid today holds no finding that is an error (main.rs `every_rule`) and none
    /// saying the check did not finish.
    Clean,
    /// Its record valid today says its types were checked whole with no error, and its borrow
    /// check was refused or not run: `rule` is the note's (`body_types::BORROWS_NOT_CHECKED`).
    /// Run, as `check` answers "0 errors, incomplete" for it (D151).
    Typed { rule: u16 },
    /// No record valid today: [`Missing`] says which step found nothing.
    Absent(Missing),
    /// Its record valid today holds `errors` findings that are errors of the program; `first`
    /// is the first one's rule. At least one (finding 13): a count of none is no error verdict,
    /// and the table's read refuses that row ([`TableError::NoErrors`]).
    ///
    /// ```compile_fail
    /// // No `Errors` verdict without an error: the count is a `NonZeroU32`.
    /// let none = crate::run_ids::Checked::Errors { errors: 0, first: 0 };
    /// ```
    Errors { errors: NonZeroU32, first: u16 },
    /// Its record valid today says its types were not checked whole: `rule` is
    /// `body_types::TYPES_NOT_CHECKED` (or `BORROWS_NOT_CHECKED` once borrows are errors), or
    /// work.rs `NOT_RUN` (refused, panicked, or an input it reads could not be made). There is
    /// no typed body to lower.
    Incomplete { rule: u16 },
}

impl Checked {
    /// THE GATE (D151, the type's doc), for the workspace body `body` this verdict is of: a
    /// [`Runnable`] naming it when its types were checked whole and it has no error, else the
    /// verdict as [`NotRunnable::Checked`]. A `Typed` body's borrow findings are notes until
    /// `BORROWS_ARE_ERRORS`; from then its unread borrows are an error's place, and it is not
    /// run. The one maker of a workspace body's proof (states.md item 13).
    pub(crate) fn runs(self, body: ItemKey) -> Result<Runnable, NotRunnable> {
        let basis = match self {
            Checked::Clean => Basis::Clean,
            Checked::Typed { rule } if !crate::body_types::BORROWS_ARE_ERRORS => Basis::Typed { rule },
            Checked::Typed { .. } | Checked::Absent(_) | Checked::Errors { .. } | Checked::Incomplete { .. } => return Err(NotRunnable::Checked(self)),
        };
        Ok(Runnable { body: RunsFor::Fn(body), basis })
    }
}

/// THE PROOF THAT A BODY MAY RUN (states.md item 13, rules 74 and 76): what the parent's step
/// takes (run_parent.rs), so no body is lowered and placed without the gate having let it.
/// Made only here, by name, each maker saying why its grain may run:
/// - [`Checked::runs`]: a workspace body whose check today is `Clean` or `Typed` (D151);
/// - [`Runnable::toolchain`] and [`Runnable::locked`]: a body of the pinned toolchain or of a
///   locked dependency, whose coarse fact covers it ([`RunIds::runnable`]);
/// - [`Runnable::listed`]: a row a start list written under TODAY's certificate holds (the run
///   that wrote it gated it, under that same certificate).
///
/// It names its body; the step takes it paired with its own row by construction (run_parent.rs
/// `Cleared`), so a proof of one fn does not step another.
///
/// ```compile_fail
/// // Outside run_ids.rs no proof is made by hand: its fields are private.
/// let forged = crate::run_ids::Runnable { body: todo!(), basis: todo!() };
/// ```
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Runnable {
    #[cfg_attr(not(test), expect(dead_code, reason = "a proof is held, not read: its body says what it is of, in Debug and the tests' `covers`"))]
    body: RunsFor,
    #[expect(dead_code, reason = "a proof is held, not read: which maker made it, in Debug")]
    basis: Basis,
}

/// The make path's proof (blaze-ladder body_ids.rs `Grant`): the run's maker is
/// `Maker<'_, Runnable>` (run_parent.rs `RunMaker`), so no instance is made without one.
impl blaze_ladder::body_ids::Grant for Runnable {}

/// Which body a [`Runnable`] is of.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum RunsFor {
    /// A fn by its checker key (the gate's, asked at the call).
    Fn(ItemKey),
    /// A start list's row by its symbol (placed before any call, so by no key yet).
    Listed(blaze_launch::wire::run::Symbol),
}

/// Why a [`Runnable`] may run: which maker made it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Basis {
    Clean,
    Typed { rule: u16 },
    Toolchain,
    Locked,
    Listed,
}

impl Runnable {
    /// A body of the pinned toolchain's crates. It needs no check of its own: its key and its
    /// one read are the toolchain's id (the module doc, case 1), which names the cut the
    /// toolchain was built and checked at as a whole when it was made; no `BodyCheck` task is
    /// made for its bodies, and D151's verdict is a workspace check's, not a toolchain's.
    fn toolchain(body: ItemKey) -> Runnable {
        Runnable { body: RunsFor::Fn(body), basis: Basis::Toolchain }
    }

    /// A body of a locked dependency (a fetched package). It needs no check of its own: its key
    /// and its one read are its crate's closure digest (the module doc, case 2), over a locked
    /// package that does not change under the command and depends on no checkout crate (`new`
    /// refuses that, [`RunIdsError::LockedReadsWorkspace`]); no `BodyCheck` task is made for
    /// its bodies, and D151's verdict is a workspace check's.
    fn locked(body: ItemKey) -> Runnable {
        Runnable { body: RunsFor::Fn(body), basis: Basis::Locked }
    }

    /// A row of a start list under TODAY's certificate: a [`TodayGroup`] is made only by a list
    /// whose certificate is the command's own checked answer's, and the run that wrote the row
    /// answered it under that same certificate, so through its gate; every input of that
    /// verdict is under the certificate. The list's proof, taken as it is (rule 74). A list of
    /// another certificate gives no group, and its rows resolve like any first call.
    pub(crate) fn listed(group: &crate::start_list::TodayGroup<'_>) -> Runnable {
        Runnable { body: RunsFor::Listed(group.code().symbol), basis: Basis::Listed }
    }

    /// Whether this proof is of the fn `item` whose symbol is `symbol`: a gated proof by its
    /// key, a listed one by its row's symbol. A test's view: the step never compares, since its
    /// proof is made from its own row (run_parent.rs `Cleared`).
    #[cfg(test)]
    pub(crate) fn covers(&self, item: ItemKey, symbol: blaze_launch::wire::run::Symbol) -> bool {
        match self.body {
            RunsFor::Fn(body) => body == item,
            RunsFor::Listed(listed) => listed == symbol,
        }
    }
}

/// Why a body may not run ([`RunIds::runnable`]): each names what refused it (rule 19).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum NotRunnable {
    /// A workspace body whose check today is not one D151 runs: the verdict.
    Checked(Checked),
    /// A workspace fn the table has no body row for: what it holds beside it.
    NoRow(NoRow),
    /// `function`'s key is of no crate of the list: no grain says how its body is keyed.
    NoGrain { function: ItemKey },
    /// A workspace body whose check runs, but whose reads could not be read to key it today:
    /// why ([`Unread`], naming the check).
    Unread(Unread),
}

impl fmt::Display for NotRunnable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NotRunnable::Checked(Checked::Clean) => f.write_str("its check is clean"),
            NotRunnable::Checked(Checked::Typed { rule }) => write!(f, "its borrows were not checked (rule {rule}), and borrows are errors"),
            NotRunnable::Checked(Checked::Absent(missing)) => write!(f, "no check of it is valid today: {missing:?}"),
            NotRunnable::Checked(Checked::Errors { errors, first }) => write!(f, "its check found {errors} error(s), the first of rule {first}"),
            NotRunnable::Checked(Checked::Incomplete { rule }) => write!(f, "its types were not checked whole (rule {rule})"),
            NotRunnable::NoRow(held) => write!(f, "no body row of it: {held}"),
            NotRunnable::NoGrain { function } => write!(f, "the fn {:#034x} is of no crate of the program's list (blaze-cli run_ids.rs `RunIds::runnable`)", function.0),
            NotRunnable::Unread(why) => write!(f, "its check runs, and its reads could not be read: {why}"),
        }
    }
}

impl core::error::Error for NotRunnable {}

/// The grain of a crate (the module doc), with the fact its items' bodies read.
#[derive(Clone, Copy, Debug)]
enum CrateGrain {
    Toolchain,
    Locked(Read),
    /// Its `CrateScope` read today, when the command has one.
    Workspace(Option<Read>),
}

#[derive(Clone, Copy, Debug)]
struct CrateRow {
    prefix: [u8; 8],
    key: CrateItemKey,
    grain: CrateGrain,
}

#[derive(Clone, Copy, Debug)]
struct BodyRow {
    item: ItemKey,
    key: ContentId,
    checked: Checked,
    /// The [`WorkspaceBody`] it was made from: its check's name, text and file, so a later walk
    /// that does not reach the check names the body again from this row.
    check: Name,
    text: ContentId,
    file: Name,
}

/// The most workspace crate rows a [`NoRow`] names by prefix (the rest are counted).
pub(crate) const NO_ROW_CRATES: usize = 8;

/// What the table holds beside a workspace fn it has no body row for ([`RunIds::no_row`]): the
/// words of the parent's refusal (rule 19), so a missing row tells its causes apart. The called
/// crate's rows: if it has none, the program called a unit no check covered. If it has rows but
/// none for this item, the item is a body no check of that unit lists in its covers: NO ROW AT
/// THIS UNIT'S VIEW (D191: every unit that holds a body checks it under its own view, and a row
/// checked under another unit is never taken for this one, [`workspace_of`]).
///
/// NOT SAID: which body (path, file) the fn is, nor which unit it WAS checked under. An item's
/// key hashes its parent's whole key, crate prefix included (blaze-scope task/lower.rs
/// `key_of`), so one fn's key differs in every byte between two units, and naming "checked under
/// that unit, called under this one" needs the item's place (its file and chain), which the
/// table does not hold. D152 point 1 removes the gap (a package's test unit is its library under
/// one more fact, one key).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct NoRow {
    /// The called item's crate prefix.
    pub(crate) called: [u8; 8],
    /// The workspace crate rows' prefixes, the first [`NO_ROW_CRATES`] in prefix order.
    pub(crate) crates: [[u8; 8]; NO_ROW_CRATES],
    /// How many of `crates` are filled.
    pub(crate) named: u8,
    /// How many workspace crate rows the table holds.
    pub(crate) workspace: u32,
    /// Whether `called` is one of them.
    pub(crate) called_held: bool,
    /// How many body rows the table holds under `called`.
    pub(crate) under_called: u32,
}

impl fmt::Display for NoRow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let hex = |f: &mut fmt::Formatter<'_>, prefix: &[u8; 8]| prefix.iter().try_for_each(|byte| write!(f, "{byte:02x}"));
        write!(f, "the table holds {} workspace crate row(s) (", self.workspace)?;
        for (i, prefix) in self.crates[..usize::from(self.named)].iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            hex(f, prefix)?;
        }
        if self.workspace > u32::from(self.named) {
            write!(f, ", and {} more", self.workspace - u32::from(self.named))?;
        }
        f.write_str("); the called prefix ")?;
        hex(f, &self.called)?;
        match (self.called_held, self.under_called) {
            (false, _) => f.write_str(" is none of them")?,
            (true, 0) => f.write_str(" is one of them, with no body row: no check covered that unit (no row at this unit's view, D191)")?,
            (true, n) => write!(
                f,
                " is one of them, with {n} body row(s), none this fn's: no check of that unit lists it in its covers (no row at this unit's view, D191; a row another unit checked is never taken for it)"
            )?,
        }
        Ok(())
    }
}

/// Why [`RunIds::new`] stopped: each names its crate or its body (rule 19).
#[derive(Debug)]
pub(crate) enum RunIdsError<'n> {
    /// Two crates of the list begin with one prefix, so a checker key cannot say which (rule 21;
    /// blaze-items refuses it as `CrateKeyClash` when it keys items).
    PrefixClash { first: &'n str, second: &'n str, prefix: [u8; 8] },
    /// The list places crate `krate` nowhere this file knows.
    Unplaced { krate: &'n str, why: &'static str },
    /// The locked crate `krate` depends on the checkout's crate `dep`: its body's check reads
    /// a workspace crate, which no coarse fact covers.
    LockedReadsWorkspace { krate: &'n str, dep: &'n str },
    /// The locked crates' dependencies loop through `krate`.
    Cycle { krate: &'n str },
    /// A dependency names no row of the list.
    NoRow { krate: &'n str, dep: usize },
    /// The caller's workspace row `item` is of no crate of the list, or of a crate that is not
    /// the checkout's.
    NotWorkspace { item: ItemKey },
    /// Two workspace rows for one checker key.
    TwoRows { item: ItemKey },
    /// The caller's name of `item`'s check is neither a `BodyCheck` nor a head task's
    /// (`Signature`, a constructor's: [`workspace_of`]'s doc).
    NotABodyCheck { item: ItemKey, name: Name },
    /// The facts today are not sorted by name, one each.
    TodayUnsorted { at: usize },
    /// The engine's answer holds no row, or an unreadable one, for the `BodyCheck` `check`:
    /// `what` says which.
    EngineBody { check: Name, what: &'static str },
    /// The engine's stored answer could not be read for `name`: `what`.
    EngineAnswer { name: Name, what: &'static str },
    /// The table record's bytes are not a table of this form.
    Table(TableError),
    /// Two digests of one fact today: a row carried from the last answer's table read a fact
    /// this walk moved (`RunIds::new`'s INVARIANT broken).
    TwoDigests { fact: [u8; 32] },
    /// The store could not read or write the table record of certificate `cert`.
    TableStore { cert: Key, why: CliError },
    /// The store could not read the record the answer's row of `name` stands at.
    AnswerStore { name: Name, why: CliError },
    /// The answer has a row for `name` whose record the store does not hold.
    AnswerGone { name: Name },
    /// The record the answer's row of `name` stands at does not read as the drive's.
    AnswerRecord { name: Name, why: WireError },
    /// The answer's row of `name` stands at a shape, not a record.
    AnswerShape { name: Name },
    /// The walk ([`workspace_of`]) reached `name`, which the command does not know today and the
    /// last answer's table did not walk.
    NotWalked { name: Name },
    /// The walk crossed more than `most` records.
    TooManyWalked { most: usize },
    /// The row at the view `check` (a unit's name of a body, D191) covers a key of the crate
    /// whose prefix is `covered`, not of the unit `unit` whose view it is: its check ran under
    /// another unit, so its keys are not this unit's (the planner's fault, named; never taken).
    ViewOfAnotherUnit { check: Name, unit: [u8; 8], covered: [u8; 8] },
    /// The member unit `krate`'s root `root` is not a checkout path (`/0/...`): the walk cannot
    /// name the file its module tree starts at.
    UnitRootOutside { krate: &'n str, root: &'n str },
    /// The walk handed `count` workspace body rows, more than the `most` a table holds
    /// ([`Most::bodies`]); `krate` has the most of them (`rows`). Refused before any row is made.
    TooManyBodies { krate: &'n str, rows: usize, count: usize, most: u32 },
    /// A source of reads handed to the table refused while settling ([`RunIds::reading`]).
    Unread(Unread),
}

impl fmt::Display for RunIdsError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let rule = "(blaze-cli run_ids.rs `RunIds::new`)";
        match self {
            RunIdsError::PrefixClash { first, second, prefix } => {
                write!(f, "crates `{first}` and `{second}` begin with one key prefix ")?;
                prefix.iter().try_for_each(|b| write!(f, "{b:02x}"))?;
                write!(f, ", so a checker key cannot say which crate it is of (rule 21) {rule}")
            }
            RunIdsError::Unplaced { krate, why } => write!(f, "crate `{krate}` has no grain for `blaze run`'s keys: {why} {rule}"),
            RunIdsError::LockedReadsWorkspace { krate, dep } => {
                write!(f, "the locked crate `{krate}` depends on `{dep}`, a crate of the checkout: no coarse fact covers its bodies' checks {rule}")
            }
            RunIdsError::Cycle { krate } => write!(f, "the locked crates' dependencies loop through `{krate}` {rule}"),
            RunIdsError::NoRow { krate, dep } => write!(f, "crate `{krate}` depends on row {dep}, which the crate list does not have {rule}"),
            RunIdsError::NotWorkspace { item } => write!(f, "the workspace body {:#034x} is of no crate of the checkout {rule}", item.0),
            RunIdsError::TwoRows { item } => write!(f, "two workspace rows for the body {:#034x} {rule}", item.0),
            RunIdsError::NotABodyCheck { item, name } => write!(f, "the body {:#034x} is named by {name}, which is no `BodyCheck` and no head task's `Signature` {rule}", item.0),
            RunIdsError::TodayUnsorted { at } => write!(f, "today's facts are not sorted by name, one each, at row {at} {rule}"),
            RunIdsError::EngineBody { check, what } => write!(f, "the engine's answer, for the body check {check}: {what} (blaze-cli engine_run_answer.rs `EngineAnswer::body`)"),
            RunIdsError::EngineAnswer { name, what } => write!(f, "the engine's stored answer, reading {name}: {what} (blaze-cli engine_run_answer.rs)"),
            RunIdsError::Table(error) => write!(f, "the body id table: {error}"),
            RunIdsError::TwoDigests { fact } => write!(
                f,
                "two digests of the fact {:?} today: a body carried from the last answer's table read a fact this walk moved {rule}",
                ContentId(*fact)
            ),
            RunIdsError::TableStore { cert, why } => write!(f, "the store, at the body id table of certificate {:?}: {why} {rule}", cert.0),
            RunIdsError::AnswerStore { name, why } => write!(f, "the store, reading the answer's record of {name}: {why} (blaze-cli run_ids.rs `workspace_of`)"),
            RunIdsError::AnswerGone { name } => write!(f, "the answer has a row for {name} whose record the store does not hold (blaze-cli run_ids.rs `workspace_of`)"),
            RunIdsError::AnswerRecord { name, why } => write!(f, "the answer's record of {name}: {why} (blaze-cli run_ids.rs `workspace_of`)"),
            RunIdsError::AnswerShape { name } => write!(f, "the answer's row of {name} stands at a shape, not a record (blaze-cli run_ids.rs `workspace_of`)"),
            RunIdsError::NotWalked { name } => write!(
                f,
                "the walk reached {name}, which this request did not settle and the last answer's body id table did not walk (blaze-cli run_ids.rs `workspace_of`)"
            ),
            RunIdsError::TooManyWalked { most } => write!(f, "the walk crossed more than {most} records (blaze-cli run_ids.rs `workspace_of`)"),
            RunIdsError::ViewOfAnotherUnit { check, unit, covered } => {
                let hex = |f: &mut fmt::Formatter<'_>, prefix: &[u8; 8]| prefix.iter().try_for_each(|b| write!(f, "{b:02x}"));
                write!(f, "the row of {check}, a body at the view of the unit ")?;
                hex(f, unit)?;
                f.write_str(", covers a key of the crate ")?;
                hex(f, covered)?;
                f.write_str(": it was checked under another unit, whose keys this unit's calls never name (D191; blaze-cli run_ids.rs `workspace_of`)")
            }
            RunIdsError::UnitRootOutside { krate, root } => {
                write!(f, "the member unit `{krate}` has its root at `{root}`, not a checkout path (`/0/...`): no file starts its module tree (blaze-cli run_ids.rs `unit_roots`)")
            }
            RunIdsError::TooManyBodies { krate, rows, count, most } => {
                write!(f, "{count} workspace body rows, more than the {most} a body id table holds; crate `{krate}` has the most ({rows}) {rule}")
            }
            RunIdsError::Unread(why) => write!(f, "the body id table's reads: {why}"),
        }
    }
}

impl core::error::Error for RunIdsError<'_> {}

// --- The table as a record (the coordinator's decision, 2026-11-17: "THE TABLE IS A RECORD") ---
//
// FORM (little-endian, every row fixed-width, every table sorted and unique, nothing of the
// machine: two builders of one certificate write equal bytes, rule 25):
//   header (HEADER bytes): magic `BZRI`, version u32, then u32 counts of crate rows, body rows,
//     today rows and edges, 8 zero bytes, the toolchain id (32), the check's producers (32);
//   crate rows (CRATE_ROW each), sorted by prefix: prefix 8, crate key 32, grain tag 1, 7 zero,
//     the grain's read: fact 32, digest 32 (zero for the toolchain and a workspace crate with none);
//   body rows (BODY_ROW each), sorted by checker key: item 16 (big-endian, so byte order is key
//     order), its check's key 32, its check's kind 1 (`BodyCheck`, or `Signature` for a
//     constructor: [`check_kind`]), 7 zero (version 3's read range: no read list is held), verdict
//     tag 1, a name's kind 1, a rule u16, an error count u32, a name's subject 32
//     (`Missing::NotToday`'s name), then the body's check's subject 32, its text 32 and its
//     file's subject 32 ([`WorkspaceBody`]);
//   edges (EDGE each), sorted by (parent, child) name, one each: parent kind 1, child kind 1,
//     6 zero, parent subject 32, child subject 32 (the walked tree, [`workspace_of`]);
//   today (READ each), sorted by fact, one each: the CRATES' facts (the toolchain, each locked
//     crate's closure, each workspace crate's `CrateScope`); a body's proof is read by need.
// Read checks every count, size, order, tag and range ONCE ([`RunIds::read`], rule 74); every
// lookup after it is a binary search over the rows in place.

const TABLE_MAGIC: [u8; 4] = *b"BZRI";
/// 1: the first form. 2: a body row names its check, text and file; the walked tree's edges.
/// 3: the verdict tag `VerdictTag::Typed` (types whole, borrows not checked: run, D151).
/// 4: no read list: a workspace body's one read is its check's proof, read by need ([`Proof`]).
/// 5: a body row names its check's kind (byte 48): a constructor's check is its ADT's head task
/// (`Signature`, [`workspace_of`]'s doc), a body's its `BodyCheck`.
/// The version is in the key's domain too, so a table of another form is never found.
const TABLE_VERSION: u32 = 5;
const TABLE_KEY_DOMAIN: &str = "blaze-cli run ids table key 5";
const HEADER: usize = 96;
const CRATE_ROW: usize = 112;
const BODY_ROW: usize = 192;
const EDGE: usize = 72;
const READ: usize = 64;
/// The most rows of each table a record may say it has (rule 18): far above any workspace
/// (reth: about 300 crates, tens of thousands of bodies), far below what a u32 offset can hold.
const MOST_ROWS: u32 = 1 << 24;

/// A crate row's grain tag (the FORM), decoded once at [`RunIds::read`] (finding 12): a byte no
/// tag is is [`TableError::Tag`], never read as another grain.
#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum GrainTag {
    Toolchain = 0,
    Locked = 1,
    /// A workspace crate with no `CrateScope` read.
    WorkspaceNone = 2,
    Workspace = 3,
}

impl TryFrom<u8> for GrainTag {
    type Error = u8;

    fn try_from(byte: u8) -> Result<GrainTag, u8> {
        Ok(match byte {
            0 => GrainTag::Toolchain,
            1 => GrainTag::Locked,
            2 => GrainTag::WorkspaceNone,
            3 => GrainTag::Workspace,
            found => return Err(found),
        })
    }
}

impl GrainTag {
    fn of(grain: CrateGrain) -> GrainTag {
        match grain {
            CrateGrain::Toolchain => GrainTag::Toolchain,
            CrateGrain::Locked(_) => GrainTag::Locked,
            CrateGrain::Workspace(None) => GrainTag::WorkspaceNone,
            CrateGrain::Workspace(Some(_)) => GrainTag::Workspace,
        }
    }
}

/// A body row's verdict tag (the FORM), decoded once at [`RunIds::read`] with the fields it
/// takes into a [`Checked`] (finding 12).
#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum VerdictTag {
    Clean = 0,
    Errors = 1,
    Incomplete = 2,
    NoRecord = 3,
    NotToday = 4,
    NoValue = 5,
    Cells = 6,
    /// Version 3: types whole, borrows not checked (D151).
    Typed = 7,
}

impl TryFrom<u8> for VerdictTag {
    type Error = u8;

    fn try_from(byte: u8) -> Result<VerdictTag, u8> {
        Ok(match byte {
            0 => VerdictTag::Clean,
            1 => VerdictTag::Errors,
            2 => VerdictTag::Incomplete,
            3 => VerdictTag::NoRecord,
            4 => VerdictTag::NotToday,
            5 => VerdictTag::NoValue,
            6 => VerdictTag::Cells,
            7 => VerdictTag::Typed,
            found => return Err(found),
        })
    }
}

/// Why bytes are not a body id table of this form (rule 19: each says where).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum TableError {
    /// Fewer bytes than the header.
    Short { len: usize },
    /// Not `BZRI`.
    Magic,
    /// Another form's table: made again by the next walk, never read.
    Version { found: u32 },
    /// A count above [`MOST_ROWS`].
    TooMany { what: &'static str, count: u32 },
    /// The bytes are not exactly the header and the rows its counts say (truncated or with
    /// bytes after).
    Size { expected: u64, found: usize },
    /// Header bytes that must be zero are not.
    Padding,
    /// Rows of `what` not sorted and unique at row `at`.
    Unsorted { what: &'static str, at: usize },
    /// A crate row's grain tag or a body row's verdict tag no form writes.
    Tag { what: &'static str, at: usize, tag: u8 },
    /// A body row's `NotToday` name has a kind byte no kind is.
    NameKind { at: usize, byte: u8 },
    /// A body row's zero bytes (49..56, version 3's read range) are not zero.
    BodyPadding { at: usize },
    /// A body row's check kind byte (48) is neither `BodyCheck` nor `Signature`.
    CheckKind { at: usize, byte: u8 },
    /// A body row's verdict is `Errors` with a count of none (finding 13: no form writes it).
    NoErrors { at: usize },
    /// An edge row's kind bytes name no kind, or its zero bytes are not zero.
    EdgeKind { at: usize },
    /// The table found under a toolchain's key was made under another toolchain.
    OtherToolchain,
}

impl fmt::Display for TableError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let rule = "(blaze-cli run_ids.rs, the table's FORM)";
        match self {
            TableError::Short { len } => write!(f, "{len} bytes, fewer than the {HEADER}-byte header {rule}"),
            TableError::Magic => write!(f, "the bytes do not begin with `BZRI` {rule}"),
            TableError::Version { found } => write!(f, "a table of form {found}, not {TABLE_VERSION} {rule}"),
            TableError::TooMany { what, count } => write!(f, "{count} {what}, more than the {MOST_ROWS} a table may hold {rule}"),
            TableError::Size { expected, found } => write!(f, "{found} bytes where the header's counts say {expected} {rule}"),
            TableError::Padding => write!(f, "the header's zero bytes are not zero {rule}"),
            TableError::Unsorted { what, at } => write!(f, "the {what} are not sorted and unique at row {at} {rule}"),
            TableError::Tag { what, at, tag } => write!(f, "{what} {at} holds tag {tag}, which no form writes {rule}"),
            TableError::NameKind { at, byte } => write!(f, "body row {at} names a kind {byte}, which no kind is {rule}"),
            TableError::BodyPadding { at } => write!(f, "body row {at}'s zero bytes are not zero {rule}"),
            TableError::CheckKind { at, byte } => write!(f, "body row {at}'s check is of kind {byte}, neither a `BodyCheck` nor a head task's `Signature` {rule}"),
            TableError::NoErrors { at } => write!(f, "body row {at} is an errors verdict with no error {rule}"),
            TableError::EdgeKind { at } => write!(f, "edge {at} names a kind no kind is, or its zero bytes are not zero {rule}"),
            TableError::OtherToolchain => write!(f, "the table under this toolchain's key was made under another toolchain {rule}"),
        }
    }
}

/// The store key of the body id table of the request whose certificate is `cert`: the
/// certificate covers the tool, the command and every input's digest (request.rs `cert_of`), and
/// the toolchain and every kind's tool are folded in as well, so nothing the table is made from
/// is outside its key (rule 21) and under one key the table is valid as it is.
pub(crate) fn table_key(cert: Key, toolchain: ToolchainId, tools: &[Tool; blaze_drive::key::KINDS]) -> Key {
    let mut h = Hasher::new(TABLE_KEY_DOMAIN);
    h.id(&cert.0).update(&toolchain.0);
    for tool in tools {
        h.id(&tool.0);
    }
    Key(h.finish())
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn word_at(b: &[u8], at: usize) -> [u8; 32] {
    let mut w = [0u8; 32];
    w.copy_from_slice(&b[at..at + 32]);
    w
}

fn put_crate_row<A: core::alloc::Allocator>(out: &mut Vec<u8, A>, row: &CrateRow) {
    out.extend_from_slice(&row.prefix);
    out.extend_from_slice(&row.key.0.0);
    let read = match row.grain {
        CrateGrain::Toolchain | CrateGrain::Workspace(None) => None,
        CrateGrain::Locked(read) | CrateGrain::Workspace(Some(read)) => Some(read),
    };
    out.push(GrainTag::of(row.grain) as u8);
    out.extend_from_slice(&[0; 7]);
    // The FORM's zero words for a grain with no read (no fallback: nothing reads them back).
    let read = read.unwrap_or(Read { fact: [0; 32], digest: [0; 32] });
    out.extend_from_slice(&read.fact);
    out.extend_from_slice(&read.digest);
}

/// Crate row `b` (CRATE_ROW bytes) under its grain `tag`, decoded at [`RunIds::read`] (or laid
/// by `new`).
fn crate_row(b: &[u8], tag: GrainTag) -> CrateRow {
    let mut prefix = [0u8; 8];
    prefix.copy_from_slice(&b[..8]);
    let key = CrateItemKey(ContentId(word_at(b, 8)));
    let read = Read { fact: word_at(b, 48), digest: word_at(b, 80) };
    let grain = match tag {
        GrainTag::Toolchain => CrateGrain::Toolchain,
        GrainTag::Locked => CrateGrain::Locked(read),
        GrainTag::Workspace => CrateGrain::Workspace(Some(read)),
        GrainTag::WorkspaceNone => CrateGrain::Workspace(None),
    };
    CrateRow { prefix, key, grain }
}

fn put_body_row<A: core::alloc::Allocator>(out: &mut Vec<u8, A>, row: &BodyRow) {
    out.extend_from_slice(&row.item.0.to_be_bytes());
    out.extend_from_slice(&row.key.0);
    out.push(row.check.kind as u8);
    out.extend_from_slice(&[0; 7]);
    let (tag, name, rule, errors) = match row.checked {
        Checked::Clean => (VerdictTag::Clean, None, 0, 0),
        Checked::Errors { errors, first } => (VerdictTag::Errors, None, first, errors.get()),
        Checked::Incomplete { rule } => (VerdictTag::Incomplete, None, rule, 0),
        Checked::Typed { rule } => (VerdictTag::Typed, None, rule, 0),
        Checked::Absent(Missing::NoRecord) => (VerdictTag::NoRecord, None, 0, 0),
        Checked::Absent(Missing::NotToday { name }) => (VerdictTag::NotToday, Some(name), 0, 0),
        Checked::Absent(Missing::NoValue) => (VerdictTag::NoValue, None, 0, 0),
        Checked::Absent(Missing::Cells) => (VerdictTag::Cells, None, 0, 0),
    };
    out.push(tag as u8);
    out.push(name.map_or(0, |name| name.kind as u8));
    out.extend_from_slice(&rule.to_le_bytes());
    out.extend_from_slice(&errors.to_le_bytes());
    out.extend_from_slice(&name.map_or([0; 32], |name| name.subject.0));
    out.extend_from_slice(&row.check.subject.0);
    out.extend_from_slice(&row.text.0);
    out.extend_from_slice(&row.file.subject.0);
}

/// One edge of the walked tree as its row (the FORM): `parent`'s record lists `child` as a
/// part; [`Edge::Walked`] is the row `(name, name)`.
fn put_edge(out: &mut AVec<'_, u8>, edge: Edge) {
    let (parent, child) = edge.pair();
    out.push(parent.kind as u8);
    out.push(child.kind as u8);
    out.extend_from_slice(&[0; 6]);
    out.extend_from_slice(&parent.subject.0);
    out.extend_from_slice(&child.subject.0);
}

/// Edge row `b` (EDGE bytes), decoded once at [`RunIds::read`]: `None` when a kind byte names
/// no kind or its zero bytes are not zero.
fn edge(b: &[u8]) -> Option<Edge> {
    if b[2..8] != [0; 6] {
        return None;
    }
    let pair = (Name::new(Kind::from_u8(b[0])?, ContentId(word_at(b, 8))), Name::new(Kind::from_u8(b[1])?, ContentId(word_at(b, 40))));
    Some(Edge::of_pair(pair))
}

fn body_item(b: &[u8]) -> ItemKey {
    let mut item = [0u8; 16];
    item.copy_from_slice(&b[..16]);
    ItemKey(u128::from_be_bytes(item))
}

/// Body row `b`'s verdict (BODY_ROW bytes at `at`), decoded once at [`RunIds::read`]: every tag,
/// name kind and count is checked here, so nothing after it reads the bytes as a verdict again.
fn verdict(b: &[u8], at: usize) -> Result<Checked, TableError> {
    let (tag, kind, rule, errors) = (b[56], b[57], u16_at(b, 58), u32_at(b, 60));
    let tag = VerdictTag::try_from(tag).map_err(|tag| TableError::Tag { what: "body row", at, tag })?;
    Ok(match tag {
        VerdictTag::Clean => Checked::Clean,
        VerdictTag::Errors => Checked::Errors { errors: NonZeroU32::new(errors).ok_or(TableError::NoErrors { at })?, first: rule },
        VerdictTag::Incomplete => Checked::Incomplete { rule },
        VerdictTag::Typed => Checked::Typed { rule },
        VerdictTag::NoRecord => Checked::Absent(Missing::NoRecord),
        VerdictTag::NotToday => {
            let kind = Kind::from_u8(kind).ok_or(TableError::NameKind { at, byte: kind })?;
            Checked::Absent(Missing::NotToday { name: Name::new(kind, ContentId(word_at(b, 64))) })
        }
        VerdictTag::NoValue => Checked::Absent(Missing::NoValue),
        VerdictTag::Cells => Checked::Absent(Missing::Cells),
    })
}

/// Body row `b` (BODY_ROW bytes) with its verdict `checked`, decoded at [`RunIds::read`] (or
/// laid by `new`).
fn body_row(b: &[u8], checked: Checked) -> BodyRow {
    BodyRow {
        item: body_item(b),
        key: ContentId(word_at(b, 16)),
        checked,
        check: row_check(b),
        text: ContentId(word_at(b, 128)),
        file: Name::new(Kind::FileItems, ContentId(word_at(b, 160))),
    }
}

/// The kind of a body row's check (byte 48, the FORM): a `BodyCheck`, or a head task
/// (`Signature`) for a constructor ([`workspace_of`]'s doc); any other byte is no row's.
fn check_kind(byte: u8) -> Option<Kind> {
    Kind::from_u8(byte).filter(|kind| matches!(kind, Kind::BodyCheck | Kind::Signature))
}

/// Body row `b`'s check: its kind ([`check_kind`]) and subject. Every row reaching here was
/// laid by [`put_body_row`] or checked by [`RunIds::read`] (`TableError::CheckKind`), so a kind
/// byte that is no check's is this file's bug, said by name.
fn row_check(b: &[u8]) -> Name {
    let kind = check_kind(b[48]).unwrap_or_else(|| panic!("a body row whose check kind byte {} is no check's, past `RunIds::read` (run_ids.rs `row_check`)", b[48]));
    Name::new(kind, ContentId(word_at(b, 96)))
}

fn read_at(b: &[u8]) -> Read {
    Read { fact: word_at(b, 0), digest: word_at(b, 32) }
}

/// The command's body ids for `blaze run` (the module doc). Its crate and body rows are the
/// table record's bytes ([`RunIds::read`]: the store's, in place, on a certificate hit; the
/// builder's, on a walk); today's table (the crates' facts, one per crate) is one copy in the
/// caller's allocator, since a [`BodyId`] hands out `&[Read]` and blaze-run's `Read` has no
/// in-place view of bytes. It holds NO read list: a workspace body's one read is its check's
/// [`Proof`], read by need from [`ReadsFrom`] at the first call that steps it, once a check
/// ([`Slot`]).
///
/// `Copy`: every field is a borrow of the rows (or a fixed word), so a copy is a second view of
/// the same table, never a copy of a row (rule 11): the run's typing reads its own view
/// (engine_body_record.rs `StoreTyped`) beside the parent's.
#[derive(Clone, Copy)]
pub(crate) struct RunIds<'a> {
    toolchain: ToolchainId,
    /// The toolchain fact with today's digest, the toolchain's id.
    toolchain_read: Read,
    /// The check's producers (`CHECK_KINDS`' tools), in every toolchain and locked body key.
    producers: ContentId,
    /// CRATE_ROW-wide rows, sorted by prefix, one each.
    crates: &'a [u8],
    /// Each crate row's grain, decoded once ([`RunIds::read`], finding 12): row `i`'s is `grains[i]`.
    grains: &'a [GrainTag],
    /// BODY_ROW-wide rows, sorted by item, one each.
    bodies: &'a [u8],
    /// Each body row's verdict, decoded once: row `i`'s is `verdicts[i]`.
    verdicts: &'a [Checked],
    /// The walked tree ([`workspace_of`]), sorted, one each, decoded once.
    edges: &'a [Edge],
    /// Every crate fact's digest today, sorted by fact, one each.
    today: &'a [Read],
    /// One a distinct check of the body rows, sorted by [`body_fact`], its proof read by need.
    slots: &'a [Slot],
    /// Where the proofs are read from; `None` until [`RunIds::reading`] (then a proof is
    /// [`Unread::NoSource`], never a guess).
    from: Option<ReadsFrom<'a>>,
}

impl<'a> RunIds<'a> {
    /// The body ids of a command: `toolchain` the cut it opened, `crates` the `UnitGraph`
    /// record's crate list, `tools` its kinds' tools (home.rs `tools`), `workspace` the bodies
    /// of the checkout and `edges` the tree they were found by ([`workspace_of`]'s), and `answer`
    /// the command's standing answer: every drive read's out today ([`Today`]) and each body's
    /// key, verdict and reads ([`Answer::body`], where its `BodyCheck` record is found). Every
    /// table is in `alloc`.
    ///
    /// `previous`: the table of the answer this request walked FORWARD from (the last answer's
    /// certificate's, [`RunIds::from_record`]), or `None` for a walk from the units down. A
    /// forward walk settles only what is reached from what changed (request.rs `begin`'s seeds),
    /// so `today` does not know the outs of what it did not reach. INVARIANT relied on (the
    /// drive's, drive.md "candidates are found forward from what changed"): a name the walk did
    /// not reach stands as the last answer has it. So a workspace body this run cannot key
    /// (`NotToday`: an out it needs is one the walk did not reach) takes its row from `previous`
    /// as it is, and a workspace crate whose `CrateScope` out is not known takes its read from
    /// `previous`. A body or crate `previous` does not hold stays `NotToday` / without a fact:
    /// never a guess.
    #[expect(clippy::too_many_arguments, reason = "the seam's edge: each argument one input the command already holds")]
    pub(crate) fn new<'n, C: CrateTable<'n> + ?Sized>(
        cx: &Cx,
        toolchain: ToolchainId,
        crates: &C,
        tools: &[Tool; blaze_drive::key::KINDS],
        workspace: &[WorkspaceBody],
        edges: &[Edge],
        answer: &mut dyn Answer,
        previous: Option<&RunIds<'_>>,
        most: Most,
        bump: Bump<'a>,
    ) -> Result<RunIds<'a>, RunIdsError<'n>> {
        let _cx = cx.span("run ids: the body ids of the program's crates");
        let alloc = bump.alloc();
        let producers = {
            let mut h = Hasher::new(PRODUCERS_DOMAIN);
            for kind in CHECK_KINDS {
                h.update(&[kind as u8]).id(&tools[kind.index()].0);
            }
            h.finish()
        };
        let toolchain_read = Read { fact: toolchain_fact(), digest: toolchain.0 };
        let mut facts: Vec<Read, Bump<'a>> = Vec::new_in(bump);
        facts.push(toolchain_read);

        // The crates' grains, and each locked crate's closure digest.
        let n = crates.len();
        let digests = locked_digests(crates, toolchain, alloc)?;
        let mut rows: AVec<'a, CrateRow> = Vec::with_capacity_in(n, alloc);
        for at in 0..n {
            let key = crates.key(at);
            let grain = match crates.place(at) {
                Place::Toolchain => CrateGrain::Toolchain,
                Place::Fetched { .. } => {
                    let read = Read { fact: locked_fact(key), digest: digests[at] };
                    facts.push(read);
                    CrateGrain::Locked(read)
                }
                Place::Checkout => {
                    let scope = blaze_drive::key::crate_scope_name(&key);
                    let read = answer.out(scope).map(|out| Read { fact: fact_of(scope), digest: out.0 });
                    // Not reached this walk: the last answer's read stands (`previous`, above).
                    let read = read.or_else(|| match previous?.crate_by_key(key)?.grain {
                        CrateGrain::Workspace(read) => read,
                        _ => None,
                    });
                    facts.extend(read);
                    CrateGrain::Workspace(read)
                }
                Place::Unplaced { why } => return Err(RunIdsError::Unplaced { krate: crates.name(at), why }),
            };
            rows.push(CrateRow { prefix: key.prefix(), key, grain });
        }
        rows.sort_unstable_by_key(|row| row.prefix);
        if let Some(pair) = rows.windows(2).find(|pair| pair[0].prefix == pair[1].prefix) {
            let name = |key: CrateItemKey| (0..n).find(|at| crates.key(*at) == key).map_or("<unnamed>", |at| crates.name(at));
            return Err(RunIdsError::PrefixClash { first: name(pair[0].key), second: name(pair[1].key), prefix: pair[0].prefix });
        }

        // The cap, before any row is made (rule 18): the crate with the most rows is named.
        if workspace.len() > most.bodies as usize {
            let mut per: AVec<'a, usize> = Vec::with_capacity_in(rows.len(), alloc);
            per.resize(rows.len(), 0);
            for body in workspace {
                if let Ok(at) = rows.binary_search_by_key(&prefix_of(body.item), |row| row.prefix) {
                    per[at] += 1;
                }
            }
            let (at, most_rows) = per.iter().copied().enumerate().max_by_key(|(_, count)| *count).unwrap_or((0, 0));
            let krate = rows.get(at).and_then(|row| (0..n).find(|k| crates.key(*k) == row.key)).map_or("<no crate>", |k| crates.name(k));
            return Err(RunIdsError::TooManyBodies { krate, rows: most_rows, count: workspace.len(), most: most.bodies });
        }

        // Each workspace body: its key and verdict as the answer holds them ([`Answer::body`]).
        // Its reads are not asked: its check's proof is read by need ([`RunIds::reading`]).
        let tool = tools[Kind::BodyCheck.index()];
        let mut bodies: AVec<'a, BodyRow> = Vec::with_capacity_in(workspace.len(), alloc);
        let mut scratch = BodyScratch::new(alloc);
        for body in workspace {
            let item = body.item;
            let crate_row = rows.binary_search_by_key(&prefix_of(item), |row| row.prefix).ok().map(|at| rows[at]);
            if !matches!(crate_row, Some(CrateRow { grain: CrateGrain::Workspace(_), .. })) {
                return Err(RunIdsError::NotWorkspace { item });
            }
            if check_kind(body.check.kind as u8).is_none() {
                return Err(RunIdsError::NotABodyCheck { item, name: body.check });
            }
            let held = answer.body(cx, body.check, tool, Some((body.text, body.file)), &mut scratch)?;
            // The answer does not know the check today: only the last answer's row can stand.
            let (key, checked) = match held {
                Some(held) => (held.key, held.checked),
                None => match previous.and_then(|previous| previous.body(item)) {
                    Some(_) => (ABSENT, Checked::Absent(Missing::NotToday { name: body.check })),
                    None => return Err(RunIdsError::NotWalked { name: body.check }),
                },
            };
            // Not reached this walk: the last answer's row stands, key and verdict (`new`'s doc,
            // `previous`); its proof is read by need like any other's.
            if let (Checked::Absent(Missing::NotToday { .. }), Some(previous)) = (checked, previous)
                && let Some(row) = previous.body(item)
            {
                bodies.push(BodyRow { item, key: row.key, checked: row.checked, check: body.check, text: body.text, file: body.file });
                continue;
            }
            bodies.push(BodyRow { item, key, checked, check: body.check, text: body.text, file: body.file });
        }
        bodies.sort_unstable_by_key(|row| row.item);
        if let Some(pair) = bodies.windows(2).find(|pair| pair[0].item == pair[1].item) {
            return Err(RunIdsError::TwoRows { item: pair[0].item });
        }

        // Today's table: the crates' facts, one digest a fact (each is made by one function of
        // its name, so two rows of one fact carry one digest; the first is kept). Two digests
        // of one fact are refused by name, never one picked.
        facts.sort_unstable();
        facts.dedup();
        if let Some(pair) = facts.windows(2).find(|pair| pair[0].fact == pair[1].fact) {
            return Err(RunIdsError::TwoDigests { fact: pair[0].fact });
        }

        // Laid out as the record's rows (the one representation every lookup reads), in the
        // bodies' sorted order, so the caller's order of `workspace` reaches no byte (rule 25).
        let mut crate_bytes: Vec<u8, Bump<'a>> = Vec::with_capacity_in(rows.len() * CRATE_ROW, bump);
        let mut grains: Vec<GrainTag, Bump<'a>> = Vec::with_capacity_in(rows.len(), bump);
        for row in rows.iter() {
            put_crate_row(&mut crate_bytes, row);
            grains.push(GrainTag::of(row.grain));
        }
        let mut body_bytes: Vec<u8, Bump<'a>> = Vec::with_capacity_in(bodies.len() * BODY_ROW, bump);
        let mut verdicts: Vec<Checked, Bump<'a>> = Vec::with_capacity_in(bodies.len(), bump);
        for row in bodies.iter() {
            put_body_row(&mut body_bytes, row);
            verdicts.push(row.checked);
        }
        let body_bytes: &'a [u8] = body_bytes.into_bump_slice();
        let slots = slots_of(body_bytes, bump);
        // The walked tree, sorted and one each whatever order the walk handed it in.
        let mut tree: Vec<Edge, Bump<'a>> = Vec::with_capacity_in(edges.len(), bump);
        tree.extend_from_slice(edges);
        tree.sort_unstable();
        tree.dedup();
        Ok(RunIds {
            toolchain,
            toolchain_read,
            producers,
            crates: crate_bytes.into_bump_slice(),
            grains: grains.into_bump_slice(),
            bodies: body_bytes,
            verdicts: verdicts.into_bump_slice(),
            edges: tree.into_bump_slice(),
            today: facts.into_bump_slice(),
            slots,
            from: None,
        })
    }

    /// The table with its source of proofs: `from` is where each workspace body's [`Proof`] is
    /// read, by its check, and `R` when ([`Reading`]: the command's is [`Production`], by need;
    /// a test's control reads every row's at once). The one seam of the strategy (rule 78).
    pub(crate) fn reading<R: Reading>(self, from: ReadsFrom<'a>) -> Result<RunIds<'a>, Unread> {
        let ids = RunIds { from: Some(from), ..self };
        R::settle(&ids)?;
        Ok(ids)
    }

    /// The table's workspace body rows.
    pub(crate) fn rows(&self) -> u32 {
        u32::try_from(self.bodies.len() / BODY_ROW).unwrap_or(u32::MAX)
    }

    /// The bytes of the table's record ([`RunIds::write`]'s length), without writing it.
    pub(crate) fn table_bytes(&self) -> u64 {
        (HEADER + self.crates.len() + self.bodies.len() + self.edges.len() * EDGE + self.today.len() * READ) as u64
    }

    /// The profile's line ([`IdsLine`]) of this table, what `asked` counted, and `ns` to make it.
    pub(crate) fn line(&self, asked: &Asked, ns: u64) -> IdsLine {
        IdsLine { rows: self.rows(), read: asked.bodies(), folded: asked.reads(), bytes: self.table_bytes(), ns }
    }

    /// The proof of the check `check`, read at its first ask and kept ([`Slot`]): `Err` names
    /// why it cannot be (no source, no row, a refused page, past the cap), the same each ask.
    fn proof_of(&self, check: Name) -> Result<&Proof, Unread> {
        let fact = body_fact(check);
        let Ok(at) = self.slots.binary_search_by(|slot| slot.fact.cmp(&fact)) else { return Err(Unread::NoRow { check }) };
        self.settled(&self.slots[at])
    }

    /// `slot`'s proof, read now if it was not (counted in the source's [`Asked`]).
    fn settled<'s>(&'s self, slot: &'s Slot) -> Result<&'s Proof, Unread> {
        let held = slot.proof.get_or_init(|| {
            let Some(from) = self.from else { return Err(Unread::NoSource { check: slot.check }) };
            from.asked.bodies.set(from.asked.bodies.get().saturating_add(1));
            let proof = from.source.proof(slot.check, from.most.reads)?;
            from.asked.reads.set(from.asked.reads.get().saturating_add(u64::from(proof.reads)));
            Ok(proof)
        });
        held.as_ref().map_err(|why| *why)
    }

    /// Every proof a row whose check runs needs, read now (the [`Eager`] control): each check
    /// once, a verdict that does not run (`Absent`, `Errors`, `Incomplete`) asking none.
    #[cfg(test)]
    fn settle_every(&self) -> Result<(), Unread> {
        for (b, checked) in self.bodies.chunks_exact(BODY_ROW).zip(self.verdicts.iter().copied()) {
            let row = body_row(b, checked);
            if checked.runs(row.item).is_ok() {
                self.proof_of(row.check)?;
            }
        }
        Ok(())
    }

    /// The table as its record's bytes (the FORM above), in `alloc`. Equal tables write equal
    /// bytes: every row and every arena is in its sorted order. No proof is in it: proofs are
    /// today's, read by need.
    pub(crate) fn write<'b>(&self, alloc: Alloc<'b>) -> AVec<'b, u8> {
        let len = self.table_bytes() as usize;
        let mut out: AVec<'b, u8> = Vec::with_capacity_in(len, alloc);
        out.extend_from_slice(&TABLE_MAGIC);
        out.extend_from_slice(&TABLE_VERSION.to_le_bytes());
        for count in [self.crates.len() / CRATE_ROW, self.bodies.len() / BODY_ROW, self.today.len(), self.edges.len()] {
            // Within MOST_ROWS: the builder's tables are bounded by the command's crates and
            // bodies (`Most::bodies`); a table past it is refused on read, never truncated here.
            out.extend_from_slice(&(count as u32).to_le_bytes());
        }
        out.extend_from_slice(&[0; 8]);
        out.extend_from_slice(&self.toolchain.0);
        out.extend_from_slice(&self.producers.0);
        out.extend_from_slice(self.crates);
        out.extend_from_slice(self.bodies);
        for edge in self.edges.iter().copied() {
            put_edge(&mut out, edge);
        }
        for read in self.today.iter() {
            out.extend_from_slice(&read.fact);
            out.extend_from_slice(&read.digest);
        }
        out
    }

    /// The table a record's `bytes` hold, its crate and body rows read in place, today's table
    /// copied once into `alloc` (the struct's doc). Every count, size, order, tag and range is
    /// checked here, once (rule 74); nothing after it checks again. Its proofs have no source
    /// until [`RunIds::reading`].
    pub(crate) fn read(bytes: &'a [u8], bump: Bump<'a>) -> Result<RunIds<'a>, TableError> {
        if bytes.len() < HEADER {
            return Err(TableError::Short { len: bytes.len() });
        }
        if bytes[..4] != TABLE_MAGIC {
            return Err(TableError::Magic);
        }
        let version = u32_at(bytes, 4);
        if version != TABLE_VERSION {
            return Err(TableError::Version { found: version });
        }
        let mut counts = [0u32; 4];
        for (i, what) in ["crate rows", "body rows", "today rows", "edges"].into_iter().enumerate() {
            counts[i] = u32_at(bytes, 8 + 4 * i);
            if counts[i] > MOST_ROWS {
                return Err(TableError::TooMany { what, count: counts[i] });
            }
        }
        if bytes[24..32] != [0; 8] {
            return Err(TableError::Padding);
        }
        let [n_crates, n_bodies, n_today, n_edges] = counts.map(|c| c as usize);
        let expected = (HEADER + n_crates * CRATE_ROW + n_bodies * BODY_ROW + n_edges * EDGE + n_today * READ) as u64;
        if expected != bytes.len() as u64 {
            return Err(TableError::Size { expected, found: bytes.len() });
        }
        let toolchain = ToolchainId(word_at(bytes, 32));
        let producers = ContentId(word_at(bytes, 64));
        let mut at = HEADER;
        let crates = &bytes[at..at + n_crates * CRATE_ROW];
        at += crates.len();
        let bodies = &bytes[at..at + n_bodies * BODY_ROW];
        at += bodies.len();
        let edge_bytes = &bytes[at..at + n_edges * EDGE];
        at += edge_bytes.len();
        let mut edges: Vec<Edge, Bump<'a>> = Vec::with_capacity_in(n_edges, bump);
        for (i, row) in edge_bytes.chunks_exact(EDGE).enumerate() {
            let edge = edge(row).ok_or(TableError::EdgeKind { at: i })?;
            if edges.last().is_some_and(|last| *last >= edge) {
                return Err(TableError::Unsorted { what: "edges", at: i });
            }
            edges.push(edge);
        }
        let mut today: Vec<Read, Bump<'a>> = Vec::with_capacity_in(n_today, bump);
        for row in bytes[at..].chunks_exact(READ) {
            today.push(read_at(row));
        }

        let mut grains: Vec<GrainTag, Bump<'a>> = Vec::with_capacity_in(n_crates, bump);
        for (i, row) in crates.chunks_exact(CRATE_ROW).enumerate() {
            let grain = GrainTag::try_from(row[40]).map_err(|tag| TableError::Tag { what: "crate row", at: i, tag })?;
            if row[41..48] != [0; 7] {
                return Err(TableError::Tag { what: "crate row", at: i, tag: row[40] });
            }
            if i > 0 && crates[(i - 1) * CRATE_ROW..(i - 1) * CRATE_ROW + 8] >= row[..8] {
                return Err(TableError::Unsorted { what: "crate rows", at: i });
            }
            grains.push(grain);
        }
        let mut verdicts: Vec<Checked, Bump<'a>> = Vec::with_capacity_in(n_bodies, bump);
        for (i, row) in bodies.chunks_exact(BODY_ROW).enumerate() {
            verdicts.push(verdict(row, i)?);
            if i > 0 && body_item(&bodies[(i - 1) * BODY_ROW..i * BODY_ROW]) >= body_item(row) {
                return Err(TableError::Unsorted { what: "body rows", at: i });
            }
            if check_kind(row[48]).is_none() {
                return Err(TableError::CheckKind { at: i, byte: row[48] });
            }
            if row[49..56] != [0; 7] {
                return Err(TableError::BodyPadding { at: i });
            }
        }
        if let Some(k) = today.windows(2).position(|pair| pair[0].fact >= pair[1].fact) {
            return Err(TableError::Unsorted { what: "today rows", at: k + 1 });
        }
        let toolchain_read = Read { fact: toolchain_fact(), digest: toolchain.0 };
        Ok(RunIds {
            toolchain,
            toolchain_read,
            producers,
            crates,
            grains: grains.into_bump_slice(),
            bodies,
            verdicts: verdicts.into_bump_slice(),
            edges: edges.into_bump_slice(),
            today: today.into_bump_slice(),
            slots: slots_of(bodies, bump),
            from: None,
        })
    }

    /// The parts the walk followed from `parent`'s record ([`workspace_of`]), when the walk that
    /// made this table crossed it ([`Edge::Walked`] of it); `None`: it did not.
    pub(crate) fn children(&self, parent: Name) -> Option<impl Iterator<Item = Name> + '_> {
        let from = self.edges.partition_point(|edge| edge.parent() < parent);
        let to = self.edges.partition_point(|edge| edge.parent() <= parent);
        let under = &self.edges[from..to];
        under.contains(&Edge::Walked(parent)).then(|| {
            under.iter().filter_map(|edge| match *edge {
                Edge::Part { child, .. } => Some(child),
                Edge::Walked(_) => None,
            })
        })
    }

    /// Every workspace body row as the [`WorkspaceBody`] it was made from, in `alloc`.
    pub(crate) fn workspace_rows<'b>(&self, alloc: Alloc<'b>) -> AVec<'b, WorkspaceBody> {
        let mut rows: AVec<'b, WorkspaceBody> = Vec::with_capacity_in(self.bodies.len() / BODY_ROW, alloc);
        for (b, checked) in self.bodies.chunks_exact(BODY_ROW).zip(self.verdicts.iter().copied()) {
            let row = body_row(b, checked);
            rows.push(WorkspaceBody { item: row.item, check: row.check, text: row.text, file: row.file });
        }
        rows
    }

    /// The table written as the record of the command whose certificate is `cert`
    /// ([`table_key`]), through the one store every answer record goes through (`Store::put`).
    /// A key already holding equal bytes stands (rule 25); other bytes are the store's refusal.
    pub(crate) fn put(&self, cx: &Cx, store: &mut dyn Store<'_>, cert: Key, tools: &[Tool; blaze_drive::key::KINDS], scratch: Alloc<'_>) -> Result<(), RunIdsError<'static>> {
        let _cx = cx.span("run ids: write the body id table");
        let bytes = self.write(scratch);
        store.put(cx, table_key(cert, self.toolchain, tools), &bytes).map_err(|why| RunIdsError::TableStore { cert, why })
    }

    /// The table record of certificate `cert`, read in place ([`RunIds::read`]); `Ok(None)` when
    /// the store holds none (dropped, or never written): the caller then builds the table from
    /// its answer (engine_start.rs `body_ids`), never guesses.
    pub(crate) fn from_record<'m: 'a>(
        cx: &Cx,
        store: &mut dyn Store<'m>,
        cert: Key,
        toolchain: ToolchainId,
        tools: &[Tool; blaze_drive::key::KINDS],
        bump: Bump<'a>,
    ) -> Result<Option<RunIds<'a>>, RunIdsError<'static>> {
        let _cx = cx.span("run ids: read the body id table");
        let held = store.get(cx, table_key(cert, toolchain, tools)).map_err(|why| RunIdsError::TableStore { cert, why })?;
        let Some(held) = held else { return Ok(None) };
        let bytes: &'a [u8] = held.bytes;
        let ids = RunIds::read(bytes, bump).map_err(RunIdsError::Table)?;
        if ids.toolchain != toolchain {
            // The key holds the toolchain: bytes of another toolchain under it are a store's bug.
            return Err(RunIdsError::Table(TableError::OtherToolchain));
        }
        Ok(Some(ids))
    }

    /// Why the workspace body `item` has no reads, when it has none.
    pub(crate) fn missing(&self, item: ItemKey) -> Option<Missing> {
        match self.body(item)?.checked {
            Checked::Absent(missing) => Some(missing),
            _ => None,
        }
    }

    /// The grain `item`'s body is keyed at; `None`: its key's prefix is of no crate of the list.
    pub(crate) fn grain(&self, item: ItemKey) -> Option<Grain> {
        Some(match self.crate_of(item)?.grain {
            CrateGrain::Toolchain => Grain::Toolchain,
            CrateGrain::Locked(_) => Grain::Dependency,
            CrateGrain::Workspace(_) => Grain::Workspace,
        })
    }

    /// What the store holds of the workspace body `item`'s check today; `None`: `item` is no
    /// workspace body the command named (a toolchain or dependency body has no `BodyCheck`: its
    /// key is its coarse fact's). A body the caller did not list at all is `None` too, and the
    /// parent refuses it by name like any body whose check does not run ([`Checked::runs`]).
    pub(crate) fn checked(&self, item: ItemKey) -> Option<Checked> {
        self.body(item).map(|row| row.checked)
    }

    /// The `BodyCheck` task of the workspace body `item`'s row: its run facts record names the
    /// body's typed record (engine_body_record.rs `StoreTyped`). `None`: no body row (a
    /// toolchain or dependency body, or one the command did not name).
    pub(crate) fn check_of(&self, item: ItemKey) -> Option<Name> {
        self.body(item).map(|row| row.check)
    }

    /// THE GATE, asked at the call (states.md item 13): the proof that the fn `item` may run,
    /// by its grain. A toolchain or locked body's coarse fact covers it ([`Runnable::toolchain`],
    /// [`Runnable::locked`]); a workspace body runs by its check today ([`Checked::runs`]); a
    /// workspace fn with no body row is [`NotRunnable::NoRow`]; a fn of no crate of the list is
    /// [`NotRunnable::NoGrain`], never given a grain by a guess.
    pub(crate) fn runnable(&self, item: ItemKey) -> Result<Runnable, NotRunnable> {
        let Some(row) = self.crate_of(item) else { return Err(NotRunnable::NoGrain { function: item }) };
        match row.grain {
            CrateGrain::Toolchain => Ok(Runnable::toolchain(item)),
            CrateGrain::Locked(_) => Ok(Runnable::locked(item)),
            // A body whose check runs is keyed by its proof, read here by need (the call is the
            // first to step it): one it cannot read refuses this call by name, the rest run.
            CrateGrain::Workspace(_) => match self.body(item) {
                Some(row) => {
                    let ran = row.checked.runs(item)?;
                    self.proof_of(row.check).map_err(NotRunnable::Unread)?;
                    Ok(ran)
                }
                None => Err(NotRunnable::NoRow(self.no_row(item))),
            },
        }
    }

    /// What the table holds beside the workspace fn `item` it has no body row for ([`NoRow`]),
    /// for the parent's refusal (rule 19). Read only on that error's path: one pass over the
    /// crate rows and a binary search of the body rows, nothing allocated.
    pub(crate) fn no_row(&self, item: ItemKey) -> NoRow {
        let called = prefix_of(item);
        let mut held = NoRow { called, crates: [[0; 8]; NO_ROW_CRATES], named: 0, workspace: 0, called_held: false, under_called: 0 };
        for (b, tag) in self.crates.chunks_exact(CRATE_ROW).zip(self.grains.iter().copied()) {
            let row = crate_row(b, tag);
            if !matches!(row.grain, CrateGrain::Workspace(_)) {
                continue;
            }
            if usize::from(held.named) < NO_ROW_CRATES {
                held.crates[usize::from(held.named)] = row.prefix;
                held.named += 1;
            }
            held.workspace = held.workspace.saturating_add(1);
            held.called_held |= row.prefix == called;
        }
        // The rows are sorted by item, and an item's first 8 bytes are its prefix: the called
        // crate's rows are one run.
        let n = self.bodies.len() / BODY_ROW;
        let at = |i: usize| body_item(&self.bodies[i * BODY_ROW..(i + 1) * BODY_ROW]);
        let from = partition(n, |i| prefix_of(at(i)) < called);
        let to = partition(n, |i| prefix_of(at(i)) <= called);
        held.under_called = u32::try_from(to - from).unwrap_or(u32::MAX);
        held
    }

    /// The crate row whose prefix is `item`'s: a binary search over the rows in place.
    fn crate_of(&self, item: ItemKey) -> Option<CrateRow> {
        let prefix = prefix_of(item);
        let n = self.crates.len() / CRATE_ROW;
        let at = search(n, |i| self.crates[i * CRATE_ROW..i * CRATE_ROW + 8].cmp(&prefix[..]))?;
        Some(crate_row(&self.crates[at * CRATE_ROW..(at + 1) * CRATE_ROW], self.grains[at]))
    }

    /// The body row of `item`: a binary search over the rows in place.
    fn body(&self, item: ItemKey) -> Option<BodyRow> {
        let n = self.bodies.len() / BODY_ROW;
        let at = search(n, |i| body_item(&self.bodies[i * BODY_ROW..(i + 1) * BODY_ROW]).cmp(&item))?;
        Some(body_row(&self.bodies[at * BODY_ROW..(at + 1) * BODY_ROW], self.verdicts[at]))
    }

    /// The crate row of the crate `key` (its prefix's row, when the row is that crate's).
    fn crate_by_key(&self, key: CrateItemKey) -> Option<CrateRow> {
        let prefix = key.prefix();
        let n = self.crates.len() / CRATE_ROW;
        let at = search(n, |i| self.crates[i * CRATE_ROW..i * CRATE_ROW + 8].cmp(&prefix[..]))?;
        let row = crate_row(&self.crates[at * CRATE_ROW..(at + 1) * CRATE_ROW], self.grains[at]);
        (row.key == key).then_some(row)
    }

    /// `fact`'s row of today's table, borrowed from it.
    fn today_row(&self, fact: &[u8; 32]) -> Option<&Read> {
        self.today.binary_search_by_key(fact, |read| read.fact).ok().map(|at| &self.today[at])
    }
}

/// The first of `n` rows where `before(i)` is false, `before` true on a prefix: O(log n).
fn partition(n: usize, before: impl Fn(usize) -> bool) -> usize {
    let (mut lo, mut hi) = (0usize, n);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if before(mid) { lo = mid + 1 } else { hi = mid }
    }
    lo
}

/// The row of `n` sorted rows where `order(i)` is `Equal`: a binary search, O(log n).
fn search(n: usize, order: impl Fn(usize) -> core::cmp::Ordering) -> Option<usize> {
    let (mut lo, mut hi) = (0usize, n);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        match order(mid) {
            core::cmp::Ordering::Less => lo = mid + 1,
            core::cmp::Ordering::Greater => hi = mid,
            core::cmp::Ordering::Equal => return Some(mid),
        }
    }
    None
}

/// A cell read's fact: its name and cell as 32 bytes (beside [`fact_of`] for a whole read).
pub(crate) fn cell_fact(name: Name, cell: Cell) -> [u8; 32] {
    let mut h = Hasher::new(DRIVE_CELL_FACT_DOMAIN);
    h.update(&[name.kind as u8]).id(&name.subject).update(&cell.0);
    h.finish().0
}

/// Each crate's closure digest (the module doc, case 2), by row: a locked crate's over its key,
/// its pack and its dependencies' digests; a toolchain crate's the toolchain's id; a checkout
/// crate's [`ABSENT`] (never read: its bodies are keyed by their checks). Iterative, each crate
/// hashed once: O(crates + dependency edges).
fn locked_digests<'n, 'a, C: CrateTable<'n> + ?Sized>(crates: &C, toolchain: ToolchainId, alloc: Alloc<'a>) -> Result<AVec<'a, [u8; 32]>, RunIdsError<'n>> {
    const UNSEEN: u8 = 0;
    const OPEN: u8 = 1;
    const DONE: u8 = 2;
    let n = crates.len();
    let mut digests: AVec<'a, [u8; 32]> = Vec::with_capacity_in(n, alloc);
    digests.resize(n, ABSENT.0);
    let mut state: AVec<'a, u8> = Vec::with_capacity_in(n, alloc);
    state.resize(n, UNSEEN);
    let mut deps: AVec<'a, usize> = Vec::new_in(alloc);
    // (row, its dependencies pushed)
    let mut stack: AVec<'a, (usize, bool)> = Vec::new_in(alloc);
    for root in 0..n {
        if !matches!(crates.place(root), Place::Fetched { .. }) || state[root] == DONE {
            continue;
        }
        stack.push((root, false));
        while let Some((at, expanded)) = stack.pop() {
            if state[at] == DONE {
                continue;
            }
            let Place::Fetched { source, files } = crates.place(at) else {
                // Only a locked crate is pushed (its dependencies are checked below).
                state[at] = DONE;
                continue;
            };
            deps.clear();
            crates.each_dep(at, &mut |dep| deps.push(dep));
            if !expanded {
                if state[at] == OPEN {
                    return Err(RunIdsError::Cycle { krate: crates.name(at) });
                }
                state[at] = OPEN;
                stack.push((at, true));
                for dep in deps.iter().copied() {
                    if dep >= n {
                        return Err(RunIdsError::NoRow { krate: crates.name(at), dep });
                    }
                    match crates.place(dep) {
                        Place::Toolchain => {
                            digests[dep] = toolchain.0;
                            state[dep] = DONE;
                        }
                        Place::Checkout => return Err(RunIdsError::LockedReadsWorkspace { krate: crates.name(at), dep: crates.name(dep) }),
                        Place::Unplaced { why } => return Err(RunIdsError::Unplaced { krate: crates.name(dep), why }),
                        Place::Fetched { .. } if state[dep] == OPEN => return Err(RunIdsError::Cycle { krate: crates.name(dep) }),
                        Place::Fetched { .. } if state[dep] == UNSEEN => stack.push((dep, false)),
                        Place::Fetched { .. } => {}
                    }
                }
                continue;
            }
            let mut h = Hasher::new(LOCKED_DIGEST_DOMAIN);
            h.id(&crates.key(at).0).id(&source).update(&files);
            h.update(&(deps.len() as u64).to_le_bytes());
            for dep in deps.iter().copied() {
                h.update(&digests[dep]);
            }
            digests[at] = h.finish().0;
            state[at] = DONE;
        }
    }
    Ok(digests)
}

impl BodyIds for RunIds<'_> {
    fn body_check(&self, item: ItemKey) -> Option<BodyId<'_>> {
        let row = self.crate_of(item)?;
        match row.grain {
            CrateGrain::Toolchain => {
                let mut h = Hasher::new(TOOLCHAIN_BODY_DOMAIN);
                h.id(&self.producers).update(&self.toolchain.0).id(&row.key.0).update(&item.0.to_be_bytes());
                Some(BodyId { key: h.finish(), reads: core::slice::from_ref(&self.toolchain_read) })
            }
            CrateGrain::Locked(read) => {
                let mut h = Hasher::new(LOCKED_BODY_DOMAIN);
                h.id(&self.producers).id(&row.key.0).update(&item.0.to_be_bytes());
                // The crate's read is in today's table (`new` files every locked crate's read
                // there, one digest a fact): borrowed from it, the table's own.
                let read = self.today_row(&read.fact)?;
                Some(BodyId { key: h.finish(), reads: core::slice::from_ref(read) })
            }
            // No record valid today: no reads (the walk's `Unkeyed::NoRecord`). Else ONE read,
            // the check's proof, read by need (the gate read it already for a stepped body); one
            // that cannot be read is no id (the gate said why first, `NotRunnable::Unread`).
            CrateGrain::Workspace(_) => {
                let body = self.body(item)?;
                if matches!(body.checked, Checked::Absent(_)) {
                    return Some(BodyId { key: body.key, reads: &[] });
                }
                let proof = self.proof_of(body.check).ok()?;
                Some(BodyId { key: body.key, reads: core::slice::from_ref(&proof.read) })
            }
        }
    }

    /// A crate's fact from today's table; a workspace body's proof fact from its slot, its
    /// digest today the fold over today's digests ([`Proof::today`]), read by need.
    fn today(&self, fact: &[u8; 32]) -> Option<[u8; 32]> {
        if let Some(read) = self.today_row(fact) {
            return Some(read.digest);
        }
        let at = self.slots.binary_search_by(|slot| slot.fact.cmp(fact)).ok()?;
        self.settled(&self.slots[at]).ok().map(|proof| proof.today)
    }

    fn crate_fact(&self, item: ItemKey) -> Option<Read> {
        match self.crate_of(item)?.grain {
            CrateGrain::Toolchain => Some(self.toolchain_read),
            CrateGrain::Locked(read) => Some(read),
            CrateGrain::Workspace(read) => read,
        }
    }

    /// A crate of the pinned toolchain's shelf is the toolchain's (store/roots.rs
    /// `Rooted<Toolchain>`); a locked or workspace crate, and an item of no crate, the program's.
    fn class(&self, item: ItemKey) -> CodeClass {
        match self.crate_of(item).map(|row| row.grain) {
            Some(CrateGrain::Toolchain) => CodeClass::Toolchain,
            _ => CodeClass::Program,
        }
    }
}
