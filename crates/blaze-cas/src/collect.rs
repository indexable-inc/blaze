//! The store's collector (docs/gc.md, "The store's budget and collector"): what keeps a store
//! under a budget by forgetting the records nothing uses, without a pause.
//!
//! - **A period's end is a scan off the store's thread** ([`PeriodScan`]): the walk of every row
//!   that plans the cold tier's moves also finds the pins ([`Pin`], records of
//!   [`Kind::Pin`]) and what the rooted records name inside the store (a chunked value's
//!   chunks, a derived value's base). The store hands the scan out ([`Store::plan_take`]) and
//!   takes it back ([`Store::plan_put`]); neither walks a row.
//! - **A key is forgotten with its pack** ([`CollectBatch`]): the store's tables are
//!   append-only, so nothing is taken out of them. The collector retires a whole pack as a
//!   settle retires a half-dead one: the records to keep are copied on into the pack being
//!   written, and the head that makes the copies durable names the pack removed. A slot of
//!   the index table that points into a retired pack finds nothing
//!   ([`crate::read::TierView::gone`]), in memory at once and after any reopen.
//! - **Roots.** A record is kept when one of its four flags of use is set (read or made in
//!   this period or the one before; [`crate::Reader::has`] and a put of a value the store
//!   has count), when a pin names it, when any record still in the store names it (a chunked
//!   value's chunks, a derived value's base), when its key was placed at a path and not let
//!   go of, or when it is not found through the index table (a record the delta holds is
//!   newer than the last index).
//!   A pack is sealed only while its caller says no build is under way
//!   ([`Collector::observe`]), so what a build found it never loses before it ends.
//! - **The rule of when** is [`Collector`]: a core on an injected clock that owns no file and
//!   no thread. Its driver runs each batch's [`CollectBatch::work`] wherever it likes (a
//!   worker) and hands it back; on the store's thread a step copies a bounded number of
//!   bytes ([`Pace::copy_most`]) and never compresses, hashes or reads a file.
//!
//! The invariant its simulation checks (`tests/collect.rs`): no record a live root names is
//! ever forgotten; after any crash the store opens and reads every record that was durable
//! and rooted; a build never finds a key and then misses it.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use std::{alloc::Global, collections::VecDeque, sync::Arc, time::Duration};

use blaze_core::{Effect, FileName};
use blaze_hash::{ByHash, ByHashSet};
use blaze_ids::{ContentId, radix};
use blaze_reclaim::enter;

use crate::{
    DEMOTE_FROM, Key, Kind, Loc, Mismatch, Moves, PACK_HEADER, RECORD_HEADER, RecordHeader,
    Reader, Store, Ticket, Unit, Use, derived,
    disk::{self, Form},
    flags::{Flag, Row},
    read::{At, Found, Index, Look, Place, Spot},
    table::{Held, Table},
};

/// What a pin's key is hashed under: a pin is kept under the hash of its own bytes.
const PIN_DOMAIN: &str = "blaze v2 store pin 1";
/// What a pin's bytes start with; the last byte is the layout's number.
const PIN_MAGIC: [u8; 8] = *b"BLZPIN\x00\x01";
/// The fixed part of a pin's bytes: the magic, when, the lengths of its name and its why, and
/// how many keys.
const PIN_FIXED: usize = 8 + 8 + 4 + 4 + 8;
/// What the id of the cold packs' unit is hashed under ([`Store::cold_unit`]).
const COLD_UNIT_DOMAIN: &str = "blaze v2 store cold unit 1";
/// The id the cold packs' unit is listed under in the head.
pub(crate) fn cold_unit_id() -> ContentId {
    ContentId::of(COLD_UNIT_DOMAIN, b"")
}

/// A scan lets go of its task's guard after walking this many bytes of a pack, so the pins of
/// the pages it read are given back as it goes.
const SCAN_SLICE: u64 = 4 << 20;

/// A set of keys the collector keeps whatever their last use, under a name: a record of the
/// store ([`Kind::Pin`]). The pin of a name made last is the name's, whatever its maker's
/// clock did: across runs by `when` (then by its key), which [`Store::pin`] keeps later than
/// every pin of the name the store knows of, and within a run by the order they were made,
/// which a scan that reads an older pin of the name keeps ([`Store::keep_scan`]). One with no
/// keys lets the name go, and is kept itself (a scan roots each name's newest pin) so that no
/// older pin of the name comes back once it is forgotten.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Pin {
    pub name: String,
    /// Why it is kept, for whoever asks what holds the space.
    pub why: String,
    /// When it was made, on its maker's clock (nanoseconds since the Unix epoch).
    pub when: u64,
    pub keys: Vec<Key>,
}

impl Pin {
    /// Its bytes as the store keeps them (layout 1): the magic, `when`, the lengths, the name,
    /// the why, then each key. Every number is little-endian.
    pub fn encode(&self) -> Vec<u8> {
        // alloc: a pin's record, once per pin made
        let mut out = Vec::with_capacity(
            PIN_FIXED + self.name.len() + self.why.len() + 32 * self.keys.len(),
        );
        out.extend_from_slice(&PIN_MAGIC);
        out.extend_from_slice(&self.when.to_le_bytes());
        out.extend_from_slice(&(self.name.len() as u32).to_le_bytes());
        out.extend_from_slice(&(self.why.len() as u32).to_le_bytes());
        out.extend_from_slice(&(self.keys.len() as u64).to_le_bytes());
        out.extend_from_slice(self.name.as_bytes());
        out.extend_from_slice(self.why.as_bytes());
        for key in &self.keys {
            out.extend_from_slice(&key.0.0);
        }
        out
    }

    /// The pin `bytes` hold, or why they are none: other bytes, a layout this blaze does not
    /// read (named), or a length that does not match.
    pub fn decode(bytes: &[u8]) -> Result<Pin, String> {
        if bytes.len() < PIN_FIXED || bytes[..7] != PIN_MAGIC[..7] {
            // alloc: an error
            return Err(format!("{} bytes that do not start as a pin does", bytes.len()));
        }
        if bytes[7] != PIN_MAGIC[7] {
            // alloc: an error
            return Err(format!(
                "a pin of layout {}, where this blaze reads layout {}",
                bytes[7], PIN_MAGIC[7]
            ));
        }
        let u64_at = |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().expect("8 bytes"));
        let u32_at = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().expect("4 bytes"));
        let (when, name, why, keys) =
            (u64_at(8), u32_at(16) as usize, u32_at(20) as usize, u64_at(24));
        let want = (keys as u128) * 32 + (PIN_FIXED + name + why) as u128;
        if want != bytes.len() as u128 {
            // alloc: an error
            return Err(format!(
                "a pin of {} bytes whose lengths say {want}",
                bytes.len()
            ));
        }
        let text = |at: usize, len: usize, what: &str| {
            String::from_utf8(bytes[at..at + len].to_vec()) // alloc: a pin read, once per pin a scan
                // alloc: an error
                .map_err(|_| format!("a pin whose {what} is not UTF-8"))
        };
        let from = PIN_FIXED + name + why;
        Ok(Pin {
            name: text(PIN_FIXED, name, "name")?,
            why: text(PIN_FIXED + name, why, "why")?,
            when,
            keys: bytes[from..]
                .chunks_exact(32)
                .map(|it| Key(ContentId(it.try_into().expect("32 bytes"))))
                // alloc: a pin read, once per pin a scan
                .collect(),
        })
    }

    /// The key its record is kept under: the hash of its bytes.
    pub fn key(&self) -> Key {
        Key(ContentId::of(PIN_DOMAIN, &self.encode()))
    }
}

/// Of `pins` (each with its record's key), the one each name has now: the newest by `when`,
/// then by key, in the order of their names. One with no keys stays: its name was let go, and
/// its record is what keeps an older pin of the name from being the name's again.
fn newest_pins(mut pins: Vec<(Key, Pin)>) -> Vec<(Key, Pin)> {
    pins.sort_by(|(a_key, a), (b_key, b)| {
        (&a.name, b.when, b_key).cmp(&(&b.name, a.when, a_key))
    });
    pins.dedup_by(|later, first| later.1.name == first.1.name);
    pins
}

/// What the last scan of a period's end found the collector must keep beyond the flags of use:
/// the keys pins name, the keys placed at a path, and everything those and the flagged records
/// name inside the store. A batch that scans a pack reads it; a seal checks it is still the
/// store's (`generation`), since a pin or a place made after it names keys it does not hold.
pub(crate) struct Mark {
    generation: u64,
    rooted: ByHashSet<Key, Global>, // alloc: the mark's set, held while the mark is the store's
    /// What only records that may themselves be forgotten name (a chunk of a value no root
    /// keeps), each with the records that name it: it goes with the last of them, in the
    /// same seal, never before ([`CollectBatch::scan`]).
    weak: ByHash<Key, Vec<Key>, Global>, // alloc: the mark's table, held while the mark is the store's
    /// Every referent could be read: a mark that could not follow one keeps everything.
    sound: bool,
    /// How many keys readers had ever kept ([`Keep`]) when its scan was taken: a key kept
    /// since is in no set of it, so a seal under it is refused.
    kept_changes: u64,
    /// How many roots had been added ([`Collecting::roots`]) when its scan was taken: a
    /// record forgotten alone under it ([`crate::forget`]) is one no root added since names.
    roots: u64,
}

/// A reader's hold on a key for longer than a build: while it lives, the collector keeps the
/// key's record (and what it names) whatever its flags of use and its pack's age. For what
/// reads a record a part at a time over a database's whole life (`hir_def`'s def map pages)
/// and must never find it gone between two parts. Taken while the build that makes the reader
/// is under way: a pack is sealed only between builds and only as scanned since the last one
/// began, so a key kept during a build is in the next mark before any seal. It is memory's
/// only: a daemon that restarts keeps nothing, and its readers are gone with it.
pub struct Keep {
    shared: Arc<crate::read::Shared>,
    key: Key,
}

impl Keep {
    pub fn key(&self) -> Key {
        self.key
    }
}

impl Drop for Keep {
    fn drop(&mut self) {
        self.shared.let_go_of(&self.key);
    }
}

impl Reader {
    /// Keeps `key` from the collector for as long as the [`Keep`] lives.
    pub fn keep(&self, key: Key) -> Keep {
        self.shared.keep(key);
        // clone: a count, once per key kept
        Keep { shared: Arc::clone(&self.shared), key }
    }
}

