//! A fresh home takes a published set whole, once (D138; docs/v2/plan/design/published.md).
//!
//! WHY HERE, BEFORE THE FIRST PHASE. The engine's pool workers read the store through a
//! `Snapshot` (local segments only, lock-free), which never fetches. So a set named by
//! `--origin` is taken on the coordinator's thread before the first phase is planned
//! ([`before_first_phase`], one line in engine_check.rs), and every record of it is then a local
//! hit for every worker, as if this home had made it.
//!
//! WHEN. A take runs when an origin is named and the home's `tool.<toolchain>` root is absent or
//! older than the set's generation. What the home holds of the set (store/published.rs `Seen`):
//! the generation it took (its rollback floor) and when the head was last asked, by the
//! command's handed wall clock (rule 75).
//!
//! THE FRESHNESS WINDOW ([`FRESH_NS`], ten minutes). The origin's head is asked at most once per
//! window: a command whose home holds the toolchain's root and took the set's generation with its
//! head asked less than a window ago asks the origin NOTHING. Past the window (or with the wall
//! clock stepped back past the last ask) it asks the head once: the same generation takes
//! nothing and starts the window again; a later one takes the set whole. So a second command
//! reads the head at most once, and none within the window.
//!
//! WHAT A TAKE DOES, in this order, each step stopping the command by name on a refusal:
//!
//! 1. The head, checked: signed over the set's id by a key trusted for the origin (blaze-remote
//!    trust.rs `trusted_for`), at a generation no lower than the home's floor (a rollback).
//! 2. The `prefix` (the head, the manifest, every summary), its head compared byte for byte with
//!    the one just checked (so its signature is verified once, not twice: rule 74), its
//!    manifest against the head's digest and its summaries against the rows.
//! 3. Each segment the manifest names, read whole (ranged reads of at most [`READ_CHUNK`]), its
//!    bytes checked ONCE against the digest its row names (blaze-cas `check_segment`: one pass
//!    over every record's bytes, the chain up to the signed head), and each record put as this
//!    home's own `LOCAL` record under its own tag; a record already held with equal bytes is
//!    counted "already here". The segment's records are committed before the next segment is
//!    read, so memory holds one segment, and a take stopped part way leaves records no root
//!    names, which the next take finds already here.
//! 4. The toolchain's root names every record taken, then the floor (the generation, the ask's
//!    time); then the one commit that makes both durable. The root is named LAST: a crash
//!    anywhere before leaves no root, never half of one.
//!
//! A set that cannot be taken (an unreachable folder or host, a refused head, a segment that is
//! not its row's) stops the command by name: nothing is made in its place (rule 3). With no
//! `--origin` nothing here runs and nothing is read.

use core::fmt;

use blaze_cas::Key as CasKey;
use blaze_cas::plain::{ENTRY, Entry, HEADER, Lent, check_segment};
use blaze_core::Driver;
use blaze_drive::Key;
use blaze_ids::ContentId;
use blaze_keyed::store::{Put, StoreError};
use blaze_remote::PublicKey;
use blaze_remote::published::set::{SetCheck, SetName, SetNameError};
use blaze_remote::read::form::{HEAD_DOMAIN, HEAD_LEN, HEAD_SIGNED, PREFIX_MOST, decode_prefix};
use blaze_remote::read::{Head, HeadCheck, Namespace, Object, Origin, Refused, SignatureWhy, Unfetched};
use blaze_toolchain::ToolchainId;
use bstd::clock::Clock;
use bstd::cx::Cx;
use bstd::out::Out;

use super::plain_place::{PlaceError, PlainPlace, StoreAlloc};
use super::published::{self, PublishedError};
use super::roots::{self, RootError, RootName};
use super::{FolderOrigin, HttpOrigin, NamedOrigin, Opened, OriginAt};
use crate::CliError;

