//! A CLOSED DEPENDENCY'S HEAD IS FOUND BY ITS KEY (engine_heads_once.rs, lane H9), proven in the
//! deterministic simulation (rule 43, `blaze_sim`). A child of main.rs's tests.
//!
//! THE MODEL: one closed dependency crate of `CELLS` head cells; a model store (committed rows,
//! and puts not yet committed, which a crash drops); each cell's freeze a pure function of the
//! cell and the crate's features. A command is two workers asking every cell, interleaved as the
//! tape draws (a look, then on a miss a freeze and a put into the table), the phase's puts
//! (`HeadsOnce::pending_puts`, `mark_put`), a reader asking every cell again, then either a
//! CRASH between the put and the commit or the commit (`HeadsOnce::committed`) and a reader
//! once more. Two commands run on one home, their features equal or not as the tape draws.
//!
//! THE ORACLE: every answer a reader is handed is byte-equal to the model's freeze of that cell
//! under that command's features; a cell put is still answered before the commit; the store never
//! holds two values under one key; the first command finds nothing; the second, after a commit
//! and with equal features, finds every cell and freezes none (the floor), and after a crash or a
//! change of features finds none.
//!
//! CONTROLS (rule 23): the planted "taken as stored at the put" (`OncePlant::ForgetAtPut`, a
//! reader handed to the store before its commit is durable) and a key that omits the features
//! (two values under one key) are each caught by the oracle.

use super::engine_check_tests::opts;
use super::*;

use blaze_drive::Key;
use blaze_drive::key::Tool;
use blaze_id::{CrateItemKey, ItemKey};
use blaze_ids::ContentId;
use blaze_ladder::front2::heads::Want;
use blaze_ladder::front2::tables::head::{Known, KnownHeads, WANT_BYTES, want_bytes};
use blaze_own::{Alloc, Vec};
use blaze_sim::{Broke, Config, Tape, check as sim_check, control};

use crate::engine_heads_once::{HeadsOnce, OncePlant, Sources};
use crate::engine_seeds::{CrateAt, DependencyFinder, HeadRecords, KEY_BYTES, Keyed, Unreadable};

/// The dependency crate's cells.
const CELLS: usize = 3;
/// Its key's prefix: every cell's item key begins with it.
const PREFIX: [u8; 8] = [7, 7, 7, 7, 7, 7, 7, 7];

/// The rule, or one of the two planted bugs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Plant {
    Rule,
    /// The table's own planted bug (`OncePlant::ForgetAtPut`).
    ForgetAtPut,
    /// The model's names key omits the crate's features: two values under one key.
    FeatureBlindKey,
}

fn oracle(what: &'static str) -> Broke {
    Broke::Oracle(std::string::String::from(what))
}

/// The request's heap, by the helpers' own form (engine_check_tests.rs `opts`).
fn heap() -> &'static blaze_own::Heap {
    let io = Io::simulated(SimWorld::new(1, Limits::USUAL));
    opts(&io, 2, false).heap
}

/// Cell `at` of the dependency crate.
fn cell(at: usize) -> Want {
    let mut bytes = [0u8; 32];
    bytes[..8].copy_from_slice(&PREFIX);
    bytes[8] = u8::try_from(at).unwrap_or(u8::MAX);
    Want::Head(ItemKey(ContentId(bytes)))
}

/// The crate's names record's key: of its features (the rule), or not (the planted bug).
fn names_at(features: u8, plant: Plant) -> Key {
    let seen = match plant {
        Plant::FeatureBlindKey => 0,
        Plant::Rule | Plant::ForgetAtPut => features,
    };
    Key(ContentId::of("blaze-cli engine heads found tests names", &[seen]))
}

/// `parts` joined, lent from `g` for the test's run.
fn joined(parts: &[&[u8]], g: Alloc<'static>) -> &'static [u8] {
    let mut out: Vec<u8, Alloc<'static>> = Vec::new_in(g);
    for part in parts.iter() {
        out.extend_from_slice(part);
    }
    out.leak_static()
}

/// The model's freeze of `cell` under `features`: a function of both, as head.rs `freeze_seeded`
/// is of the cell and the names.
fn freeze(cell: Want, features: u8, g: Alloc<'static>) -> &'static [u8] {
    joined(&[&want_bytes(cell), &[features]], g)
}

/// The committed records, sorted by key: what a command's snapshot reads.
#[derive(Clone, Copy)]
struct Records {
    rows: &'static [(Key, &'static [u8])],
}