/// What the store keeps for its collector.
pub(crate) struct Collecting {
    mark: Option<Arc<Mark>>,
    generations: u64,
    /// A period's scan is out ([`Store::plan_take`]).
    scan_out: bool,
    /// The pack a batch is out for.
    out: Option<usize>,
    /// Each name's pin, as the last scan and the pins made since have it, a name let go
    /// with its pin of no keys.
    pins: Vec<(Key, Pin)>,
    /// The record keys of the pins this store made since it opened, the last of each name:
    /// the name's whatever a scan reads ([`Store::keep_scan`]).
    made: Vec<Key>,
    /// What the last scan measured.
    scanned: Scanned,
    counts: Counts,
    /// The flags of use an index swap left to be carried over, until a driver takes it
    /// ([`Store::carry_take`]); and whether one is out.
    carry: Option<FlagCarry>,
    carry_out: bool,
    /// A commit the collector asks for writes no index table ([`Store::collect_flush`]).
    pub(crate) small_commit: bool,
    /// The commit in flight is one the collector began.
    pub(crate) commit_ours: bool,
    /// How many times a root was added (a path placed with a key it did not hold, a pin
    /// made): a mark of a scan taken before the last of them does not hold it.
    roots: u64,
    /// A planted bug for the collector's simulation (`tests/collect.rs`).
    #[cfg(test)]
    pub(crate) plant: Plant,
}

impl Collecting {
    pub(crate) fn new() -> Collecting {
        Collecting {
            mark: None,
            generations: 0,
            scan_out: false,
            out: None,
            pins: Vec::new(), // alloc: empty, allocates nothing
            made: Vec::new(), // alloc: empty, allocates nothing
            scanned: Scanned::default(),
            counts: Counts::default(),
            carry: None,
            carry_out: false,
            small_commit: false,
            commit_ours: false,
            roots: 0,
            #[cfg(test)]
            plant: Plant::None,
        }
    }

    /// A path holds a key it did not before (the store's placed roots, `whole::Names::hold`):
    /// the mark made before does not hold it.
    pub(crate) fn rooted(&mut self) {
        self.mark = None;
        self.roots += 1;
    }

    /// Whether the store's mark says that no root names `key`: a sound mark, of a scan taken
    /// after the last root was added and with the readers' keeps as they are (`kept_changes`
    /// now), that neither roots `key` nor has it named by a record ([`Mark::weak`]), and no
    /// pin names it. What a record forgotten alone needs ([`crate::forget`]): the collector's
    /// rule of what a root names, asked of one record. No mark: not known, so `false`.
    pub(crate) fn unrooted(&self, key: &Key, kept_changes: u64) -> bool {
        #[cfg(test)]
        if self.plant == Plant::ForgetIgnoresRoots {
            return true;
        }
        let Some(mark) = &self.mark else { return false };
        mark.sound
            && mark.roots == self.roots
            && mark.kept_changes == kept_changes
            && !mark.rooted.contains(key)
            && !mark.weak.contains_key(key)
            && !self.pins.iter().any(|(_, pin)| pin.keys.contains(key))
    }

    /// Whether the store holds a mark at all: a forget waits for one ([`crate::forget`]).
    pub(crate) fn marked(&self) -> bool {
        self.mark.as_ref().is_some_and(|it| it.roots == self.roots)
    }

    /// An index swap left `carry` to be run. One not yet taken is replaced: its tables are
    /// two swaps old.
    pub(crate) fn carry(&mut self, carry: FlagCarry) {
        // What the older carry knew is in the tables this one reads only if it ran: its
        // count of periods stands for both when it did not.
        let periods = self.carry.take().map_or(carry.periods, |it| it.periods.min(carry.periods));
        self.carry = Some(FlagCarry { periods, ..carry });
    }

    /// A commit begins; `ours`: the collector asked for it.
    pub(crate) fn commit_begun(&mut self, ours: bool) {
        self.commit_ours = ours;
        self.counts.commits += u64::from(ours);
    }

    pub(crate) fn flush_waited(&mut self) {
        self.counts.flush_waits += 1;
    }
}

/// Planted bugs the simulation's oracle must catch.
#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) enum Plant {
    #[default]
    None,
    /// A scan keeps no record for a pin's sake.
    PinIgnored,
    /// A scan keeps no record for its flags of use: what a build found goes.
    RootMissed,
    /// A pack is sealed without its kept records copied on.
    SealBeforeCopies,
    /// A record is forgotten alone whatever roots it ([`crate::forget`]).
    ForgetIgnoresRoots,
    /// A head lists no record forgotten alone, though it lists the whole files they named as
    /// removed: after a crash or a reopen the record is found and its file is gone.
    ForgetUnlisted,
}

/// The flags of use of an index swap, carried from the tables before it to the new index
/// table by key, on any thread ([`FlagCarry::run`]): a swap numbers the rows afresh, and
/// without this every record would look unused until two more periods ended. It only sets
/// flags (a read's own set is not lost to it), so it can only make the store keep more.
pub struct FlagCarry {
    from_index: Option<Arc<Index>>,
    from_delta: Arc<Table<Spot>>,
    to: Arc<Index>,
    /// The periods ended before the swap: the store's again once the carry is taken back.
    periods: u32,
    carried: u64,
}

impl FlagCarry {
    pub(crate) fn new(
        from_index: Option<Arc<Index>>,
        from_delta: Arc<Table<Spot>>,
        to: Arc<Index>,
        periods: u32,
    ) -> FlagCarry {
        FlagCarry { from_index, from_delta, to, periods, carried: 0 }
    }

    /// How many records' flags it carried.
    pub fn carried(&self) -> u64 {
        self.carried
    }

    /// Sets, for each record of the new index table, the flags of use it had before the swap:
    /// its spot's in the delta before, else its slot's in the index table before.
    pub fn run(&mut self) {
        let guard = enter();
        let held = Held::task(&guard);
        let uses = [Flag::Read, Flag::ReadBefore, Flag::New, Flag::NewBefore];
        for (slot, key, loc) in self.to.table().iter() {
            let Ok(loc) = loc else { continue };
            let mut any = false;
            if let Some((row, spot)) = self.from_delta.get(held, &key) {
                for flag in uses {
                    if self.from_delta.flag(row, flag as usize) {
                        self.to.bits.set(slot, flag);
                        any = true;
                    }
                }
                // Tried is of the record where it is: carried only if it has not moved.
                if spot.place == Place::Pack(loc) && self.from_delta.flag(row, Flag::Tried as usize) {
                    self.to.bits.set(slot, Flag::Tried);
                }
            } else if let Some(before) = &self.from_index
                && let Some((old, Ok(was))) = before.table().get(&key)
            {
                for flag in uses {
                    if before.bits.get(old, flag) {
                        self.to.bits.set(slot, flag);
                        any = true;
                    }
                }
                if was == loc && before.bits.get(old, Flag::Tried) {
                    self.to.bits.set(slot, Flag::Tried);
                }
            }
            self.carried += u64::from(any);
        }
    }
}

/// What a period's scan measured of the own tier's records in packs. Bytes are as stored,
/// padding included.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Scanned {
    pub records: u64,
    pub live_bytes: u64,
    /// Of `live_bytes`, what a root keeps.
    pub rooted_bytes: u64,
}

/// What the collector did since the store was opened.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Counts {
    pub packs_retired: u64,
    /// Bytes of the packs retired, as they were on disk.
    pub retired_bytes: u64,
    pub records_forgotten: u64,
    pub forgotten_bytes: u64,
    pub records_copied: u64,
    pub copied_bytes: u64,
    pub packs_skipped: u64,
    /// Commits the collector began ([`Store::collect_flush`], [`Store::collect_seal`]).
    pub commits: u64,
    /// Times a flush someone else asked for found a commit the collector began still in
    /// flight, and so waits for the rest of it: the one wait a request can meet.
    pub flush_waits: u64,
}

/// A store's bytes, as it knows them without reading its directory.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Usage {
    /// What its own directory holds that a head names: its packs as far as they are committed
    /// (their checks are a thousandth more), the index table and the whole files.
    pub bytes: u64,
    /// Of `bytes`, records no key finds any more.
    pub dead_bytes: u64,
    pub packs: u64,
    pub scanned: Scanned,
    pub counts: Counts,
    /// Whether a scan has said what is rooted since the last pin or place.
    pub marked: bool,
    /// Scans taken back since the store was opened: from the first on, its pins are read
    /// ([`Store::pins`]).
    pub scans: u64,
    /// Periods ended since the rows' flags were last made afresh: under two, the flags do
    /// not say what is unused yet and nothing is forgotten.
    pub periods: u32,
}

impl Usage {
    /// What a collection could give back: the dead bytes, and the live ones no root keeps.
    pub fn collectable_bytes(&self) -> u64 {
        self.dead_bytes
            .saturating_add(self.scanned.live_bytes.saturating_sub(self.scanned.rooted_bytes))
            .min(self.bytes)
    }
}

/// The scan a period's end is: the own tier's rows walked once, on any thread. It plans the
/// cold tier's moves as [`Store::settle`] says, lists the pins, follows what the rooted
/// records name, measures, and (`rotate`) ends the period of reading. [`PeriodScan::run`] is
/// the work; the store takes it back with [`Store::plan_put`].
pub struct PeriodScan {
    reader: Reader,
    index: Option<Arc<Index>>,
    delta: Arc<Table<Spot>>,
    rotate: bool,
    periods: u32,
    short_listed: [bool; Kind::ALL.len()],
    /// Each own pack that was removed or retiring when the scan was taken.
    passed: Vec<bool>,
    placed: Vec<Key>,
    /// The keys readers keep ([`Keep`]), and how many were ever kept, as it was taken.
    kept: Vec<Key>,
    kept_changes: u64,
    /// [`Collecting::roots`] as it was taken.
    roots: u64,
    done: bool,
    pub(crate) demote: Vec<(Key, Loc, Row)>,
    pub(crate) promote: Vec<(Key, Loc, Row)>,
    pub(crate) named: ByHash<ContentId, u32, Global>,
    pins: Vec<(Key, Pin)>,
    rooted: ByHashSet<Key, Global>,
    weak: ByHash<Key, Vec<Key>, Global>,
    sound: bool,
    scanned: Scanned,
    problems: Vec<String>,
}

impl PeriodScan {
    /// How many rows it walked.
    pub fn records(&self) -> u64 {
        self.scanned.records
    }

