//! THE REQUEST'S FROZEN HEADS (plan rows A1's wiring and A3; runtime.md points 3, 4, 5): every
//! body of `blaze engine-check` is checked from frozen heads, in one round, lowering no head.
//!
//! ONE TABLE A REQUEST ([`RequestTable`]), made on the coordinator after the Heads stage's
//! merge, before the Bodies phase, and read only while it runs:
//! - `members`: every head task's cells (engine_heads.rs `heads_of`; a record payload is "want
//!   bytes, a u32 LE length, then the frozen bytes", one such run a cell), from the task's
//!   result when it was entered this request, else from its standing record in the store, read
//!   by the key the stored answer keeps (as engine_heads.rs `complete` reads a crate's tables);
//! - `stored`: the dependency heads this machine froze before (A3, "once a machine"), each a
//!   record under [`dependency_key`] (the dependency crate's names record key, the head's cell,
//!   the freezing tool), listed by the root [`root_name`] and taken only when its crate's names
//!   record key is today's (valid by its key: nothing of a dependency moves unless its names
//!   record does).
//!
//! ON FIRST READ, for a dependency's head (a crate with no head tasks: core, std, a registry
//! crate; or a member's head the table holds under another unit's keys): the body's worker
//! freezes it on the spot (front2 tables/seeds.rs `RequestSeed`) and publishes the bytes into
//! `fresh`, the request-wide map, so no other body freezes it again. The map only ever inserts:
//! a freeze is a function of its cell and the names (head.rs `freeze_seeded`: the cell's key
//! holds its crate's key, which hashes the crate's identity and its dependencies' keys, and the
//! names answer from that crate's names record), so two workers that freeze one cell at once
//! make equal bytes: the first is kept, the second counted (`twice`), other bytes refused.
//!
//! AT THE MERGE ([`RequestTable::merge`]): each dependency head frozen now is put under its key,
//! sorted by key (rule 25), and the root lists them with those it listed before, so the next
//! request, another checkout or a run after a reboot freezes none of them again.
//!
//! A body's read of a head is unchanged by where its bytes came from (engine_bodies.rs `widen`):
//! a member's head is its head task's cell, a dependency's its crate's names record whole.

use core::sync::atomic::{AtomicU32, Ordering};
use std::alloc::Global;

use blaze_core::Driver;
use blaze_drive::{Input, Key, Kind, Name, Tool};
use blaze_engine::{Planned, Stage, Tools, View};
use blaze_ids::ContentId;
use blaze_ladder::front2::heads::Want;
use blaze_ladder::front2::tables::head::{FrozenHead, KnownHeads, VERSION, WANT_BYTES, want_bytes};
use blaze_ladder::front2::tables::seeds::{FrozenTable, MOST_CELLS, RequestHeads, SeedError};
use blaze_world::crate_list::{CrateGraph, CrateList};
use bstd::cx::Cx;
use bstd::mem::Frame;

use crate::CliError;
use crate::engine_heads::HeadsStage;
use crate::engine_names::NamesStage;
use crate::store::{CommandStore, Sharing, Store as _};

/// Where a request's bodies take their heads from: the one seam (rule 78), each tested equal
/// (engine_check_tests.rs `engine_check_checks_bodies_from_frozen_heads`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum HeadsFrom {
    /// From the request's frozen heads ([`RequestTable`]): one round a body.
    Frozen,
    /// Each body lowers the heads it needs in its own store (the path before frozen heads).
    #[cfg_attr(not(test), expect(dead_code, reason = "the comparison's other side: only the tests take it (rule 78)"))]
    Lowered,
}

/// The domain of a dependency head's record key ([`dependency_key`]).
const DEPENDENCY_DOMAIN: &str = "blaze-cli engine dependency head 1";
/// The domain of a cell's key in the request's shared tables.
const CELL_DOMAIN: &str = "blaze-cli engine frozen head cell 1";
/// The domain of the root's name ([`root_name`]).
const ROOT_DOMAIN: &str = "blaze-cli engine dependency heads root 1";
/// The domain of the digest of the heads frozen in one request ([`SeedCounts::fresh`]).
const FRESH_DOMAIN: &str = "blaze-cli engine heads frozen now 1";
/// The bytes of a names record key at the front of a dependency head's record.
const KEY_BYTES: usize = 32;

