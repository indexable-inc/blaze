//! THE REQUEST'S FROZEN HEADS (plan rows A1's wiring and A3; runtime.md points 3, 4, 5): every
//! body of `blaze engine-check` is checked from frozen heads, in one round, lowering no head.
//!
//! ONE TABLE A REQUEST ([`RequestTable`]), made on the coordinator after the Heads stage's
//! records are committed, before the Bodies phase, and read by the workers while it runs. It
//! reads NO head record when made: only each head task's record key (the stored answer's) and
//! the root's list. Every head is read by the worker whose body asks it, on its first ask,
//! in place through the store's `Sync` snapshot (blaze-cas plain/snapshot.rs), checked once and
//! kept as "what was read" in a request-wide map that only inserts:
//! - a MEMBER's head: the cell of its outline entry's head task (engine_heads.rs `head_cell`, by
//!   the item's name), found in that task's record (a payload run is "want bytes, the cell, a
//!   u32 LE length, then the frozen bytes"; the runs are followed by the task's run facts,
//!   [`runs_from`]). A stretch places a want ONE way ([`StretchHeads`]):
//!   engine_heads.rs `Owners::place` over its own names, the function a body's read of the head
//!   is placed by (engine_bodies.rs `widen`) and a head task lists its cells by
//!   (`heads_of_crate`), so the cell a body asks is the cell of the defining unit in its
//!   resolved scope. A head task freezes once per unit that holds its item, one cell per unit
//!   (the cell holds the defining unit's crate prefix, engine_heads.rs `head_cell`; lane RU1),
//!   so a reader gets bytes frozen under that unit's keys (lane EY's annoyance row
//!   head-cell-by-checker-key-is-per-unit). A cell whose task froze under other units only is
//!   refused by path, naming those units (`Lack::OtherUnits`, `SeedError::NotInItsUnit`);
//! - a DEPENDENCY's head (a crate with no head tasks: core, std, a registry crate): the record
//!   this machine froze before (A3, "once a machine") under [`dependency_key`] (the dependency
//!   crate's names record key today, the head's cell, the freezing tool): valid by its key,
//!   nothing of a dependency moves unless its names record does.
//!
//! A MEMBER'S HEAD IS NEVER FROZEN BY A BODY: the table holds its head task's run for the
//! requested defining unit, or the body is refused, typed, before any freeze (`StretchHeads::known` answers
//! `Known::Refused`): a head its task ran for and could not freeze is that failure, the task's
//! own reason quoted (`NotTheirs::Failed`, seeded as the freeze's error: the body is NOT
//! CHECKED, as `check` leaves it, never a second failure); the engine's own lack fails the
//! body: `SeedError::NotInItsUnit` when the task froze it under other units only,
//! `SeedError::NoMemberCell` otherwise, naming which (the task has no record this request, or
//! holds no such cell). Each cell's refusal words are made once a request.
//!
//! ON FIRST READ of a DEPENDENCY's head neither the store nor another worker holds (one never
//! frozen here before): the body's worker
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

use core::cell::RefCell;
use core::sync::atomic::{AtomicU32, Ordering};
use std::alloc::Global;

use blaze_cas::plain::Snapshot;
use blaze_core::Driver;
use blaze_drive::cells::Cell;
use blaze_drive::record::Stored;
use blaze_drive::{Key, Kind, Name, Tool};
use blaze_engine::{CellId, Planned, Stage, Tools, View};
use blaze_ids::ContentId;
use blaze_ladder::front2::heads::Want;
use blaze_ladder::front2::item_shown::ItemShown;
use blaze_ladder::front2::source::{NamesSource, Records};
use blaze_ladder::front2::tables::head::{FreezeError, FrozenHead, Known, KnownHeads, NotTheirs, VERSION, WANT_BYTES, cell_id as head_cell_id, want_bytes};
use blaze_ladder::front2::tables::seeds::{BodyLocals, MOST_CELLS, RequestHeads, SeedError};
pub(crate) use blaze_ladder::front2::tables::seeds::RequestSeed as BodySeed;
use blaze_own::{Alloc, SharedAlloc};
use blaze_world::crate_list::{CrateGraph, CrateList};
use bstd::cx::Cx;
use bstd::mem::Frame;

use crate::CliError;
use crate::engine_head_notes::Failed;
use crate::engine_heads::{HeadAt, Owners, Subjects};
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
pub(crate) const KEY_BYTES: usize = 32;
/// The bytes of a head task's cell in each run of its payload (engine_heads.rs `heads_of`).
const CELL_BYTES: usize = 16;

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
    pub(crate) prefix: [u8; 8],
    /// Its key whole: what a worker's names name its crate by (`RequestHeads::publish`).
    pub(crate) key: blaze_id::CrateItemKey,
    /// Its names record's key this request (`None`: its names failed).
    pub(crate) names_at: Option<Key>,
    pub(crate) member: bool,
    /// CLOSED (lane H9): no member is in it or in what it depends on, transitively. Its text is
    /// then a fetched pack's (a registry crate's, a git commit's, the toolchain's: a member is a
    /// crate whose root is in the checkout, blaze-world crate_list.rs `member`), named by its key
    /// (blaze-world list.rs `unit_crate_key`: the lock's package, features, cfg and every
    /// dependency's key; toolchain.rs: the pack's digest, features, cfg, the dependencies'
    /// keys), so everything derived from it is valid by that key ([`dependency_key`]). A crate
    /// that depends on a member is not: the member's text moves under an equal key.
    pub(crate) closed: bool,
    /// Whether this crate belongs to the toolchain (`core`/`alloc`/`std`/`bstd`/etc.): its closed
    /// dependency head records are rooted under `tool.<id>` across all checkouts.
    pub(crate) toolchain: bool,
}

/// Every crate of `list` by its key's prefix, sorted, with its names record's key, by the names
/// inputs seam (engine_names.rs `NamesInputs`): on the ready driver a crate's standing row's.
pub(crate) fn crates_of(list: &CrateList<'static>, names: crate::engine_names::NamesInputs<'_, '_>) -> std::vec::Vec<CrateAt, Global> {
    let mut crates = std::vec::Vec::with_capacity_in(CrateGraph::len(list), Global);
    each_crate(list, names, &mut |one| crates.push(one));
    close(list, &mut crates);
    crates.sort_unstable_by_key(|one| one.prefix);
    crates
}

/// [`crates_of`] in the caller's allocator (lane H9: the Heads stage's finder of dependency
/// heads, engine_heads_once.rs `HeadsOnce::find_dependencies`).
pub(crate) fn crates_in(list: &CrateList<'static>, names: &NamesStage<'_>, alloc: SharedAlloc<'static>) -> blaze_own::Vec<CrateAt, SharedAlloc<'static>> {
    let mut crates: blaze_own::Vec<CrateAt, SharedAlloc<'static>> = blaze_own::Vec::new_in(alloc);
    // The Heads phases' (the phase path's): the stage's entries alone.
    each_crate(list, crate::engine_names::NamesInputs::ran(names), &mut |one| crates.push(one));
    close(list, &mut crates);
    crates.sort_unstable_by_key(|one| one.prefix);
    crates
}

/// Every crate of `list` by its key's prefix, sorted, its names record's key NOT asked
/// (`names_at` is `None` for every crate): the ready driver's table ([`RequestTable::paged`])
/// asks a crate's standing names row on the first head of it a body asks
/// ([`CrateNames::Standing`]), never one row page per crate of the list (piece C).
pub(crate) fn crates_unasked(list: &CrateList<'static>) -> std::vec::Vec<CrateAt, Global> {
    let mut crates = std::vec::Vec::with_capacity_in(CrateGraph::len(list), Global);
    each_crate_by(list, &|_: Name| None::<Key>, &mut |one| crates.push(one));
    close(list, &mut crates);
    crates.sort_unstable_by_key(|one| one.prefix);
    crates
}

/// Each crate of `list`, in its order (each closed but a member, until [`close`]).
fn each_crate(list: &CrateList<'static>, names: crate::engine_names::NamesInputs<'_, '_>, push: &mut dyn FnMut(CrateAt)) {
    each_crate_by(list, &|name: Name| names.input_of(name).map(|input| input.at), push);
}

/// Each crate of `list`, in its order, its names record's key from `names_at` (by its
/// `CrateScope` name).
fn each_crate_by(list: &CrateList<'static>, names_at: &dyn Fn(Name) -> Option<Key>, push: &mut dyn FnMut(CrateAt)) {
    for at in 0..CrateGraph::len(list) {
        let key = list.key(at);
        let names_at = names_at(blaze_drive::key::crate_scope_name(&key));
        let member = list.member(at);
        let checkout_backed = member || list.root(at).starts_with("/0/");
        let toolchain = CrateGraph::toolchain(list, at);
        push(CrateAt { prefix: key.prefix(), key, names_at, member, closed: !checkout_backed, toolchain });
    }
}

/// `crates` (the list's rows, in its order) each closed or open over the list's edges. Closed is
/// a greatest fixed point: a member is open, and a crate with an open dependency is open; each
/// round opens at least one crate or ends, so at most `len` rounds (rule 18).
fn close(list: &CrateList<'static>, crates: &mut [CrateAt]) {
    for _ in 0..CrateGraph::len(list) {
        let mut opened = false;
        for at in 0..crates.len() {
            if crates[at].closed && CrateGraph::deps(list, at).any(|dep| crates.get(dep).is_none_or(|one| !one.closed)) {
                crates[at].closed = false;
                opened = true;
            }
        }
        if !opened {
            break;
        }
    }
}

/// The frozen head a dependency head's record holds, checked: the record begins with the names
/// key its key was made of, and its bytes read as `cell`'s frozen head (rule 21). The one check
/// of such a record (the Bodies stage's [`RequestTable`] and the Heads stage's finder).
pub(crate) fn dependency_head(bytes: &[u8], names_at: Key, cell: Want) -> Result<&[u8], &'static str> {
    if bytes.len() < KEY_BYTES || bytes[..KEY_BYTES] != names_at.0.0 {
        return Err("a dependency head's record does not begin with the names key its key holds (engine_seeds.rs `dependency_key`)");
    }
    match FrozenHead::read(&bytes[KEY_BYTES..]) {
        Ok(head) if head.want() == cell => Ok(&bytes[KEY_BYTES..]),
        Ok(_) => Err("a dependency head's record holds another cell than its key names (engine_seeds.rs `dependency_key`)"),
        Err(_) => Err("a dependency head's record does not read as a frozen head (engine_seeds.rs `dependency_head`)"),
    }
}

