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
//! 3. WORKSPACE (a crate whose root is in the checkout, tree 0): key = the drive's `BodyCheck`
//!    task key, `task_key(tool, name, body_source(text))` (body_types.rs `body_source`, CALLED,
//!    so lane R8's fix to it flows through); reads = the read list of the `BodyCheck` record
//!    valid today (found through its shape and today's digests, as the request machine finds
//!    it), each read with the digest its check saw. No record valid today: `reads: &[]` (the
//!    walk makes that instance unkeyed, body_ids.rs `Unkeyed::NoRecord`), and [`RunIds::missing`]
//!    says which of the three steps found nothing; a key is never invented.
//!
//! A drive read `(Name, out)` is the 32-byte fact [`fact_of`] (the name hashed) with the out as
//! its digest. [`BodyIds::crate_fact`] of a workspace crate's item is its `CrateScope` read
//! today (the crate's exports); of a toolchain item the toolchain fact; of a locked item the
//! crate's fact.
//!
//! THE TABLE IS A RECORD (the FORM below): written once per WALK run at the request's
//! certificate ([`table_key`]: the certificate, the toolchain and every kind's tool), through the
//! same store put the answer's pages, root and certificate take (main.rs `Effect::Put`), and read
//! in place on a certificate hit ([`RunIds::from_record`], via [`on_hit`]), lazily, when the
//! parent first needs a body id. A hit with no table answers in full ([`OnHit::AnswerInFull`]).
//! A forward walk knows only what it reached; what it did not reach is carried from the last
//! answer's table ([`RunIds::new`], `previous`).
//!
//! COST: [`RunIds::new`] reads the crate list (one row a crate, O(crates + dependency edges)
//! hashes for the locked closure digests) and, per workspace body the walk reached, at most two
//! records of the store (shape, then value); one it did not reach is a copy of `previous`'s
//! row. It solves no names and reads no body. A hit costs one get and one pass over the bytes
//! that checks them. Every answer after it is a binary search over the record's bytes:
//! O(log crates) for the grain, O(log bodies) for a workspace body, O(log facts) for `today`.
//!
//! THE JOIN (checker key to the body's `BodyCheck` name, its entry's text digest and its file,
//! a [`WorkspaceBody`]) is the answer's own: `FileTypes::check` (body_types.rs) files the checker
//! key of every fn its check covered (covered.rs `each_covered`) into the record, whose payload
//! names them sorted (work.rs `body_payload`), and [`workspace_of`] reads them from the units
//! down. No second join here (rule 30). A fn a macro wrote inside a body is keyed by the
//! covering body's check; one a macro wrote at module level has no row (`workspace_of`'s doc).
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