/// How long after its head was asked a home's set is current: within it no command asks the
/// origin anything (the module's freshness window).
pub(crate) const FRESH_NS: u64 = 10 * 60 * 1_000_000_000;
/// The most bytes one read asks: under the HTTP origin's body bound (store.rs `ORIGIN_WIRE`).
pub(crate) const READ_CHUNK: usize = 8 << 20;
/// Each read's deadline, from its start.
const READ_WAIT_NS: u64 = 30 * 1_000_000_000;
/// A segment taken whole is at most this: a published set cuts its segments at 32 MiB of
/// payloads (store/published.rs `SEGMENT_BYTES`), so twice that leaves room for its index.
pub(crate) const SEGMENT_TAKEN_MOST: u64 = 64 << 20;
/// The records and bytes one take holds at most: past either the set is refused by name.
pub(crate) const RECORDS_TAKEN_MOST: u64 = 1 << 22;
pub(crate) const BYTES_TAKEN_MOST: u64 = 8 << 30;

/// What one command did with its named set: the profile's `published:` line and stderr's.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Taken {
    pub set: ContentId,
    /// The set's generation this home holds after the command.
    pub generation: u64,
    pub how: How,
    /// Records put by this take, and their bytes; records it found already held, equal.
    pub records: u32,
    pub bytes: u64,
    pub already: u32,
    /// The take's time, and of it the time checking segments' digests.
    pub take_ns: u64,
    pub verify_ns: u64,
    /// Reads of the origin: of its head, and of anything.
    pub head_reads: u32,
    pub reads: u32,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum How {
    /// Within the freshness window: nothing asked.
    Fresh { asked_ago_ns: u64 },
    /// The head asked once: the home holds its generation; nothing taken.
    Current,
    /// The set taken whole.
    Took,
}

impl fmt::Display for Taken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("published: set ")?;
        for byte in &self.set.0[..6] {
            write!(f, "{byte:02x}")?;
        }
        let ms = |ns: u64| ns as f64 / 1e6;
        match self.how {
            How::Took => write!(
                f,
                " generation {}: {} records, {} bytes taken in {:.1} ms (verify {:.1} ms); {} already here",
                self.generation,
                self.records,
                self.bytes,
                ms(self.take_ns),
                ms(self.verify_ns),
                self.already
            ),
            How::Current => write!(f, " generation {}: nothing taken, the home holds it (its head asked once)", self.generation),
            How::Fresh { asked_ago_ns } => write!(
                f,
                " generation {}: nothing taken, the origin not asked (its head asked {} s ago, within the {} s window)",
                self.generation,
                asked_ago_ns / 1_000_000_000,
                FRESH_NS / 1_000_000_000
            ),
        }
    }
}

/// The planted bugs and stops of the take's tests (store/take_tests.rs): none outside a test.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct TakePlant {
    /// A segment's digest unchecked: its header and footer read alone, as a ranged read sees
    /// them. A flipped payload byte is then taken as the record.
    #[cfg(test)]
    pub unverified: bool,
    /// The root named after each segment, naming what is taken so far: a stop between two
    /// segments then leaves half a root.
    #[cfg(test)]
    pub root_each_segment: bool,
    /// The take stops (as a crash would) once this many records are put or found.
    #[cfg(test)]
    pub stop_after: Option<u32>,
}

impl TakePlant {
    pub(crate) const NONE: TakePlant = TakePlant {
        #[cfg(test)]
        unverified: false,
        #[cfg(test)]
        root_each_segment: false,
        #[cfg(test)]
        stop_after: None,
    };
}