    /// Walks the rows. Reads no value but the pins' and the lists of the rooted records that
    /// name others (a chunked value's list, a derived value's recipe, an item set's
    /// listing).
    ///
    /// The rows are walked twice, one at a time ([`Look::each_row`]), and never listed:
    /// what a scan holds is the keys it roots, the records that name others and the moves
    /// it plans, not a row of the store each. The first walk reads the pins, plans each
    /// row and notes the records that name others; then the roots are followed; the second
    /// walk measures what is rooted. A record read between the two walks counts as used in
    /// the measure alone.
    pub fn run(&mut self) {
        let guard = enter();
        let shared = Arc::clone(&self.reader.shared); // clone: a count, once a scan
        let look = Look::new(&shared, Held::task(&guard));
        // alloc: once a scan (a worker's); empty unless a slot is not whole
        let mut bad = Vec::new();
        // alloc: once a scan, the pins it finds: a few
        let mut pins = Vec::new();
        // What is kept beyond the flags, and of it what is still to be looked at for the
        // records it names.
        // alloc: once a scan, the keys to follow
        let mut work: Vec<Key> = Vec::new();
        // alloc: once a scan, the records that name others and may be forgotten
        let mut maybe: Vec<(Key, Found<'_>)> = Vec::new();
        let used = |found: &Found<'_>| {
            let flag = |flag| found.at.get(flag);
            flag(Flag::Read) || flag(Flag::ReadBefore) || flag(Flag::New) || flag(Flag::NewBefore)
        };
        // alloc: once a scan, the pins' rows: a few
        let mut pin_rows: Vec<(Key, Found<'_>)> = Vec::new();
        look.each_row(0, &mut bad, |key, found| {
            // Read after the walk: a value is not read inside the walk's frame.
            if found.place.kind() == Kind::Pin {
                pin_rows.push((key, found));
            }
            // What a record names it needs for as long as it is there. One that cannot be
            // forgotten (used, or held by the delta and not the index table) roots what it
            // names; one that can is looked at below, once what the roots reach is known.
            if names_others(found.place) {
                match used(&found) || !matches!(found.at, At::Slot(..)) {
                    true => work.push(key),
                    false => maybe.push((key, found)),
                }
            }
            self.plan_row(look, key, found);
        });
        for (key, found) in pin_rows {
            match look.value(found, &key, Use::Check) {
                Ok((_, bytes)) => match Pin::decode(&bytes) {
                    Ok(pin) => pins.push((key, pin)),
                    Err(why) => {
                        self.sound = false;
                        // alloc: a problem reported
                        self.problems.push(format!("the pin {:x} is not read: {why}", key.0));
                    }
                },
                Err(corrupt) => {
                    self.sound = false;
                    // alloc: a problem reported
                    self.problems.push(format!("a pin is not read: {corrupt}"));
                }
            }
        }
        // Each name's newest pin is kept, its record too: one of no keys among them, so that
        // an older pin of its name is never the name's again.
        self.pins = newest_pins(pins);
        for (key, pin) in &self.pins {
            self.rooted.insert(*key);
            for key in &pin.keys {
                if self.rooted.insert(*key) {
                    work.push(*key);
                }
            }
        }
        for key in self.placed.iter().chain(&self.kept) {
            if self.rooted.insert(*key) {
                work.push(*key);
            }
        }
        while let Some(key) = work.pop() {
            let Some(found) = look.find(&key) else { continue };
            let found = match found {
                Ok(found) => found,
                Err(corrupt) => {
                    self.sound = false;
                    // alloc: a problem reported
                    self.problems.push(format!("what a kept record names is not followed: {corrupt}"));
                    continue;
                }
            };
            if let Err(why) = named_by(look, found, &key, |named| {
                if self.rooted.insert(named) {
                    work.push(named);
                }
            }) {
                self.sound = false;
                self.problems.push(why);
            }
        }
        // What a record no root reaches names is kept for as long as that record is: it
        // never names a record that was forgotten before it.
        for (key, found) in maybe {
            if self.rooted.contains(&key) {
                continue;
            }
            let weak = &mut self.weak;
            let rooted = &self.rooted;
            if let Err(why) = named_by(look, found, &key, |named| {
                if !rooted.contains(&named) {
                    // alloc: once a scan, the records naming one that only they keep: one or a few
                    weak.entry(named).or_insert_with(Vec::new).push(key);
                }
            }) {
                self.sound = false;
                self.problems.push(why);
            }
        }
        // The second walk: slots that were not whole were said by the first.
        // alloc: once a scan; empty unless a slot is not whole
        let mut bad_again = Vec::new();
        look.each_row(0, &mut bad_again, |key, found| {
            let Place::Pack(loc) = found.place else { return };
            let span = disk::span_of(loc.stored).unwrap_or(0);
            self.scanned.records += 1;
            self.scanned.live_bytes += span;
            if used(&found) || self.rooted.contains(&key) {
                self.scanned.rooted_bytes += span;
            }
        });
        radix::sort_by_key(&mut self.demote, |(_, loc, _)| {
            (u128::from(loc.kind.code()) << 96) | radix::pack_64(u64::from(loc.pack), loc.offset)
        });
        radix::sort_by_key(&mut self.promote, |(_, loc, _)| {
            radix::pack_64(u64::from(loc.pack), loc.offset)
        });
        if self.rotate {
            if let Some(index) = &self.index {
                index.bits.rotate();
            }
            self.delta.rotate(Flag::Read as usize, Flag::ReadBefore as usize);
            self.delta.rotate(Flag::New as usize, Flag::NewBefore as usize);
        }
        self.done = true;
    }

    /// One row of the cold tier's plan: whether its record moves colder or back hot, as of the
    /// period ending now, and which whole file it names. A record still in a segment is new,
    /// and stays.
    fn plan_row(&mut self, look: Look<'_>, key: Key, found: Found<'_>) {
        if found.place.form() == Form::External
            && let Ok((header, _)) = look.record(found, &key)
        {
            *self.named.entry(header.content).or_insert(0) += 1;
        }
        let Place::Pack(loc) = found.place else { return };
        let Some(&passed) = self.passed.get(loc.pack as usize) else { return };
        // A short record goes colder only as `short_listed` says. One of a cold kind never
        // comes back hot (its kind is read rarely, by its nature); one of a hot kind does,
        // as a long one does.
        let short = loc.raw < DEMOTE_FROM;
        if passed || (short && !self.short_listed[loc.kind.code() as usize]) {
            return;
        }
        let flag = |flag| found.at.get(flag);
        let (read, before) = (flag(Flag::Read), flag(Flag::ReadBefore));
        match loc.form {
            // A file of its own goes cold as a record does: into a pack, compressed, and
            // its file is let go ([`Store::external_replaced`]). Machine code stays a file,
            // which its loader maps ([`Kind::Machine`]), and so does an output whose placed
            // file is its home ([`crate::home`]): that file is what its places clone, so a
            // copy in a pack would hold its bytes twice again. A pin stays as it is: every
            // scan reads it; so does a listing of an item set a scan follows ([`Kind::Set`]).
            Form::Raw | Form::Lz4 | Form::Warm | Form::External
                if (loc.form != Form::External || !matches!(loc.kind, Kind::Machine | Kind::Output))
                    && !matches!(loc.kind, Kind::Pin | Kind::Set)
                    && self.periods >= 2
                    && !read
                    && !before
                    && !flag(Flag::New)
                    && !flag(Flag::NewBefore)
                    && !flag(Flag::Tried) =>
            {
                self.demote.push((key, loc, found.at.row()));
            }
            Form::Zstd | Form::Warm | Form::ColdColumns
                if read && before && (!short || !loc.kind.cold()) =>
            {
                self.promote.push((key, loc, found.at.row()));
            }
            _ => {}
        }
    }
}

/// Whether a record at `place` may name other records of the store.
fn names_others(place: Place) -> bool {
    place.form() == Form::Chunked || matches!(place.kind(), Kind::Derived | Kind::Set)
}

#[cfg(test)]
thread_local! {
    /// The planted bug of `tests::sets`: the collector follows no listing of an item set
    /// ([`Kind::Set`]) to what it names.
    pub(crate) static SET_NOT_FOLLOWED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Gives `each` every key the record of `key` names inside the store: a chunked value's
/// chunks, a derived value's base. `Err` with why when its list cannot be read.
fn named_by(
    look: Look<'_>,
    found: Found<'_>,
    key: &Key,
    mut each: impl FnMut(Key),
) -> Result<(), String> {
    if found.place.form() == Form::Chunked {
        let (_, payload) = look
            .record(found, key)
            // alloc: a problem reported
            .map_err(|corrupt| format!("a chunked value's chunks are not followed: {corrupt}"))?;
        let chunks = disk::decode_chunks(payload)
            // alloc: a problem reported
            .map_err(|what| format!("the chunks of {:x} are not followed: {what}", key.0))?;
        for (chunk, _) in chunks {
            each(chunk);
        }
    } else if found.place.kind() == Kind::Derived {
        let (_, bytes) = look
            .value(found, key, Use::Check)
            // alloc: a problem reported
            .map_err(|corrupt| format!("a derived value's base is not followed: {corrupt}"))?;
        let recipe = derived::decode(&bytes)
            // alloc: a problem reported
            .map_err(|why| format!("the base of {:x} is not followed: it holds {why}", key.0))?;
        each(recipe.base);
    }
    // A listing of an item set names its leaves, or its items (`sets`): read whole, each
    // time a scan follows it.
    if found.place.kind() == Kind::Set {
        #[cfg(test)]
        if SET_NOT_FOLLOWED.with(|it| it.get()) {
            return Ok(());
        }
        let (_, bytes) = look
            .value(found, key, Use::Check)
            // alloc: a problem reported
            .map_err(|corrupt| format!("a listing's items are not followed: {corrupt}"))?;
        let listing = crate::sets::decode(&bytes)
            // alloc: a problem reported
            .map_err(|why| format!("the listing {:x} is not followed: {why}", key.0))?;
        for named in listing.named() {
            each(named);
        }
    }
    Ok(())
}

/// One pack being collected: named by the store ([`Store::collect_take`]), read on any thread
/// ([`CollectBatch::work`]), its kept records copied on by the store a few at a time
/// ([`Store::collect_copy`]), then sealed ([`Store::collect_seal`]) or given up
/// ([`Store::collect_drop`]).
pub struct CollectBatch {
    reader: Reader,
    mark: Arc<Mark>,
    pack: u32,
    file: FileName,
    unit: u32,
    committed: u64,
    /// Unrooted records are forgotten; else every record a key finds is kept (a pack that is
    /// half dead bytes, rewritten for them alone).
    forget: bool,
    state: Batch,
    /// The records to copy on, by offset, and how far [`CollectBatch::work`] has read them.
    keep: Vec<u64>,
    read: usize,
    /// Read and waiting for the store to copy them.
    ready: VecDeque<(u64, RecordHeader, Vec<u8>)>,
    ready_bytes: u64,
    kept_bytes: u64,
    copied: u64,
    copied_bytes: u64,
    forgotten: u64,
    forgotten_bytes: u64,
    /// The whole files the forgotten records named ([`Form::External`]), by content.
    externals: Vec<ContentId>,
    /// The whole files kept to place the forgotten outputs by cloning, by content (a derived
    /// value's by its key).
    files: Vec<ContentId>,
    /// The records that only records which may be forgotten name, until the scan's end says
    /// which of them go with those ([`Mark::weak`]).
    weakly: Vec<Judged>,
    /// The forgotten records that name others, by key.
    going: ByHashSet<Key, Global>,
    /// How many bytes one [`CollectBatch::work`] reads ahead of the store's copies.
    fill_most: u64,
    problems: Vec<String>,
    #[cfg(test)]
    plant: Plant,
    /// The keys it forgets, for the simulation's controls.
    #[cfg(test)]
    pub(crate) forgotten_keys: Vec<Key>,
}

/// A record a scan may forget.
struct Judged {
    offset: u64,
    key: Key,
    span: u64,
    names_others: bool,
    /// The whole file it names ([`Form::External`]), by content.
    external: Option<ContentId>,
    /// The whole file kept to place it by cloning, by content (a derived value's by its key).
    file: Option<ContentId>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Batch {
    /// Named, not read yet.
    Taken,
    /// Scanned: what to keep is known.
    Scanned,
    /// Not worth collecting, or not readable: why.
    Passed(Pass),
}

/// Why a pack was passed by.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Pass {
    /// More than three quarters of it is kept: copying it on would free too little.
    MostlyKept,
    /// It could not be read through (said in the store's problems).
    Unreadable,
    /// The store changed under the batch (a pin, a place, a period's end, a build): scanned
    /// again later.
    Stale,
}

impl CollectBatch {
    pub fn pack(&self) -> u32 {
        self.pack
    }

    /// The bytes of the pack as it was taken.
    pub fn bytes(&self) -> u64 {
        self.committed
    }

    /// Bytes of the records it keeps, as far as it is scanned.
    pub fn kept_bytes(&self) -> u64 {
        self.kept_bytes
    }

    /// Records it forgets, and their bytes, as far as it is scanned.
    pub fn forgotten(&self) -> (u64, u64) {
        (self.forgotten, self.forgotten_bytes)
    }

    /// Why it is passed by, if it is.
    pub fn passed(&self) -> Option<Pass> {
        match self.state {
            Batch::Passed(pass) => Some(pass),
            _ => None,
        }
    }

    /// Whether [`CollectBatch::work`] has something to do: it is not scanned, or records it
    /// keeps are not read yet and none waits to be copied.
    pub fn wants_work(&self) -> bool {
        match self.state {
            Batch::Taken => true,
            Batch::Scanned => self.ready.is_empty() && self.read < self.keep.len(),
            Batch::Passed(_) => false,
        }
    }

    /// Whether every record it keeps was copied on: it can be sealed.
    pub fn copied_all(&self) -> bool {
        self.state == Batch::Scanned && self.ready.is_empty() && self.read >= self.keep.len()
    }

    /// The batch's reading, for any thread: the first call scans the pack (every record's
    /// header, and whether its key still finds it and a root keeps it); each call then reads
    /// the next records to keep, at most [`Pace::fill_most`] bytes of them, for the store to
    /// copy. It takes its bytes as any reader does, through the store's pool.
    pub fn work(&mut self) {
        if self.state == Batch::Taken {
            self.scan();
        }
        if self.state == Batch::Scanned {
            self.fill();
        }
    }

    fn scan(&mut self) {
        let shared = Arc::clone(&self.reader.shared); // clone: a count, once a scan
        let mut offset = PACK_HEADER;
        let mut why = None;
        'slices: while offset < self.committed {
            let guard = enter();
            let look = Look::new(&shared, Held::task(&guard));
            let view = look.view();
            let slice_end = offset.saturating_add(SCAN_SLICE);
            while offset < self.committed && offset < slice_end {
                let bytes = |at: u64, len: u64| look.pool_bytes(0, view, self.pack, at, len);
                let Ok(start) = bytes(offset, 4) else {
                    // alloc: a problem reported
                    why = Some(format!("at {offset} could not be read through the store's pool"));
                    break 'slices;
                };
                if start == [0; 4] {
                    let next = offset.next_multiple_of(disk::BLOCK);
                    if next == offset {
                        // alloc: a problem reported
                        why = Some(format!("at {offset} is neither a record nor padding"));
                        break 'slices;
                    }
                    offset = next;
                    continue;
                }
                let header = match bytes(offset, RECORD_HEADER).map(RecordHeader::decode) {
                    Ok(Ok(header)) => header,
                    Ok(Err(what)) => {
                        // alloc: a problem reported
                        why = Some(format!("the record at {offset}: {what}"));
                        break 'slices;
                    }
                    Err(_) => {
                        // alloc: a problem reported
                        why = Some(format!("the record at {offset} is outside the pack"));
                        break 'slices;
                    }
                };
                let span = header.span();
                if offset.checked_add(span).is_none_or(|end| end > self.committed) {
                    // alloc: a problem reported
                    why = Some(format!("the record at {offset} runs past the pack"));
                    break 'slices;
                }
                // What is judged dead is deleted: a format-9 header is trusted only once its
                // record's CRC, which covers the payload too, matches.
                let checked = bytes(offset, RECORD_HEADER).and_then(|head| {
                    let payload = bytes(offset + RECORD_HEADER, header.stored)?;
                    Ok(RecordHeader::check(head, payload))
                });
                if let Ok(Err(what)) | Err(what) = checked.map_err(|_| "it could not be read through the store's pool") {
                    // alloc: a problem reported
                    why = Some(format!("the record at {offset}: {what}"));
                    break 'slices;
                }
                self.judge(look, offset, &header);
                offset += span;
            }
        }
        if let Some(why) = why {
            // alloc: a problem reported
            self.problems.push(format!("{} is not collected: {why}", self.file));
            self.state = Batch::Passed(Pass::Unreadable);
            return;
        }
        // What only forgettable records name goes with the last of them, here, or stays:
        // settled in rounds, since a base may be named by a derived value named by another.
        let mut unsettled = std::mem::take(&mut self.weakly);
        loop {
            let before = unsettled.len();
            let mut still = Vec::new(); // alloc: once a round of a scan, what is not settled yet
            for record in unsettled {
                let named_by = self.mark.weak.get(&record.key).map_or(&[][..], Vec::as_slice);
                match named_by.iter().all(|it| self.going.contains(it)) {
                    true => self.forget(record),
                    false => still.push(record),
                }
            }
            unsettled = still;
            if unsettled.len() == before || unsettled.is_empty() {
                break;
            }
        }
        for record in unsettled {
            self.keep.push(record.offset);
            self.kept_bytes += record.span;
        }
        self.keep.sort_unstable();
        let live = self.kept_bytes + self.forgotten_bytes;
        // More than three quarters of what a key finds in it is kept: copying it on would
        // free too little. (A pack that is half dead bytes is rewritten all the same.)
        let mostly_kept = self.kept_bytes * 4 > live.max(1) * 3;
        self.state = match self.forget && mostly_kept && live * 2 > self.committed {
            true => Batch::Passed(Pass::MostlyKept),
            false => Batch::Scanned,
        };
    }

    /// Whether the record at `offset` is kept, forgotten or dead already.
    fn judge(&mut self, look: Look<'_>, offset: u64, header: &RecordHeader) {
        if header.form == Form::Dictionary {
            return;
        }
        let Some(Ok(found)) = look.find(&header.key) else { return };
        let here = matches!(
            found.place,
            Place::Pack(loc) if loc.pack == self.pack && loc.offset == offset
        );
        if found.tier != 0 || !here {
            return;
        }
        let flag = |flag| found.at.get(flag);
        let used = flag(Flag::Read)
            || flag(Flag::ReadBefore)
            || flag(Flag::New)
            || flag(Flag::NewBefore);
        let pinned = self.mark.rooted.contains(&header.key);
        #[cfg(test)]
        let (used, pinned) = match self.plant {
            Plant::RootMissed => (false, pinned),
            Plant::PinIgnored => (used, false),
            _ => (used, pinned),
        };
        // Only a record the index table holds can be forgotten: a slot into a retired pack
        // finds nothing, where a spot of the delta would go on naming it.
        let slot = matches!(found.at, At::Slot(..));
        let span = header.span();
        if !self.forget || !self.mark.sound || used || pinned || !slot {
            self.keep.push(offset);
            self.kept_bytes += span;
            return;
        }
        let record = Judged {
            offset,
            key: header.key,
            span,
            names_others: header.form == Form::Chunked
                || matches!(header.kind, Kind::Derived | Kind::Set),
            external: (header.form == Form::External).then_some(header.content),
            file: match header.kind {
                Kind::Output => Some(header.content),
                Kind::Derived => Some(header.key.0),
                _ => None,
            },
        };
        match self.mark.weak.contains_key(&header.key) {
            true => self.weakly.push(record),
            false => self.forget(record),
        }
    }

    /// `record` is forgotten with the pack.
    fn forget(&mut self, record: Judged) {
        self.forgotten += 1;
        self.forgotten_bytes += record.span;
        #[cfg(test)]
        self.forgotten_keys.push(record.key);
        if record.names_others {
            self.going.insert(record.key);
        }
        self.externals.extend(record.external);
        self.files.extend(record.file);
    }

    /// Reads the next records to keep, for the store to copy on.
    fn fill(&mut self) {
        if !self.ready.is_empty() {
            return;
        }
        let guard = enter();
        let shared = Arc::clone(&self.reader.shared); // clone: a count, once a fill
        let look = Look::new(&shared, Held::task(&guard));
        let view = look.view();
        while self.read < self.keep.len() && self.ready_bytes < self.fill_most {
            let offset = self.keep[self.read];
            // Checked whole before it is copied on: a record rotted in its pack is not carried
            // into a new one under a fresh check.
            let record = look.pool_bytes(0, view, self.pack, offset, RECORD_HEADER).ok().and_then(|head| {
                let stored = RecordHeader::decode(head).ok()?.stored;
                let payload = look.pool_bytes(0, view, self.pack, offset + RECORD_HEADER, stored).ok()?;
                let (header, _) = RecordHeader::check(head, payload).ok()?;
                // alloc: a record copied on, held until the store has copied it
                Some((header, payload.to_vec()))
            });
            let Some((header, payload)) = record else {
                // alloc: a problem reported
                self.problems.push(format!(
                    "{} is not collected: its record at {offset} could not be read again",
                    self.file
                ));
                self.state = Batch::Passed(Pass::Unreadable);
                return;
            };
            self.read += 1;
            self.ready_bytes += header.span();
            self.ready.push_back((offset, header, payload));
        }
    }
}

/// A pack the collector may take ([`Store::collect_candidates`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Candidate {
    pub pack: usize,
    /// Its committed bytes.
    pub bytes: u64,
    /// At least half of it is dead bytes: worth rewriting whatever the budget.
    pub half_dead: bool,
    /// When it was last used, in seconds on the store's clock; 0 for not known.
    pub used: u64,
}