/// The store key of dependency head `cell`'s record: from the dependency crate's names record
/// key (`names_at`: its text and what it depends on), the cell's bytes, the frozen form's
/// version and the freezing tool (the `Signature` kind's, home.rs `tools`). Known before the
/// freeze, of nothing of the machine (rules 21, 25).
pub(crate) fn dependency_key(names_at: Key, cell: Want, tool: Tool) -> Key {
    let mut h = blaze_ids::Hasher::new(DEPENDENCY_DOMAIN);
    h.update(&names_at.0.0).update(&want_bytes(cell)).update(&VERSION.to_le_bytes()).update(&tool.0.0);
    Key(h.finish())
}

/// The freezing tool: the `Signature` kind's (a head task's, engine_heads.rs).
pub(crate) fn tool_of(tools: &Tools) -> Tool {
    tools.0[Kind::Signature.index()]
}

/// The root that lists this machine's dependency heads: one a toolchain of tools (every
/// checkout shares it), never a checkout's.
pub(crate) fn root_name<'f>(tools: &Tools, frame: &'f Frame) -> blaze_own::String<&'f Frame> {
    use core::fmt::Write as _;
    let mut input: blaze_own::Vec<u8, &Frame> = blaze_own::Vec::new_in(frame);
    for tool in tools.0.iter() {
        input.extend_from_slice(&tool.0.0);
    }
    let id = ContentId::of(ROOT_DOMAIN, &input);
    let mut name = blaze_own::String::new_in(frame);
    name.push_str("engine-dependency-heads.");
    for byte in &id.0[..8] {
        let _ = write!(name, "{byte:02x}");
    }
    name
}

/// A cell's key in the request's shared tables: the digest of its bytes (`want_bytes`).
fn cell_key(cell: Want) -> ContentId {
    ContentId::of(CELL_DOMAIN, &want_bytes(cell))
}

/// One crate of the request's list, as the merge and the store's filter read it.
#[derive(Clone, Copy)]
pub(crate) struct CrateAt {
    /// Its key's first 8 bytes: what every item key of it begins with.
    prefix: [u8; 8],
    /// Its key whole: what a worker's names name its crate by (`RequestHeads::publish`).
    key: blaze_id::CrateItemKey,
    /// Its names record's key this request (`None`: its names failed).
    names_at: Option<Key>,
    member: bool,
}

/// Every crate of `list` by its key's prefix, sorted, with its names record's key.
pub(crate) fn crates_of(list: &CrateList<'static>, names: &NamesStage<'_>) -> std::vec::Vec<CrateAt, Global> {
    let mut crates = std::vec::Vec::with_capacity_in(CrateGraph::len(list), Global);
    for at in 0..CrateGraph::len(list) {
        let key = list.key(at);
        let names_at = names.input_of(blaze_drive::key::crate_scope_name(&key)).map(|input| input.at);
        crates.push(CrateAt { prefix: key.prefix(), key, names_at, member: list.member(at) });
    }
    crates.sort_unstable_by_key(|one| one.prefix);
    crates
}

/// The crate of the request's list whose key is `key` (the one a worker's names gave).
fn crate_keyed(crates: &[CrateAt], key: blaze_id::CrateItemKey) -> Option<CrateAt> {
    let from = crates.partition_point(|one| one.prefix < key.prefix());
    crates[from..].iter().take_while(|one| one.prefix == key.prefix()).find(|one| one.key == key).copied()
}

/// A head frozen this request, its bytes in the request's heap (lent to every worker).
#[derive(Clone, Copy)]
struct Fresh {
    cell: Want,
    /// Its crate as the freezing worker's names name it (`None`: they hold its item in none).
    krate: Option<blaze_id::CrateItemKey>,
    bytes: &'static [u8],
}

/// Why heads frozen this request were not put (rule 53: every head frozen now is put or
/// counted here, so `put + member + unlisted + no_names == frozen`).
#[derive(Clone, Copy, Default, Debug)]
pub(crate) struct NotPut {
    /// A member crate's head: its head task's cell, frozen at a seed because the head tasks'
    /// table did not hold it (a member's head stays the request's: it moves with every edit).
    pub(crate) member: u32,
    /// Its crate is none the request's crate list holds (or the worker's names hold its item
    /// in no crate).
    pub(crate) unlisted: u32,
    /// Its crate has no names record this request (its names failed): no key to put it under.
    pub(crate) no_names: u32,
}