/// Why a segment was refused.
#[derive(Debug)]
pub enum SegmentWhy {
    /// blaze-cas's check of a whole segment refused it (its digest, its form).
    Check(StoreError),
    /// A segment, but not the one its row names (another digest, count, index or namespace).
    NotItsRow,
    /// An entry of its index does not place a record inside its payloads.
    Entry { what: &'static str, at: u64 },
}

/// Why a named set was not taken: the command stops, by name (rule 3).
#[derive(Debug)]
pub enum TakeError {
    /// This blaze's toolchain and target make no set's name.
    Set(SetNameError),
    /// A read of the origin failed, came short, or was refused (the head's signature, the
    /// set, a rollback, the prefix's form or digests): blaze-remote's words, naming the object.
    Read(Unfetched),
    /// The prefix's head is not the head checked a moment before: the set was published again
    /// during the take.
    Moved,
    /// A segment's bytes, by object.
    Segment { object: Object, why: SegmentWhy },
    /// Past what a take holds.
    TooLarge { what: &'static str, len: u64, most: u64 },
    /// Other bytes under a key this home holds (rule 21).
    TwoValues { key: ContentId },
    Place(PlaceError),
    Published(PublishedError),
    Root(RootError),
    /// The planted stop ([`TakePlant::stop_after`]).
    #[cfg(test)]
    Stopped,
}

const STOPS: &str = "the command stops: nothing is made in place of a set an --origin names (rule 3); run it again with the origin reachable, or without --origin";

impl fmt::Display for TakeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the published set of --origin was not taken: ")?;
        match self {
            TakeError::Set(error) => write!(f, "this blaze's set name: {error}")?,
            TakeError::Read(why) => write!(f, "{why}")?,
            TakeError::Moved => f.write_str("prefix: its head is not the head read a moment before: the set was published again during the take")?,
            TakeError::Segment { object, why: SegmentWhy::Check(error) } => write!(f, "{object}: {error}")?,
            TakeError::Segment { object, why: SegmentWhy::NotItsRow } => write!(f, "{object}: a segment, but not the one the manifest's row names (its digest, count, index or namespace)")?,
            TakeError::Segment { object, why: SegmentWhy::Entry { what, at } } => write!(f, "{object}: the index entry at byte {at}: {what}")?,
            TakeError::TooLarge { what, len, most } => write!(f, "{what} is {len}, past the {most} a take holds")?,
            TakeError::TwoValues { key } => write!(f, "the set's record {key} has other bytes than the one this home holds (rule 21)")?,
            TakeError::Place(error) => write!(f, "{error}")?,
            TakeError::Published(error) => write!(f, "{error}")?,
            TakeError::Root(error) => write!(f, "{error}")?,
            #[cfg(test)]
            TakeError::Stopped => f.write_str("stopped by the test")?,
        }
        write!(f, "; {STOPS}")
    }
}

impl From<TakeError> for CliError {
    fn from(error: TakeError) -> CliError {
        CliError::Store(super::StoreFailure::Take(error))
    }
}

/// The engine's one call (engine_check.rs, before the first phase): with `--origin` named, the
/// set of this blaze's toolchain and target taken into `opened`'s store as the module says, its
/// line on `err` when records were taken and in the profile (`phases`) always. With no
/// `--origin`, nothing.
#[expect(clippy::too_many_arguments, reason = "the take's edge: each argument one specific capability or handle")]
pub(crate) fn before_first_phase(
    cx: &Cx,
    args: Option<&bstd::args::Args>,
    fs: &bstd::fs::Fs,
    net: Option<&bstd::net::Net>,
    opened: &Opened,
    toolchain: ToolchainId,
    phases: &mut crate::phases::Phases,
    err: &Out,
) -> Result<(), CliError> {
    // A command given no argument list (a test's check called directly) names no origin.
    let Some(args) = args else { return Ok(()) };
    let flags = crate::flags(args)?;
    let Some(origin) = flags.origin else {
        if flags.origin_key.is_some() {
            return Err(CliError::Usage("--origin-key names the key of an --origin: name the origin too"));
        }
        return Ok(());
    };
    let hex = toolchain.hex();
    let set = SetName::new(hex.as_str(), crate::graph::TRIPLE).map_err(TakeError::Set)?;
    let id = set.id();
    let named = NamedOrigin::published(cx, fs, net, origin, flags.origin_key, set, &opened.lent)?;
    let clock = phases.clock().clone();
    let taken = take(cx, &named, id, &opened.lent, toolchain, &clock, TakePlant::NONE)?;
    if taken.how == How::Took {
        err.line(cx, format_args!("blaze: {taken}"))?;
    }
    phases.published = Some(taken);
    Ok(())
}

/// [`take_from`] over the origin `named` reaches: its folder, or its `http://` base.
pub(crate) fn take<D: Driver>(cx: &Cx, named: &NamedOrigin, set: ContentId, lent: &Lent<D, StoreAlloc>, toolchain: ToolchainId, clock: &Clock, plant: TakePlant) -> Result<Taken, TakeError> {
    let keys = named.check.named_keys();
    match &named.at {
        OriginAt::Folder(folder) => take_from(&mut FolderOrigin { cx, folder }, keys, set, lent, toolchain, clock, plant),
        OriginAt::Http(at) => take_from(&mut HttpOrigin::new(cx, at, clock), keys, set, lent, toolchain, clock, plant),
    }
}

