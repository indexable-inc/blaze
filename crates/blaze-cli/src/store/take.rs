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
//! 1. On an already-rooted home past the freshness window, the 192-byte head is asked first so
//!    `How::Current` transfers no prefix bytes. On a fresh or unrooted home, `prefix` (the head,
//!    the manifest, every summary) is fetched directly in 1 RTT and its leading `HEAD_LEN` bytes
//!    checked as the head: signed over the set's id by a key trusted for the origin (blaze-remote
//!    trust.rs `trusted_for`), at a generation no lower than the home's floor (a rollback). Its
//!    signature is verified ONCE (rule 74), its manifest against the head's digest, and its
//!    summaries against the rows.
//! 2. Each wave of up to [`SEGMENT_IN_FLIGHT`] segments the manifest names: optionally accelerated
//!    by a LAN `blaze peer` (`HAVE_SEGZ` UDP probe + concurrent keep-alive `GET /v1/segz/<blake3>`
//!    with full BLAKE3 verification before accepting any bytes), with any remaining wave segments
//!    fetched concurrently via `Origin::read_batch`. Every segment's bytes are checked ONCE
//!    against the digest its row names (streaming BLAKE3 via `decode_wire_segment_verified`, the
//!    chain up to the signed head), and each record is put as this home's own `LOCAL` record under
//!    its own tag; a record already held with equal bytes is counted "already here". Commits are
//!    coalesced to at most one per intermediate wave, and the final wave rides step 3's single
//!    atomic commit.
//! 3. The toolchain's root names every record taken, then the floor (the generation, the ask's
//!    time); then the one commit that makes both durable. The root is named LAST: a crash
//!    anywhere before leaves no root, never half of one.
//!
//! A set that cannot be taken (an unreachable folder or host, a refused head, a segment that is
//! not its row's) stops the command by name: nothing is made in its place (rule 3). With no
//! `--origin` nothing here runs and nothing is read.

use core::fmt;
use core::net::{IpAddr, Ipv4Addr, SocketAddr};
use core::num::NonZeroUsize;
use core::time::Duration;

use blaze_cas::Key as CasKey;
#[cfg(test)]
use blaze_cas::plain::{ENTRY, Entry, HEADER};
use blaze_cas::plain::Lent;
use blaze_core::Driver;
use blaze_drive::Key;
use blaze_ids::ContentId;
use blaze_keyed::store::{Put, StoreError};
use blaze_remote::PublicKey;
use blaze_remote::published::set::{SetCheck, SetName, SetNameError};
use blaze_remote::read::form::{HEAD_DOMAIN, HEAD_LEN, HEAD_SIGNED, PREFIX_MOST, decode_prefix};
use blaze_remote::read::{BatchOutcome, BatchRead, Head, HeadCheck, Namespace, Object, Origin, OriginError, Refused, SignatureWhy, Unfetched};
#[cfg(test)]
use blaze_remote::wire_segment::decode_wire_segment;
use blaze_remote::wire_segment::{VerifiedWireSegment, WireSegmentError, decode_wire_segment_verified};
use blaze_toolchain::ToolchainId;
use bstd::clock::Clock;
use bstd::cx::Cx;
use bstd::mem::TASK_FRAME;
use bstd::out::Out;
use bstd::par::{self, Grain};

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
/// Bounded concurrent `BZS1` segment downloads & parallel decodes per wave (rule 18; matches `ORIGIN_IN_FLIGHT`).
pub(crate) const SEGMENT_IN_FLIGHT: usize = 16;
/// Bounded UDP discovery window for LAN `blaze peer` `HAVE_SEGZ` probes (rule 18).
const LAN_SEGZ_PROBE_WAIT: Duration = Duration::from_millis(20);
/// Maximum concurrent keep-alive TCP workers fetching `BZS1` segments from one LAN peer (rule 18).
const LAN_SEGZ_WORKERS_MOST: usize = 4;
/// Default LAN multicast + broadcast probe targets for `blaze peer` `HAVE_SEGZ` discovery.
const DEFAULT_LAN_TARGETS: [SocketAddr; 2] = [
    SocketAddr::new(IpAddr::V4(blaze_fetch::peer::MULTICAST_GROUP), blaze_fetch::peer::DEFAULT_UDP_PORT),
    SocketAddr::new(IpAddr::V4(Ipv4Addr::BROADCAST), blaze_fetch::peer::DEFAULT_UDP_PORT),
];

/// Explicit LAN `blaze peer` discovery parameters for `BZS1` segment acceleration (rule 75).
#[derive(Clone, Copy)]
pub(crate) struct LanSegzPeer<'a> {
    pub net: &'a bstd::net::Net,
    pub targets: &'a [SocketAddr],
    pub wait: Duration,
}
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
    /// `BZS1` wire decoding refused it before segment verification.
    Wire(WireSegmentError),
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
            TakeError::Segment { object, why: SegmentWhy::Wire(error) } => write!(f, "{object}: {error}")?,
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
    let lan_peer = match (&named.at, net) {
        (OriginAt::Http(_), Some(net)) if !cx.is_simulated() => Some(LanSegzPeer {
            net,
            targets: &DEFAULT_LAN_TARGETS,
            wait: LAN_SEGZ_PROBE_WAIT,
        }),
        _ => None,
    };
    let taken = take_with_lan_peer(cx, &named, id, &opened.lent, toolchain, &clock, lan_peer, TakePlant::NONE)?;
    if taken.how == How::Took {
        err.line(cx, format_args!("blaze: {taken}"))?;
    }
    phases.published = Some(taken);
    Ok(())
}