/// What each Bodies run counted (engine_bodies.rs `run_stretch`), summed by the stage.
#[derive(Clone, Copy, Default, Debug)]
pub(crate) struct SeedTally {
    /// Bodies checked from the request's frozen heads (no seed refused).
    pub(crate) seeded: u32,
    /// Bodies refused at a seed: a head the frozen form cannot hold yet, or a freeze error.
    pub(crate) refused: u32,
    /// Of those, refused because a head needs a constant's value and the freeze has no
    /// evaluator yet (front2 tables/head.rs `FreezeError::Const`; `consts: None`): each body's
    /// note names the head and the constant by path and the evaluator's reason.
    pub(crate) refused_const: u32,
    /// The check rounds of the seeded bodies (front2 tables/cost.rs `Spent::rounds`).
    pub(crate) rounds: u64,
}

impl SeedTally {
    pub(crate) fn add(&mut self, other: &SeedTally) {
        self.seeded = self.seeded.saturating_add(other.seeded);
        self.refused = self.refused.saturating_add(other.refused);
        self.refused_const = self.refused_const.saturating_add(other.refused_const);
        self.rounds = self.rounds.saturating_add(other.rounds);
    }
}

/// The request's counts of its frozen heads (rule 53): in `Checked` and the profile.
#[derive(Clone, Copy, Default, Debug)]
pub(crate) struct SeedCounts {
    /// Cells from head tasks (the table).
    pub(crate) table: u32,
    /// Distinct dependency heads read (by a body or a freeze).
    pub(crate) read: u32,
    /// Of those, found in the store.
    pub(crate) found: u32,
    /// Distinct heads frozen now.
    pub(crate) frozen: u32,
    /// Freezes of a cell another worker had frozen already (equal bytes).
    pub(crate) twice: u32,
    /// Dependency heads taken from the store into the table (read or not).
    pub(crate) stored: u32,
    /// Keys the root lists whose record the store no longer holds.
    pub(crate) missing: u32,
    /// Dependency heads frozen now and put as records.
    pub(crate) put: u32,
    /// Keys the root could not list past its cap.
    pub(crate) unlisted: u32,
    /// Heads frozen now and not put, by why.
    pub(crate) not_put: NotPut,
    /// The digest of every head frozen now, by cell (`fresh_digest`).
    pub(crate) fresh: ContentId,
    pub(crate) bodies: SeedTally,
}

/// The profile's line of the request's frozen heads.
pub(crate) fn profile(counts: &SeedCounts, into: &mut blaze_own::String<&Frame>) {
    use core::fmt::Write as _;
    let per = if counts.bodies.seeded == 0 { 0.0 } else { counts.bodies.rounds as f64 / f64::from(counts.bodies.seeded) };
    let _ = writeln!(
        into,
        "heads: table {} cells from head tasks; dependency heads read {}, found in the store {}, frozen now {}, frozen twice {}; bodies seeded {}, bodies refused at a seed {}, refused at a seed for a constant: {}; {per:.2} rounds a seeded body",
        counts.table, counts.read, counts.found, counts.frozen, counts.twice, counts.bodies.seeded, counts.bodies.refused, counts.bodies.refused_const
    );
    let _ = writeln!(
        into,
        "heads: {} dependency head(s) taken from the store, {} listed but gone, {} put now, {} not listed past the root's cap; frozen now digest {:?}",
        counts.stored, counts.missing, counts.put, counts.unlisted, counts.fresh
    );
    let _ = writeln!(
        into,
        "heads: frozen now and not put: {} a member's (the head tasks' table lacked it), {} of no crate of the list, {} of a crate with no names record",
        counts.not_put.member, counts.not_put.unlisted, counts.not_put.no_names
    );
}

/// The request's frozen heads (module docs). Read only while the Bodies phase runs, but for
/// `fresh` and `read`, which only insert.
pub(crate) struct RequestTable {
    members: FrozenTable<'static>,
    stored: FrozenTable<'static>,
    /// The keys the root listed when the request began, sorted.
    listed: std::vec::Vec<Key, Global>,
    missing: u32,
    fresh: blaze_core::SharedMap<ContentId, Fresh, Global>,
    /// Every cell outside `members` asked this request: the dependency heads read.
    read: blaze_core::SharedSet<ContentId, Global>,
    twice: AtomicU32,
    heap: &'static blaze_own::Heap,
}