/// Why the records a finder reads could not be read (the store's snapshot refused).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Unreadable;

/// The records a dependency head is found in by its key (rule 78: one seam; the command's is the
/// store's committed snapshot, read in place by every worker; a test's is a model store whose
/// commit can be torn, engine_heads_found_tests.rs).
pub(crate) trait HeadRecords: Sync {
    /// The record under `key`, in place; `None` when there is none.
    fn record(&self, key: Key) -> Result<Option<&[u8]>, Unreadable>;
    /// The frozen head `bytes` holds when it is `cell`'s record made over `names_at`
    /// ([`dependency_head`], the command's).
    fn head<'b>(&self, bytes: &'b [u8], names_at: Key, cell: Want) -> Result<&'b [u8], &'static str> {
        dependency_head(bytes, names_at, cell)
    }
}

impl<A: core::alloc::Allocator + Send + Sync> HeadRecords for Snapshot<A> {
    fn record(&self, key: Key) -> Result<Option<&[u8]>, Unreadable> {
        self.get(&blaze_cas::Key(key.0)).map_err(|_| Unreadable)
    }
}

/// How a cell's crate stands to the rule "a dependency head is valid by its key".
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Keyed {
    /// A CLOSED dependency's head ([`CrateAt::closed`]): its record's key, the names key it is
    /// made of, and its crate's scope name (whose `WHOLE` cell is the one cell a reader reads).
    Closed { key: Key, names_at: Key, scope: Name },
    /// A member's head, or a dependency's whose closure holds a member: valid by its reads.
    Open,
    /// Its crate is none of the list, or has no names record this request: no key.
    Unkeyed,
}

/// The Heads stage's finder of dependency heads by key (lane H9): the request's crates by
/// prefix, the freezing tool, and the records (the snapshot taken before the phase, renewed
/// after each commit: engine_heads_once.rs `HeadsOnce::committed`).
pub(crate) struct DependencyFinder<R> {
    pub(crate) records: R,
    crates: blaze_own::Vec<CrateAt, SharedAlloc<'static>>,
    tool: Tool,
}

impl<R: HeadRecords> DependencyFinder<R> {
    /// `crates`: [`crates_in`]'s (sorted by prefix).
    pub(crate) fn new(records: R, crates: blaze_own::Vec<CrateAt, SharedAlloc<'static>>, tool: Tool) -> DependencyFinder<R> {
        DependencyFinder { records, crates, tool }
    }

    /// Cell `cell`'s standing (by its item key's crate prefix: unique in a list, blaze-surface
    /// crate_graph.rs `PrefixClash`).
    pub(crate) fn keyed(&self, cell: Want) -> Keyed {
        let prefix = cell.key().crate_prefix();
        let from = self.crates.partition_point(|one| one.prefix < prefix);
        let Some(at) = self.crates.get(from).filter(|one| one.prefix == prefix) else { return Keyed::Unkeyed };
        if !at.closed {
            return Keyed::Open;
        }
        match at.names_at {
            Some(names_at) => Keyed::Closed { key: dependency_key(names_at, cell, self.tool), names_at, scope: blaze_drive::key::crate_scope_name(&at.key) },
            None => Keyed::Unkeyed,
        }
    }
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
    /// A member crate's head frozen at a seed: placed as a dependency's, as its crate has no
    /// file among the members' outlines (engine_heads.rs `Subjects`; `Owners::place`). Never a
    /// head a macro wrote: that is its entry's head task's, or its reader is refused by name
    /// (engine_heads.rs `HeadAt::Unlisted`). Not put: a member's head moves with every edit.
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
    /// The reads the checked bodies' recorders were offered and kept, summed (front2
    /// tables/recorder.rs `ReadTally`): the Bodies stage's reads line.
    pub(crate) reads: blaze_ladder::front2::tables::recorder::ReadTally,
    /// Expansions' origin columns the stretches' records opened (front2 source.rs
    /// `Opened::origins`): once per expansion asked per stretch.
    pub(crate) origins_opened: u64,
}

impl SeedTally {
    pub(crate) fn add(&mut self, other: &SeedTally) {
        self.seeded = self.seeded.saturating_add(other.seeded);
        self.refused = self.refused.saturating_add(other.refused);
        self.refused_const = self.refused_const.saturating_add(other.refused_const);
        self.rounds = self.rounds.saturating_add(other.rounds);
        self.reads.add(other.reads);
        self.origins_opened = self.origins_opened.saturating_add(other.origins_opened);
    }
}

/// The request's counts of its frozen heads (rule 53): in `Checked` and the profile.
#[derive(Clone, Copy, Default, Debug)]
pub(crate) struct SeedCounts {
    /// Head tasks whose records the table reads a member's head from (none read when made).
    pub(crate) table: u32,
    /// Distinct member heads taken from their head task's record (read through the snapshot).
    pub(crate) taken: u32,
    /// Distinct heads asked that the head tasks' records did not answer (by a body or a freeze).
    pub(crate) read: u32,
    /// Of those, found in the store (a dependency's, frozen by an earlier request).
    pub(crate) found: u32,
    /// Distinct heads frozen now.
    pub(crate) frozen: u32,
    /// Freezes of a cell another worker had frozen already (equal bytes).
    pub(crate) twice: u32,
    /// Keys of dependency heads the root listed when the request began.
    pub(crate) listed: u32,
    /// Distinct dependency head records looked up in the store (found or not): only those a
    /// body asked, never the root's whole list.
    pub(crate) looked: u32,
    /// Dependency heads frozen now and put as records.
    pub(crate) put: u32,
    /// Keys the root could not list past its cap.
    pub(crate) unlisted: u32,
    /// Heads frozen now and not put, by why.
    pub(crate) not_put: NotPut,
    /// The digest of every head frozen now, by cell (`fresh_digest`).
    pub(crate) fresh: ContentId,
    pub(crate) bodies: SeedTally,
    /// What the head tasks entered this request froze, by unit (engine_heads.rs `UnitCounts`).
    pub(crate) units: crate::engine_heads::UnitCounts,
    /// The Heads stage's table of frozen heads (engine_heads_once.rs `OnceCounts`).
    pub(crate) once: crate::engine_heads_once::OnceCounts,
}

/// The profile's line of the request's frozen heads.
pub(crate) fn profile(counts: &SeedCounts, into: &mut blaze_own::String<&Frame>) {
    use core::fmt::Write as _;
    let per = if counts.bodies.seeded == 0 { 0.0 } else { counts.bodies.rounds as f64 / f64::from(counts.bodies.seeded) };
    let _ = writeln!(
        into,
        "heads: table of {} head tasks; member heads taken from the table: {}; heads not in it read {}, found in the store {}, frozen now {}, frozen twice {}; bodies seeded {}, bodies refused at a seed {}, refused at a seed for a constant: {}; {per:.2} rounds a seeded body",
        counts.table, counts.taken, counts.read, counts.found, counts.frozen, counts.twice, counts.bodies.seeded, counts.bodies.refused, counts.bodies.refused_const
    );
    let _ = writeln!(
        into,
        "heads: dependency heads listed {}, read {}; {} put now, {} not listed past the root's cap; frozen now digest {:?}",
        counts.listed, counts.looked, counts.put, counts.unlisted, counts.fresh
    );
    let _ = write!(
        into,
        "heads: frozen now and not put: {} a member crate's placed as a dependency's (no file of it among the members' outlines), {} of no crate of the list, {} of a crate with no names record",
        counts.not_put.member, counts.not_put.unlisted, counts.not_put.no_names
    );
    let _ = writeln!(into);
    crate::engine_heads::unit_profile(&counts.units, into);
    crate::engine_heads_once::profile(&counts.once, into);
}