/// What [`Store::pin_add`] did.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PinAdded {
    /// The pin was written with the keys added; durable with the ticket.
    Written(Ticket),
    /// Every key was the pin's already: nothing written.
    Unchanged,
    /// The store's pins are not read yet (no scan since it opened): nothing written; ask
    /// again after the next period's end.
    NotRead,
}

/// What a seal did, or why it did not.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Sealed {
    /// The pack is retired: the commit begun names it removed, and its file goes once that
    /// is durable. The bytes it held, and the records and bytes forgotten with it.
    Retired { bytes: u64, kept_bytes: u64, forgotten: u64, forgotten_bytes: u64 },
    /// The batch no longer holds (a pin, a place or a scan since it was taken, or its pack
    /// changed): given up, to be taken again.
    Stale,
}

impl Store {
    /// Keeps `keys` under `name` whatever their last use, until the next pin of the name: a
    /// record of the store, durable with its ticket. No keys lets the name go. `when` is the
    /// caller's clock, in nanoseconds since the Unix epoch, and is written later than every
    /// pin of the name the store knows of: so the pin made last is the name's, in this run and
    /// in the next, whatever that clock did. A pin of an earlier run that this one has not
    /// read yet, and that a clock gone back left later, is outdone when a scan reads it
    /// ([`Store::keep_scan`]).
    pub fn pin(
        &mut self,
        name: &str,
        why: &str,
        when: u64,
        keys: &[Key],
    ) -> Result<Ticket, Mismatch> {
        let when = self
            .collecting
            .pins
            .iter()
            .filter(|(_, it)| it.name == name)
            .fold(when, |when, (_, it)| when.max(it.when.saturating_add(1)));
        // alloc: a pin made, kept as the name's
        let pin = Pin { name: name.to_owned(), why: why.to_owned(), when, keys: keys.to_vec() };
        let key = pin.key();
        let ticket = self.put(key, Kind::Pin, &pin.encode())?;
        let collecting = &mut self.collecting;
        // alloc: the pins as they are now, a few
        let mut pins = std::mem::take(&mut collecting.pins);
        pins.push((key, pin));
        collecting.pins = newest_pins(pins);
        // The one made last of each name: the one before of this name is no longer its.
        let pins = &collecting.pins;
        collecting.made.retain(|made| pins.iter().any(|(it, _)| it == made));
        collecting.made.push(key);
        // The mark made before does not hold the keys it names.
        collecting.rooted();
        Ok(ticket)
    }