impl RequestTable {
    /// The request's table (module docs), on the coordinator: the head tasks' cells, then the
    /// dependency heads the root lists whose crate's names record is today's.
    #[expect(clippy::too_many_arguments, reason = "the stage's edge: each argument one handle of it")]
    pub(crate) fn build<'m, D: Driver, S: Sharing>(
        cx: &Cx,
        store: &mut CommandStore<'m, '_, D, S>,
        heads: &HeadsStage,
        planned: &[Planned],
        view: View<'_, '_>,
        record: &dyn Fn(Name) -> Option<Key>,
        crates: &[CrateAt],
        root: &str,
        tool: Tool,
        heap: &'static blaze_own::Heap,
        frame: &Frame,
    ) -> Result<RequestTable, CliError> {
        let cx = cx.span("engine: the request's frozen heads");
        // 1. The head tasks' cells: this request's results, else the standing records.
        let mut reads: std::vec::Vec<crate::work::Read<'_>, Global> = std::vec::Vec::new_in(Global);
        let mut payloads: std::vec::Vec<&[u8], Global> = std::vec::Vec::new_in(Global);
        for one in planned.iter().filter(|one| one.name.kind == Kind::Signature) {
            if heads.result(one.name).is_some() {
                continue;
            }
            let Some(row) = view.row(one.name) else { continue };
            if row.failed {
                // A failed head task holds no frozen head: a body that reads it fails by that
                // read (engine_bodies.rs `widen`).
                continue;
            }
            let Some(at) = record(one.name) else {
                return Err(CliError::Text { what: "a standing head task's row has no record key (pages.rs `records`; engine_seeds.rs `RequestTable::build`)" });
            };
            reads.push(crate::work::payload(&cx, &mut *store, &Input { name: one.name, at, out: row.out }, frame)?);
        }
        for one in planned.iter().filter(|one| one.name.kind == Kind::Signature) {
            if let Some(made) = heads.result(one.name) {
                payloads.push(&made.payload[..]);
            }
        }
        payloads.extend(reads.iter().map(|read| read.payload()));
        let mut cells: std::vec::Vec<&[u8], Global> = std::vec::Vec::new_in(Global);
        for payload in payloads.iter() {
            cells_of(payload, &mut cells)?;
        }
        let members = FrozenTable::from_frozen(cells.iter().copied(), heap.alloc()).map_err(|_| CliError::Text { what: "the head tasks' frozen heads do not make one table: a record does not read, two cells disagree, or past its cap (front2 tables/seeds.rs `FrozenTable::from_frozen`)" })?;
        // 2. The dependency heads the root lists, each valid by its key: its crate's names
        //    record key is today's (a dependency's, never a member's).
        let mut today: std::vec::Vec<Key, Global> = std::vec::Vec::new_in(Global);
        today.extend(crates.iter().filter(|one| !one.member).filter_map(|one| one.names_at));
        today.sort_unstable();
        let mut listed: std::vec::Vec<Key, Global> = std::vec::Vec::new_in(Global);
        if let Some(keys) = store.named(root)? {
            if keys.len() % 32 != 0 {
                return Err(CliError::Text { what: "the dependency heads' root lists bytes that are no whole keys (engine_seeds.rs `root_name`)" });
            }
            for chunk in keys.chunks_exact(32) {
                let mut key = [0u8; 32];
                key.copy_from_slice(chunk);
                listed.push(Key(ContentId(key)));
            }
        }
        listed.sort_unstable();
        listed.dedup();
        let mut found: std::vec::Vec<&'m [u8], Global> = std::vec::Vec::new_in(Global);
        let mut missing = 0u32;
        for key in listed.iter() {
            let Some(held) = store.get(&cx, *key)? else {
                missing = missing.saturating_add(1);
                continue;
            };
            let bytes: &'m [u8] = held.bytes;
            if bytes.len() < KEY_BYTES {
                return Err(CliError::Text { what: "a dependency head's record is shorter than its names key (engine_seeds.rs `RequestTable::merge`)" });
            }
            let mut names_at = [0u8; 32];
            names_at.copy_from_slice(&bytes[..KEY_BYTES]);
            let names_at = Key(ContentId(names_at));
            if today.binary_search(&names_at).is_err() {
                continue;
            }
            let frozen = &bytes[KEY_BYTES..];
            // Its bytes are the head its key names (rule 21): verified once, here.
            let want = FrozenHead::read(frozen).map_err(|_| CliError::Text { what: "a dependency head's record does not read as a frozen head (engine_seeds.rs)" })?.want();
            if dependency_key(names_at, want, tool) != *key {
                return Err(CliError::Text { what: "a dependency head's record holds another cell than its key names (engine_seeds.rs `dependency_key`)" });
            }
            found.push(frozen);
        }
        let stored = FrozenTable::from_frozen(found.iter().copied(), heap.alloc()).map_err(|_| CliError::Text { what: "the stored dependency heads do not make one table: two records of one cell disagree, or past its cap (front2 tables/seeds.rs)" })?;
        Ok(RequestTable {
            members,
            stored,
            listed,
            missing,
            fresh: blaze_core::SharedMap::new_in("engine.frozen heads, made this request", Global),
            read: blaze_core::SharedSet::new_in("engine.dependency heads read", Global),
            twice: AtomicU32::new(0),
            heap,
        })
    }

    /// Cell `cell` asked outside the head tasks' table: one dependency head read.
    fn mark_read(&self, id: ContentId) {
        let read = self.read.pin();
        if !read.contains(&id) {
            read.insert(id);
        }
    }

    /// One more freeze of a cell another worker published first: equal bytes or a refusal.
    fn again(&self, cell: Want, held: &[u8], bytes: &[u8]) -> Result<(), SeedError> {
        if held != bytes {
            return Err(SeedError::Unequal { cell });
        }
        self.twice.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// After the Bodies phase, on the coordinator: each dependency head frozen now put under
    /// its key, sorted (rule 25), the root listing them with those it listed, and the counts.
    #[expect(clippy::too_many_arguments, reason = "the stage's edge: each argument one handle of it")]
    pub(crate) fn merge<'m, D: Driver, S: Sharing>(
        &self,
        cx: &Cx,
        store: &mut CommandStore<'m, '_, D, S>,
        crates: &[CrateAt],
        root: &str,
        tool: Tool,
        ledger: &mut crate::engine_store::Ledger,
        bodies: SeedTally,
    ) -> Result<SeedCounts, CliError> {
        let cx = cx.span("engine: the dependency heads frozen now, put");
        let fresh = self.fresh.pin();
        let mut puts: std::vec::Vec<(Key, std::vec::Vec<u8, Global>), Global> = std::vec::Vec::new_in(Global);
        let mut not_put = NotPut::default();
        for (_, one) in fresh.iter() {
            // Only a dependency's: a member's head moves with its crate's every edit, so it
            // stays the request's. Its crate is the one the freezing worker's names gave
            // (`RequestHeads::publish`), never guessed from the cell key's bytes.
            let Some(krate) = one.krate.and_then(|key| crate_keyed(crates, key)) else {
                not_put.unlisted = not_put.unlisted.saturating_add(1);
                continue;
            };
            if krate.member {
                not_put.member = not_put.member.saturating_add(1);
                continue;
            }
            let Some(names_at) = krate.names_at else {
                not_put.no_names = not_put.no_names.saturating_add(1);
                continue;
            };
            let mut payload = std::vec::Vec::with_capacity_in(KEY_BYTES + one.bytes.len(), Global);
            payload.extend_from_slice(&names_at.0.0);
            payload.extend_from_slice(one.bytes);
            puts.push((dependency_key(names_at, one.cell, tool), payload));
        }
        puts.sort_unstable_by_key(|(key, _)| *key);
        puts.dedup_by_key(|(key, _)| *key);
        for (key, payload) in puts.iter() {
            // Its payload is this vector's, freed below: none held after the merge.
            ledger.put(Stage::Bodies, *key, &[&payload[..]], 0);
            store.put(&cx, *key, payload)?;
        }
        let mut unlisted = 0u32;
        if !puts.is_empty() {
            let mut keys: std::vec::Vec<Key, Global> = std::vec::Vec::with_capacity_in(self.listed.len() + puts.len(), Global);
            keys.extend_from_slice(&self.listed);
            keys.extend(puts.iter().map(|(key, _)| *key));
            keys.sort_unstable();
            keys.dedup();
            // Rule 18: the root lists at most a table's cells; past it, counted and said.
            if keys.len() > MOST_CELLS {
                unlisted = u32::try_from(keys.len() - MOST_CELLS).unwrap_or(u32::MAX);
                keys.truncate(MOST_CELLS);
            }
            store.pin(root, &keys)?;
        }
        let read = self.read.pin();
        let found = self.stored.iter().filter(|(cell, _)| read.contains(&cell_key(*cell))).count();
        Ok(SeedCounts {
            table: u32::try_from(self.members.len()).unwrap_or(u32::MAX),
            read: u32::try_from(read.len()).unwrap_or(u32::MAX),
            found: u32::try_from(found).unwrap_or(u32::MAX),
            frozen: u32::try_from(fresh.len()).unwrap_or(u32::MAX),
            twice: self.twice.load(Ordering::Relaxed),
            stored: u32::try_from(self.stored.len()).unwrap_or(u32::MAX),
            missing: self.missing,
            put: u32::try_from(puts.len()).unwrap_or(u32::MAX),
            unlisted,
            not_put,
            fresh: fresh_digest(&self.fresh),
            bodies,
        })
    }

}