/// Why a member's head was not in its head task's record (the profile's words, rule 19).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Lack {
    /// Its entry's head task failed this request (its refusal says why).
    Failed,
    /// Its entry has no head task with a record (none planned, or its row is gone).
    NoTask,
    /// Its head task's record holds no run of that cell.
    NoCell,
    /// Its head task froze its heads under these units only, none the requested defining unit (lane RU1,
    /// engine_heads.rs `head_cell`): a typed refusal, never another unit's bytes.
    OtherUnits(Units),
}

impl Lack {
    fn words(self) -> &'static str {
        match self {
            Lack::Failed => "its entry's head task failed",
            Lack::NoTask => "its entry has no head task record",
            Lack::NoCell => "its head task's record holds no such cell",
            Lack::OtherUnits(_) => "its head task froze it under units other than the requested defining unit",
        }
    }
}

/// A member head looked up in its head task's record through the snapshot, once (rule 74).
#[derive(Clone, Copy, Debug)]
enum MemberRead {
    /// Its frozen bytes at `at..at + len` of the record under `key` (checked once, here).
    Found { key: Key, at: usize, len: usize },
    Lacked(Lack),
    /// A record that does not read as its form (the request fails at the merge, rule 3).
    Bad(&'static str),
}

/// A dependency head's record looked up through the snapshot, once (rule 74).
#[derive(Clone, Copy, Debug)]
enum DependencyRead {
    /// Under its key, checked: its frozen bytes follow its names key.
    Found,
    /// No record under its key: frozen on this ask, put at the merge.
    Absent,
    /// A record that does not read as the head its key names (the request fails at the merge).
    Bad(&'static str),
}

/// What the table knows of one head task: its [`HeadEntries`]' answer for a name.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum HeadEntry {
    /// It froze its heads into the record under this key.
    Record(Key),
    /// It failed (this request, or when it last ran and stands).
    Failed,
    /// No head task with a record: none planned today, no row, or a head no unit holds
    /// (engine_heads.rs `no_units_out`).
    None,
}

/// How the table finds a head task's row (rule 78: one seam, two strategies, one answer). The
/// phase path builds every planned head task's entry before its Bodies phase; the ready path
/// finds the one a body asks, on the stored answer's own page of that row, on its first ask. Each
/// gives the same answer for every name: tests/engine_ready_edit_tests.rs checks the ready edit
/// (paged) against the phase path (built) byte for byte, over an edit whose body reads member
/// heads, and its control plants a paged miss (`CommitPlant::PagedHeadsMiss`).
pub(crate) enum HeadEntries<'p> {
    /// From the whole plan (the phase path): each head task with a record, sorted by name, and
    /// each that failed, sorted.
    Built { entries: std::vec::Vec<(Name, Key), Global>, failed: std::vec::Vec<Name, Global> },
    /// On first ask, from the stored answer's pages (the ready path).
    Paged(PagedHeads<'p>),
}

impl HeadEntries<'_> {
    /// `name`'s entry.
    fn entry(&self, name: Name) -> HeadEntry {
        match self {
            HeadEntries::Built { entries, failed } => match entries.binary_search_by_key(&name, |(one, _)| *one) {
                Ok(at) => HeadEntry::Record(entries[at].1),
                Err(_) if failed.binary_search(&name).is_ok() => HeadEntry::Failed,
                Err(_) => HeadEntry::None,
            },
            HeadEntries::Paged(paged) => paged.entry(name),
        }
    }

    /// The head tasks with a record the table holds: every planned one built, or those found
    /// so far paged (the profile's `table of N head tasks`).
    fn count(&self) -> u32 {
        match self {
            HeadEntries::Built { entries, .. } => u32::try_from(entries.len()).unwrap_or(u32::MAX),
            HeadEntries::Paged(paged) => paged.records.load(Ordering::Relaxed),
        }
    }

    /// The first row a paged lookup could not take (rule 3: the request's error after its drive,
    /// as the built table's [`RequestTable::build`] stops at once); `None` when built.
    fn fault(&self) -> Option<&'static str> {
        match self {
            HeadEntries::Built { .. } => None,
            HeadEntries::Paged(paged) => paged.fault.get().copied(),
        }
    }
}

/// The ready path's planted bug for [`PagedHeads`] (rule 23): `Misses` answers every name
/// [`HeadEntry::None`], so a body reading a member head is refused and the answer moves.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum PagedPlant {
    None,
    Misses,
}

/// On-demand head entries: a head task's row is served from a small sorted in-memory `overlay`
/// of `Signature` tasks that ran in `Stage::Heads` during this request (empty on the ready path),
/// and on miss read where the stored answer keeps it, on the first ask (blaze-engine
/// `StoredPages::header`, from the row's own page). A stored row is planned today when its file
/// is not edited (the file's plan digest stands) or an edited file's plan lists it.
pub(crate) struct PagedHeads<'p> {
    pages: &'p dyn blaze_engine::StoredPages,
    /// Today's plan of each edited file (the engine's `Edit`), a deleted one's empty.
    files: &'p [(Name, &'p [Planned])],
    /// Sorted overlay of `Signature` rows updated by `Stage::Heads` during this request.
    overlay: blaze_own::Vec<(Name, HeadEntry), SharedAlloc<'static>>,
    found: blaze_core::SharedMap<Name, HeadEntry, SharedAlloc<'static>>,
    records: AtomicU32,
    fault: std::sync::OnceLock<&'static str>,
    plant: PagedPlant,
}

impl<'p> PagedHeads<'p> {
    pub(crate) fn new(pages: &'p dyn blaze_engine::StoredPages, files: &'p [(Name, &'p [Planned])], plant: PagedPlant, heap: &'static blaze_own::Heap) -> PagedHeads<'p> {
        Self::with_overlay(pages, files, blaze_own::Vec::new_in(heap.shared()), plant, heap)
    }

    /// [`Self::new`] with a sorted in-memory `overlay` of `Signature` rows produced in
    /// `Stage::Heads` this request, checked before paging from `pages`.
    pub(crate) fn with_overlay(
        pages: &'p dyn blaze_engine::StoredPages,
        files: &'p [(Name, &'p [Planned])],
        overlay: blaze_own::Vec<(Name, HeadEntry), SharedAlloc<'static>>,
        plant: PagedPlant,
        heap: &'static blaze_own::Heap,
    ) -> PagedHeads<'p> {
        PagedHeads {
            pages,
            files,
            overlay,
            found: blaze_core::SharedMap::new_in("engine.head task rows found on the stored answer's pages", heap.shared()),
            records: AtomicU32::new(0),
            fault: std::sync::OnceLock::new(),
            plant,
        }
    }

    /// `name`'s entry, read once a request.
    fn entry(&self, name: Name) -> HeadEntry {
        if let Some(found) = self.found.pin().get(&name).copied() {
            return found;
        }
        let read = self.read(name);
        match self.found.pin().try_insert(name, read) {
            Ok(_) => {
                if matches!(read, HeadEntry::Record(_)) {
                    self.records.fetch_add(1, Ordering::Relaxed);
                }
                read
            }
            Err(occupied) => *occupied.current,
        }
    }

    /// [`HeadEntries::Built`]'s answer for `name`, from the moved-heads overlay or its stored row:
    /// a planned `Signature` task's row, failed, or holding a record (one no unit holds holds none).
    fn read(&self, name: Name) -> HeadEntry {
        if name.kind != Kind::Signature || self.plant == PagedPlant::Misses {
            return HeadEntry::None;
        }
        if let Some(&(_, entry)) = self.overlay.binary_search_by_key(&name, |(one, _)| *one).ok().and_then(|at| self.overlay.get(at)) {
            return entry;
        }
        let header = match self.pages.header(name) {
            Ok(Some(header)) => header,
            Ok(None) => return HeadEntry::None,
            // The page's error is kept by the stored answer (its `refusal`), the request's error
            // after the drive.
            Err(_) => return self.faulted("a head task's row page could not be read from the stored answer (engine_seeds.rs `PagedHeads::read`)"),
        };
        let facts = header.facts;
        if facts.inline {
            return HeadEntry::None;
        }
        if let Some((_, plan)) = self.files.iter().find(|(file, _)| *file == facts.file)
            && !plan.iter().any(|one| one.name == name)
        {
            return HeadEntry::None;
        }
        if facts.failed {
            return HeadEntry::Failed;
        }
        if facts.out == crate::engine_heads::no_units_out(name) {
            return HeadEntry::None;
        }
        match header.record {
            Some(key) => HeadEntry::Record(key),
            None => self.faulted("a head task's row has no record key (pages.rs `records`; engine_seeds.rs `PagedHeads::read`)"),
        }
    }

    fn faulted(&self, what: &'static str) -> HeadEntry {
        let _first_kept = self.fault.set(what);
        HeadEntry::None
    }
}