    /// Adds `keys` to the pin `name`: the name's keys as they are and `keys`, written as its
    /// new pin when a key is new ([`Store::pin`]), `why` said with it. The one way to grow a
    /// pin, so no writer has to know its rule: until a scan has read the store's pins since
    /// it opened ([`Usage::scans`]) nothing is written and the answer is
    /// [`PinAdded::NotRead`], since a pin written then would leave out the keys of the one
    /// an earlier run wrote; the caller asks again later.
    pub fn pin_add(
        &mut self,
        name: &str,
        why: &str,
        when: u64,
        keys: &[Key],
    ) -> Result<PinAdded, Mismatch> {
        self.pin_change(name, why, when, keys, &[])
    }

    /// [`Store::pin_add`] of `add` with `drop` let go: the name's keys as they are, less
    /// `drop`, and `add` (a key in both stays). For a pin that names index values (an item
    /// set's root, [`crate::sets`]): a set's new root takes the place of its root before,
    /// so the pin stays a key a set and what only the old root named is let go. Written
    /// only when the keys change; [`PinAdded::NotRead`] as [`Store::pin_add`] says it.
    pub fn pin_change(
        &mut self,
        name: &str,
        why: &str,
        when: u64,
        add: &[Key],
        drop: &[Key],
    ) -> Result<PinAdded, Mismatch> {
        if self.collecting.generations == 0 {
            return Ok(PinAdded::NotRead);
        }
        // alloc: once per pin changed, its keys
        let had: Vec<Key> =
            self.pins().find(|pin| pin.name == name).map_or_else(Vec::new, |pin| pin.keys.clone());
        // alloc: as above
        let mut all: Vec<Key> = had.iter().filter(|key| !drop.contains(key)).copied().collect();
        all.extend_from_slice(add);
        all.sort_unstable();
        all.dedup();
        // alloc: as above
        let mut before = had;
        before.sort_unstable();
        before.dedup();
        if all == before {
            return Ok(PinAdded::Unchanged);
        }
        self.pin(name, why, when, &all).map(PinAdded::Written)
    }

    /// Whether a scan has read the store's pins since it opened: [`Store::pins`] is whole,
    /// and [`Store::pin_add`] writes.
    pub fn pins_read(&self) -> bool {
        self.collecting.generations > 0
    }

    /// Each name's pin, in the order of their names: as the last period's scan found them
    /// and as the pins made since changed them; a name let go is not among them. Empty until
    /// the first scan after an open, but for the pins made since.
    pub fn pins(&self) -> impl Iterator<Item = &Pin> {
        self.collecting.pins.iter().map(|(_, pin)| pin).filter(|pin| !pin.keys.is_empty())
    }

    /// The bytes the records of `keys` take in the own tier's packs, as stored: what a pin of
    /// them keeps. A key it has no committed record of counts nothing.
    pub fn stored_bytes(&self, keys: &[Key]) -> u64 {
        let look = self.look();
        // Only its own: a layer's or another writer's record is not its to keep or count.
        keys.iter()
            .filter_map(|key| match look.find(key)?.ok()? {
                Found { tier: 0, place: Place::Pack(loc), .. } => disk::span_of(loc.stored),
                _ => None,
            })
            .sum()
    }

    /// What the store holds and what its collector did.
    pub fn usage(&self) -> Usage {
        let own = &self.tiers[0];
        let mut usage = Usage {
            scanned: self.collecting.scanned,
            counts: self.collecting.counts,
            marked: self.collecting.mark.is_some(),
            scans: self.collecting.generations,
            periods: self.periods,
            ..Usage::default()
        };
        for pack in own.packs.iter().filter(|it| !it.removed && !it.retiring) {
            usage.packs += 1;
            usage.bytes += pack.committed;
            usage.dead_bytes += pack.dead.min(pack.committed);
        }
        usage.bytes += own.head.index.map_or(0, |it| it.len);
        usage.bytes += own.externals.values().filter(|it| !it.dropped).map(|it| it.len).sum::<u64>();
        usage
    }

    /// Whether the commit in flight, if one is, is one the collector began: a flush asked
    /// for now waits for the rest of it.
    pub fn collect_committing(&self) -> bool {
        self.commit.is_some() && self.collecting.commit_ours
    }

    /// Whether the store can plan or collect now: it is open and whole, and no commit, index
    /// swap or fold is in flight.
    fn at_rest(&self) -> bool {
        self.broken.is_none()
            && self.commit.is_none()
            && !self.index_mapping
            && self.migrating.is_none()
    }

    pub(crate) fn scan_new(&self, rotate: bool) -> PeriodScan {
        let own = &self.tiers[0];
        let (kept, kept_changes) = self.shared.kept();
        PeriodScan {
            kept,
            kept_changes,
            roots: self.collecting.roots,
            reader: self.reader(),
            // clone: an `Arc`, once a scan
            index: own.index.clone(),
            delta: Arc::clone(&own.delta), // clone: an `Arc`, once a scan
            rotate,
            periods: self.periods,
            // By kind code: whether a short record of the kind is listed: to be moved colder
            // once the kind has a dictionary and, before, to be sampled while its training
            // wants samples (after a reopen, the only way its records already kept are).
            // Every kind's is: most records of a hot kind are short too (a function's code,
            // a small query result: 23 of hello-world's 26 MiB of them), and a dictionary
            // takes four fifths of each.
            short_listed: std::array::from_fn(|code| {
                Kind::from_code(code as u8).is_some_and(|kind| {
                    self.dictionary_of(kind).is_some() || self.training[code].wants_samples()
                })
            }),
            // alloc: once a scan, a flag a pack
            passed: own.packs.iter().map(|it| it.removed || it.retiring).collect(),
            // alloc: once a scan, the keys the paths hold: a few
            placed: self.names.held_keys().copied().collect(),
            done: false,
            demote: Vec::new(),  // alloc: empty, allocates nothing
            promote: Vec::new(), // alloc: empty, allocates nothing
            named: ByHash::new_in(Global), // alloc: once a scan, a count for each whole file a record names: a few
            pins: Vec::new(), // alloc: empty, allocates nothing
            rooted: ByHashSet::new_in(Global), // alloc: once a scan, the keys pins and kept records name
            weak: ByHash::new_in(Global), // alloc: once a scan, what only forgettable records name
            sound: true,
            scanned: Scanned::default(),
            problems: Vec::new(), // alloc: empty, allocates nothing
        }
    }

    /// The scan the period ending now is, to be run on any thread ([`PeriodScan::run`]) and
    /// handed back to [`Store::plan_put`]: the store's thread walks no row. `None`, and no
    /// period ends, while the store cannot plan (it is broken, a scan is out, or a commit, an
    /// index swap or its fold is in flight): ask again later.
    pub fn plan_take(&mut self) -> Option<PeriodScan> {
        assert!(self.tiers[0].opened, "a period ended of a store that is not open");
        self.tidy();
        // No period ends while an index swap's flags are still to be carried over: the
        // flags it would read and rotate are not whole.
        if !self.at_rest() || self.collecting.scan_out || self.carrying() {
            return None;
        }
        self.collecting.scan_out = true;
        Some(self.scan_new(true))
    }

    /// Whether an index swap's flags of use are still to be carried to the new table.
    pub fn carrying(&self) -> bool {
        self.collecting.carry.is_some() || self.collecting.carry_out
    }

    /// The carry an index swap left, to be run on any thread ([`FlagCarry::run`]) and handed
    /// back to [`Store::carry_put`]. `None` when there is none, or one is out.
    pub fn carry_take(&mut self) -> Option<FlagCarry> {
        if self.collecting.carry_out {
            return None;
        }
        let carry = self.collecting.carry.take()?;
        self.collecting.carry_out = true;
        Some(carry)
    }

    /// A carry that ran, or (`None`) was lost with its worker: when the index table it
    /// carried to is still the store's and no later swap waits, the periods ended before the
    /// swap count again, so the flags say what was unused as they did. Lost, they count from
    /// 0, as after an open.
    pub fn carry_back(&mut self, carry: Option<FlagCarry>) {
        self.collecting.carry_out = false;
        let Some(carry) = carry else { return };
        let current = self.tiers[0].index.as_ref().is_some_and(|it| Arc::ptr_eq(it, &carry.to));
        if current && self.collecting.carry.is_none() {
            self.periods = self.periods.max(carry.periods);
        }
    }

    /// [`Store::carry_back`] of a carry that ran.
    pub fn carry_put(&mut self, carry: FlagCarry) {
        self.carry_back(Some(carry));
    }

    /// A scan that reads the store's pins and marks its roots without ending a period of
    /// reading: for a request that needs the pins before any build has ended (an import, an
    /// export, a status of the store), run on any thread ([`PeriodScan::run`]) and handed
    /// back to [`Store::pins_put`]. `None` while one cannot be taken (a scan is out, or a
    /// commit, an index swap or its carry is in flight): ask again.
    pub fn pins_take(&mut self) -> Option<PeriodScan> {
        assert!(self.tiers[0].opened, "the pins asked of a store that is not open");
        self.tidy();
        if !self.at_rest() || self.collecting.scan_out || self.carrying() {
            return None;
        }
        self.collecting.scan_out = true;
        Some(self.scan_new(false))
    }

    /// Takes back a scan [`Store::pins_take`] gave: the pins are read ([`Store::pins_read`])
    /// and the roots marked. No period ended and no move is planned by it.
    pub fn pins_put(&mut self, scan: PeriodScan) {
        self.tidy();
        self.collecting.scan_out = false;
        assert!(scan.done, "a scan of the pins handed back before it ran");
        if self.broken.is_some() {
            return;
        }
        // Its plan of moves is of no period's end: let go.
        _ = self.keep_scan(scan);
    }

    /// A scan was lost with its worker: the store keeps the plan and the mark it had, and a
    /// scan can be taken again.
    pub fn plan_lost(&mut self) {
        self.collecting.scan_out = false;
    }