/// The digest of every head frozen this request, each its cell's bytes and its frozen bytes'
/// digest, in the cells' order (rule 25: a function of what was frozen, never of which worker
/// froze it first): what the tests compare across worker counts.
fn fresh_digest(fresh: &blaze_core::SharedMap<ContentId, Fresh, Global>) -> ContentId {
    let pinned = fresh.pin();
    let mut each: std::vec::Vec<([u8; WANT_BYTES], ContentId), Global> = std::vec::Vec::with_capacity_in(pinned.len(), Global);
    each.extend(pinned.iter().map(|(_, one)| (want_bytes(one.cell), ContentId::of(CELL_DOMAIN, one.bytes))));
    each.sort_unstable();
    let mut h = blaze_ids::Hasher::new(FRESH_DOMAIN);
    for (cell, digest) in each.iter() {
        h.update(cell).update(&digest.0);
    }
    h.finish()
}

/// Each frozen head of one head task's payload ("want bytes, a u32 LE length, then the frozen
/// bytes", one run a cell: engine_heads.rs `heads_of`) pushed onto `cells`.
fn cells_of<'p>(payload: &'p [u8], cells: &mut std::vec::Vec<&'p [u8], Global>) -> Result<(), CliError> {
    let malformed = CliError::Text { what: "a head task's payload is not runs of a cell's want, a length and its frozen bytes (engine_heads.rs `heads_of`)" };
    let mut at = 0usize;
    while at < payload.len() {
        let len_at = at + WANT_BYTES;
        let Some(len) = payload.get(len_at..len_at + 4) else { return Err(malformed) };
        let len = u32::from_le_bytes([len[0], len[1], len[2], len[3]]) as usize;
        let from = len_at + 4;
        let Some(bytes) = payload.get(from..from + len) else { return Err(malformed) };
        cells.push(bytes);
        at = from + len;
    }
    Ok(())
}