/// Where the request table reads a crate's names record key (rule 78: one seam, the same key
/// either way, tested equal by the ready edit's world test, whose oracle compares both drivers'
/// roots). `Listed`: the crate list's [`CrateAt::names_at`], asked of every crate when the
/// table is made (the phase path's: its names stage holds every crate's entry in memory).
/// `ByNeed`: the crate's input by the names seam (engine_names.rs `NamesKeys`: entered this
/// command, else its standing row on the stored answer), asked only for a crate a body asks a
/// dependency head of or whose head was frozen now; never one row page per crate of the list
/// (piece C). A crate asked twice reads its row's page once (blaze-engine headers.rs).
#[derive(Clone, Copy)]
pub(crate) enum CrateNames<'p> {
    Listed,
    ByNeed(crate::engine_names::NamesKeys<'p>),
}

/// The request's frozen heads (module docs). Read only while the Bodies phase runs, but for
/// its maps, which only insert. `Sync`: the snapshot is, and so is every map.
pub(crate) struct RequestTable<'p> {
    /// The committed store as the Bodies phase began: every head record is read through it.
    snapshot: Snapshot<Global>,
    /// Each head task's entry: built, or found on its first ask (rule 78).
    heads: HeadEntries<'p>,
    /// The request's crates (as [`crates_of`] lists them; [`crates_unasked`] on the ready driver).
    crates: std::vec::Vec<CrateAt, Global>,
    /// Where a crate's names record key is read: [`CrateNames`] (rule 78's seam).
    names: CrateNames<'p>,
    tool: Tool,
    /// Whether the dependency heads root existed when the request began, and how many keys it held.
    had_root: bool,
    listed_from_root: usize,
    /// Additional dependency head keys from the Heads stage (`list_too`), empty on warm edits.
    added_listed: std::vec::Vec<Key, Global>,
    /// What was read of the head tasks' records, by member cell.
    members: blaze_core::SharedMapBy<CellId, MemberRead, blaze_hash::DigestState, Global>,
    /// The member cells taken from them.
    taken: blaze_core::SharedSetBy<CellId, blaze_hash::DigestState, Global>,
    /// What was read of the dependency heads' records, by key.
    dependencies: blaze_core::SharedMapBy<Key, DependencyRead, blaze_hash::DigestState, Global>,
    fresh: blaze_core::SharedMap<ContentId, Fresh, Global>,
    /// Every cell asked that no head task's record answered.
    read: blaze_core::SharedSet<ContentId, Global>,
    /// Each cell whose first freeze failed this request, by any stretch (`first_failure`).
    first_failed: blaze_core::SharedMap<ContentId, FirstFailure, blaze_own::SharedAlloc<'static>>,
    /// Failures past [`MOST_FIRST_FAILED`], counted, their reasons not kept.
    first_past_cap: AtomicU32,
    /// Each member cell refused to a body, with its words, made once a request
    /// (`StretchHeads::not_theirs`): a cell a thousand bodies ask is formatted once.
    refused: blaze_core::SharedMapBy<CellId, NotTheirs, blaze_hash::DigestState, SharedAlloc<'static>>,
    twice: AtomicU32,
    heap: &'static blaze_own::Heap,
    /// Where a body's need of an item declared in that body is made (front2 tables/seeds.rs
    /// `BodyLocals`): the rule, `Lowered`, set by [`RequestTable::build`]; a test plants
    /// `AskTheRequest` through engine_check.rs `Opts::body_locals` (rule 23).
    pub(crate) body_locals: BodyLocals,
}