use blaze_drive::key::{ABSENT, task_key, value_key_with_cells};
use blaze_drive::record::{Stored, open};
use blaze_drive::{Cell, CellRead, Key, Kind, Name, Tool, WireError};
use blaze_id::CrateItemKey;
use blaze_ids::{ContentId, Hasher};
use blaze_ladder::body_ids::{BodyId, BodyIds, Read};
use blaze_own::{AVec, Alloc, Vec};
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
        if self.member(at) {
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

/// The request machine's answer, after the gate: `Request::settled_input`, the drive's one
/// accessor for what a name is now (made or kept this run, else the last answer's row).
///
/// WHAT IT COVERS, measured by reading request.rs (not run): `settled_input` answers a name this
/// run settled (`now`) or a row of an answer PAGE this run loaded (`old`). A request that stood on
/// its certificate loads no root and no page (request.rs `certified`), so it knows the ask's
/// facts and nothing else; a forward walk knows the names it touched and the pages it read. A
/// body whose rows were not touched is then `Missing::NotToday`, and the parent refuses it: a
/// refusal, never a guess. [`for_run`] refuses a certified request whole instead
/// ([`RunIdsError::StoodUnread`]), since every body of it would be.
impl Today for blaze_drive::Request<'_> {
    fn out(&self, name: Name) -> Option<ContentId> {
        self.settled_input(name).map(|input| input.out)
    }
    /// `Request::cell_now`: NOT IN blaze-drive YET. The accessor's exact text is in lane I's
    /// P1 report (an addition to request.rs, which lane RC owns); until it is applied this line
    /// does not compile.
    fn cell(&self, name: Name, cell: Cell) -> Option<ContentId> {
        self.cell_now(name, cell)
    }
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

/// Whether the request at its `Effect::StartProgram` WALKED (it settled a `UnitGraph` this run
/// or read its row) or STOOD on its certificate (request.rs `certified`: no root, no page, so
/// `settled_input` knows only the ask's facts).
pub(crate) fn walked(req: &blaze_drive::Request<'_>) -> bool {
    req.settled_input(Name::unit_graph()).is_some()
}

/// What [`workspace_of`] is handed of one record of the answer.
pub(crate) enum Seen<'b> {
    /// One part the record lists.
    Part(blaze_drive::Part),
    /// Its payload (handed once, after its parts).
    Payload(&'b [u8]),
}

/// The answer's records as the walk reads them, sans-IO (rule 14): the request's settled rows
/// over the store ([`Standing`]), or a test's.
pub(crate) trait Answer {
    /// Hands `seen` each part, then the payload, of the record `name` stands at today;
    /// `Ok(false)`: the command does not know `name`'s record today (a forward walk did not
    /// reach it).
    fn open(&mut self, cx: &Cx, name: Name, seen: &mut dyn FnMut(Seen<'_>) -> Result<(), RunIdsError<'static>>) -> Result<bool, RunIdsError<'static>>;
}

/// The request's answer: a name's row (`Request::settled_input`, `at`) and its record in the
/// store, opened whole.
pub(crate) struct Standing<'q, 'r, 's, 'm> {
    pub(crate) req: &'q blaze_drive::Request<'r>,
    pub(crate) store: &'s mut dyn Store<'m>,
}

impl Answer for Standing<'_, '_, '_, '_> {
    fn open(&mut self, cx: &Cx, name: Name, seen: &mut dyn FnMut(Seen<'_>) -> Result<(), RunIdsError<'static>>) -> Result<bool, RunIdsError<'static>> {
        let Some(input) = self.req.settled_input(name) else { return Ok(false) };
        let held = self.store.get(cx, input.at).map_err(|why| RunIdsError::AnswerStore { name, why })?;
        // A row the request settled whose record the store does not hold: never a guess.
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
}

/// The workspace bodies of the answer, each a [`WorkspaceBody`], and the tree walked to find
/// them (the edges [`RunIds`] keeps, so the next forward walk crosses what it did not reach).
pub(crate) struct Walked<'a> {
    pub(crate) bodies: AVec<'a, WorkspaceBody>,
    /// `(parent, child)` sorted, one each; `(name, name)` marks `name` walked.
    pub(crate) edges: AVec<'a, (Name, Name)>,
}

/// The most records the walk crosses (rule 18): far above any checkout's units and files.
const MOST_WALKED: usize = 1 << 22;

/// THE JOIN (the coordinator's decision, 2026-11-17: "the BodyCheck record lists the checker
/// keys it covered"): each workspace body from the answer alone. From the root `Units` down:
/// each member's `Units`, its crate root's `FileItems`, each `mod x;`'s `FileItems` (a data
/// file's part is not followed), and each `FileItems` record's `BodyCheck` parts. A `BodyCheck`
/// record's payload names its file, its entry's text and every checker key its check covered
/// (work.rs `body_payload`, filed by body_types.rs `FileTypes::check`): one row per key, all
/// under that one check's name.
///
/// So a fn a macro wrote INSIDE a body (the launcher's `main` and the written `main` in it, a fn
/// a macro call in a body declares) is keyed by the covering body's `BodyCheck`. A fn a macro
/// wrote at MODULE level is in no outline entry, so no `BodyCheck` part names it and no check
/// covers it: it gets no row, [`RunIds::checked`] and `body_check` say `None` for it, and the
/// parent refuses it by name (never keyed by a guess).
///
/// A record the command does not know today (a forward walk did not reach it): a `Units` or
/// `FileItems` record's parts are `previous`'s edges (it stands as the last answer has it,
/// `RunIds::new`'s INVARIANT); a `BodyCheck`'s rows are `previous`'s rows of that check. Not in
/// `previous` either: [`RunIdsError::NotWalked`] names it. An empty payload is a filed failure's
/// record (work.rs `filed`): it covers nothing.
///
/// COST: one store get per record the walk knows today, breadth first, each level sorted once
/// (O(names log names)); a record it does not know is a binary search of `previous`'s edges.
pub(crate) fn workspace_of<'a>(cx: &Cx, answer: &mut dyn Answer, previous: Option<&RunIds<'_>>, alloc: Alloc<'a>) -> Result<Walked<'a>, RunIdsError<'static>> {
    let _cx = cx.span("run ids: the workspace bodies of the answer");
    let mut edges: AVec<'a, (Name, Name)> = Vec::new_in(alloc);
    let mut visited: AVec<'a, Name> = Vec::new_in(alloc);
    let mut frontier: AVec<'a, Name> = Vec::new_in(alloc);
    let mut next: AVec<'a, Name> = Vec::new_in(alloc);
    let mut checks: AVec<'a, Name> = Vec::new_in(alloc);
    frontier.push(Name::units());
    while !frontier.is_empty() {
        frontier.sort_unstable();
        frontier.dedup();
        frontier.retain(|name| visited.binary_search(name).is_err());
        if visited.len() + frontier.len() > MOST_WALKED {
            return Err(RunIdsError::TooManyWalked { most: MOST_WALKED });
        }
        for parent in frontier.iter().copied() {
            edges.push((parent, parent));
            let mut child = |name: Name, edges: &mut AVec<'a, (Name, Name)>| match name.kind {
                Kind::Units | Kind::FileItems => {
                    edges.push((parent, name));
                    next.push(name);
                }
                Kind::BodyCheck => {
                    edges.push((parent, name));
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
    for check in checks.iter().copied() {
        let known = answer.open(cx, check, &mut |seen| {
            let Seen::Payload(bytes) = seen else { return Ok(()) };
            if bytes.is_empty() {
                return Ok(());
            }
            let payload = crate::work::body_payload(bytes).map_err(|why| RunIdsError::BodyPayload { check, why })?;
            for item in payload.covered() {
                bodies.push(WorkspaceBody { item, check, text: payload.text, file: payload.file });
            }
            Ok(())
        })?;
        if known {
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
    Ok(Walked { bodies, edges })
}

/// The body ids of a `blaze run` that WALKED, at its `Effect::StartProgram` (main.rs), built and
/// written as the record of its certificate ([`table_key`]), so the next run that stands on the
/// certificate reads it ([`RunIds::from_record`]) with no lookup and no store read per body.
/// `previous` is the table of the last answer this walk went forward from (`new`'s doc):
/// `RunIds::from_record` at the last answer's certificate, `None` when there is no last answer
/// or its table is not held (then a body this walk did not reach is `NotToday` and the parent
/// refuses it by name; the next walk from the units down fills it).
///
/// The workspace bodies are the answer's own ([`workspace_of`]: each `BodyCheck` record lists
/// the checker keys it covered), so the caller names none.
///
/// COST: [`workspace_of`]'s (one get a record the walk knows today), [`RunIds::new`]'s (one row
/// a crate, at most two store reads a workspace body this walk reached, a copy of `previous`'s
/// row for one it did not) and one put of the table (about 192 bytes a body, 72 an edge and 64
/// a read). It runs on a walk only: a run that stands on its certificate reads the table
/// instead, lazily.
///
/// Refused whole with [`RunIdsError::StoodUnread`] when the request stood (see [`walked`]).
#[expect(clippy::too_many_arguments, reason = "the StartProgram site's edge: each argument one thing it holds")]
pub(crate) fn for_run<'n, 'a>(
    cx: &Cx,
    req: &blaze_drive::Request<'_>,
    toolchain: ToolchainId,
    crates: &CrateList<'n>,
    tools: &[Tool; blaze_drive::key::KINDS],
    previous: Option<&RunIds<'_>>,
    store: &mut dyn Store<'_>,
    alloc: Alloc<'a>,
) -> Result<RunIds<'a>, RunIdsError<'n>> {
    if !walked(req) {
        return Err(RunIdsError::StoodUnread);
    }
    let walk = workspace_of(cx, &mut Standing { req, store: &mut *store }, previous, alloc)?;
    let ids = RunIds::new(cx, toolchain, crates, tools, &walk.bodies, &walk.edges, req, previous, store, alloc)?;
    ids.put(cx, store, req.certificate(), tools, alloc)?;
    Ok(ids)
}

/// What a run that STOOD on its certificate does when its parent first needs a body id: the
/// table of the certificate, or that the request must answer in full.
pub(crate) enum OnHit<'a> {
    Ids(RunIds<'a>),
    /// The store holds no table for the certificate (dropped, or never written, as by a blaze
    /// before tables): never a guess. The command answers in full: it drops this request
    /// without starting the program and asks again cold, its leaf table and last answer left
    /// out (main.rs `request`: `leaves = Leaves::EMPTY`, so `cold` and the layout is laid, and
    /// `last = None`, so request.rs `start` goes straight to `begin` with no certificate get and
    /// `begin` walks from the units down). Every record stands in the store, so no task runs
    /// again; every name settles, and that walk's `for_run` writes the table. No blaze-drive
    /// change is needed.
    AnswerInFull,
}

/// [`OnHit`] for the request `req` that stood: the closure the parent calls on its first
/// unresolved need. One store get, then the table read in place.
pub(crate) fn on_hit<'m: 'a, 'a>(
    cx: &Cx,
    req: &blaze_drive::Request<'_>,
    toolchain: ToolchainId,
    tools: &[Tool; blaze_drive::key::KINDS],
    store: &mut dyn Store<'m>,
    alloc: Alloc<'a>,
) -> Result<OnHit<'a>, RunIdsError<'static>> {
    Ok(match RunIds::from_record(cx, store, req.certificate(), toolchain, tools, alloc)? {
        Some(ids) => OnHit::Ids(ids),
        None => OnHit::AnswerInFull,
    })
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

/// What the store holds of a workspace body's check, today (the gate before the start must have
/// left `Clean`; anything else is a typed stop of the parent, never a second check).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Checked {
    /// Its record valid today holds no finding that is an error (main.rs `every_rule`) and none
    /// saying the check did not finish.
    Clean,
    /// No record valid today: [`Missing`] says which step found nothing.
    Absent(Missing),
    /// Its record valid today holds `errors` findings that are errors of the program; `first`
    /// is the first one's rule.
    Errors { errors: u32, first: u16 },
    /// Its record valid today says the check did not finish: `rule` is
    /// `body_types::TYPES_NOT_CHECKED` (or `BORROWS_NOT_CHECKED` once borrows are errors), or
    /// work.rs `NOT_RUN` (an input it reads could not be made).
    Incomplete { rule: u16 },
}

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
    /// Its reads in [`RunIds::reads`]: (start, length); none unless `checked` is a record's.
    at: u32,
    len: u32,
    checked: Checked,
    /// The [`WorkspaceBody`] it was made from: its check's name, text and file, so a later walk
    /// that does not reach the check names the body again from this row.
    check: Name,
    text: ContentId,
    file: Name,
}

/// The verdict a whole `BodyCheck` record's findings give (see [`Checked`]). The record says it
/// by its findings alone: a rule of the not-finished kind, or a rule that is an error.
fn verdict(whole: &blaze_drive::record::WholeView<'_>) -> Checked {
    let mut errors = 0u32;
    let mut first = 0u16;
    for finding in whole.findings() {
        let rule = finding.rule;
        let unfinished = rule == crate::body_types::TYPES_NOT_CHECKED
            || rule == crate::work::NOT_RUN
            || (crate::body_types::BORROWS_ARE_ERRORS && rule == crate::body_types::BORROWS_NOT_CHECKED);
        if unfinished {
            return Checked::Incomplete { rule };
        }
        if crate::every_rule(rule) {
            if errors == 0 {
                first = rule;
            }
            errors = errors.saturating_add(1);
        }
    }
    if errors == 0 { Checked::Clean } else { Checked::Errors { errors, first } }
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
    /// The caller's name of `item`'s check is not a `BodyCheck` name.
    NotABodyCheck { item: ItemKey, name: Name },
    /// The facts today are not sorted by name, one each.
    TodayUnsorted { at: usize },
    /// The request holds no `UnitGraph` row, so no row of its answer: it stood on its
    /// certificate ([`for_run`]).
    StoodUnread,
    /// A record of `item`'s check does not read as the drive's.
    Record { item: ItemKey, why: WireError },
    /// The value record found for `item` is a shape.
    ValueIsShape { item: ItemKey },
    /// The store could not be read for `item`.
    Store { item: ItemKey, why: CliError },
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
    /// The `BodyCheck` record of `check` holds a payload not of work.rs's form.
    BodyPayload { check: Name, why: crate::work::BodyPayloadError },
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
            RunIdsError::NotABodyCheck { item, name } => write!(f, "the body {:#034x} is named by {name}, which is no `BodyCheck` {rule}", item.0),
            RunIdsError::TodayUnsorted { at } => write!(f, "today's facts are not sorted by name, one each, at row {at} {rule}"),
            RunIdsError::StoodUnread => write!(
                f,
                "the request stood on its certificate and holds none of its answer's rows, so no workspace body has a key: blaze-drive has no read of an answer row by name yet {rule}"
            ),
            RunIdsError::Record { item, why } => write!(f, "a `BodyCheck` record of the body {:#034x}: {why} {rule}", item.0),
            RunIdsError::ValueIsShape { item } => write!(f, "the value key of the body {:#034x}'s check holds a shape, not its record {rule}", item.0),
            RunIdsError::Store { item, why } => write!(f, "the store, reading the check of the body {:#034x}: {why} {rule}", item.0),
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
            RunIdsError::BodyPayload { check, why } => write!(f, "the record of {check}: {why} (blaze-cli run_ids.rs `workspace_of`)"),
        }
    }
}

impl core::error::Error for RunIdsError<'_> {}

// --- The table as a record (the coordinator's decision, 2026-11-17: "THE TABLE IS A RECORD") ---
//
// FORM (little-endian, every row fixed-width, every table sorted and unique, nothing of the
// machine: two builders of one certificate write equal bytes, rule 25):
//   header (HEADER bytes): magic `BZRI`, version u32, then u32 counts of crate rows, body rows,
//     reads, today rows and edges, 4 zero bytes, the toolchain id (32), the check's producers (32);
//   crate rows (CRATE_ROW each), sorted by prefix: prefix 8, crate key 32, grain tag 1, 7 zero,
//     the grain's read: fact 32, digest 32 (zero for the toolchain and a workspace crate with none);
//   body rows (BODY_ROW each), sorted by checker key: item 16 (big-endian, so byte order is key
//     order), BodyCheck key 32, reads start u32, reads length u32, verdict tag 1, a name's kind 1,
//     a rule u16, an error count u32, a name's subject 32 (`Missing::NotToday`'s name), then the
//     body's check's subject 32, its text 32 and its file's subject 32 ([`WorkspaceBody`]);
//   edges (EDGE each), sorted by (parent, child) name, one each: parent kind 1, child kind 1,
//     6 zero, parent subject 32, child subject 32 (the walked tree, [`workspace_of`]);
//   reads (READ each): fact 32, digest 32, each body's run sorted by fact;
//   today (READ each), sorted by fact, one each.
// Read checks every count, size, order, tag and range ONCE ([`RunIds::read`], rule 74); every
// lookup after it is a binary search over the rows in place.

const TABLE_MAGIC: [u8; 4] = *b"BZRI";
/// 1: the first form. 2: a body row names its check, text and file; the walked tree's edges.
/// The version is in the key's domain too, so a table of another form is never found.
const TABLE_VERSION: u32 = 2;
const TABLE_KEY_DOMAIN: &str = "blaze-cli run ids table key 2";
const HEADER: usize = 96;
const CRATE_ROW: usize = 112;
const BODY_ROW: usize = 192;
const EDGE: usize = 72;
const READ: usize = 64;
/// The most rows of each table a record may say it has (rule 18): far above any workspace
/// (reth: about 300 crates, tens of thousands of bodies), far below what a u32 offset can hold.
const MOST_ROWS: u32 = 1 << 24;

const GRAIN_TOOLCHAIN: u8 = 0;
const GRAIN_LOCKED: u8 = 1;
const GRAIN_WORKSPACE_NONE: u8 = 2;
const GRAIN_WORKSPACE: u8 = 3;

const CHECKED_CLEAN: u8 = 0;
const CHECKED_ERRORS: u8 = 1;
const CHECKED_INCOMPLETE: u8 = 2;
const ABSENT_NO_RECORD: u8 = 3;
const ABSENT_NOT_TODAY: u8 = 4;
const ABSENT_NO_VALUE: u8 = 5;
const ABSENT_CELLS: u8 = 6;

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
    /// A body row's reads run past the reads table.
    ReadsOut { at: usize },
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
            TableError::ReadsOut { at } => write!(f, "body row {at}'s reads run past the reads table {rule}"),
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

fn put_crate_row(out: &mut AVec<'_, u8>, row: &CrateRow) {
    out.extend_from_slice(&row.prefix);
    out.extend_from_slice(&row.key.0.0);
    let (tag, read) = match row.grain {
        CrateGrain::Toolchain => (GRAIN_TOOLCHAIN, None),
        CrateGrain::Locked(read) => (GRAIN_LOCKED, Some(read)),
        CrateGrain::Workspace(None) => (GRAIN_WORKSPACE_NONE, None),
        CrateGrain::Workspace(Some(read)) => (GRAIN_WORKSPACE, Some(read)),
    };
    out.push(tag);
    out.extend_from_slice(&[0; 7]);
    let read = read.unwrap_or(Read { fact: [0; 32], digest: [0; 32] });
    out.extend_from_slice(&read.fact);
    out.extend_from_slice(&read.digest);
}

/// Crate row `b` (CRATE_ROW bytes), its tag checked at [`RunIds::read`].
fn crate_row(b: &[u8]) -> CrateRow {
    let mut prefix = [0u8; 8];
    prefix.copy_from_slice(&b[..8]);
    let key = CrateItemKey(ContentId(word_at(b, 8)));
    let read = Read { fact: word_at(b, 48), digest: word_at(b, 80) };
    let grain = match b[40] {
        GRAIN_TOOLCHAIN => CrateGrain::Toolchain,
        GRAIN_LOCKED => CrateGrain::Locked(read),
        GRAIN_WORKSPACE => CrateGrain::Workspace(Some(read)),
        _ => CrateGrain::Workspace(None),
    };
    CrateRow { prefix, key, grain }
}

fn put_body_row(out: &mut AVec<'_, u8>, row: &BodyRow) {
    out.extend_from_slice(&row.item.0.to_be_bytes());
    out.extend_from_slice(&row.key.0);
    out.extend_from_slice(&row.at.to_le_bytes());
    out.extend_from_slice(&row.len.to_le_bytes());
    let (tag, name, rule, errors) = match row.checked {
        Checked::Clean => (CHECKED_CLEAN, None, 0, 0),
        Checked::Errors { errors, first } => (CHECKED_ERRORS, None, first, errors),
        Checked::Incomplete { rule } => (CHECKED_INCOMPLETE, None, rule, 0),
        Checked::Absent(Missing::NoRecord) => (ABSENT_NO_RECORD, None, 0, 0),
        Checked::Absent(Missing::NotToday { name }) => (ABSENT_NOT_TODAY, Some(name), 0, 0),
        Checked::Absent(Missing::NoValue) => (ABSENT_NO_VALUE, None, 0, 0),
        Checked::Absent(Missing::Cells) => (ABSENT_CELLS, None, 0, 0),
    };
    out.push(tag);
    out.push(name.map_or(0, |name| name.kind as u8));
    out.extend_from_slice(&rule.to_le_bytes());
    out.extend_from_slice(&errors.to_le_bytes());
    out.extend_from_slice(&name.map_or([0; 32], |name| name.subject.0));
    out.extend_from_slice(&row.check.subject.0);
    out.extend_from_slice(&row.text.0);
    out.extend_from_slice(&row.file.subject.0);
}

/// One edge of the walked tree (the FORM): `parent`'s record lists `child` as a part; a row
/// `(name, name)` says `name`'s record was walked.
fn put_edge(out: &mut AVec<'_, u8>, (parent, child): (Name, Name)) {
    out.push(parent.kind as u8);
    out.push(child.kind as u8);
    out.extend_from_slice(&[0; 6]);
    out.extend_from_slice(&parent.subject.0);
    out.extend_from_slice(&child.subject.0);
}

/// Edge row `b` (EDGE bytes), its kinds checked at [`RunIds::read`].
fn edge(b: &[u8]) -> Option<(Name, Name)> {
    Some((Name::new(Kind::from_u8(b[0])?, ContentId(word_at(b, 8))), Name::new(Kind::from_u8(b[1])?, ContentId(word_at(b, 40)))))
}

fn body_item(b: &[u8]) -> ItemKey {
    let mut item = [0u8; 16];
    item.copy_from_slice(&b[..16]);
    ItemKey(u128::from_be_bytes(item))
}

/// Body row `b` (BODY_ROW bytes), its tag and name kind checked at [`RunIds::read`].
fn body_row(b: &[u8]) -> BodyRow {
    let (tag, kind, rule, errors) = (b[56], b[57], u16_at(b, 58), u32_at(b, 60));
    let checked = match tag {
        CHECKED_CLEAN => Checked::Clean,
        CHECKED_ERRORS => Checked::Errors { errors, first: rule },
        CHECKED_INCOMPLETE => Checked::Incomplete { rule },
        ABSENT_NOT_TODAY => match Kind::from_u8(kind) {
            Some(kind) => Checked::Absent(Missing::NotToday { name: Name::new(kind, ContentId(word_at(b, 64))) }),
            None => Checked::Absent(Missing::NoRecord),
        },
        ABSENT_NO_VALUE => Checked::Absent(Missing::NoValue),
        ABSENT_CELLS => Checked::Absent(Missing::Cells),
        _ => Checked::Absent(Missing::NoRecord),
    };
    BodyRow {
        item: body_item(b),
        key: ContentId(word_at(b, 16)),
        at: u32_at(b, 48),
        len: u32_at(b, 52),
        checked,
        check: Name::new(Kind::BodyCheck, ContentId(word_at(b, 96))),
        text: ContentId(word_at(b, 128)),
        file: Name::new(Kind::FileItems, ContentId(word_at(b, 160))),
    }
}

fn read_at(b: &[u8]) -> Read {
    Read { fact: word_at(b, 0), digest: word_at(b, 32) }
}

/// The command's body ids for `blaze run` (the module doc). Its crate and body rows are the
/// table record's bytes ([`RunIds::read`]: the store's, in place, on a certificate hit; the
/// builder's, on a walk); its reads and today's table are one copy each in the caller's
/// allocator, since a [`BodyId`] hands out `&[Read]` and blaze-run's `Read` has no in-place
/// view of bytes (no `unsafe` here; the fix is a `Read::slice_of` beside blaze-id's
/// `ItemKey::slice_of`, blaze-run's to add).
pub(crate) struct RunIds<'a> {
    toolchain: ToolchainId,
    /// The toolchain fact with today's digest, the toolchain's id.
    toolchain_read: Read,
    /// The check's producers (`CHECK_KINDS`' tools), in every toolchain and locked body key.
    producers: ContentId,
    /// CRATE_ROW-wide rows, sorted by prefix, one each.
    crates: &'a [u8],
    /// BODY_ROW-wide rows, sorted by item, one each.
    bodies: &'a [u8],
    /// EDGE-wide rows, sorted by (parent, child), one each: the walked tree ([`workspace_of`]).
    edges: &'a [u8],
    reads: &'a [Read],
    /// Every fact's digest today, sorted by fact, one each.
    today: &'a [Read],
}

impl<'a> RunIds<'a> {
    /// The body ids of a command: `toolchain` the cut it opened, `crates` the `UnitGraph`
    /// record's crate list, `tools` its kinds' tools (home.rs `tools`), `workspace` the bodies
    /// of the checkout and `edges` the tree they were found by ([`workspace_of`]'s), `today`
    /// every drive read's out today (sorted by name), and `store` where its `BodyCheck` records
    /// are. Every table is in `alloc`.
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
        edges: &[(Name, Name)],
        today: &dyn Today,
        previous: Option<&RunIds<'_>>,
        store: &mut dyn Store<'_>,
        alloc: Alloc<'a>,
    ) -> Result<RunIds<'a>, RunIdsError<'n>> {
        let _cx = cx.span("run ids: the body ids of the program's crates");
        let today_of = |name: Name| today.out(name);
        let producers = {
            let mut h = Hasher::new(PRODUCERS_DOMAIN);
            for kind in CHECK_KINDS {
                h.update(&[kind as u8]).id(&tools[kind.index()].0);
            }
            h.finish()
        };
        let toolchain_read = Read { fact: toolchain_fact(), digest: toolchain.0 };
        let mut facts: AVec<'a, Read> = Vec::new_in(alloc);
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
                    let scope = crate::names::name_of(key);
                    let read = today_of(scope).map(|out| Read { fact: fact_of(scope), digest: out.0 });
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

        // Each workspace body: its task key, and the read list of its record valid today.
        let tool = tools[Kind::BodyCheck.index()];
        let mut bodies: AVec<'a, BodyRow> = Vec::with_capacity_in(workspace.len(), alloc);
        let mut reads: AVec<'a, Read> = Vec::new_in(alloc);
        let mut names: AVec<'a, Name> = Vec::new_in(alloc);
        let mut seen: AVec<'a, (Name, ContentId)> = Vec::new_in(alloc);
        let mut cells: AVec<'a, CellRead> = Vec::new_in(alloc);
        for body in workspace {
            let item = body.item;
            let crate_row = rows.binary_search_by_key(&prefix_of(item), |row| row.prefix).ok().map(|at| rows[at]);
            if !matches!(crate_row, Some(CrateRow { grain: CrateGrain::Workspace(_), .. })) {
                return Err(RunIdsError::NotWorkspace { item });
            }
            if body.check.kind != Kind::BodyCheck {
                return Err(RunIdsError::NotABodyCheck { item, name: body.check });
            }
            // The key check files the record under (body_types.rs `body_check_key`, the one
            // derivation): the task key with today's crate list and the file's owners folded in.
            // An out the command does not know is not guessed `ABSENT`: the body is not today's,
            // and its row keeps the bare task key (it has no reads, so no id is made from it).
            let graph_name = Name::unit_graph();
            let owners_name = blaze_drive::key::owners_of(body.file);
            let start = reads.len();
            let (task, checked) = match (today_of(graph_name), today_of(owners_name)) {
                (Some(graph), Some(owners)) => {
                    let task = crate::body_types::body_check_key(tool, body.check, body.text, body.file, graph, owners);
                    (task, record_reads(cx, store, item, task, today, &mut names, &mut seen, &mut cells)?)
                }
                (None, _) => (task_key(tool, body.check, &crate::body_types::body_source(body.text)), Checked::Absent(Missing::NotToday { name: graph_name })),
                (Some(_), None) => (task_key(tool, body.check, &crate::body_types::body_source(body.text)), Checked::Absent(Missing::NotToday { name: owners_name })),
            };
            // Not reached this walk: the last answer's row stands, key, reads and verdict (`new`'s
            // doc, `previous`), with the digests it carried as today's.
            if let (Checked::Absent(Missing::NotToday { .. }), Some(previous)) = (checked, previous)
                && let Some(row) = previous.body(item)
            {
                let (from, len) = (row.at as usize, row.len as usize);
                for read in &previous.reads[from..from + len] {
                    reads.push(*read);
                    facts.push(*read);
                }
                bodies.push(BodyRow { item, key: row.key, at: start as u32, len: len as u32, checked: row.checked, check: body.check, text: body.text, file: body.file });
                continue;
            }
            if !matches!(checked, Checked::Absent(_)) {
                for (name, saw) in seen.iter().copied() {
                    let fact = fact_of(name);
                    reads.push(Read { fact, digest: saw.0 });
                    // Today's digest of the fact, when the command has it: the walk compares.
                    if let Some(out) = today_of(name) {
                        facts.push(Read { fact, digest: out.0 });
                    }
                }
                // A cell read is a fact of its own ([`cell_fact`]): what the cell held when the
                // record was made, against what it holds today (`Today::cell`).
                for read in cells.iter().copied() {
                    let fact = cell_fact(read.name, read.cell);
                    reads.push(Read { fact, digest: read.saw.0 });
                    if let Some(now) = today.cell(read.name, read.cell) {
                        facts.push(Read { fact, digest: now.0 });
                    }
                }
                // Sorted by fact, one each (body_ids.rs `BodyId::reads`).
                reads[start..].sort_unstable();
                let mut kept = start;
                for i in start..reads.len() {
                    if kept == start || reads[kept - 1].fact != reads[i].fact {
                        reads[kept] = reads[i];
                        kept += 1;
                    }
                }
                reads.truncate(kept);
            }
            bodies.push(BodyRow { item, key: task.0, at: start as u32, len: (reads.len() - start) as u32, checked, check: body.check, text: body.text, file: body.file });
        }
        bodies.sort_unstable_by_key(|row| row.item);
        if let Some(pair) = bodies.windows(2).find(|pair| pair[0].item == pair[1].item) {
            return Err(RunIdsError::TwoRows { item: pair[0].item });
        }

        // Today's table: one digest a fact (each is made by one function of its name, so two
        // rows of one fact carry one digest; the first is kept).
        // Two digests of one fact (a body carried from `previous` read a fact the walk moved,
        // which the forward walk would have re-settled it for) is refused by name, never one
        // picked.
        facts.sort_unstable();
        facts.dedup();
        if let Some(pair) = facts.windows(2).find(|pair| pair[0].fact == pair[1].fact) {
            return Err(RunIdsError::TwoDigests { fact: pair[0].fact });
        }

        // Laid out as the record's rows (the one representation every lookup reads). The reads
        // are laid again in the bodies' sorted order, so the caller's order of `workspace`
        // reaches no byte (rule 25).
        let mut crate_bytes: AVec<'a, u8> = Vec::with_capacity_in(rows.len() * CRATE_ROW, alloc);
        for row in rows.iter() {
            put_crate_row(&mut crate_bytes, row);
        }
        let mut laid: AVec<'a, Read> = Vec::with_capacity_in(reads.len(), alloc);
        let mut body_bytes: AVec<'a, u8> = Vec::with_capacity_in(bodies.len() * BODY_ROW, alloc);
        for row in bodies.iter() {
            let (from, len) = (row.at as usize, row.len as usize);
            let at = laid.len() as u32;
            laid.extend_from_slice(&reads[from..from + len]);
            put_body_row(&mut body_bytes, &BodyRow { at, ..*row });
        }
        // The walked tree, sorted and one each whatever order the walk handed it in.
        let mut tree: AVec<'a, (Name, Name)> = Vec::with_capacity_in(edges.len(), alloc);
        tree.extend_from_slice(edges);
        tree.sort_unstable();
        tree.dedup();
        let mut edge_bytes: AVec<'a, u8> = Vec::with_capacity_in(tree.len() * EDGE, alloc);
        for pair in tree.iter().copied() {
            put_edge(&mut edge_bytes, pair);
        }
        Ok(RunIds {
            toolchain,
            toolchain_read,
            producers,
            crates: crate_bytes.leak(),
            bodies: body_bytes.leak(),
            edges: edge_bytes.leak(),
            reads: laid.leak(),
            today: facts.leak(),
        })
    }

    /// The table as its record's bytes (the FORM above), in `alloc`. Equal tables write equal
    /// bytes: every row and every arena is in its sorted order.
    pub(crate) fn write<'b>(&self, alloc: Alloc<'b>) -> AVec<'b, u8> {
        let len = HEADER + self.crates.len() + self.bodies.len() + self.edges.len() + (self.reads.len() + self.today.len()) * READ;
        let mut out: AVec<'b, u8> = Vec::with_capacity_in(len, alloc);
        out.extend_from_slice(&TABLE_MAGIC);
        out.extend_from_slice(&TABLE_VERSION.to_le_bytes());
        for count in [self.crates.len() / CRATE_ROW, self.bodies.len() / BODY_ROW, self.reads.len(), self.today.len(), self.edges.len() / EDGE] {
            // Within MOST_ROWS: the builder's tables are bounded by the command's crates, bodies
            // and their reads; a table past it is refused on read, never truncated here.
            out.extend_from_slice(&(count as u32).to_le_bytes());
        }
        out.extend_from_slice(&[0; 4]);
        out.extend_from_slice(&self.toolchain.0);
        out.extend_from_slice(&self.producers.0);
        out.extend_from_slice(self.crates);
        out.extend_from_slice(self.bodies);
        out.extend_from_slice(self.edges);
        for read in self.reads.iter().chain(self.today.iter()) {
            out.extend_from_slice(&read.fact);
            out.extend_from_slice(&read.digest);
        }
        out
    }

    /// The table a record's `bytes` hold, its crate and body rows read in place, its reads and
    /// today's table copied once into `alloc` (the struct's doc). Every count, size, order, tag
    /// and range is checked here, once (rule 74); nothing after it checks again.
    pub(crate) fn read(bytes: &'a [u8], alloc: Alloc<'a>) -> Result<RunIds<'a>, TableError> {
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
        let mut counts = [0u32; 5];
        for (i, what) in ["crate rows", "body rows", "reads", "today rows", "edges"].into_iter().enumerate() {
            counts[i] = u32_at(bytes, 8 + 4 * i);
            if counts[i] > MOST_ROWS {
                return Err(TableError::TooMany { what, count: counts[i] });
            }
        }
        if bytes[28..32] != [0; 4] {
            return Err(TableError::Padding);
        }
        let [n_crates, n_bodies, n_reads, n_today, n_edges] = counts.map(|c| c as usize);
        let expected = (HEADER + n_crates * CRATE_ROW + n_bodies * BODY_ROW + n_edges * EDGE + (n_reads + n_today) * READ) as u64;
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
        let edges = &bytes[at..at + n_edges * EDGE];
        at += edges.len();
        let mut last: Option<(Name, Name)> = None;
        for (i, row) in edges.chunks_exact(EDGE).enumerate() {
            let Some(pair) = edge(row).filter(|_| row[2..8] == [0; 6]) else {
                return Err(TableError::Tag { what: "edge", at: i, tag: row[0] });
            };
            if last.is_some_and(|last| last >= pair) {
                return Err(TableError::Unsorted { what: "edges", at: i });
            }
            last = Some(pair);
        }
        let mut reads: AVec<'a, Read> = Vec::with_capacity_in(n_reads, alloc);
        for row in bytes[at..at + n_reads * READ].chunks_exact(READ) {
            reads.push(read_at(row));
        }
        at += n_reads * READ;
        let mut today: AVec<'a, Read> = Vec::with_capacity_in(n_today, alloc);
        for row in bytes[at..].chunks_exact(READ) {
            today.push(read_at(row));
        }

        for (i, row) in crates.chunks_exact(CRATE_ROW).enumerate() {
            if row[40] > GRAIN_WORKSPACE || row[41..48] != [0; 7] {
                return Err(TableError::Tag { what: "crate row", at: i, tag: row[40] });
            }
            if i > 0 && crates[(i - 1) * CRATE_ROW..(i - 1) * CRATE_ROW + 8] >= row[..8] {
                return Err(TableError::Unsorted { what: "crate rows", at: i });
            }
        }
        for (i, row) in bodies.chunks_exact(BODY_ROW).enumerate() {
            if row[56] > ABSENT_CELLS {
                return Err(TableError::Tag { what: "body row", at: i, tag: row[56] });
            }
            if row[56] == ABSENT_NOT_TODAY && Kind::from_u8(row[57]).is_none() {
                return Err(TableError::NameKind { at: i, byte: row[57] });
            }
            if i > 0 && body_item(&bodies[(i - 1) * BODY_ROW..i * BODY_ROW]) >= body_item(row) {
                return Err(TableError::Unsorted { what: "body rows", at: i });
            }
            let (start, len) = (u32_at(row, 48) as usize, u32_at(row, 52) as usize);
            let Some(run) = start.checked_add(len).and_then(|end| reads.get(start..end)) else {
                return Err(TableError::ReadsOut { at: i });
            };
            if let Some(k) = run.windows(2).position(|pair| pair[0].fact >= pair[1].fact) {
                return Err(TableError::Unsorted { what: "reads of a body", at: start + k + 1 });
            }
        }
        if let Some(k) = today.windows(2).position(|pair| pair[0].fact >= pair[1].fact) {
            return Err(TableError::Unsorted { what: "today rows", at: k + 1 });
        }
        let toolchain_read = Read { fact: toolchain_fact(), digest: toolchain.0 };
        Ok(RunIds { toolchain, toolchain_read, producers, crates, bodies, edges, reads: reads.leak(), today: today.leak() })
    }

    /// The parts the walk followed from `parent`'s record ([`workspace_of`]), when the walk that
    /// made this table crossed it; `None`: it did not.
    pub(crate) fn children(&self, parent: Name) -> Option<impl Iterator<Item = Name> + '_> {
        let n = self.edges.len() / EDGE;
        // Checked at [`RunIds::read`] or laid by `new`: every row decodes.
        let row = move |i: usize| edge(&self.edges[i * EDGE..(i + 1) * EDGE]).unwrap_or((parent, parent));
        let from = partition(n, |i| row(i).0 < parent);
        let to = partition(n, |i| row(i).0 <= parent);
        // The marker `(parent, parent)` says it was walked.
        (from..to).any(|i| row(i).1 == parent).then(|| (from..to).map(row).map(|pair| pair.1).filter(move |child| *child != parent))
    }

    /// Every workspace body row as the [`WorkspaceBody`] it was made from, in `alloc`.
    pub(crate) fn workspace_rows<'b>(&self, alloc: Alloc<'b>) -> AVec<'b, WorkspaceBody> {
        let mut rows: AVec<'b, WorkspaceBody> = Vec::with_capacity_in(self.bodies.len() / BODY_ROW, alloc);
        for b in self.bodies.chunks_exact(BODY_ROW) {
            let row = body_row(b);
            rows.push(WorkspaceBody { item: row.item, check: row.check, text: row.text, file: row.file });
        }
        rows
    }

    /// The table written as the record of the request whose certificate is `cert`
    /// ([`table_key`]), through the one store every answer record goes through (`Store::put`,
    /// as main.rs performs the request's `Effect::Put` of its pages, root and certificate). A
    /// key already holding equal bytes stands (rule 25); other bytes are the store's refusal.
    pub(crate) fn put(&self, cx: &Cx, store: &mut dyn Store<'_>, cert: Key, tools: &[Tool; blaze_drive::key::KINDS], scratch: Alloc<'_>) -> Result<(), RunIdsError<'static>> {
        let _cx = cx.span("run ids: write the body id table");
        let bytes = self.write(scratch);
        store.put(cx, table_key(cert, self.toolchain, tools), &bytes).map_err(|why| RunIdsError::TableStore { cert, why })
    }

    /// The table record of certificate `cert`, read in place ([`RunIds::read`]); `Ok(None)` when
    /// the store holds none (dropped, or never written): the caller then answers in full
    /// ([`OnHit::AnswerInFull`]), never guesses.
    pub(crate) fn from_record<'m: 'a>(
        cx: &Cx,
        store: &mut dyn Store<'m>,
        cert: Key,
        toolchain: ToolchainId,
        tools: &[Tool; blaze_drive::key::KINDS],
        alloc: Alloc<'a>,
    ) -> Result<Option<RunIds<'a>>, RunIdsError<'static>> {
        let _cx = cx.span("run ids: read the body id table");
        let held = store.get(cx, table_key(cert, toolchain, tools)).map_err(|why| RunIdsError::TableStore { cert, why })?;
        let Some(held) = held else { return Ok(None) };
        let bytes: &'a [u8] = held.bytes;
        let ids = RunIds::read(bytes, alloc).map_err(RunIdsError::Table)?;
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
    /// parent refuses it by name like any body not `Clean`.
    pub(crate) fn checked(&self, item: ItemKey) -> Option<Checked> {
        self.body(item).map(|row| row.checked)
    }

    /// The crate row whose prefix is `item`'s: a binary search over the rows in place.
    fn crate_of(&self, item: ItemKey) -> Option<CrateRow> {
        let prefix = prefix_of(item);
        let n = self.crates.len() / CRATE_ROW;
        let at = search(n, |i| self.crates[i * CRATE_ROW..i * CRATE_ROW + 8].cmp(&prefix[..]))?;
        Some(crate_row(&self.crates[at * CRATE_ROW..(at + 1) * CRATE_ROW]))
    }

    /// The body row of `item`: a binary search over the rows in place.
    fn body(&self, item: ItemKey) -> Option<BodyRow> {
        let n = self.bodies.len() / BODY_ROW;
        let at = search(n, |i| body_item(&self.bodies[i * BODY_ROW..(i + 1) * BODY_ROW]).cmp(&item))?;
        Some(body_row(&self.bodies[at * BODY_ROW..(at + 1) * BODY_ROW]))
    }

    /// The crate row of the crate `key` (its prefix's row, when the row is that crate's).
    fn crate_by_key(&self, key: CrateItemKey) -> Option<CrateRow> {
        let prefix = key.prefix();
        let n = self.crates.len() / CRATE_ROW;
        let at = search(n, |i| self.crates[i * CRATE_ROW..i * CRATE_ROW + 8].cmp(&prefix[..]))?;
        let row = crate_row(&self.crates[at * CRATE_ROW..(at + 1) * CRATE_ROW]);
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

/// The record of the task `task` valid today: its read list into `seen` and its verdict
/// ([`verdict`]), or [`Checked::Absent`] saying why there is none. `names` is scratch for the
/// shape's names; `cells` gets the record's cell reads (each with what it saw).
#[expect(clippy::too_many_arguments, reason = "the task, today, and the three scratch lists one body's lookup fills")]
fn record_reads<'n>(
    cx: &Cx,
    store: &mut dyn Store<'_>,
    item: ItemKey,
    task: Key,
    today: &dyn Today,
    names: &mut AVec<'_, Name>,
    seen: &mut AVec<'_, (Name, ContentId)>,
    cells: &mut AVec<'_, CellRead>,
) -> Result<Checked, RunIdsError<'n>> {
    seen.clear();
    names.clear();
    cells.clear();
    let held = store.get(cx, task).map_err(|why| RunIdsError::Store { item, why })?;
    let Some(held) = held else { return Ok(Checked::Absent(Missing::NoRecord)) };
    match open(held.bytes).map_err(|why| RunIdsError::Record { item, why })? {
        // Valid by its key alone (a record with no reads is stored at its task key).
        Stored::Whole(whole) => {
            if whole.cells().len() > 0 {
                // A record at its task key reads no cell (it would be at a value key): another
                // writer's form, said, never read as valid.
                return Ok(Checked::Absent(Missing::Cells));
            }
            seen.extend(whole.reads());
            return Ok(verdict(&whole));
        }
        Stored::Shape(shape) => {
            names.extend(shape.names());
            // Today's digest of each cell the value key reads (`Today::cell`); one the request
            // did not look up this run is not guessed.
            for (name, cell) in shape.cells() {
                let Some(saw) = today.cell(name, cell) else { return Ok(Checked::Absent(Missing::Cells)) };
                cells.push(CellRead { name, cell, saw });
            }
        }
    }
    // The value key from today's digests, in the shape's order (sorted by name, cells by (name,
    // cell)), as the request machine computes it. A name the command has no digest for is not
    // guessed absent.
    for name in names.iter().copied() {
        let Some(out) = today.out(name) else { return Ok(Checked::Absent(Missing::NotToday { name })) };
        seen.push((name, out));
    }
    let value = value_key_with_cells(task, &seen[..], &cells[..]);
    seen.clear();
    cells.clear();
    let held = store.get(cx, value).map_err(|why| RunIdsError::Store { item, why })?;
    let Some(held) = held else { return Ok(Checked::Absent(Missing::NoValue)) };
    match open(held.bytes).map_err(|why| RunIdsError::Record { item, why })? {
        Stored::Whole(whole) => {
            seen.extend(whole.reads());
            cells.extend(whole.cells());
            Ok(verdict(&whole))
        }
        Stored::Shape(_) => Err(RunIdsError::ValueIsShape { item }),
    }
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
            CrateGrain::Workspace(_) => {
                let body = self.body(item)?;
                let (at, len) = (body.at as usize, body.len as usize);
                Some(BodyId { key: body.key, reads: &self.reads[at..at + len] })
            }
        }
    }

    fn today(&self, fact: &[u8; 32]) -> Option<[u8; 32]> {
        self.today_row(fact).map(|read| read.digest)
    }

    fn crate_fact(&self, item: ItemKey) -> Option<Read> {
        match self.crate_of(item)?.grain {
            CrateGrain::Toolchain => Some(self.toolchain_read),
            CrateGrain::Locked(read) => Some(read),
            CrateGrain::Workspace(read) => read,
        }
    }
}