/// The set `set`, read from `origin` and believed from a key trusted for it (`keys`: those the
/// command named), taken into `lent` as the module says.
pub(crate) fn take_from<D: Driver, O: Origin + ?Sized>(origin: &mut O, keys: &[PublicKey], set: ContentId, lent: &Lent<D, StoreAlloc>, toolchain: ToolchainId, clock: &Clock, plant: TakePlant) -> Result<Taken, TakeError> {
    let began = now_ns(clock);
    let wall = clock.wall().as_ns();
    let seen = published::seen(lent, &set).map_err(TakeError::Published)?;
    let rooted = lent.root_named(RootName::toolchain(toolchain).as_str()).is_some();
    let mut taken = Taken { set, generation: seen.generation, how: How::Current, records: 0, bytes: 0, already: 0, take_ns: 0, verify_ns: 0, head_reads: 0, reads: 0 };
    // The window: a wall clock stepped back past the last ask asks again.
    if rooted && seen.generation > 0 && wall >= seen.asked_wall_ns && wall - seen.asked_wall_ns < FRESH_NS {
        taken.how = How::Fresh { asked_ago_ns: wall - seen.asked_wall_ns };
        return Ok(taken);
    }
    let mut origin = Counted { origin, heads: 0, reads: 0 };
    let result = take_counted(&mut origin, keys, set, lent, toolchain, clock, plant, (seen.generation, wall, rooted), &mut taken);
    taken.head_reads = origin.heads;
    taken.reads = origin.reads;
    taken.take_ns = now_ns(clock).saturating_sub(began);
    result.map(|()| taken)
}