/// [`take_from`] over the origin `named` reaches: its folder, or its `http://` / `https://` base.
pub(crate) fn take<D: Driver>(cx: &Cx, named: &NamedOrigin, set: ContentId, lent: &Lent<D, StoreAlloc>, toolchain: ToolchainId, clock: &Clock, plant: TakePlant) -> Result<Taken, TakeError> {
    take_with_lan_peer(cx, named, set, lent, toolchain, clock, None, plant)
}

#[expect(clippy::too_many_arguments, reason = "take entry with explicit optional LAN peer acceleration (rule 75)")]
pub(crate) fn take_with_lan_peer<D: Driver>(
    cx: &Cx,
    named: &NamedOrigin,
    set: ContentId,
    lent: &Lent<D, StoreAlloc>,
    toolchain: ToolchainId,
    clock: &Clock,
    lan_peer: Option<LanSegzPeer<'_>>,
    plant: TakePlant,
) -> Result<Taken, TakeError> {
    let keys = named.check.named_keys();
    match &named.at {
        OriginAt::Folder(folder) => take_from_inner(Some(cx), &mut FolderOrigin { cx, folder }, keys, set, lent, toolchain, clock, lan_peer, plant),
        OriginAt::Http(at) => take_from_inner(Some(cx), &mut HttpOrigin::new(cx, at, clock), keys, set, lent, toolchain, clock, lan_peer, plant),
    }
}

/// The set `set`, read from `origin` and believed from a key trusted for it (`keys`: those the
/// command named), taken into `lent` as the module says.
pub(crate) fn take_from<D: Driver, O: Origin + ?Sized>(origin: &mut O, keys: &[PublicKey], set: ContentId, lent: &Lent<D, StoreAlloc>, toolchain: ToolchainId, clock: &Clock, plant: TakePlant) -> Result<Taken, TakeError> {
    take_from_inner(None, origin, keys, set, lent, toolchain, clock, None, plant)
}

#[expect(clippy::too_many_arguments, reason = "the take's internal entry with optional parallel decode and LAN peer context")]
fn take_from_inner<D: Driver, O: Origin + ?Sized>(
    cx: Option<&Cx>,
    origin: &mut O,
    keys: &[PublicKey],
    set: ContentId,
    lent: &Lent<D, StoreAlloc>,
    toolchain: ToolchainId,
    clock: &Clock,
    lan_peer: Option<LanSegzPeer<'_>>,
    plant: TakePlant,
) -> Result<Taken, TakeError> {
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
    let result = take_counted(cx, &mut origin, keys, set, lent, toolchain, clock, lan_peer, plant, (seen.generation, wall, rooted), &mut taken);
    taken.head_reads = origin.heads;
    taken.reads = origin.reads;
    taken.take_ns = now_ns(clock).saturating_sub(began);
    result.map(|()| taken)
}