impl<'p> RequestTable<'p> {
    /// The ready path's table: its head entries found on their first ask on the stored
    /// answer's pages ([`PagedHeads`]), never a scan of the plan; the rest as [`RequestTable::build`].
    /// `crates`: [`crates_unasked`]'s; each crate's names record key read by need from `names`
    /// ([`CrateNames::ByNeed`]).
    #[expect(clippy::too_many_arguments, reason = "the stage's edge: each argument one handle of it")]
    pub(crate) fn paged<'m, D: Driver, S: Sharing>(
        cx: &Cx,
        store: &mut CommandStore<'m, '_, D, S>,
        heads: PagedHeads<'p>,
        (crates, names): (std::vec::Vec<CrateAt, Global>, crate::engine_names::NamesKeys<'p>),
        root: &str,
        tool: Tool,
        heap: &'static blaze_own::Heap,
    ) -> Result<RequestTable<'p>, CliError> {
        let _cx = cx.span("engine: the request's frozen heads, paged");
        Self::of(store, HeadEntries::Paged(heads), (crates, CrateNames::ByNeed(names)), root, tool, heap)
    }

    /// Crate `at`'s names record key this request (`None`: its names failed, or it has no
    /// names row): by [`CrateNames`].
    fn names_of(&self, at: &CrateAt) -> Option<Key> {
        match self.names {
            CrateNames::Listed => at.names_at,
            CrateNames::ByNeed(names) => names.input_of(blaze_drive::key::crate_scope_name(&at.key)).map(|input| input.at),
        }
    }

    /// The first head task row the paged table could not take, for the request's error.
    pub(crate) fn fault(&self) -> Option<&'static str> {
        self.heads.fault()
    }

    /// The request's table (module docs), on the coordinator, after the Heads stage's records
    /// are committed: the snapshot, each head task's record key (none of its bytes read), and
    /// the keys the root lists (none of their records read).
    #[expect(clippy::too_many_arguments, reason = "the stage's edge: each argument one handle of it")]
    pub(crate) fn build<'m, D: Driver, S: Sharing>(
        cx: &Cx,
        store: &mut CommandStore<'m, '_, D, S>,
        planned: &[Planned],
        view: View<'_, '_>,
        record: &dyn Fn(Name) -> Option<Key>,
        crates: std::vec::Vec<CrateAt, Global>,
        root: &str,
        tool: Tool,
        heap: &'static blaze_own::Heap,
    ) -> Result<RequestTable<'p>, CliError> {
        let _cx = cx.span("engine: the request's frozen heads");
        let mut entries: std::vec::Vec<(Name, Key), Global> = std::vec::Vec::new_in(Global);
        let mut failed: std::vec::Vec<Name, Global> = std::vec::Vec::new_in(Global);
        for one in planned.iter().filter(|one| one.name.kind == Kind::Signature) {
            let Some(row) = view.row(one.name) else { continue };
            if row.failed {
                // A failed head task holds no frozen head: a body that reads it fails by that
                // read (engine_bodies.rs `widen`).
                failed.push(one.name);
                continue;
            }
            // A head no unit holds under its cfg (engine_heads.rs `no_units_out`): a row with no
            // finding and no record. It froze nothing, so the table holds nothing of it; any
            // OTHER row without a record key is the stop below.
            if row.out == crate::engine_heads::no_units_out(one.name) {
                continue;
            }
            let Some(at) = record(one.name) else {
                return Err(CliError::Text { what: "a head task's row has no record key (pages.rs `records`; engine_seeds.rs `RequestTable::build`)" });
            };
            entries.push((one.name, at));
        }
        entries.sort_unstable_by_key(|(name, _)| *name);
        failed.sort_unstable();
        Self::of(store, HeadEntries::Built { entries, failed }, (crates, CrateNames::Listed), root, tool, heap)
    }

    /// The table over `heads`: the snapshot and the keys the root lists (none of their records
    /// read).
    fn of<'m, D: Driver, S: Sharing>(
        store: &mut CommandStore<'m, '_, D, S>,
        heads: HeadEntries<'p>,
        (crates, names): (std::vec::Vec<CrateAt, Global>, CrateNames<'p>),
        root: &str,
        tool: Tool,
        heap: &'static blaze_own::Heap,
    ) -> Result<RequestTable<'p>, CliError> {
        let (had_root, listed_from_root) = match store.named(root)? {
            Some(keys) => {
                if keys.len() % 32 != 0 {
                    return Err(CliError::Text { what: "the dependency heads' root lists bytes that are no whole keys (engine_seeds.rs `root_name`)" });
                }
                (true, keys.len() / 32)
            }
            None => (false, 0),
        };
        Ok(RequestTable {
            snapshot: store.snapshot_in(Global)?,
            heads,
            crates,
            names,
            tool,
            had_root,
            listed_from_root,
            added_listed: std::vec::Vec::new_in(Global),
            members: blaze_core::SharedMapBy::new_in("engine.member heads read from their head tasks' records", Global),
            taken: blaze_core::SharedSetBy::new_in("engine.member heads taken", Global),
            dependencies: blaze_core::SharedMapBy::new_in("engine.dependency head records read", Global),
            fresh: blaze_core::SharedMap::new_in("engine.frozen heads, made this request", Global),
            read: blaze_core::SharedSet::new_in("engine.heads not in the head tasks' records, read", Global),
            first_failed: blaze_core::SharedMap::new_in("engine.heads whose first freeze failed, with their reasons", heap.shared()),
            first_past_cap: AtomicU32::new(0),
            refused: blaze_core::SharedMapBy::new_in("engine.member heads refused to a body, as said", heap.shared()),
            twice: AtomicU32::new(0),
            heap,
            body_locals: BodyLocals::Lowered,
        })
    }

    /// Member cell `id`'s frozen bytes, read from its head task's record on the first ask, by a
    /// reader of unit `unit` (the cell holds it: engine_heads.rs `head_cell`).
    fn member(&self, id: CellId, unit: [u8; 8]) -> Option<&[u8]> {
        let read = {
            let members = self.members.pin();
            match members.get(&id) {
                Some(read) => *read,
                None => {
                    let read = self.read_member(id, unit);
                    match members.try_insert(id, read) {
                        Ok(_) => read,
                        Err(occupied) => *occupied.current,
                    }
                }
            }
        };
        let MemberRead::Found { key, at, len } = read else { return None };
        let bytes = self.snapshot.get(&blaze_cas::Key(key.0)).ok().flatten()?.get(at..at + len)?;
        let taken = self.taken.pin();
        if !taken.contains(&id) {
            taken.insert(id);
        }
        Some(bytes)
    }

    /// Member cell `id` looked up in its entry's head task's record (module docs), for a reader
    /// of unit `unit`.
    fn read_member(&self, id: CellId, unit: [u8; 8]) -> MemberRead {
        let key = match self.heads.entry(id.of) {
            HeadEntry::Record(key) => key,
            HeadEntry::Failed => return MemberRead::Lacked(Lack::Failed),
            HeadEntry::None => return MemberRead::Lacked(Lack::NoTask),
        };
        let bytes = match self.snapshot.get(&blaze_cas::Key(key.0)) {
            Ok(Some(bytes)) => bytes,
            Ok(None) => return MemberRead::Bad("a head task's record the answer names is not in the store's snapshot (engine_seeds.rs `RequestTable::read_member`)"),
            Err(_) => return MemberRead::Bad("the store's snapshot could not read a head task's record (engine_seeds.rs `RequestTable::read_member`)"),
        };
        let payload = match blaze_drive::record::open(bytes) {
            Ok(Stored::Whole(view)) => view.payload,
            Ok(Stored::Shape(_)) | Err(_) => return MemberRead::Bad("a head task's record does not read as a whole record (engine_seeds.rs `RequestTable::read_member`)"),
        };
        let base = payload.as_ptr() as usize - bytes.as_ptr() as usize;
        match run_of(payload, id.cell) {
            Ok(Some((from, len))) => match FrozenHead::read(&payload[from..from + len]) {
                Ok(_) => MemberRead::Found { key, at: base + from, len },
                Err(_) => MemberRead::Bad("a head task's frozen head does not read (engine_seeds.rs `RequestTable::read_member`)"),
            },
            // No run of the cell: when the task froze under other units only, the reader's
            // unit is one no run covers (a typed refusal at the publish, `StretchHeads`).
            Ok(None) => match Units::other_than(payload, unit) {
                Ok(Some(units)) => MemberRead::Lacked(Lack::OtherUnits(units)),
                Ok(None) => MemberRead::Lacked(Lack::NoCell),
                Err(why) => MemberRead::Bad(why),
            },
            Err(why) => MemberRead::Bad(why),
        }
    }

    /// Dependency head `cell` of crate `krate` (as a stretch's names give it): its record
    /// frozen by an earlier request, looked up under its key on the first ask. The body seed
    /// makes the signature's const-parameter facts available beside its on-demand body/value
    /// evaluation.
    /// `None`: no record (or its crate is a member's, or has no names record today).
    fn dependency(&self, cell: Want, krate: blaze_id::CrateItemKey) -> Option<&[u8]> {
        // Only a CLOSED dependency's head is valid by its key (lane H9, [`CrateAt::closed`]): a
        // crate that depends on a member has a head that moves with the member's text.
        let at = crate_keyed(&self.crates, krate).filter(|one| one.closed)?;
        let names_at = self.names_of(&at)?;
        let key = dependency_key(names_at, cell, self.tool);
        let read = {
            let dependencies = self.dependencies.pin();
            match dependencies.get(&key) {
                Some(read) => *read,
                None => {
                    let read = self.read_dependency(key, names_at, cell);
                    match dependencies.try_insert(key, read) {
                        Ok(_) => read,
                        Err(occupied) => *occupied.current,
                    }
                }
            }
        };
        match read {
            DependencyRead::Found => self.snapshot.get(&blaze_cas::Key(key.0)).ok().flatten().and_then(|bytes| bytes.get(KEY_BYTES..)),
            DependencyRead::Absent | DependencyRead::Bad(_) => None,
        }
    }

    /// The record under dependency key `key`, checked once: its names key is `names_at` and its
    /// bytes are the frozen head of `cell` (rule 21).
    fn read_dependency(&self, key: Key, names_at: Key, cell: Want) -> DependencyRead {
        let bytes = match self.snapshot.get(&blaze_cas::Key(key.0)) {
            Ok(Some(bytes)) => bytes,
            Ok(None) => return DependencyRead::Absent,
            Err(_) => return DependencyRead::Bad("the store's snapshot could not read a dependency head's record (engine_seeds.rs `RequestTable::read_dependency`)"),
        };
        match dependency_head(bytes, names_at, cell) {
            Ok(_) => DependencyRead::Found,
            Err(why) => DependencyRead::Bad(why),
        }
    }

    /// Keys of dependency heads the Heads stage put this request (engine_heads_once.rs
    /// `HeadsOnce::put_keys`): listed by the root with the rest at the merge, so the records
    /// that stage put live as long as those this stage puts (lane H9).
    pub(crate) fn list_too(&mut self, keys: &[Key]) {
        if keys.is_empty() {
            return;
        }
        self.added_listed.extend_from_slice(keys);
    }

    /// Cell `cell`'s first failed freeze this request, by any worker's stretch (front2
    /// tables/seeds.rs `RequestHeads::refused`): the head named and its own reason, what a body
    /// that meets the failure again quotes (engine_bodies.rs `own_reason`). Past the cap, the
    /// count kept: the one case a reason is not kept (rule 18).
    pub(crate) fn first_failure(&self, cell: Want) -> FirstFailed {
        match self.first_failed.pin().get(&cell_key(cell)) {
            Some(one) => FirstFailed::Kept(*one),
            None if self.first_past_cap.load(Ordering::Relaxed) > 0 => FirstFailed::PastCap { kept: self.first_failed.len() },
            None => FirstFailed::None,
        }
    }

    /// Cell `cell`'s first failed freeze kept (the first worker's words; another's for the
    /// same cell are the same reason), or counted past [`MOST_FIRST_FAILED`].
    fn keep_first_failure(&self, cell: Want, one: FirstFailure) {
        let failed = self.first_failed.pin();
        if failed.len() >= MOST_FIRST_FAILED {
            self.first_past_cap.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let _first = failed.try_insert(cell_key(cell), one).is_ok();
    }

    /// Cell `cell`'s bytes frozen this request by any worker.
    fn fresh_of(&self, cell: Want) -> Option<&[u8]> {
        self.fresh.pin().get(&cell_key(cell)).map(|one| one.bytes)
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
        root: &str,
        ledger: &mut crate::engine_store::Ledger,
        bodies: SeedTally,
    ) -> Result<SeedCounts, CliError> {
        let cx = cx.span("engine: the dependency heads frozen now, put");
        let (crates, tool) = (&self.crates[..], self.tool);
        // A head record that did not read as its form stops the request here (rule 3): its
        // bodies were not given it, and nothing is put over it.
        if let Some(why) = self.members.pin().iter().find_map(|(_, read)| match read {
            MemberRead::Bad(why) => Some(*why),
            MemberRead::Found { .. } | MemberRead::Lacked(_) => None,
        }) {
            return Err(CliError::Text { what: why });
        }
        let mut looked = 0u32;
        let mut found = 0u32;
        for (_, read) in self.dependencies.pin().iter() {
            looked = looked.saturating_add(1);
            match read {
                DependencyRead::Found => found = found.saturating_add(1),
                DependencyRead::Absent => {}
                DependencyRead::Bad(why) => return Err(CliError::Text { what: *why }),
            }
        }
        let fresh = self.fresh.pin();
        let mut puts: std::vec::Vec<(Key, bool, std::vec::Vec<u8, Global>), Global> = std::vec::Vec::new_in(Global);
        let mut not_put = NotPut::default();
        for (_, one) in fresh.iter() {
            // Only a dependency's: a member's head moves with its crate's every edit, so it
            // stays the request's. Its crate is the one the freezing worker's names gave
            // (`RequestHeads::publish`), never guessed from the cell key's bytes.
            let Some(krate) = one.krate.and_then(|key| crate_keyed(crates, key)) else {
                not_put.unlisted = not_put.unlisted.saturating_add(1);
                continue;
            };
            // A member's, or a dependency's whose closure holds a member (lane H9: not valid by
            // its key, [`CrateAt::closed`]): counted with the members'.
            if !krate.closed {
                not_put.member = not_put.member.saturating_add(1);
                continue;
            }
            let Some(names_at) = self.names_of(&krate) else {
                not_put.no_names = not_put.no_names.saturating_add(1);
                continue;
            };
            let mut payload = std::vec::Vec::with_capacity_in(KEY_BYTES + one.bytes.len(), Global);
            payload.extend_from_slice(&names_at.0.0);
            payload.extend_from_slice(one.bytes);
            puts.push((dependency_key(names_at, one.cell, tool), krate.toolchain, payload));
        }
        puts.sort_unstable_by_key(|(key, _, _)| *key);
        puts.dedup_by_key(|(key, _, _)| *key);
        for (key, is_toolchain, payload) in puts.iter() {
            // Its payload is this vector's, freed below: none held after the merge.
            ledger.put(Stage::Bodies, *key, &[&payload[..]], 0);
            store.put(&cx, *key, payload)?;
            if *is_toolchain {
                store.keep_with_toolchain(*key)?;
            }
        }
        const RENEW_SENTINEL: blaze_toolchain::ToolchainId = blaze_toolchain::ToolchainId(*b"blaze-renew-no-toolchain-root\0\0\0");
        let mut unlisted = 0u32;
        let listed_count;
        // Set again by every request that lists heads, not only one that puts some: a root a
        // command reaches is renewed (store/roots.rs), so heads every checkout reads never expire
        // while one reads them. When nothing new was added to a live root (the common warm-edit
        // case), renew the root in O(1) without probing, sorting, or hashing all listed keys.
        if puts.is_empty() && self.added_listed.is_empty() && self.had_root && self.listed_from_root <= MOST_CELLS {
            listed_count = self.listed_from_root;
            if self.listed_from_root > 0 && !store.renew_one(root)? {
                return Err(CliError::Text { what: "the dependency heads' root expired before merge (engine_seeds.rs `merge`)" });
            }
        } else if !puts.is_empty() || !self.added_listed.is_empty() || self.listed_from_root > 0 {
            let mut keys: std::vec::Vec<Key, Global> = std::vec::Vec::with_capacity_in(self.listed_from_root + self.added_listed.len() + puts.len(), Global);
            if let Some(existing) = store.named(root)? {
                if existing.len() % 32 != 0 {
                    return Err(CliError::Text { what: "the dependency heads' root lists bytes that are no whole keys (engine_seeds.rs `root_name`)" });
                }
                for chunk in existing.chunks_exact(32) {
                    let mut key = [0u8; 32];
                    key.copy_from_slice(chunk);
                    keys.push(Key(ContentId(key)));
                }
            } else if self.had_root && self.listed_from_root > 0 {
                return Err(CliError::Text { what: "the dependency heads' root expired before merge (engine_seeds.rs `merge`)" });
            }
            if !self.added_listed.is_empty() {
                keys.extend_from_slice(&self.added_listed);
                keys.sort_unstable();
                keys.dedup();
            }
            listed_count = keys.len();
            keys.extend(puts.iter().map(|(key, _, _)| *key));
            keys.sort_unstable();
            keys.dedup();
            // Rule 18: the root lists at most a table's cells; past it, counted and said.
            if keys.len() > MOST_CELLS {
                unlisted = u32::try_from(keys.len() - MOST_CELLS).unwrap_or(u32::MAX);
                keys.truncate(MOST_CELLS);
            }
            store.pin(root, &keys)?;
        } else {
            listed_count = 0;
        }
        Ok(SeedCounts {
            table: self.heads.count(),
            taken: u32::try_from(self.taken.pin().len()).unwrap_or(u32::MAX),
            read: u32::try_from(self.read.pin().len()).unwrap_or(u32::MAX),
            found,
            frozen: u32::try_from(fresh.len()).unwrap_or(u32::MAX),
            twice: self.twice.load(Ordering::Relaxed),
            listed: u32::try_from(listed_count).unwrap_or(u32::MAX),
            looked,
            put: u32::try_from(puts.len()).unwrap_or(u32::MAX),
            unlisted,
            not_put,
            fresh: fresh_digest(&self.fresh),
            bodies,
            // The Heads stage's, set by the command after the merge (engine_check.rs).
            units: crate::engine_heads::UnitCounts::default(),
            once: crate::engine_heads_once::OnceCounts::default(),
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

/// Cell `cell`'s run in one head task's payload ("want bytes, the cell, a u32 LE length, then
/// the frozen bytes", one run a (unit, cell): engine_heads.rs `heads_of`, `run_items`): where
/// its frozen bytes start in `payload` and their length.
fn run_of(payload: &[u8], cell: Cell) -> Result<Option<(usize, usize)>, &'static str> {
    let mut found = None;
    each_run(payload, &mut |_, this, from, len| {
        if found.is_none() && this == &cell.0[..] {
            found = Some((from, len));
        }
    })?;
    Ok(found)
}

/// One head task's payload split in two: its RUNS (from byte 0, so a run's offsets are the
/// payload's) and its RUN FACTS (engine_body_facts.rs's form: the constructors its entry's
/// ADTs give, engine_heads.rs `ctor_facts`), which come after the runs and are followed by
/// their length (u32 LE), the payload's last 4 bytes. A payload whose last 4 bytes give no
/// length within it is refused by name.
pub(crate) fn runs_from(payload: &[u8]) -> Result<(&[u8], &[u8]), &'static str> {
    const NO_FACTS: &str = "a head task's payload does not end in its run facts and their length (engine_heads.rs `ctor_facts`, engine_seeds.rs `runs_from`)";
    let Some((rest, len)) = payload.split_last_chunk::<4>() else { return Err(NO_FACTS) };
    let len = usize::try_from(u32::from_le_bytes(*len)).map_err(|_| NO_FACTS)?;
    let Some(facts_at) = rest.len().checked_sub(len) else { return Err(NO_FACTS) };
    Ok(rest.split_at(facts_at))
}

/// Each run of one head task's payload ([`runs_from`]'s runs): its want's bytes, its cell's,
/// where its frozen bytes start in `payload` and their length; a payload that is not whole runs
/// then its facts is refused.
pub(crate) fn each_run(payload: &[u8], each: &mut dyn FnMut(&[u8], &[u8], usize, usize)) -> Result<(), &'static str> {
    const MALFORMED: &str = "a head task's payload is not runs of a cell's want, its cell, a length and its frozen bytes (engine_heads.rs `heads_of`)";
    let (runs, _) = runs_from(payload)?;
    let mut at = 0usize;
    while at < runs.len() {
        let cell_at = at + WANT_BYTES;
        let len_at = cell_at + CELL_BYTES;
        let (Some(want), Some(this), Some(len)) = (runs.get(at..cell_at), runs.get(cell_at..len_at), runs.get(len_at..len_at + 4)) else { return Err(MALFORMED) };
        let len = u32::from_le_bytes([len[0], len[1], len[2], len[3]]) as usize;
        let from = len_at + 4;
        if runs.get(from..from + len).is_none() {
            return Err(MALFORMED);
        }
        each(want, this, from, len);
        at = from + len;
    }
    Ok(())
}

/// A want's unit: its key's crate prefix (front2 tables/head.rs `want_bytes`, bytes 1 to 8;
/// engine_heads.rs `head_cell` holds the same bytes).
fn unit_of(want: Want) -> [u8; 8] {
    unit_in(&want_bytes(want))
}

/// The unit of a want's bytes ([`unit_of`]).
fn unit_in(want: &[u8]) -> [u8; 8] {
    let mut unit = [0u8; 8];
    if let Some(bytes) = want.get(1..9) {
        unit.copy_from_slice(bytes);
    }
    unit
}

/// How many units a refusal names at most ([`Units`]); past it, counted.
const MOST_UNITS_SAID: usize = 4;

/// The units a head task's record froze its heads under (each run's want's unit), sorted: what
/// a reader of another unit is told (rule 19).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Units {
    said: [[u8; 8]; MOST_UNITS_SAID],
    /// How many units, past [`MOST_UNITS_SAID`] too.
    count: u32,
}