    /// Takes a scan back: its plan is the moves [`Store::settle_step`] and
    /// [`Store::settle_take`] make, its pins and roots are what the collector keeps, and the
    /// period it ended is counted. `false` when the plan is not taken: the store broke, or
    /// its tables were swapped under the scan (an index written meanwhile numbers the rows
    /// afresh), so the next period's end plans again.
    pub fn plan_put(&mut self, scan: PeriodScan) -> bool {
        self.tidy();
        self.collecting.scan_out = false;
        assert!(scan.done, "a period's scan handed back before it ran");
        let own = &self.tiers[0];
        let same = match (&own.index, &scan.index) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        } && Arc::ptr_eq(&own.delta, &scan.delta)
            && own.prev.is_none();
        if self.broken.is_some() || !same {
            return false;
        }
        self.periods = self.periods.saturating_add(1);
        let moves = self.keep_scan(scan);
        self.moves = Some(moves);
        true
    }

    /// The plan, the pins and the mark of a scan that ran, kept: what [`Store::plan_put`]
    /// and the scans the store runs itself end with.
    ///
    /// The pins a scan read are the store's, but for a name this store pinned since it
    /// opened: that pin was made after every pin an earlier run wrote, so it stays the name's
    /// whatever the scan found (one made after the scan walked, which it did not see; one of
    /// an earlier run whose `when` a clock gone back left later). A pin read later than this
    /// store's own is written again, later than it, so that the next run reads the order they
    /// were made in; and a mark of a scan taken at the store's roots as they are now, which
    /// rooted the older pin's keys, is not kept.
    pub(crate) fn keep_scan(&mut self, scan: PeriodScan) -> Moves {
        let PeriodScan {
            demote, promote, named, mut pins, rooted, weak, sound, scanned, kept_changes, roots, mut problems, ..
        } = scan;
        self.problems.append(&mut problems);
        for (content, external) in &mut self.tiers[0].externals {
            external.keys = named.get(content).copied().unwrap_or(0);
        }
        let collecting = &mut self.collecting;
        let before = std::mem::take(&mut collecting.pins);
        // alloc: once a scan, the pins of this store's that a scan read as older: none unless a clock went back
        let mut again: Vec<Pin> = Vec::new();
        let mut outdone = false;
        for made in &collecting.made {
            let Some((_, mine)) = before.iter().find(|(key, _)| key == made) else { continue };
            match pins.iter_mut().find(|(_, it)| it.name == mine.name) {
                Some((key, _)) if key == made => {}
                Some(found) => {
                    outdone = true;
                    if (found.1.when, found.0) > (mine.when, *made) {
                        // clone: a pin written again, once a clock went back
                        again.push(Pin { when: found.1.when.saturating_add(1), ..mine.clone() });
                    }
                    // clone: the name's pin as this store made it, a few
                    *found = (*made, mine.clone());
                }
                None => {
                    outdone = true;
                    // clone: the name's pin as this store made it, a few
                    pins.push((*made, mine.clone()));
                }
            }
        }
        pins.sort_by(|(_, a), (_, b)| a.name.cmp(&b.name));
        collecting.pins = pins;
        collecting.scanned = scanned;
        collecting.generations += 1;
        // alloc: once a scan, the mark the collector's batches share
        collecting.mark = Some(Arc::new(Mark {
            generation: collecting.generations,
            rooted,
            weak,
            sound,
            kept_changes,
            roots,
        }));
        // A mark that rooted another pin than the name's, at the roots as they are: not the
        // store's. (One of a scan taken before a root was added holds for no seal already.)
        if outdone && roots == collecting.roots {
            collecting.rooted();
        }
        for pin in again {
            if let Err(mismatch) = self.pin(&pin.name, &pin.why, pin.when, &pin.keys) {
                // alloc: a problem reported
                self.problems.push(format!("the pin {:?} was not written again: {mismatch:?}", pin.name));
            }
        }
        Moves { demote, demoted: 0, promote, promoted: 0 }
    }

    /// The unit of the packs records moved cold are written to, made the first time one is:
    /// a unit as a crate version's is, under an id of its own, so the head lists it.
    pub(crate) fn cold_unit(&mut self) -> Unit {
        if let Some(unit) = self.cold_unit {
            return unit;
        }
        let unit = self.unit(cold_unit_id());
        self.cold_unit = Some(unit);
        unit
    }

    /// Tells the store the time, in seconds since the Unix epoch on its driver's clock: what
    /// a read stamps its pack with, and what the collector reads a pack's age against. The
    /// first time, every pack whose head held no time of use (a store of format 5, or one
    /// opened before any clock) is stamped with it: nothing is known to be older.
    pub fn clock(&mut self, seconds: u64) {
        if self.shared.tick() == seconds {
            return;
        }
        let first = self.shared.tick() == 0;
        self.shared.set_tick(seconds);
        if first {
            for pack in &self.tiers[0].packs {
                let relaxed = std::sync::atomic::Ordering::Relaxed;
                if pack.used.load(relaxed) == 0 {
                    pack.used.store(seconds, relaxed);
                }
            }
        }
    }

    /// The own packs that can be collected now, in the order of their numbers: wholly
    /// committed, holding no dictionary that did not load, and no pack a batch whose moves
    /// wait moves records into ([`Store::retirable`]'s rule).
    pub fn collect_candidates(&self) -> Vec<Candidate> {
        let own = &self.tiers[0];
        // alloc: once a candidate asked (between requests), a flag a pack
        let mut moved_into = vec![false; own.packs.len()];
        for it in self.pending.iter().chain([&self.draining]).flat_map(|it| &it.moves) {
            moved_into[it.to.pack as usize] = true;
        }
        (0..own.packs.len())
            .filter(|&at| {
                let pack = &own.packs[at];
                !moved_into[at]
                    && !pack.removed
                    && !pack.retiring
                    && !pack.closing
                    && pack.end == pack.committed
                    && pack.usable == pack.committed
                    && pack.committed > PACK_HEADER
                    && !own.dictionaries.iter().any(|it| it.pack as usize == at && it.bytes.is_err())
            })
            .map(|at| {
                let pack = &own.packs[at];
                Candidate {
                    pack: at,
                    bytes: pack.committed,
                    half_dead: pack.dead * 2 >= pack.committed - PACK_HEADER,
                    used: pack.used.load(std::sync::atomic::Ordering::Relaxed),
                }
            })
            // alloc: once a candidate asked (between requests), the packs: a few
            .collect()
    }

    /// Names own pack `pack` for collection: a batch to read on any thread
    /// ([`CollectBatch::work`]). `forget`: its unrooted records are forgotten; else every
    /// record a key finds is kept and only its dead bytes go. From now on nothing is appended
    /// to the pack. `None` while it cannot be taken: no scan has marked the roots since the
    /// last pin or place, the flags of use are younger than two periods (`forget` only), a
    /// batch is out already, the store is not at rest, or the pack is no candidate.
    pub fn collect_take(&mut self, pack: usize, forget: bool, pace: &Pace) -> Option<CollectBatch> {
        self.tidy();
        if !self.at_rest() || self.collecting.out.is_some() {
            return None;
        }
        // clone: an `Arc`, once a batch
        let mark = self.collecting.mark.clone()?;
        if forget && self.periods < 2 {
            return None;
        }
        if !self.collect_candidates().iter().any(|it| it.pack == pack) {
            return None;
        }
        let reader = self.reader();
        let it = &mut self.tiers[0].packs[pack];
        it.closing = true;
        self.collecting.out = Some(pack);
        Some(CollectBatch {
            reader,
            mark,
            pack: pack as u32,
            // clone: the batch names its pack in what it reports; once a batch
            file: it.file.clone(),
            unit: it.unit,
            committed: it.committed,
            forget,
            state: Batch::Taken,
            keep: Vec::new(),  // alloc: empty, allocates nothing
            read: 0,
            ready: VecDeque::new(), // alloc: empty, allocates nothing
            ready_bytes: 0,
            kept_bytes: 0,
            copied: 0,
            copied_bytes: 0,
            forgotten: 0,
            forgotten_bytes: 0,
            externals: Vec::new(), // alloc: empty, allocates nothing
            files: Vec::new(),     // alloc: empty, allocates nothing
            weakly: Vec::new(),    // alloc: empty, allocates nothing
            going: ByHashSet::new_in(Global), // alloc: once a batch, the forgotten records that name others
            fill_most: pace.fill_most,
            problems: Vec::new(), // alloc: empty, allocates nothing
            #[cfg(test)]
            plant: self.collecting.plant,
            #[cfg(test)]
            forgotten_keys: Vec::new(),
        })
    }

    /// Copies the next records `batch` read on into the store's own segment, at most `most`
    /// bytes of them (one record, however long): each is durable with the next commit, and
    /// its key points at the copy from now on. A record its key no longer finds in the pack
    /// (moved cold or put again meanwhile) is passed by. `None` while the store makes no
    /// moves (a commit or an index swap in flight): call again.
    pub fn collect_copy(&mut self, batch: &mut CollectBatch, most: u64) -> Option<u64> {
        self.tidy();
        if self.broken.is_some() {
            batch.state = Batch::Passed(Pass::Stale);
            return Some(0);
        }
        if !self.at_rest() {
            return None;
        }
        let mut copied = 0;
        while let Some((_, header, _)) = batch.ready.front() {
            let span = header.span();
            if copied > 0 && copied + span > most {
                break;
            }
            let (offset, header, payload) = batch.ready.pop_front().expect("a record in front");
            batch.ready_bytes -= span;
            let here = matches!(
                self.look().find(&header.key),
                Some(Ok(found)) if found.tier == 0 && matches!(
                    found.place,
                    Place::Pack(loc) if loc.pack == batch.pack && loc.offset == offset
                )
            );
            if !here {
                continue;
            }
            // The planted bug: the record is counted as copied and is not.
            #[cfg(test)]
            if self.collecting.plant == Plant::SealBeforeCopies {
                continue;
            }
            self.rehome(Unit(batch.unit), header, &payload);
            copied += span;
            batch.copied += 1;
            batch.copied_bytes += span;
            self.collecting.counts.records_copied += 1;
        }
        self.collecting.counts.copied_bytes += copied;
        Some(copied)
    }

    /// Retires the pack of `batch`, every record it keeps copied on: the pack's keys find
    /// nothing from now on, and the commit begun here names it removed, after which its file
    /// goes. The whole files its forgotten records named go with it. The caller says by
    /// calling that no build is under way: what a build found it must not lose before it
    /// ends.
    ///
    /// `Err` hands the batch back: not now (a commit or an index swap is in flight), ask again.
    pub fn collect_seal(
        &mut self,
        batch: CollectBatch,
        effects: &mut Vec<Effect>,
    ) -> Result<Sealed, CollectBatch> {
        self.tidy();
        let at = batch.pack as usize;
        assert!(
            batch.copied_all(),
            "a seal of {} before the records it keeps were copied on",
            batch.file
        );
        if self.broken.is_some() {
            self.collect_drop(batch);
            return Ok(Sealed::Stale);
        }
        if !self.at_rest() {
            return Err(batch);
        }
        let own = &self.tiers[0];
        let pack = &own.packs[at];
        let moved_into = self
            .pending
            .iter()
            .chain([&self.draining])
            .flat_map(|it| &it.moves)
            .any(|it| it.to.pack as usize == at);
        let holds = self.collecting.mark.as_ref().is_some_and(|it| it.generation == batch.mark.generation)
            // A key a reader kept since the mark is in no set of it.
            && self.shared.kept_changes() == batch.mark.kept_changes
            && !pack.removed
            && !pack.retiring
            && pack.closing
            && pack.end == batch.committed
            && pack.committed == batch.committed
            && !moved_into;
        if !holds {
            self.collect_drop(batch);
            return Ok(Sealed::Stale);
        }
        self.tiers[0].packs[at].retiring = true;
        self.tiers[0].packs[at].closing = false;
        self.collecting.out = None;
        self.move_dictionaries(at);
        for content in &batch.externals {
            self.external_replaced(content);
        }
        for content in &batch.files {
            // A whole file a path still names stays until that path is placed again.
            if self.files.contains_key(content) && self.placed_looked && self.names.names(content) == 0 {
                self.let_go(*content, effects);
            }
        }
        let counts = &mut self.collecting.counts;
        counts.packs_retired += 1;
        counts.retired_bytes += batch.committed;
        counts.records_forgotten += batch.forgotten;
        counts.forgotten_bytes += batch.forgotten_bytes;
        // alloc: none unless the batch met a problem
        self.problems.extend(batch.problems);
        // From here no lookup finds a record of the pack: before any build's task can load
        // a view, since the caller holds the store.
        self.publish_view();
        self.collecting.small_commit = true;
        self.start_commit(effects);
        Ok(Sealed::Retired {
            bytes: batch.committed,
            kept_bytes: batch.copied_bytes,
            forgotten: batch.forgotten,
            forgotten_bytes: batch.forgotten_bytes,
        })
    }

    /// A batch was lost with its worker: its pack takes appends again.
    pub fn collect_lost(&mut self) {
        if let Some(at) = self.collecting.out.take() {
            self.tiers[0].packs[at].closing = false;
        }
    }

    /// Gives a batch up: its pack takes appends again, and what was copied on stays copied.
    pub fn collect_drop(&mut self, batch: CollectBatch) {
        let at = batch.pack as usize;
        if self.collecting.out == Some(at) {
            self.collecting.out = None;
            self.tiers[0].packs[at].closing = false;
        }
        if batch.passed().is_some() {
            self.collecting.counts.packs_skipped += 1;
        }
        // alloc: none unless the batch met a problem
        self.problems.extend(batch.problems);
    }

    /// Commits what the collector copied on, as [`Store::flush`] does, writing no index table:
    /// a commit the collector asks for stays as small as what it copied. Nothing happens
    /// while a commit is in flight.
    pub fn collect_flush(&mut self, effects: &mut Vec<Effect>) {
        self.tidy();
        if self.broken.is_some() || self.commit.is_some() {
            return;
        }
        self.collecting.small_commit = true;
        self.start_commit(effects);
    }
}