#[expect(clippy::too_many_arguments, reason = "the take's edge, its counted origin, optional LAN peer, and its tally")]
fn take_counted<D: Driver, O: Origin + ?Sized>(
    cx: Option<&Cx>,
    origin: &mut Counted<'_, O>,
    keys: &[PublicKey],
    set: ContentId,
    lent: &Lent<D, StoreAlloc>,
    toolchain: ToolchainId,
    clock: &Clock,
    lan_peer: Option<LanSegzPeer<'_>>,
    plant: TakePlant,
    (floor, wall, rooted): (u64, u64, bool),
    taken: &mut Taken,
) -> Result<(), TakeError> {
    // The plant is read in a test alone.
    let _ = plant;
    let refused = |why: Refused| TakeError::Read(Unfetched::Refused(why));
    let alloc = *lent.alloc();
    let mut place = PlainPlace::new(lent, false);
    let mut bytes: std::vec::Vec<u8, StoreAlloc> = std::vec::Vec::new_in(alloc);
    // 1. Head + prefix:
    //    - On an already-rooted home past the freshness window, ask the 192-byte Head first so
    //      `How::Current` transfers zero prefix bytes.
    //    - On a fresh or unrooted home, fetch `Object::Prefix` directly in 1 RTT and parse its
    //      leading `HEAD_LEN` bytes as `Head` (1 Ed25519 verification, rule 74).
    let (head, checked) = if rooted && floor > 0 {
        let mut head_bytes = [0u8; HEAD_LEN + 1];
        let n = read_at(origin, Object::Head, 0, &mut head_bytes, clock)?;
        let head = Head::decode(&head_bytes[..n], Namespace::PUBLIC, &SetCheck { set, keys, seen: floor }).map_err(refused)?;
        let mut checked = [0u8; HEAD_LEN];
        checked.copy_from_slice(&head_bytes[..HEAD_LEN]);
        if head.generation == floor {
            // Current: the window starts again; nothing else is read.
            published::keep_floor(&mut place, &set, floor, wall).map_err(TakeError::Published)?;
            place.commit().map_err(TakeError::Place)?;
            taken.how = How::Current;
            return Ok(());
        }
        read_to_end(origin, Object::Prefix, PREFIX_MOST, &mut bytes, clock)?;
        if bytes.len() < HEAD_LEN || bytes[..HEAD_LEN] != checked[..] {
            return Err(TakeError::Moved);
        }
        (head, checked)
    } else {
        read_to_end(origin, Object::Prefix, PREFIX_MOST, &mut bytes, clock)?;
        let head_slice = bytes.get(..HEAD_LEN).unwrap_or(bytes.as_slice());
        let head = Head::decode(head_slice, Namespace::PUBLIC, &SetCheck { set, keys, seen: floor }).map_err(refused)?;
        let mut checked = [0u8; HEAD_LEN];
        checked.copy_from_slice(head_slice);
        (head, checked)
    };
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
    // 2. Segments fetched in bounded parallel waves (`SEGMENT_IN_FLIGHT = 16`), optionally served
    //    first by a verified LAN `blaze peer` (`BZS1` over keep-alive TCP) and remaining segments
    //    via `Origin::read_batch`, decoded and verified ONCE via streaming BLAKE3, then put in
    //    deterministic row order with commits coalesced to at most one per intermediate wave.
    let records_cap = usize::try_from(records_named).unwrap_or(0);
    let mut kept: blaze_own::Vec<CasKey, StoreAlloc> = blaze_own::Vec::with_capacity_in(records_cap, alloc);
    let kept_alloc = blaze_mem::Kept(blaze_mem::Tag::Store);
    let mut lan_wire_bufs: blaze_own::Vec<blaze_own::Vec<u8, blaze_mem::Kept>, StoreAlloc> = blaze_own::Vec::with_capacity_in(SEGMENT_IN_FLIGHT, alloc);
    let mut wire_bufs: blaze_own::Vec<blaze_own::Vec<u8, blaze_mem::Kept>, StoreAlloc> = blaze_own::Vec::with_capacity_in(SEGMENT_IN_FLIGHT, alloc);
    for _ in 0..SEGMENT_IN_FLIGHT {
        lan_wire_bufs.push(blaze_own::Vec::new_in(kept_alloc));
        wire_bufs.push(blaze_own::Vec::new_in(kept_alloc));
    }

    let rows = prefixed.rows.as_slice();
    let mut wave_start = 0usize;
    while wave_start < rows.len() {
        let wave_end = (wave_start + SEGMENT_IN_FLIGHT).min(rows.len());
        let Some(wave_rows) = rows.get(wave_start..wave_end) else { break };
        let wave_len = wave_rows.len();

        // Validate every row's declared lengths BEFORE allocating or fetching any bytes.
        let mut wire_lens: blaze_own::Vec<usize, StoreAlloc> = blaze_own::Vec::with_capacity_in(wave_len, alloc);
        for row in wave_rows {
            if row.len > SEGMENT_TAKEN_MOST {
                return Err(TakeError::TooLarge { what: "a segment's length", len: row.len, most: SEGMENT_TAKEN_MOST });
            }
            if row.wire_len > SEGMENT_TAKEN_MOST {
                return Err(TakeError::TooLarge { what: "a wire segment's length", len: row.wire_len, most: SEGMENT_TAKEN_MOST });
            }
            let wire_len = usize::try_from(row.wire_len).map_err(|_| TakeError::TooLarge { what: "a wire segment's length", len: row.wire_len, most: SEGMENT_TAKEN_MOST })?;
            wire_lens.push(wire_len);
        }

        // 2a. Optional LAN `blaze peer` P2P `BZS1` acceleration (zero-trust: full canonical BLPS
        //     BLAKE3 digest + `Namespace::PUBLIC` + row metadata verified BEFORE acceptance).
        let mut lan_decoded: blaze_own::Vec<Option<VerifiedWireSegment<'_, blaze_mem::Kept>>, StoreAlloc> =
            blaze_own::Vec::with_capacity_in(wave_len, alloc);
        for _ in 0..wave_len {
            lan_decoded.push(None);
        }
        let mut lan_hit_mask = 0u32;
        #[cfg(test)]
        let skip_lan = plant.unverified;
        #[cfg(not(test))]
        let skip_lan = false;
        if !skip_lan
            && let Some(cx) = cx
            && let Some(lan) = lan_peer
            && !wave_rows.is_empty()
        {
            let mut digests: blaze_own::Vec<ContentId, StoreAlloc> = blaze_own::Vec::with_capacity_in(wave_len, alloc);
            for row in wave_rows {
                digests.push(row.segment);
            }
            let (nonce, port_seed) = lan_probe_seeds(&set, head.generation, wave_start, now_ns(clock));
            if let Ok(Some(peer)) = crate::peer::probe_lan_segz(
                cx,
                lan.net,
                clock,
                nonce,
                port_seed,
                lan.targets,
                digests.as_slice(),
                lan.wait,
            ) && peer.mask != 0
                && let Some(active_lan_bufs) = lan_wire_bufs.as_mut_slice().get_mut(..wave_len)
            {
                let verifying_lan = now_ns(clock);
                fetch_wave_from_lan_peer(
                    cx,
                    lan.net,
                    clock,
                    peer.addr,
                    peer.mask,
                    wave_rows,
                    active_lan_bufs,
                    lan_decoded.as_mut_slice(),
                    &mut lan_hit_mask,
                    kept_alloc,
                    alloc,
                );
                taken.verify_ns += now_ns(clock).saturating_sub(verifying_lan);
            }
        }

        // 2b. Parallel wave download of remaining `segz-<digest>` wire segments via `Origin::read_batch`.
        let mut missing_indices: blaze_own::Vec<usize, StoreAlloc> = blaze_own::Vec::with_capacity_in(wave_len, alloc);
        for i in 0..wave_len {
            let lan_hit = u32::try_from(i)
                .ok()
                .is_some_and(|bit| bit < 32 && (lan_hit_mask & (1u32 << bit)) != 0);
            if !lan_hit {
                missing_indices.push(i);
            }
        }
        if !missing_indices.is_empty()
            && let Some(active_bufs) = wire_bufs.as_mut_slice().get_mut(..wave_len)
        {
            let deadline = now_ns(clock).saturating_add(READ_WAIT_NS);
            let mut reads: blaze_own::Vec<BatchRead<'_>, StoreAlloc> = blaze_own::Vec::with_capacity_in(missing_indices.len(), alloc);
            for (i, (row, buf)) in wave_rows.iter().zip(active_bufs.iter_mut()).enumerate() {
                let lan_hit = u32::try_from(i)
                    .ok()
                    .is_some_and(|bit| bit < 32 && (lan_hit_mask & (1u32 << bit)) != 0);
                if lan_hit {
                    continue;
                }
                let wire_len = wire_lens.as_slice().get(i).copied().unwrap_or(0);
                buf.clear();
                buf.resize(wire_len, 0);
                reads.push(BatchRead {
                    id: u32::try_from(i).unwrap_or(0),
                    object: Object::SegmentWire(row.segment),
                    at: 0,
                    into: buf.as_mut_slice(),
                    deadline_ns: deadline,
                });
            }
            let mut outcomes: blaze_own::Vec<BatchOutcome, StoreAlloc> = blaze_own::Vec::with_capacity_in(reads.len(), alloc);
            outcomes.resize(reads.len(), BatchOutcome { result: Err(OriginError::Unreadable), at_ns: 0 });
            let mut now_fn = || now_ns(clock);
            origin.read_batch(reads.as_mut_slice(), &mut now_fn, outcomes.as_mut_slice());
            drop(reads);

            // Check outcomes in row order; finish any partial read cleanly or fail by name.
            for (&i, outcome) in missing_indices.as_slice().iter().zip(outcomes.as_slice().iter()) {
                let Some(row) = wave_rows.get(i) else { continue };
                let object = Object::SegmentWire(row.segment);
                let wire_len = wire_lens.as_slice().get(i).copied().unwrap_or(0);
                let mut got = outcome.result.map_err(|error| TakeError::Read(Unfetched::Origin { object, error }))?.min(wire_len);
                if got == 0 && wire_len > 0 {
                    return Err(TakeError::Read(Unfetched::Short { object, got: 0, wanted: row.wire_len }));
                }
                while got < wire_len {
                    let Some(buf) = active_bufs.get_mut(i) else { break };
                    let Some(tail) = buf.as_mut_slice().get_mut(got..wire_len) else { break };
                    let at_u64 = u64::try_from(got).unwrap_or(u64::MAX);
                    let n = read_at(origin, object, at_u64, tail, clock)?;
                    if n == 0 {
                        return Err(TakeError::Read(Unfetched::Short { object, got: at_u64, wanted: row.wire_len }));
                    }
                    got += n;
                }
            }
        }

        // Planted-bug test path: unverified decode without BLAKE3 digest verification.
        #[cfg(test)]
        if plant.unverified {
            let mut segment: blaze_own::Vec<u8, StoreAlloc> = blaze_own::Vec::new_in(alloc);
            for (i, row) in wave_rows.iter().enumerate() {
                let object = Object::SegmentWire(row.segment);
                let wire = wire_bufs.as_slice().get(i).map_or(&[][..], |b| b.as_slice());
                decode_wire_segment(wire, row.len, &mut segment, alloc).map_err(|why| TakeError::Segment { object, why: SegmentWhy::Wire(why) })?;
                let len = segment.len();
                let verifying = now_ns(clock);
                let described = (len >= HEADER + blaze_cas::plain::FOOTER)
                    .then(|| blaze_cas::plain::describe(&segment[..HEADER], &segment[len - blaze_cas::plain::FOOTER..], u64::try_from(len).unwrap_or(u64::MAX)))
                    .flatten()
                    .ok_or(TakeError::Segment { object, why: SegmentWhy::NotItsRow })?;
                taken.verify_ns += now_ns(clock).saturating_sub(verifying);
                if described.digest != row.segment || described.count != row.count || described.index_at != row.index_at || described.namespace != Namespace::PUBLIC {
                    return Err(TakeError::Segment { object, why: SegmentWhy::NotItsRow });
                }
                let index_at = usize::try_from(described.index_at).map_err(|_| TakeError::Segment { object, why: SegmentWhy::NotItsRow })?;
                for idx in 0..usize::try_from(described.count).unwrap_or(0) {
                    let at = index_at + idx * ENTRY;
                    let entry_at = u64::try_from(at).unwrap_or(u64::MAX);
                    let bad = |what| TakeError::Segment { object, why: SegmentWhy::Entry { what, at: entry_at } };
                    let entry = segment.get(at..at + ENTRY).ok_or(bad("past the segment's end")).and_then(|b| Entry::decode(b).map_err(bad))?;
                    let end = entry.at.checked_add(u64::from(entry.len)).filter(|end| entry.at >= u64::try_from(HEADER).unwrap_or(128) && *end <= described.index_at).ok_or(bad("its record is outside the payloads"))?;
                    let start_u = usize::try_from(entry.at).map_err(|_| bad("offset overflow"))?;
                    let end_u = usize::try_from(end).map_err(|_| bad("end overflow"))?;
                    let payload = segment.get(start_u..end_u).ok_or(bad("payload slice"))?;
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
                if plant.root_each_segment {
                    place.commit().map_err(TakeError::Place)?;
                    let mut so_far: blaze_own::Vec<CasKey, StoreAlloc> = blaze_own::Vec::with_capacity_in(kept.len(), alloc);
                    so_far.extend_from_slice(&kept);
                    so_far.sort_unstable();
                    so_far.dedup();
                    roots::keep_toolchain(lent, toolchain, &so_far, &[]).map_err(TakeError::Root)?;
                    place.commit().map_err(TakeError::Place)?;
                }
            }
            if wave_end < rows.len() {
                place.commit().map_err(TakeError::Place)?;
            }
            wave_start = wave_end;
            continue;
        }

        // 2c. Verified zero-BLPS-copy decode + streaming BLAKE3 verification for origin-fetched
        //     segments (LAN segments are already verified ONCE in `lan_decoded`, rule 74).
        let mut r2_decoded: blaze_own::Vec<Option<Result<VerifiedWireSegment<'_, blaze_mem::Kept>, SegmentWhy>>, StoreAlloc> =
            blaze_own::Vec::with_capacity_in(missing_indices.len(), alloc);
        for _ in 0..missing_indices.len() {
            r2_decoded.push(None);
        }
        if !missing_indices.is_empty() {
            let mut inputs: blaze_own::Vec<(&blaze_remote::read::form::Row, &[u8]), StoreAlloc> =
                blaze_own::Vec::with_capacity_in(missing_indices.len(), alloc);
            for &i in missing_indices.as_slice() {
                if let Some(row) = wave_rows.get(i) {
                    let wire = wire_bufs.as_slice().get(i).map_or(&[][..], |b| b.as_slice());
                    inputs.push((row, wire));
                }
            }

            let verifying = now_ns(clock);
            if let Some(cx) = cx
                && inputs.len() > 1
            {
                let scope = cx.scope();
                let grain = Grain::new(NonZeroUsize::MIN, TASK_FRAME);
                let mapped = par::map_into(cx, &scope, inputs.as_slice(), r2_decoded.as_mut_slice(), grain, |&(row, wire)| {
                    Some(verify_wave_segment(row, wire, kept_alloc))
                });
                let joined = scope.join(cx);
                if mapped.is_err() || joined.is_err() {
                    // Any worker panic/cancellation stops cleanly with a typed segment check failure.
                    let object = missing_indices
                        .as_slice()
                        .first()
                        .and_then(|&i| wave_rows.get(i))
                        .map_or(Object::Prefix, |r| Object::SegmentWire(r.segment));
                    return Err(TakeError::Segment { object, why: SegmentWhy::NotItsRow });
                }
            } else {
                for (&(row, wire), slot) in inputs.as_slice().iter().zip(r2_decoded.as_mut_slice().iter_mut()) {
                    *slot = Some(verify_wave_segment(row, wire, kept_alloc));
                }
            }
            taken.verify_ns += now_ns(clock).saturating_sub(verifying);
        }

        // 2d. Put verified records in deterministic manifest row order (LAN hits + origin fetches).
        let mut r2_cursor = 0usize;
        for (i, row) in wave_rows.iter().enumerate() {
            let object = Object::SegmentWire(row.segment);
            let verified = if let Some(v) = lan_decoded.as_mut_slice().get_mut(i).and_then(Option::take) {
                v
            } else {
                let slot = r2_decoded.as_mut_slice().get_mut(r2_cursor);
                r2_cursor += 1;
                match slot.and_then(Option::take) {
                    Some(Ok(v)) => v,
                    Some(Err(why)) => return Err(TakeError::Segment { object, why }),
                    None => return Err(TakeError::Segment { object, why: SegmentWhy::NotItsRow }),
                }
            };
            for (key, tag, payload) in verified.iter() {
                #[cfg(test)]
                if plant.stop_after == Some(taken.records + taken.already) {
                    return Err(TakeError::Stopped);
                }
                match place.put_record(Key(key), false, tag, payload).map_err(TakeError::Place)? {
                    Put::Same => taken.already += 1,
                    Put::Written => {
                        taken.records += 1;
                        taken.bytes += u64::try_from(payload.len()).unwrap_or(u64::MAX);
                    }
                    Put::Other => return Err(TakeError::TwoValues { key }),
                }
                kept.push(CasKey(key));
            }
            #[cfg(test)]
            if plant.root_each_segment {
                place.commit().map_err(TakeError::Place)?;
                let mut so_far: blaze_own::Vec<CasKey, StoreAlloc> = blaze_own::Vec::with_capacity_in(kept.len(), alloc);
                so_far.extend_from_slice(&kept);
                so_far.sort_unstable();
                so_far.dedup();
                roots::keep_toolchain(lent, toolchain, &so_far, &[]).map_err(TakeError::Root)?;
                place.commit().map_err(TakeError::Place)?;
            }
        }
        // Coalesce commits: at most ONE commit per intermediate wave (no root names them yet),
        // letting the final wave ride step 3's single atomic commit with the toolchain root & floor.
        if wave_end < rows.len() {
            place.commit().map_err(TakeError::Place)?;
        }

        wave_start = wave_end;
    }
    // 3. The root last, then the floor, then the single atomic commit that makes everything durable.
    kept.sort_unstable();
    kept.dedup();
    roots::keep_toolchain(lent, toolchain, &kept, &[]).map_err(TakeError::Root)?;
    published::keep_floor(&mut place, &set, head.generation, wall).map_err(TakeError::Published)?;
    place.commit().map_err(TakeError::Place)?;
    taken.generation = head.generation;
    taken.how = How::Took;
    Ok(())
}

/// Derives deterministic per-wave `(nonce, port_seed)` for a LAN `HAVE_SEGZ` probe (rule 75).
fn lan_probe_seeds(set: &ContentId, generation: u64, wave_start: usize, now_ns: u64) -> (u64, u64) {
    let mut buf = [0u8; 56];
    buf[..32].copy_from_slice(&set.0);
    buf[32..40].copy_from_slice(&generation.to_le_bytes());
    buf[40..48].copy_from_slice(&u64::try_from(wave_start).unwrap_or(0).to_le_bytes());
    buf[48..56].copy_from_slice(&now_ns.to_le_bytes());
    let hash = blaze_blake3::hash(&buf);
    let nonce = u64::from_le_bytes([hash[0], hash[1], hash[2], hash[3], hash[4], hash[5], hash[6], hash[7]]);
    let port_seed = u64::from_le_bytes([hash[8], hash[9], hash[10], hash[11], hash[12], hash[13], hash[14], hash[15]]);
    (nonce, port_seed)
}

/// Fetches all segments advertised in `peer_mask` from a LAN `blaze peer` using up to
/// [`LAN_SEGZ_WORKERS_MOST`] concurrent HTTP/1.1 keep-alive TCP connections, cryptographically
/// verifying each segment's full canonical `BLPS` BLAKE3 digest and manifest row metadata before
/// recording a hit.
#[expect(clippy::too_many_arguments, reason = "parallel keep-alive LAN peer wave fetcher")]
fn fetch_wave_from_lan_peer<'w>(
    cx: &Cx,
    net: &bstd::net::Net,
    clock: &Clock,
    peer_addr: SocketAddr,
    peer_mask: u32,
    wave_rows: &[blaze_remote::read::form::Row],
    lan_bufs: &'w mut [blaze_own::Vec<u8, blaze_mem::Kept>],
    lan_decoded: &mut [Option<VerifiedWireSegment<'w, blaze_mem::Kept>>],
    lan_hit_mask: &mut u32,
    kept_alloc: blaze_mem::Kept,
    alloc: StoreAlloc,
) {
    let matched = (0..wave_rows.len())
        .filter(|&i| u32::try_from(i).ok().is_some_and(|bit| bit < 32 && (peer_mask & (1u32 << bit)) != 0))
        .count();
    if matched == 0 {
        return;
    }
    let scope = cx.scope();
    let avail = usize::try_from(scope.workers()).unwrap_or(1);
    let worker_count = matched.min(LAN_SEGZ_WORKERS_MOST).min(avail).max(1);
    if worker_count <= 1 {
        let _ = scope.join(cx);
        let fetched = fetch_lan_chunk(cx, net, clock, peer_addr, peer_mask, 0, wave_rows, lan_bufs, kept_alloc);
        for (idx, verified) in fetched {
            if let Some(slot) = lan_decoded.get_mut(idx) {
                *slot = Some(verified);
                if let Ok(bit) = u32::try_from(idx) && bit < 32 {
                    *lan_hit_mask |= 1u32 << bit;
                }
            }
        }
        return;
    }

    let mut tasks: blaze_own::Vec<
        bstd::task::Task<'_, blaze_own::Vec<(usize, VerifiedWireSegment<'w, blaze_mem::Kept>), blaze_mem::Kept>>,
        StoreAlloc,
    > = blaze_own::Vec::with_capacity_in(worker_count, alloc);
    let mut rem_rows = wave_rows;
    let mut rem_bufs = lan_bufs;
    let mut offset = 0usize;
    let mut rem_workers = worker_count;
    while rem_workers > 0 && !rem_rows.is_empty() {
        let take_len = if rem_workers == 1 {
            rem_rows.len()
        } else {
            rem_rows.len().div_ceil(rem_workers)
        };
        let (rows_chunk, next_rows) = rem_rows.split_at(take_len);
        let (bufs_chunk, next_bufs) = rem_bufs.split_at_mut(take_len);
        let chunk_offset = offset;
        rem_rows = next_rows;
        rem_bufs = next_bufs;
        offset += take_len;
        rem_workers -= 1;

        let has_match = (0..rows_chunk.len()).any(|local_i| {
            u32::try_from(chunk_offset + local_i)
                .ok()
                .is_some_and(|bit| bit < 32 && (peer_mask & (1u32 << bit)) != 0)
        });
        if !has_match {
            continue;
        }
        let task = scope.may_overlap("take-lan-segz", TASK_FRAME, move |worker_cx| -> Result<_, core::convert::Infallible> {
            Ok(fetch_lan_chunk(
                worker_cx,
                net,
                clock,
                peer_addr,
                peer_mask,
                chunk_offset,
                rows_chunk,
                bufs_chunk,
                kept_alloc,
            ))
        });
        tasks.push(task);
    }
    for task in tasks {
        if let Ok(fetched) = task.join(cx) {
            for (idx, verified) in fetched {
                if let Some(slot) = lan_decoded.get_mut(idx) {
                    *slot = Some(verified);
                    if let Ok(bit) = u32::try_from(idx) && bit < 32 {
                        *lan_hit_mask |= 1u32 << bit;
                    }
                }
            }
        }
    }
    let _ = scope.join(cx);
}