impl Units {
    /// The units of `payload`'s runs when they are some and `reader`'s is none of them (a
    /// reader whose unit the task froze under, but not this cell: `None`, its head's own lack).
    /// No allocation: a payload's runs come a unit at a time, units in prefix order
    /// (engine_heads.rs `UnitsMade::take`), so each unit starts where the one before ends.
    fn other_than(payload: &[u8], reader: [u8; 8]) -> Result<Option<Units>, &'static str> {
        let mut units = Units { said: [[0u8; 8]; MOST_UNITS_SAID], count: 0 };
        let mut last: Option<[u8; 8]> = None;
        let mut theirs = false;
        each_run(payload, &mut |want, _, _, _| {
            let unit = unit_in(want);
            theirs |= unit == reader;
            if last != Some(unit) {
                if let Some(slot) = units.said.get_mut(usize::try_from(units.count).unwrap_or(usize::MAX)) {
                    *slot = unit;
                }
                units.count = units.count.saturating_add(1);
                last = Some(unit);
            }
        })?;
        if units.count == 0 || theirs {
            return Ok(None);
        }
        Ok(Some(units))
    }

    /// The units named (the first [`MOST_UNITS_SAID`]).
    fn shown(&self) -> &[[u8; 8]] {
        &self.said[..usize::try_from(self.count).unwrap_or(usize::MAX).min(MOST_UNITS_SAID)]
    }
}

/// A unit's crate prefix as the words say it: 16 hex digits.
struct UnitWords([u8; 8]);