impl HeadRecords for Records {
    fn record(&self, key: Key) -> Result<Option<&[u8]>, Unreadable> {
        Ok(self.rows.binary_search_by_key(&key, |(k, _)| *k).ok().map(|at| self.rows[at].1))
    }

    /// The model's form: the names key, then the cell's bytes, then the rest of the freeze.
    fn head<'b>(&self, bytes: &'b [u8], names_at: Key, cell: Want) -> Result<&'b [u8], &'static str> {
        if bytes.len() < KEY_BYTES + WANT_BYTES || bytes[..KEY_BYTES] != names_at.0.0 || bytes[KEY_BYTES..KEY_BYTES + WANT_BYTES] != want_bytes(cell) {
            return Err("a model record is not the head its key names");
        }
        Ok(&bytes[KEY_BYTES..])
    }
}

/// The home's store: committed rows, and puts a crash drops.
struct ModelStore {
    committed: Vec<(Key, &'static [u8]), Alloc<'static>>,
    pending: Vec<(Key, &'static [u8]), Alloc<'static>>,
    g: Alloc<'static>,
}

impl ModelStore {
    fn new(g: Alloc<'static>) -> ModelStore {
        ModelStore { committed: Vec::new_in(g), pending: Vec::new_in(g), g }
    }

    /// One put: two values under one key are the store's refusal (rule 21).
    fn put(&mut self, key: Key, bytes: &'static [u8]) -> Result<(), Broke> {
        let held = self.committed.iter().chain(self.pending.iter()).find(|(k, _)| *k == key);
        match held {
            Some((_, was)) if *was != bytes => Err(oracle("the store was handed two values under one key")),
            Some(_) => Ok(()),
            None => {
                self.pending.push((key, bytes));
                Ok(())
            }
        }
    }

    fn commit(&mut self) {
        let pending = core::mem::replace(&mut self.pending, Vec::new_in(self.g));
        self.committed.extend_from_slice(&pending);
        self.committed.sort_unstable_by_key(|(k, _)| *k);
    }

    fn crash(&mut self) {
        self.pending.clear();
    }

    fn snapshot(&self) -> Records {
        let mut rows: Vec<(Key, &'static [u8]), Alloc<'static>> = Vec::new_in(self.g);
        rows.extend_from_slice(&self.committed);
        Records { rows: rows.leak_static() }
    }
}

/// What one command did.
#[derive(Clone, Copy, Debug)]
struct Ran {
    found: u32,
    frozen: u32,
    crashed: bool,
}

/// Every cell asked once by a reader of the table: each answer byte-equal to the model's freeze.
fn read_all(table: &HeadsOnce<Records>, features: u8, g: Alloc<'static>, when: &'static str) -> Result<(), Broke> {
    for at in 0..CELLS {
        match table.known(cell(at)) {
            Known::Bytes(bytes) if bytes == freeze(cell(at), features, g) => {}
            Known::Bytes(_) => return Err(oracle("a reader was handed other bytes than its cell's freeze")),
            Known::Freeze | Known::Refused(_) => return Err(oracle(when)),
        }
    }
    Ok(())
}

/// One command on `store`'s home (module docs).
fn command(tape: &Tape, store: &mut ModelStore, features: u8, plant: Plant, heap: &'static blaze_own::Heap) -> Result<Ran, Broke> {
    let g = heap.alloc();
    let once_plant = match plant {
        Plant::ForgetAtPut => OncePlant::ForgetAtPut,
        Plant::Rule | Plant::FeatureBlindKey => OncePlant::Rule,
    };
    let mut table: HeadsOnce<Records> = HeadsOnce::empty(heap, once_plant, Sources::Job);
    let mut crates = blaze_own::Vec::new_in(heap.shared());
    let mut krate = [0u8; 32];
    krate[..8].copy_from_slice(&PREFIX);
    crates.push(CrateAt { prefix: PREFIX, key: CrateItemKey(ContentId(krate)), names_at: Some(names_at(features, plant)), member: false, closed: true, toolchain: false });
    let tool = Tool(ContentId::of("blaze-cli engine heads found tests tool", b""));
    table.find_dependencies(DependencyFinder::new(store.snapshot(), crates, tool));
    // Two workers, each asking every cell (the second in the other order), interleaved.
    let mut next = [0usize; 2];
    while next[0] < CELLS || next[1] < CELLS {
        let w = match (next[0] < CELLS, next[1] < CELLS) {
            (true, true) => usize::from(tape.bool("the second worker steps now")),
            (true, false) => 0,
            _ => 1,
        };
        let at = if w == 0 { next[0] } else { CELLS - 1 - next[1] };
        next[w] += 1;
        let want = cell(at);
        match table.known(want) {
            Known::Bytes(bytes) if bytes == freeze(want, features, g) => {}
            Known::Bytes(_) => return Err(oracle("a reader was handed other bytes than its cell's freeze")),
            Known::Refused(_) => return Err(oracle("a closed dependency head was refused")),
            Known::Freeze => {
                let Keyed::Closed { key, names_at, .. } = table.keyed(want) else {
                    return Err(oracle("a closed dependency's cell has no key"));
                };
                let bytes = freeze(want, features, g);
                if table.put_closed(want, bytes, ContentId::of("blaze-cli engine heads found tests meaning", bytes), key, names_at).is_err() {
                    return Err(oracle("two freezes of one cell were refused as unequal"));
                }
            }
        }
    }
    // The phase's commit: the puts, sorted, each over its names key.
    let puts = table.pending_puts(g);
    let mut keys: Vec<Key, Alloc<'static>> = Vec::new_in(g);
    for (key, names_at, bytes) in puts.iter() {
        store.put(*key, joined(&[&names_at.0.0, *bytes], g))?;
        keys.push(*key);
    }
    table.mark_put(&keys);
    read_all(&table, features, g, "a dependency head put but not yet committed was not answered after its put")?;
    let counts = table.counts();
    if tape.bool("a crash between the put and the commit") {
        store.crash();
        return Ok(Ran { found: counts.deps_found, frozen: counts.deps_frozen, crashed: true });
    }
    store.commit();
    table.committed(store.snapshot());
    read_all(&table, features, g, "a committed dependency head was not answered from the store")?;
    Ok(Ran { found: counts.deps_found, frozen: counts.deps_frozen, crashed: false })
}

/// Two commands on one home (module docs), the oracle over both.
fn two_commands(tape: &Tape, plant: Plant, heap: &'static blaze_own::Heap) -> Result<(), Broke> {
    let mut store = ModelStore::new(heap.alloc());
    let first_features = u8::from(tape.bool("the first command's features"));
    let second_features = u8::from(tape.bool("the second command's features"));
    let first = command(tape, &mut store, first_features, plant, heap)?;
    if first.found != 0 || first.frozen < CELLS as u32 {
        return Err(oracle("the first command on an empty home found a head or froze fewer than its cells"));
    }
    let second = command(tape, &mut store, second_features, plant, heap)?;
    let kept = !first.crashed && first_features == second_features;
    match kept {
        true if second.found != CELLS as u32 || second.frozen != 0 => Err(oracle("the second command on the same home froze a dependency head again")),
        false if second.found != 0 => Err(oracle("the second command found a head no commit made under its key")),
        _ => Ok(()),
    }
}

/// The invariant: a closed dependency head is frozen once per key (including its features), never treated as stored before commit, and found by key across interleavings and crashes.
/// Expected: passes over every interleaving, crash and change of features the tape draws.
#[test]
fn a_closed_dependency_head_is_frozen_once_a_machine_and_found_by_key() {
    let heap = heap();
    sim_check(Config::cases(500), |tape| two_commands(tape, Plant::Rule, heap));
}

/// CONTROL: a put taken as stored before its commit leaves a reader unanswered.
#[test]
fn the_oracle_catches_a_head_taken_as_stored_before_its_commit() {
    let heap = heap();
    control(Config::cases(2000), Plant::Rule, Plant::ForgetAtPut, "not answered after its put", |tape, plant| two_commands(tape, plant, heap));
}

/// CONTROL: a key that omits the features hands a reader another configuration's head.
#[test]
fn the_oracle_catches_a_key_that_omits_the_features() {
    let heap = heap();
    control(Config::cases(2000), Plant::Rule, Plant::FeatureBlindKey, "other bytes than its cell's freeze", |tape, plant| two_commands(tape, plant, heap));
}