#[expect(clippy::too_many_arguments, reason = "one keep-alive worker chunk for LAN peer BZS1 fetch")]
fn fetch_lan_chunk<'w>(
    cx: &Cx,
    net: &bstd::net::Net,
    clock: &Clock,
    peer_addr: SocketAddr,
    peer_mask: u32,
    chunk_offset: usize,
    rows_chunk: &[blaze_remote::read::form::Row],
    bufs_chunk: &'w mut [blaze_own::Vec<u8, blaze_mem::Kept>],
    kept_alloc: blaze_mem::Kept,
) -> blaze_own::Vec<(usize, VerifiedWireSegment<'w, blaze_mem::Kept>), blaze_mem::Kept> {
    let mut out: blaze_own::Vec<(usize, VerifiedWireSegment<'w, blaze_mem::Kept>), blaze_mem::Kept> =
        blaze_own::Vec::with_capacity_in(rows_chunk.len(), kept_alloc);
    let mut remaining = (0..rows_chunk.len())
        .filter(|&local_i| {
            u32::try_from(chunk_offset + local_i)
                .ok()
                .is_some_and(|bit| bit < 32 && (peer_mask & (1u32 << bit)) != 0)
        })
        .count();
    if remaining == 0 {
        return out;
    }
    if remaining == 1 {
        for (local_i, (row, buf)) in rows_chunk.iter().zip(bufs_chunk.iter_mut()).enumerate() {
            let idx = chunk_offset + local_i;
            let wanted = u32::try_from(idx)
                .ok()
                .is_some_and(|bit| bit < 32 && (peer_mask & (1u32 << bit)) != 0);
            if !wanted {
                continue;
            }
            if let Ok(verified) = crate::peer::fetch_verified_segz_from_peer(cx, net, clock, peer_addr, row.segment, row.len, buf, kept_alloc) {
                let described = verified.described();
                if described.digest == row.segment
                    && described.count == row.count
                    && described.index_at == row.index_at
                    && described.namespace == Namespace::PUBLIC
                {
                    out.push((idx, verified));
                }
            }
            break;
        }
        return out;
    }
    let Ok(mut conn) = crate::peer::SegzPeerConn::connect(cx, net, clock, peer_addr) else {
        return out;
    };
    for (local_i, (row, buf)) in rows_chunk.iter().zip(bufs_chunk.iter_mut()).enumerate() {
        let idx = chunk_offset + local_i;
        let wanted = u32::try_from(idx)
            .ok()
            .is_some_and(|bit| bit < 32 && (peer_mask & (1u32 << bit)) != 0);
        if !wanted {
            continue;
        }
        if !conn.is_open() {
            match crate::peer::SegzPeerConn::connect(cx, net, clock, peer_addr) {
                Ok(reconnected) => conn = reconnected,
                Err(_) => break,
            }
        }
        let keep_alive = remaining > 1;
        remaining -= 1;
        if let Ok(verified) = conn.fetch_verified(cx, clock, row.segment, row.len, keep_alive, buf, kept_alloc) {
            let described = verified.described();
            if described.digest == row.segment
                && described.count == row.count
                && described.index_at == row.index_at
                && described.namespace == Namespace::PUBLIC
            {
                out.push((idx, verified));
            }
        }
    }
    conn.finish(cx, clock);
    out
}

/// Decodes and streaming-verifies one `BZS1` wire segment against its manifest row's exact
/// BLAKE3 digest, record count, index offset, and `PUBLIC` namespace (zero redundant BLPS copy).
fn verify_wave_segment<'w>(
    row: &blaze_remote::read::form::Row,
    wire: &'w [u8],
    kept_alloc: blaze_mem::Kept,
) -> Result<VerifiedWireSegment<'w, blaze_mem::Kept>, SegmentWhy> {
    let verified = decode_wire_segment_verified(wire, row.len, kept_alloc).map_err(SegmentWhy::Wire)?;
    let described = verified.described();
    if described.digest != row.segment
        || described.count != row.count
        || described.index_at != row.index_at
        || described.namespace != Namespace::PUBLIC
    {
        return Err(SegmentWhy::NotItsRow);
    }
    Ok(verified)
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
    fn read(&mut self, object: &Object, at: u64, into: &mut [u8], deadline_ns: u64) -> Result<usize, OriginError> {
        self.reads += 1;
        if *object == Object::Head {
            self.heads += 1;
        }
        self.origin.read(object, at, into, deadline_ns)
    }

    fn read_batch(
        &mut self,
        reads: &mut [BatchRead<'_>],
        clock: &mut dyn FnMut() -> u64,
        outcomes: &mut [BatchOutcome],
    ) {
        for req in reads.iter() {
            self.reads += 1;
            if req.object == Object::Head {
                self.heads += 1;
            }
        }
        self.origin.read_batch(reads, clock, outcomes);
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
        let at_u64 = u64::try_from(at).unwrap_or(u64::MAX);
        if want == 0 {
            return Err(TakeError::TooLarge { what: "the prefix's length", len: at_u64, most });
        }
        into.resize(at + want, 0);
        let n = read_at(origin, object, at_u64, &mut into[at..], clock)?;
        into.truncate(at + n);
        if n < want {
            return Ok(());
        }
    }
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

#[cfg(test)]
mod pipeline_tests {
    use blaze_remote::trust::Signer;
    use bstd::mem::TASK_FRAME;
    use bstd::test::BUDGET;

    use super::super::FolderOrigin;
    use super::super::published_e2e_tests::{N, SEED, TOOLCHAIN, developer_home, path, set_path};
    use super::tests::{named_by_root, set_id, world};
    use super::{How, SEGMENT_IN_FLIGHT, TakePlant, take_from};

    #[test]
    fn fresh_home_fetches_prefix_in_one_rtt_and_coalesces_single_wave_commit() {
        let world = world(89);
        let cx = world.io.cx(BUDGET, TASK_FRAME).expect("root context");
        let segments = world.objects.iter().filter(|(name, _)| name.starts_with("seg-")).count();
        assert!(segments >= 4 && segments <= SEGMENT_IN_FLIGHT, "expected 4..={SEGMENT_IN_FLIGHT} segments, got {segments}");

        let (_home, lent) = developer_home(&world.io, &cx, "/b", 6);
        let gen_before = lent.generation();
        let clock = world.io.need_clock().expect("clock");
        let dir = world.io.need_fs().expect("fs").open_dir(&cx, &path(&set_path("o"))).expect("origin dir");
        let pub_key = Signer::from_seed(&SEED).public();
        let taken = take_from(&mut FolderOrigin { cx: &cx, folder: &dir }, &[pub_key], set_id(), &lent, TOOLCHAIN, &clock, TakePlant::NONE)
            .expect("take set");

        assert_eq!(taken.how, How::Took);
        assert_eq!(taken.records, N);
        assert_eq!(taken.head_reads, 0, "fresh take must fetch Object::Prefix directly in 1 RTT without a prior Object::Head RTT");
        assert_eq!(usize::try_from(taken.reads).unwrap_or(0), 1 + segments, "expected 1 prefix read + {segments} segment reads");
        assert_eq!(
            lent.generation() - gen_before,
            1,
            "single-wave take must coalesce all segment puts + root + floor into ONE atomic commit"
        );
        assert_eq!(named_by_root(&lent, &world.records), Ok(Some(usize::try_from(N).unwrap_or(0))));
    }
}