impl core::fmt::Display for UnitWords {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        for byte in self.0.iter() {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

/// Where a stretch's names place a want's head ([`StretchHeads::place`]).
#[derive(Clone, Copy)]
enum Placed {
    /// A member crate's head, written in a file or by a macro under an entry: its entry's head
    /// task's cell.
    Member { id: CellId, slot: Name },
    /// A member crate's head that no head task holds, and why (engine_heads.rs
    /// `HeadAt::Unlisted`): its reader is refused by these words.
    Unlisted(&'static str),
    /// A dependency's head (a crate with no head tasks), or one of no crate the names hold.
    Dependency,
    /// An item declared in a body: its body's own store makes its head (front2 tables/seeds.rs
    /// `of_the_body`), so it is asked here only under the plant `BodyLocals::AskTheRequest`.
    OfTheBody,
}

/// One stretch's view of the request's frozen heads (front2 tables/seeds.rs `RequestHeads`):
/// a want placed ONE way, by the stretch's own names, the way a body's read of the head is
/// placed (engine_heads.rs `Owners::place`, engine_bodies.rs `widen`) and a head task names its
/// cells (`heads_of_crate`, `head_cell`): a member's head is its entry's task's cell, named by
/// the item's name, never by a key of the reader's unit; a dependency's is its record under
/// [`dependency_key`]. One worker's (its `Owners` fills on first ask), over the shared table.
pub(crate) struct StretchHeads<'t, 's: 'n, 'n> {
    table: &'t RequestTable<'t>,
    /// The heads the Heads stage could not freeze, each with its own reason
    /// (engine_head_notes.rs `Failed`): what a refusal of such a head quotes.
    failed: &'t Failed,
    names: &'n Records<'n>,
    owners: RefCell<Owners<'s>>,
    g: Alloc<'static>,
}

impl<'t, 's, 'n> StretchHeads<'t, 's, 'n> {
    /// `subjects`: the member crates' files (engine_heads.rs `Subjects`), as the Bodies stage
    /// lists them for `widen`; `over`: the crates `names` was opened over, in its order (their
    /// expansion directories place a written head); `failed`: the heads the Heads stage could
    /// not freeze.
    pub(crate) fn new(table: &'t RequestTable<'t>, failed: &'t Failed, names: &'n Records<'n>, subjects: &'s Subjects, over: crate::run_names::ScopeCrates<'s, 's>, listings: &'s crate::engine_heads::Listings, g: Alloc<'static>) -> StretchHeads<'t, 's, 'n> {
        StretchHeads { table, failed, names, owners: RefCell::new(Owners::new(subjects, over, listings, g)), g }
    }

    /// Where this stretch's bodies make a need of an item declared in the body (the request's:
    /// [`RequestTable::body_locals`]), for its `RequestSeed` (engine_bodies.rs).
    pub(crate) fn body_locals(&self) -> BodyLocals {
        self.table.body_locals
    }

    /// Where `want`'s head is, by this stretch's names: the one placing (module docs).
    fn place(&self, want: Want) -> Placed {
        match self.owners.borrow_mut().place(self.names, &head_cell_id(want), self.g) {
            Some(HeadAt::Cell { cell, file }) => Placed::Member { id: cell, slot: file },
            Some(HeadAt::Unlisted { why, .. }) => Placed::Unlisted(why),
            Some(HeadAt::OfTheBody(_)) => Placed::OfTheBody,
            Some(HeadAt::Dependency(_)) | None => Placed::Dependency,
        }
    }

    /// The crate `want`'s item is in, as this stretch's names give it.
    fn krate(&self, want: Want) -> Option<blaze_id::CrateItemKey> {
        NamesSource::crate_of(self.names, want.key()).and_then(|k| NamesSource::crate_key(self.names, k))
    }

    /// Why member cell `id` (of want `cell`) was not taken from its head task's record, as
    /// the body's typed refusal (rule 19), its words made ONCE a cell a request: the first kept
    /// in the table's `refused` map is every asker's (another worker's for the same cell is the
    /// same: the read and the stage's failures are the request's, not the asker's).
    fn not_theirs(&self, cell: Want, id: CellId, slot: Name) -> NotTheirs {
        if let Some(why) = self.table.refused.pin().get(&id).copied() {
            return why;
        }
        let why = self.not_theirs_now(cell, id, slot);
        match self.table.refused.pin().try_insert(id, why) {
            Ok(_) => why,
            Err(occupied) => *occupied.current,
        }
    }

    /// [`StretchHeads::not_theirs`]'s words, made: a head its task ran for this unit and could
    /// not freeze is that failure, its task's own reason quoted (the project's head, the body
    /// not checked: engine_bodies.rs `unmade_head`); else another unit's runs only, or which
    /// lack (the engine's own inconsistency, the body's failure).
    fn not_theirs_now(&self, cell: Want, id: CellId, slot: Name) -> NotTheirs {
        // First: a failed head defines its cell but writes no run (engine_heads.rs `heads_of`),
        // so the read alone would call it "no such cell", or another unit's when every head of
        // this unit failed.
        if let Some(refused) = self.failed.find(id.of, id.cell) {
            return NotTheirs::Failed { said: self.could_not_freeze(cell, refused.cause.words) };
        }
        let read = self.table.members.pin().get(&id).copied();
        match read {
            Some(MemberRead::Lacked(Lack::OtherUnits(units))) => NotTheirs::OtherUnits { said: self.other_units(cell, units) },
            Some(MemberRead::Lacked(Lack::NoTask)) => NotTheirs::NoCell { said: self.no_task(cell, id.of, slot) },
            Some(MemberRead::Lacked(lack)) => NotTheirs::NoCell { said: self.no_cell(cell, lack.words()) },
            Some(MemberRead::Bad(why)) => NotTheirs::NoCell { said: self.no_cell(cell, why) },
            Some(MemberRead::Found { .. }) => NotTheirs::NoCell { said: self.no_cell(cell, "its head task's record was found and its bytes could not be read back from the store's snapshot") },
            None => NotTheirs::NoCell { said: self.no_cell(cell, "its head task's record was not read") },
        }
    }

    /// The refusal's words for a member head its head task ran for and could not freeze: its
    /// want's kind and the task's own reason `why` (rule 19). The head's path is the body
    /// note's ("the head of `X`: ", engine_bodies.rs `unmade_head`), not said twice.
    fn could_not_freeze(&self, cell: Want, why: &'static str) -> &'static str {
        let words = bstd::mem::format_in!(
            self.table.heap.alloc(),
            "a member crate's head ({}); its head task could not freeze it: {why} (said at its head task; a body never freezes a member's head: engine_seeds.rs `StretchHeads::known`)",
            kind_words(cell)
        );
        blaze_own::Box::leak_static(words.into_boxed_str())
    }

    /// Resolve the listing's logical file slot in the defining unit. For a macro-written
    /// head this is the file of the covering call, where the signature was planned.
    fn no_task(&self, cell: Want, entry: Name, slot: Name) -> &'static str {
        let path = self.owners.borrow().file_path(self.names, cell, slot, self.g);
        let why = Lack::NoTask.words();
        match path.as_deref().and_then(|path| path.strip_prefix("/0/")) {
            Some(path) => {
                // An explicitly named mod.rs is its own slot. Only a directory module found
                // through the other slot has a different FileItems name.
                let (stem, ext) = match path.strip_suffix("/mod.rs") {
                    Some(stem) if Name::file(path.as_bytes()) != slot => (stem, ".rs"),
                    _ => (path, ""),
                };
                self.no_cell(cell, format_args!("{why} for covering {entry} in `{path}` (slot `{stem}{ext}`, {slot})"))
            }
            None => self.no_cell(cell, format_args!("{why} for covering {entry} in slot {slot}, whose path is missing from the defining unit's records")),
        }
    }

    /// The head's path, its kind and the defining unit requested by `cell`.
    fn no_cell(&self, cell: Want, why: impl core::fmt::Display) -> &'static str {
        let shown = ItemShown::of_lib(self.names, cell.key());
        crate::engine_head_notes::leak(
            self.table.heap.alloc(),
            format_args!("the head of `{shown}` ({}) is a member crate's, and its head task defines no cell of it for the requested defining unit {} this request: {why} (a body never freezes a member's head; engine_seeds.rs `StretchHeads::known`)", kind_words(cell), UnitWords(unit_of(cell))),
        )
    }

    /// The head's path and kind, the units that froze it and the defining unit `cell` requested.
    fn other_units(&self, cell: Want, units: Units) -> &'static str {
        use core::fmt::Write as _;
        let shown = ItemShown::of_lib(self.names, cell.key());
        let mut words = bstd::mem::format_in!(self.table.heap.alloc(), "the head of `{shown}` ({}) was frozen by its head task under unit(s) ", kind_words(cell));
        for (at, unit) in units.shown().iter().enumerate() {
            let _ = write!(words, "{}{}", if at == 0 { "" } else { ", " }, UnitWords(*unit));
        }
        let more = units.count.saturating_sub(u32::try_from(units.shown().len()).unwrap_or(u32::MAX));
        if more > 0 {
            let _ = write!(words, " and {more} more");
        }
        let _ = write!(words, " and not under this body's unit for the referenced crate (requested defining unit {}): a head is frozen once a unit that holds its item (engine_heads.rs `head_cell`, `run_items`)", UnitWords(unit_of(cell)));
        blaze_own::Box::leak_static(words.into_boxed_str())
    }
}