impl KnownHeads for RequestTable {
    fn known(&self, cell: Want) -> Option<&[u8]> {
        if let Some(bytes) = self.members.bytes_of(cell) {
            return Some(bytes);
        }
        let id = cell_key(cell);
        self.mark_read(id);
        if let Some(bytes) = self.stored.bytes_of(cell) {
            return Some(bytes);
        }
        self.fresh.pin().get(&id).map(|one| one.bytes)
    }
}

impl RequestHeads for RequestTable {
    fn publish(&self, cell: Want, krate: Option<blaze_id::CrateItemKey>, bytes: &[u8]) -> Result<(), SeedError> {
        let id = cell_key(cell);
        self.mark_read(id);
        let fresh = self.fresh.pin();
        if let Some(held) = fresh.get(&id) {
            return self.again(cell, held.bytes, bytes);
        }
        if fresh.len() >= MOST_CELLS {
            return Err(SeedError::Full { cell, most: MOST_CELLS });
        }
        // In the request's heap, lent to every worker until the request ends (a map entry
        // retired by a replace never frees them: they are not its).
        let mut copy = blaze_own::Vec::with_capacity_in(bytes.len(), self.heap.alloc());
        copy.extend_from_slice(bytes);
        let lent: &'static [u8] = copy.leak();
        match fresh.try_insert(id, Fresh { cell, krate, bytes: lent }) {
            Ok(_) => Ok(()),
            Err(occupied) => self.again(cell, occupied.current.bytes, bytes),
        }
    }
}