#[expect(clippy::too_many_arguments, reason = "the take's edge, its counted origin and its tally")]
fn take_counted<D: Driver, O: Origin + ?Sized>(
    origin: &mut Counted<'_, O>,
    keys: &[PublicKey],
    set: ContentId,
    lent: &Lent<D, StoreAlloc>,
    toolchain: ToolchainId,
    clock: &Clock,
    plant: TakePlant,
    (floor, wall, rooted): (u64, u64, bool),
    taken: &mut Taken,
) -> Result<(), TakeError> {
    // The plant is read in a test alone.
    let _ = plant;
    let refused = |why: Refused| TakeError::Read(Unfetched::Refused(why));
    // 1. The head, checked: signer, set, rollback.
    let mut head_bytes = [0u8; HEAD_LEN + 1];
    let n = read_at(origin, Object::Head, 0, &mut head_bytes, clock)?;
    let head = Head::decode(&head_bytes[..n], Namespace::PUBLIC, &SetCheck { set, keys, seen: floor }).map_err(refused)?;
    let mut checked = [0u8; HEAD_LEN];
    checked.copy_from_slice(&head_bytes[..HEAD_LEN]);
    let mut place = PlainPlace::new(lent, false);
    if rooted && head.generation == floor {
        // Current: the window starts again; nothing else is read.
        published::keep_floor(&mut place, &set, floor, wall).map_err(TakeError::Published)?;
        place.commit().map_err(TakeError::Place)?;
        taken.how = How::Current;
        return Ok(());
    }
    // 2. The prefix, its head the one just checked.
    let alloc = *lent.alloc();
    let mut bytes: std::vec::Vec<u8, StoreAlloc> = std::vec::Vec::new_in(alloc);
    read_to_end(origin, Object::Prefix, PREFIX_MOST, &mut bytes, clock)?;
    if bytes.len() < HEAD_LEN || bytes[..HEAD_LEN] != checked[..] {
        return Err(TakeError::Moved);
    }
    let prefixed = decode_prefix(&bytes, PREFIX_MOST, Namespace::PUBLIC, &SameHead(&checked), alloc).map_err(refused)?;
    let records_named = prefixed.rows.iter().map(|row| u64::from(row.count)).sum::<u64>();
    if records_named > RECORDS_TAKEN_MOST {
        return Err(TakeError::TooLarge { what: "the set's records", len: records_named, most: RECORDS_TAKEN_MOST });
    }
    let bytes_named = prefixed.rows.iter().map(|row| row.len).sum::<u64>();
    if bytes_named > BYTES_TAKEN_MOST {
        return Err(TakeError::TooLarge { what: "the set's segments' bytes", len: bytes_named, most: BYTES_TAKEN_MOST });
    }
    drop(bytes);
    // 3. Each segment whole, checked once, its records put; committed segment by segment.
    let mut kept: std::vec::Vec<CasKey, StoreAlloc> = std::vec::Vec::with_capacity_in(records_named as usize, alloc);
    let mut segment: std::vec::Vec<u8, StoreAlloc> = std::vec::Vec::new_in(alloc);
    for row in prefixed.rows.iter() {
        let object = Object::Segment(row.segment);
        if row.len > SEGMENT_TAKEN_MOST {
            return Err(TakeError::TooLarge { what: "a segment's length", len: row.len, most: SEGMENT_TAKEN_MOST });
        }
        let len = row.len as usize;
        read_exact(origin, object, len, &mut segment, clock)?;
        let verifying = now_ns(clock);
        #[cfg(test)]
        let unverified = plant.unverified;
        #[cfg(not(test))]
        let unverified = false;
        let described = if unverified {
            (len >= HEADER + blaze_cas::plain::FOOTER)
                .then(|| blaze_cas::plain::describe(&segment[..HEADER], &segment[len - blaze_cas::plain::FOOTER..], len as u64))
                .flatten()
                .ok_or(TakeError::Segment { object, why: SegmentWhy::NotItsRow })?
        } else {
            check_segment(&segment).map_err(|why| TakeError::Segment { object, why: SegmentWhy::Check(why) })?
        };
        taken.verify_ns += now_ns(clock).saturating_sub(verifying);
        if described.digest != row.segment || described.count != row.count || described.index_at != row.index_at || described.namespace != Namespace::PUBLIC {
            return Err(TakeError::Segment { object, why: SegmentWhy::NotItsRow });
        }
        let index_at = described.index_at as usize;
        for i in 0..described.count as usize {
            let at = index_at + i * ENTRY;
            let entry_at = at as u64;
            let bad = |what| TakeError::Segment { object, why: SegmentWhy::Entry { what, at: entry_at } };
            let entry = segment.get(at..at + ENTRY).ok_or(bad("past the segment's end")).and_then(|bytes| Entry::decode(bytes).map_err(bad))?;
            let end = entry.at.checked_add(u64::from(entry.len)).filter(|end| entry.at >= HEADER as u64 && *end <= described.index_at).ok_or(bad("its record is outside the payloads"))?;
            let payload = &segment[entry.at as usize..end as usize];
            #[cfg(test)]
            if plant.stop_after == Some(taken.records + taken.already) {
                return Err(TakeError::Stopped);
            }
            match place.put_record(Key(entry.key), false, entry.tag, payload).map_err(TakeError::Place)? {
                Put::Same => taken.already += 1,
                Put::Written => {
                    taken.records += 1;
                    taken.bytes += u64::from(entry.len);
                }
                Put::Other => return Err(TakeError::TwoValues { key: entry.key }),
            }
            kept.push(CasKey(entry.key));
        }
        // Durable before the next segment is read; no root names them yet.
        place.commit().map_err(TakeError::Place)?;
        #[cfg(test)]
        if plant.root_each_segment {
            let mut so_far: std::vec::Vec<CasKey, StoreAlloc> = std::vec::Vec::with_capacity_in(kept.len(), alloc);
            so_far.extend_from_slice(&kept);
            so_far.sort_unstable();
            so_far.dedup();
            roots::keep_toolchain(lent, toolchain, &so_far, &[]).map_err(TakeError::Root)?;
            place.commit().map_err(TakeError::Place)?;
        }
    }
    // 4. The root last, then the floor, then the commit that makes both durable.
    kept.sort_unstable();
    kept.dedup();
    roots::keep_toolchain(lent, toolchain, &kept, &[]).map_err(TakeError::Root)?;
    published::keep_floor(&mut place, &set, head.generation, wall).map_err(TakeError::Published)?;
    place.commit().map_err(TakeError::Place)?;
    taken.generation = head.generation;
    taken.how = How::Took;
    Ok(())
}