/// [`Placed::OfTheBody`]'s refusal: an item declared in a body asked of the request.
const OF_THE_BODY: &str = "it is declared in a body, whose own store makes its head (front2 tables/seeds.rs `of_the_body`); no head task holds it and the request never freezes it";

/// A want's kind as the profile says it.
fn kind_words(cell: Want) -> &'static str {
    match cell {
        Want::Head(_) => "Head",
        Want::Adt(_) => "Adt definition",
        Want::Impl(_) => "Impl header",
        Want::Fn(_) => "Fn sig",
        Want::Members(_) => "trait Members",
        Want::MemberFn(..) => "impl MemberFn sig",
    }
}

impl KnownHeads for StretchHeads<'_, '_, '_> {
    /// A MEMBER crate's head is its head task's run for the requested defining unit, or the body is
    /// refused, typed (`Known::Refused`): a body never freezes a member's head (rule 3; the
    /// lazy freeze the table replaced). Only a DEPENDENCY's head is frozen by the asker when
    /// neither the store nor another worker holds it (one unit, frozen once a machine).
    fn known(&self, cell: Want) -> Known<'_> {
        let table = self.table;
        match self.place(cell) {
            Placed::Member { id, slot } => match table.member(id, unit_of(cell)) {
                Some(bytes) => Known::Bytes(bytes),
                None => Known::Refused(self.not_theirs(cell, id, slot)),
            },
            // A head no head task holds (its walk up the calls that wrote it failed, or no entry
            // covers it): refused by its reason, never frozen by the body (rule 3; before lane
            // RH4 a macro-written head was frozen here and counted).
            Placed::Unlisted(why) => Known::Refused(NotTheirs::NoCell { said: self.no_cell(cell, why) }),
            // An item declared in the asking body never reaches here: its body's own store makes
            // it (front2 tables/seeds.rs `of_the_body`, lane RU4), unless a test plants
            // `BodyLocals::AskTheRequest`, which is refused here by name as the real projects
            // failed.
            Placed::OfTheBody => Known::Refused(NotTheirs::NoCell { said: self.no_cell(cell, OF_THE_BODY) }),
            Placed::Dependency => {
                let held = self.krate(cell).and_then(|krate| table.dependency(cell, krate));
                // Not a head task's: a head read outside the table (rule 53: found or frozen,
                // once). The body seed supplies the dependency fn's signature facts from its
                // stretch memo when a const evaluator reaches the body.
                table.mark_read(cell_key(cell));
                match held.or_else(|| table.fresh_of(cell)) {
                    Some(bytes) => Known::Bytes(bytes),
                    None => Known::Freeze,
                }
            }
        }
    }
}

impl RequestHeads for StretchHeads<'_, '_, '_> {
    fn publish(&self, cell: Want, krate: Option<blaze_id::CrateItemKey>, bytes: &[u8]) -> Result<(), SeedError> {
        match self.place(cell) {
            // `known` refuses every member head the table lacks before a freeze, so a member's
            // head here is the engine's own fault: said and refused, never kept (rule 3).
            Placed::Member { .. } | Placed::Unlisted(_) | Placed::OfTheBody => {
                Err(SeedError::NoMemberCell { cell, said: self.no_cell(cell, "a body's freeze reached it though a member's head is only its head task's (engine_seeds.rs `StretchHeads::known`)") })
            }
            Placed::Dependency => self.table.publish(cell, krate, bytes),
        }
    }

    fn refused(&self, cell: Want, error: &FreezeError) {
        let g = self.table.heap.alloc();
        let head = crate::engine_head_consts::head_label(self.names, cell, g);
        let why = crate::engine_head_notes::leak(g, format_args!("{}", crate::engine_head_notes::Reason(error)));
        self.table.keep_first_failure(cell, FirstFailure { head, why });
    }
}

/// The most first-failed freezes a request keeps ([`RequestTable::first_failure`]): one a cell
/// the table can hold.
const MOST_FIRST_FAILED: usize = MOST_CELLS;

/// A head whose first freeze failed this request: its path as a finding shows it, and its own
/// reason in words that hold no key (engine_head_notes.rs `Reason`, so equal reasons group).
#[derive(Clone, Copy, Debug)]
pub(crate) struct FirstFailure {
    pub(crate) head: &'static str,
    pub(crate) why: &'static str,
}

/// What [`RequestTable::first_failure`] knows of a cell.
#[derive(Clone, Copy, Debug)]
pub(crate) enum FirstFailed {
    Kept(FirstFailure),
    /// Not kept: the table was past [`MOST_FIRST_FAILED`] (`kept` reasons kept).
    PastCap { kept: usize },
    /// No freeze of it failed this request, as the table was told.
    None,
}

impl RequestTable<'_> {
    /// `bytes`, frozen now for `cell` by one worker (front2 tables/seeds.rs `RequestHeads`).
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
        let lent: &'static [u8] = copy.leak_static();
        match fresh.try_insert(id, Fresh { cell, krate, bytes: lent }) {
            Ok(_) => Ok(()),
            Err(occupied) => self.again(cell, occupied.current.bytes, bytes),
        }
    }
}

#[cfg(test)]
mod renew_tests {
    use super::*;
    use blaze_cas::plain::Settings;
    use bstd::io::{Io, Limits, SimWorld};
    use bstd::mem::TASK_FRAME;
    use bstd::test::BUDGET;

    /// When a warm edit freezes 0 new dependency heads, `RequestTable::merge` renews the existing
    /// dependency-heads root in O(1) with 0 CAS root probes instead of probing every listed key.
    #[test]
    fn merge_with_no_new_heads_renews_root_with_zero_probes() {
        let io = Io::simulated(SimWorld::new(47, Limits::USUAL));
        let cx = io.cx(BUDGET, TASK_FRAME).expect("root context");
        let home = crate::store::Home::of(&cx, &io.need_fs().expect("filesystem"), Some("/h")).expect("home");
        let heap = blaze_own::testing::heap();
        let frame = cx.frame();
        let erased = blaze_own::Erased::new(frame);
        let clock = io.need_clock().expect("clock");
        let err = io.need_err().expect("stderr");
        let lent = home.open_store(&Settings::default(), crate::store::Facts { boot: Some(1), nonce: 47 }, heap.alloc()).expect("store");
        let mut store = CommandStore::new(&cx, &lent, false, 0, None, clock.clone(), &err, erased.handle()).expect("command store");
        let root = "engine-dependency-heads.test";

        // Seed 500 committed dependency head records under `root`.
        let mut initial_keys = std::vec::Vec::new();
        for i in 0..500u32 {
            let bytes = i.to_le_bytes();
            let key = Key(ContentId::of(DEPENDENCY_DOMAIN, &bytes));
            store.put(&cx, key, &bytes).expect("put head");
            initial_keys.push(key);
        }
        store.pin(root, &initial_keys).expect("pin initial root");
        lent.commit().expect("commit initial heads");

        // Warm edit: 0 new heads frozen, empty `list_too` -> 0 root probes!
        let mut table = RequestTable::of(
            &mut store,
            HeadEntries::Built { entries: std::vec::Vec::new(), failed: std::vec::Vec::new() },
            (std::vec::Vec::new(), CrateNames::Listed),
            root,
            Tool(ContentId([1; 32])),
            heap,
        ).expect("open request table");
        table.list_too(&[]);
        let probes_before = lent.put_tally().disk.root_probes;
        let mut ledger = crate::engine_store::Ledger::new();
        let counts = table.merge(&cx, &mut store, root, &mut ledger, SeedTally::default()).expect("merge warm");
        let probes_after = lent.put_tally().disk.root_probes;
        assert_eq!(counts.listed, 500, "reports all 500 listed heads");
        assert_eq!(counts.put, 0);
        assert_eq!(probes_after - probes_before, 0, "warm merge with 0 new heads probes 0 root keys");

        // Adding 1 new key via `list_too` pins the updated 501-key root.
        let extra = Key(ContentId::of(DEPENDENCY_DOMAIN, b"extra-head"));
        store.put(&cx, extra, b"extra-head").expect("put extra head");
        let mut table2 = RequestTable::of(
            &mut store,
            HeadEntries::Built { entries: std::vec::Vec::new(), failed: std::vec::Vec::new() },
            (std::vec::Vec::new(), CrateNames::Listed),
            root,
            Tool(ContentId([1; 32])),
            heap,
        ).expect("open request table 2");
        table2.list_too(&[extra]);
        let counts2 = table2.merge(&cx, &mut store, root, &mut ledger, SeedTally::default()).expect("merge with extra");
        lent.commit().expect("commit updated root");
        assert_eq!(counts2.listed, 501);
        let stored = store.named(root).expect("read updated root").expect("root live");
        assert_eq!(stored.len(), 501 * 32, "updated root keeps all 501 keys");
    }
}