/// How hard the collector works, by pressure (0 to 1000, as the collector of build dirs
/// counts it, `blaze_gc::pace`): a gap between packs that shrinks as the budget is neared,
/// and the bounds of one step on the store's thread.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Pace {
    /// The gap between two packs taken with no pressure, and at full pressure.
    pub gap_idle: Duration,
    pub gap_full: Duration,
    /// The bytes one step copies on the store's thread: its bound (docs/gc.md: 1 ms).
    pub copy_most: u64,
    /// The bytes copied after which the collector commits, while no build is under way: the
    /// most a request that arrives meanwhile waits for.
    pub flush_most: u64,
    /// The bytes a batch reads ahead of the store's copies.
    pub fill_most: u64,
    /// With every pack walked and nothing freed, how long until the next lap.
    pub lap_gap: Duration,
    /// Nothing is forgotten out of a pack used within this long.
    pub grace: Duration,
}

impl Default for Pace {
    fn default() -> Pace {
        Pace {
            gap_idle: Duration::from_secs(60),
            gap_full: Duration::from_millis(50),
            copy_most: 256 << 10,
            flush_most: 256 << 10,
            fill_most: 4 << 20,
            lap_gap: Duration::from_secs(600),
            grace: Duration::from_secs(3600),
        }
    }
}

/// Full pressure, in permille.
pub const FULL: u32 = 1000;

/// What the driver tells the collector each turn.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Facts {
    /// The bytes held against the budget (the home's), and the budget.
    pub held: u64,
    pub budget: u64,
    /// No build is under way or queued, and none is being answered: a pack may be sealed.
    pub idle: bool,
    /// Counts builds begun, ever: a batch scanned before a build began is scanned again.
    pub builds: u64,
    /// The wall clock, in seconds since the Unix epoch: what packs' times of use are in.
    pub seconds: u64,
}

impl Facts {
    /// The pressure: 0 while held is under four fifths of the budget, full at the budget.
    pub fn pressure(&self) -> u32 {
        let start = self.budget / 5 * 4;
        if self.held <= start {
            return 0;
        }
        let band = (self.budget - start).max(1);
        (((self.held - start) as u128 * FULL as u128 / band as u128).min(FULL as u128)) as u32
    }

    /// The bytes to free to be back under four fifths of the budget.
    pub fn over(&self) -> u64 {
        self.held.saturating_sub(self.budget / 5 * 4)
    }
}

/// What a step of the collector asks of its driver.
pub enum Step {
    /// Nothing now; ask again at [`Collector::next_deadline`] or when the facts change.
    Rest,
    /// Run [`CollectBatch::work`] off the store's thread and hand the batch to
    /// [`Collector::back`].
    Work(CollectBatch),
    /// It did a bounded piece of work on the store; ask again this turn or the next.
    Stepped(Did),
}

/// What a step did on the store's thread: what its span is named for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Did {
    /// Copied this many bytes of kept records on.
    Copied(u64),
    /// Retired pack `pack`: `freed` bytes given back, `forgotten` records forgotten.
    Retired { pack: u32, freed: u64, forgotten: u64 },
    /// Asked for a commit of what was copied.
    Flushed,
    /// Passed a pack by.
    Passed(Pass),
    /// The lap ended.
    LapEnded,
}

impl Did {
    pub fn name(&self) -> &'static str {
        match self {
            Did::Copied(_) => "copy",
            Did::Retired { .. } => "retire",
            Did::Flushed => "flush",
            Did::Passed(_) => "pass",
            Did::LapEnded => "lap ends",
        }
    }
}

/// What a step of the collector rested on, when it was not the time of its next pack.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Waits {
    /// No scan has marked the roots since the last pin, place or open: the next period's
    /// end does.
    Mark,
    /// A build is under way: a pack is sealed between builds, and under half pressure
    /// nothing is copied beside one.
    Build,
    /// The store has a commit or an index swap in flight.
    Store,
}

impl Waits {
    pub fn said(&self) -> &'static str {
        match self {
            Waits::Mark => "the scan of the next period's end, which marks what is kept",
            Waits::Build => "the build under way to end",
            Waits::Store => "the store's commit or index swap in flight",
        }
    }
}

/// Where the collector stands: what a person is told.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Standing {
    /// Under the budget's pressure: nothing to do.
    #[default]
    Under,
    /// Collecting, or waiting for its next pack.
    Working,
    /// Every pack was looked at and what is held is still at the budget or over: what is
    /// left is rooted, or was used within the grace. Nothing in use was removed.
    OverBudget { held: u64, budget: u64 },
}

/// One lap's sums: every pack looked at once.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Lap {
    pub packs: u64,
    pub retired: u64,
    pub skipped: u64,
    pub freed_bytes: u64,
    pub forgotten: u64,
    pub copied_bytes: u64,
    pub stale: u64,
}

enum Phase {
    Rest,
    /// A batch is with the driver's worker.
    Out,
    Have(CollectBatch),
}

/// The rule of when the store is collected: a synchronous state machine on an injected clock
/// that owns no file and no thread (`sans-io-cores`). Its driver tells it the time and the
/// facts, calls [`Collector::step`] with the store between requests, runs the batches it
/// hands out and gives them back.
///
/// Which pack: one that is half dead bytes first (rewritten for them alone); then, while
/// what is held is past four fifths of the budget, the pack least recently used, if that was
/// longer ago than the grace. A lap looks at each pack once.
pub struct Collector {
    pace: Pace,
    now: Duration,
    facts: Facts,
    phase: Phase,
    /// The builds begun when the batch in hand was taken.
    taken_at: u64,
    /// The packs this lap looked at.
    tried: Vec<usize>,
    /// How many packs the store had when this lap took its first: a pack numbered past them
    /// was made since, by this lap's own copies among others, and is the next lap's. Without
    /// the bound a lap that keeps a record copies it into a new pack, which is fresh to the
    /// lap, so it takes that one, and the next, without end (`bounded-progress`).
    lap_packs: Option<usize>,
    lap: Lap,
    /// A lap was asked for (`blaze gc`): no gap between its packs.
    asked: bool,
    /// Every unrooted record goes, whatever the budget and the grace (`blaze clean
    /// --unkept`).
    everything: bool,
    next_at: Duration,
    unflushed: u64,
    /// The last lap found no more to take.
    dry: bool,
    /// Laps ended, and the last one's sums.
    laps: u64,
    last: Lap,
    /// Steps on the store's thread, and the most bytes one copied.
    steps: u64,
    step_most: u64,
    /// Times in a row a pack's batch went stale: bounded.
    retries: u32,
    /// What the last step that rested was waiting for, when it was not the time.
    waits: Option<Waits>,
    /// A pack that is half dead bytes waits to be rewritten, as last looked ([`Pace::gap_idle`]
    /// apart while nothing else is wanted).
    half_dead: bool,
    looked_at: Duration,
    /// The last step rested on something that is not the time (no mark yet, a build under
    /// way, a commit in flight): no deadline is given for it, so no loop spins on one.
    blocked: bool,
}

/// A pack whose batch went stale this many times in a row is passed by for the lap
/// (`bounded-progress`).
const RETRIES_MOST: u32 = 3;

impl Collector {
    pub fn new(pace: Pace) -> Collector {
        Collector {
            pace,
            now: Duration::ZERO,
            facts: Facts::default(),
            phase: Phase::Rest,
            taken_at: 0,
            tried: Vec::new(), // alloc: empty, allocates nothing
            lap_packs: None,
            lap: Lap::default(),
            asked: false,
            everything: false,
            next_at: Duration::ZERO,
            unflushed: 0,
            dry: false,
            laps: 0,
            last: Lap::default(),
            steps: 0,
            step_most: 0,
            retries: 0,
            waits: None,
            half_dead: false,
            looked_at: Duration::ZERO,
            blocked: false,
        }
    }

    pub fn advance_clock(&mut self, now: Duration) {
        self.now = self.now.max(now);
    }

    pub fn observe(&mut self, facts: Facts) {
        self.facts = facts;
    }