/// The check of the prefix's head: equal, byte for byte, to the head checked by [`SetCheck`]
/// just before (compared by the caller), so its signature is that check's proof, not checked a
/// second time (rule 74). Anything else is refused.
struct SameHead<'h>(&'h [u8; HEAD_LEN]);

impl HeadCheck for SameHead<'_> {
    fn check(&self, _namespace: Namespace, signer: &PublicKey, domain: &'static str, message: &[u8], signature: &[u8; 64]) -> Result<(), SignatureWhy> {
        let held = self.0;
        let same = domain == HEAD_DOMAIN && message == &held[..HEAD_SIGNED] && signature[..] == held[HEAD_SIGNED..] && signer.0[..] == held[64..96];
        if same { Ok(()) } else { Err(SignatureWhy::UnknownSigner) }
    }
}

/// An origin whose reads are counted: of the head, and of anything.
struct Counted<'o, O: Origin + ?Sized> {
    origin: &'o mut O,
    heads: u32,
    reads: u32,
}

impl<O: Origin + ?Sized> Origin for Counted<'_, O> {
    fn read(&mut self, object: &Object, at: u64, into: &mut [u8], deadline_ns: u64) -> Result<usize, blaze_remote::read::OriginError> {
        self.reads += 1;
        if *object == Object::Head {
            self.heads += 1;
        }
        self.origin.read(object, at, into, deadline_ns)
    }
}

fn now_ns(clock: &Clock) -> u64 {
    u64::try_from(clock.now().since_start().as_nanos()).unwrap_or(u64::MAX)
}

/// One read of `into.len()` bytes at most of `object` from `at`, by a deadline [`READ_WAIT_NS`]
/// away: how many came.
fn read_at<O: Origin + ?Sized>(origin: &mut O, object: Object, at: u64, into: &mut [u8], clock: &Clock) -> Result<usize, TakeError> {
    let deadline = now_ns(clock).saturating_add(READ_WAIT_NS);
    let n = origin.read(&object, at, into, deadline).map_err(|error| TakeError::Read(Unfetched::Origin { object, error }))?;
    Ok(n.min(into.len()))
}

/// `object` whole into `into`, at most `most` bytes (one past it is refused), in reads of at
/// most [`READ_CHUNK`]: its end is the first read that comes short.
fn read_to_end<O: Origin + ?Sized>(origin: &mut O, object: Object, most: u64, into: &mut std::vec::Vec<u8, StoreAlloc>, clock: &Clock) -> Result<(), TakeError> {
    into.clear();
    let bound = usize::try_from(most).unwrap_or(usize::MAX - 1) + 1;
    loop {
        let at = into.len();
        let want = READ_CHUNK.min(bound - at);
        if want == 0 {
            return Err(TakeError::TooLarge { what: "the prefix's length", len: at as u64, most });
        }
        into.resize(at + want, 0);
        let n = read_at(origin, object, at as u64, &mut into[at..], clock)?;
        into.truncate(at + n);
        if n < want {
            return Ok(());
        }
    }
}

/// `len` bytes of `object` from its start into `into`, in reads of at most [`READ_CHUNK`]; fewer
/// is refused short.
fn read_exact<O: Origin + ?Sized>(origin: &mut O, object: Object, len: usize, into: &mut std::vec::Vec<u8, StoreAlloc>, clock: &Clock) -> Result<(), TakeError> {
    into.clear();
    into.resize(len, 0);
    let mut at = 0;
    while at < len {
        let end = (at + READ_CHUNK).min(len);
        let n = read_at(origin, object, at as u64, &mut into[at..end], clock)?;
        if n == 0 {
            return Err(TakeError::Read(Unfetched::Short { object, got: at as u64, wanted: len as u64 }));
        }
        at += n;
    }
    Ok(())
}

/// A fresh home takes the set whole once; its second command takes nothing; a flipped byte is
/// refused (planted: taken unverified); a take stopped anywhere, syncs failing, is finished by the
/// next command and never leaves half a root (a DST); over HTTP equal to the folder.
#[cfg(test)]
#[path = "take_tests.rs"]
mod tests;

/// The take over HTTP, from the simulated server serving the folder: byte-equal to the folder's.
#[cfg(test)]
#[path = "take_http_tests.rs"]
mod http_tests;