    /// Asks for one lap now, whatever the gap. `everything`: every unrooted record goes, not
    /// only what brings the store under its budget, and whatever its pack's age.
    pub fn ask(&mut self, everything: bool) {
        self.asked = true;
        self.everything = everything;
        self.tried.clear();
        self.lap_packs = None;
        self.lap = Lap::default();
        self.dry = false;
        self.next_at = self.now;
    }

    /// Whether a lap that was asked for is still under way.
    pub fn asked(&self) -> bool {
        self.asked
    }

    pub fn standing(&self) -> Standing {
        if self.dry && self.facts.budget > 0 && self.facts.held >= self.facts.budget {
            return Standing::OverBudget { held: self.facts.held, budget: self.facts.budget };
        }
        match self.wants() || !matches!(self.phase, Phase::Rest) {
            true => Standing::Working,
            false => Standing::Under,
        }
    }

    /// How many laps ended, and the last one's sums.
    pub fn last_lap(&self) -> (u64, Lap) {
        (self.laps, self.last)
    }

    /// The lap under way.
    pub fn lap(&self) -> Lap {
        self.lap
    }

    /// Steps taken on the store's thread, and the most bytes one of them copied: the bound
    /// its simulation checks ([`Pace::copy_most`], or one record).
    pub fn steps(&self) -> (u64, u64) {
        (self.steps, self.step_most)
    }

    /// What the last step that rested was waiting for, when it was not the time.
    pub fn waits(&self) -> Option<Waits> {
        self.waits
    }

    /// Whether a batch is with the driver's worker.
    pub fn out(&self) -> bool {
        matches!(self.phase, Phase::Out)
    }

    /// Whether a step now would do nothing: no batch in hand, nothing wanted, nothing copied
    /// that waits for its commit, and the next look for a half-dead pack not due.
    pub fn resting(&self) -> bool {
        matches!(self.phase, Phase::Rest)
            && !self.wants()
            && self.unflushed == 0
            && self.now < self.looked_at + self.pace.gap_idle
    }

    /// When to call [`Collector::step`] again if nothing else changes; `None` while it waits
    /// for a batch or has nothing to do.
    pub fn next_deadline(&self) -> Option<Duration> {
        match self.phase {
            Phase::Out => None,
            _ if self.blocked => None,
            Phase::Have(_) => Some(self.now),
            Phase::Rest if self.wants() => Some(self.next_at.max(self.now)),
            // The next look for a pack that is half dead bytes.
            Phase::Rest => Some(self.looked_at + self.pace.gap_idle),
        }
    }

    /// Whether it has a pack to take, now or at its next deadline.
    pub fn wants(&self) -> bool {
        self.asked || self.half_dead || self.facts.pressure() > 0
    }

    fn gap(&self) -> Duration {
        let (idle, full) = (self.pace.gap_idle.as_nanos(), self.pace.gap_full.as_nanos());
        let pressure = u128::from(self.facts.pressure());
        let nanos = idle - idle.saturating_sub(full) * pressure / u128::from(FULL);
        Duration::from_nanos(nanos as u64)
    }

    /// A batch came back from its work.
    pub fn back(&mut self, batch: CollectBatch) {
        debug_assert!(matches!(self.phase, Phase::Out));
        self.phase = Phase::Have(batch);
    }

    /// The batch's worker was lost (a panic): its pack is passed by for this lap. The store
    /// still counts a batch out for it until [`Store::collect_lost`].
    pub fn lost(&mut self, store: &mut Store) {
        store.collect_lost();
        self.phase = Phase::Rest;
        self.lap.packs += 1;
        self.lap.skipped += 1;
    }

    fn end_lap(&mut self) {
        self.laps += 1;
        self.last = std::mem::take(&mut self.lap);
        self.tried.clear();
        self.lap_packs = None;
        self.asked = false;
        self.everything = false;
        self.dry = true;
        // Walked over and over, a store full of what is in use would cost a lap each time:
        // the next lap waits.
        self.next_at = self.now + self.pace.lap_gap;
    }

    /// One step, with the store, between requests: at most one bounded piece of work on the
    /// store's thread (a batch named, [`Pace::copy_most`] bytes copied, a pack sealed, a
    /// commit asked for), or a batch for the driver to run. It never waits.
    pub fn step(&mut self, store: &mut Store, effects: &mut Vec<Effect>) -> Step {
        if store.broken().is_some() || !store.ready() {
            return Step::Rest;
        }
        store.clock(self.facts.seconds);
        self.blocked = false;
        self.waits = None;
        // What was copied is committed in small pieces while no build is under way; under
        // one, it rides the build's own commits.
        if self.unflushed >= self.pace.flush_most && self.facts.idle && !store.committing() {
            self.unflushed = 0;
            store.collect_flush(effects);
            self.steps += 1;
            return Step::Stepped(Did::Flushed);
        }
        match std::mem::replace(&mut self.phase, Phase::Rest) {
            Phase::Out => {
                self.phase = Phase::Out;
                Step::Rest
            }
            Phase::Rest => {
                let step = self.begin(store, effects);
                // Resting though a pack is due: on a mark, a build's end or the store.
                self.blocked =
                    matches!(step, Step::Rest) && self.wants() && self.now >= self.next_at;
                step
            }
            Phase::Have(batch) => {
                let step = self.advance(store, batch, effects);
                self.blocked = matches!(step, Step::Rest);
                step
            }
        }
    }

    /// Takes the next pack, if one is due.
    fn begin(&mut self, store: &mut Store, effects: &mut Vec<Effect>) -> Step {
        // A pack that is half dead bytes is rewritten whatever the budget: looked for at
        // most once a gap.
        if !self.wants() && self.now >= self.looked_at + self.pace.gap_idle {
            self.looked_at = self.now;
            self.half_dead = store.usage().marked
                && store.collect_candidates().iter().any(|it| it.half_dead);
        }
        if !self.wants() || self.now < self.next_at {
            if self.unflushed > 0 && self.facts.idle && !store.committing() {
                // The last of what was copied.
                self.unflushed = 0;
                store.collect_flush(effects);
                self.steps += 1;
                return Step::Stepped(Did::Flushed);
            }
            return Step::Rest;
        }
        // Under half pressure the collector works only while no build does.
        if !self.facts.idle && self.facts.pressure() < FULL / 2 {
            self.waits = Some(Waits::Build);
            return Step::Rest;
        }
        let usage = store.usage();
        if !usage.marked {
            // No scan has said what is rooted: the next period's end does.
            self.waits = Some(Waits::Mark);
            return Step::Rest;
        }
        // Whether to forget at all: the flags of use must say something, and there must be
        // bytes to free still.
        let free = usage.periods >= 2
            && (self.everything || self.facts.over() > self.lap.freed_bytes);
        let grace = self.pace.grace.as_secs();
        let aged = |it: &Candidate| {
            self.everything || (it.used != 0 && self.facts.seconds.saturating_sub(it.used) >= grace)
        };
        let candidates = store.collect_candidates();
        let lap_packs = *self.lap_packs.get_or_insert(store.tiers[0].packs.len());
        let fresh = || candidates.iter().filter(|it| it.pack < lap_packs && !self.tried.contains(&it.pack));
        let pick = fresh().find(|it| it.half_dead).or_else(|| {
            fresh().filter(|it| free && aged(it)).min_by_key(|it| (it.used, it.pack))
        });
        let Some(&pick) = pick else {
            self.half_dead = false;
            self.end_lap();
            return Step::Stepped(Did::LapEnded);
        };
        let forget = free && aged(&pick);
        let Some(batch) = store.collect_take(pick.pack, forget, &self.pace) else {
            // Not at rest: the same pack next time.
            self.waits = Some(Waits::Store);
            return Step::Rest;
        };
        self.tried.push(pick.pack);
        self.dry = false;
        self.taken_at = self.facts.builds;
        self.steps += 1;
        self.phase = Phase::Out;
        Step::Work(batch)
    }

    /// Moves the batch in hand one step on.
    fn advance(
        &mut self,
        store: &mut Store,
        mut batch: CollectBatch,
        effects: &mut Vec<Effect>,
    ) -> Step {
        if let Some(pass) = batch.passed() {
            let pack = batch.pack() as usize;
            store.collect_drop(batch);
            return self.passed(pack, pass);
        }
        if batch.wants_work() {
            self.phase = Phase::Out;
            return Step::Work(batch);
        }
        if !batch.copied_all() {
            // Copying while a build runs is for a store near its budget only.
            if !self.facts.idle && self.facts.pressure() < FULL / 2 {
                self.phase = Phase::Have(batch);
                self.waits = Some(Waits::Build);
                return Step::Rest;
            }
            let copied = store.collect_copy(&mut batch, self.pace.copy_most);
            self.phase = Phase::Have(batch);
            let Some(copied) = copied else {
                self.waits = Some(Waits::Store);
                return Step::Rest;
            };
            self.steps += 1;
            self.step_most = self.step_most.max(copied);
            self.unflushed += copied;
            self.lap.copied_bytes += copied;
            return Step::Stepped(Did::Copied(copied));
        }
        // Sealed only while no build is under way, and only as scanned since the last one
        // began: what a build found are flags the scan did not see.
        if !self.facts.idle || store.committing() {
            self.phase = Phase::Have(batch);
            self.waits = Some(if self.facts.idle { Waits::Store } else { Waits::Build });
            return Step::Rest;
        }
        let pack = batch.pack() as usize;
        if self.facts.builds != self.taken_at {
            store.collect_drop(batch);
            return self.passed(pack, Pass::Stale);
        }
        self.steps += 1;
        match store.collect_seal(batch, effects) {
            Ok(Sealed::Retired { bytes, kept_bytes, forgotten, .. }) => {
                self.unflushed = 0;
                self.retries = 0;
                self.lap.packs += 1;
                self.lap.retired += 1;
                let freed = bytes.saturating_sub(kept_bytes);
                self.lap.freed_bytes += freed;
                self.lap.forgotten += forgotten;
                self.next_at = if self.asked { self.now } else { self.now + self.gap() };
                Step::Stepped(Did::Retired { pack: pack as u32, freed, forgotten })
            }
            Err(batch) => {
                self.phase = Phase::Have(batch);
                self.waits = Some(Waits::Store);
                Step::Rest
            }
            Ok(Sealed::Stale) => self.passed(pack, Pass::Stale),
        }
    }

    #[expect(
        blaze::retry,
        reason = "a pack whose batch went stale is scanned again up to RETRIES_MOST times: a race with writers, not failed IO; a scan that cannot go stale must replace it"
    )]
    fn passed(&mut self, pack: usize, pass: Pass) -> Step {
        match pass {
            Pass::Stale if self.retries < RETRIES_MOST => {
                // The same pack again, scanned afresh.
                self.retries += 1;
                self.lap.stale += 1;
                self.tried.retain(|&it| it != pack);
            }
            _ => {
                self.retries = 0;
                self.lap.packs += 1;
                self.lap.skipped += 1;
            }
        }
        Step::Stepped(Did::Passed(pass))
    }
}
