//! The journal's checkpoint (engine.md D150 item 1, the spec under it): the places a
//! certificate replayed the system's journal of changes to, kept beside the stored answer so
//! the next command replays only what was appended since the last command, not since the last
//! command that STORED an answer.
//!
//! **What it holds and means.** One record, `"BJC1" | pointer key [32] | places len u32 |
//! places` (the [`Stored`] form of journal_sweep.rs), under the content id of its bytes, named
//! by the root `<answer root>.journal` ([`name`]). It says: the answer under that sealed pointer
//! was certified standing at these places, so every write after them is in the journal from
//! them. A place is a cursor of the system's journal per unit (journal_sweep.rs `Unit`).
//!
//! **Why it cannot yield another state than a whole replay.** A replay from place `p` names
//! every write after `p`; the sweep stats each row a name reaches and every row of a unit it
//! cannot replay. A certificate on pointer `A` took its places `c` BEFORE its stats (as the
//! pointer's own were taken before the reads that made the answer), then found every row the
//! journal named since `A`'s places equal to `A`'s: so at `c`, every row of `A` is today's, and
//! a replay from `c` names every write that can make one differ. Three rules keep that so:
//!
//! - it is used only for the pointer it names ([`decode`]: the key the answer's root named in
//!   THIS command): a checkpoint of another answer (older, or a concurrent command's certificate
//!   of the answer this one's root no longer names) is stale and ignored ([`Ignored::OtherAnswer`]);
//! - per unit the LATER of the pointer's place and the checkpoint's is replayed from, on one
//!   journal ([`later`]); across a reset the checkpoint's (it saw the new journal, the pointer's
//!   place is of the old one);
//! - it is written only by a certificate that stood (engine_check.rs, the certificate branch),
//!   with that command's own replay's places, in the commit after the answer.
//!
//! Torn writes: the record and its root ride one commit, all or nothing (blaze-cas
//! plain/root_commit_tests.rs), and a record is served only checked (plain_place.rs `Held`'s
//! proof); this module checks the form (magic, lengths, places whole) and the binding, not the
//! bytes again (rule 77).
//!
//! **When written** ([`due`]): when the replay from the places used covered at least
//! [`DUE_EVENTS`] event IDs (one: any place that moved), or a unit had no place on the journal
//! it was replayed on (no place saved, a reset journal: without a checkpoint every no-op after
//! it sweeps that unit whole). So the next no-op replays only the IDs since this one's place.
//!
//! **Ignored as another answer's, after an edit, is the floor.** A command that stored a new
//! answer leaves the checkpoint naming the old pointer ([`Ignored::OtherAnswer`]); the next
//! command replays from the new pointer's places, which the storing command took at its start,
//! so that replay is already the IDs since the last command.
//!
//! Sans-IO (rule 14): [`decode`], [`choose`], [`due`] and [`Counts::of`] are pure; [`read`]
//! and [`after_certificate`] are the store's thin edge.

use core::fmt;

use blaze_core::Driver;
use blaze_drive::Key;
use blaze_ids::ContentId;
use blaze_own::Vec;
use bstd::Cx;
use bstd::fs::Cursor;
use bstd::mem::Frame;

use crate::CliError;
use crate::journal_sweep::{Stored, Taken};
use crate::store::{CommandStore, Sharing, Store as _};

/// The record's form.
pub(crate) const MAGIC: &[u8; 4] = b"BJC1";
const DOMAIN: &str = "blaze-cli journal checkpoint 1";
/// The magic, the pointer's key, the places' length.
const HEADER: usize = 4 + 32 + 4;

/// A certificate writes a checkpoint once its replay covered this many event IDs: any. The
/// floor of a no-op's replay is the IDs since the last command's place (the profile's own
/// words), and only a checkpoint written by every certificate whose places moved reaches it: a
/// no-op after a no-op then replays from the last one's place, a no-op after an edit from the
/// edit's (its stored pointer's places). The threshold was 65,536 until 2026-11-20, set before
/// any measurement, so a no-op loop replayed every ID since the last stored answer until the
/// suffix passed 65,536 system-wide IDs; on tempo (utilization.md U9) 99,681 of 99,681 IDs in
/// 139 ms, and from 5,364,028 IDs 1.73 s (tools-1119/s/tempo-score.noop.profile.txt): fseventsd
/// reads its store from the place, about 0.3 microseconds a system-wide ID above a fixed cost.
/// A write costs one small record and one root, in the commit the certificate makes anyway to
/// renew its roots.
pub(crate) const DUE_EVENTS: u64 = 1;

/// When the same live journal helper instance vouched (`from.instance == to.instance`), the
/// helper answers directly from memory without replaying `fseventsd`, so volume-wide FSEvents
/// ticks need not force a checkpoint write + fsync on every warm check. Still checkpoint every
/// `HELPER_DUE_EVENTS` IDs so a helper restart later has a bounded fallback replay.
pub(crate) const HELPER_DUE_EVENTS: u64 = 65_536;

/// The root naming a checkout's checkpoint, beside its answer's root.
pub(crate) fn name<'f>(answer_root: &str, frame: &'f Frame) -> blaze_own::String<&'f Frame> {
    let mut name = blaze_own::String::new_in(frame);
    name.push_str(answer_root);
    name.push_str(".journal");
    name
}

/// A bug the tests plant, to show their oracle catches it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Plant {
    None,
    /// The checkpoint is used whichever pointer it names.
    AnyAnswer,
}

/// Why a checkpoint is not used: the pointer's places are replayed instead (the whole suffix
/// since the answer was stored, the journal's own recovery: the same answer, a longer replay).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Ignored {
    /// Its root names other than one key.
    Names { bytes: usize },
    /// The record its root names is not in the store.
    Missing { key: Key },
    /// Not this form: another magic, lengths that do not fit, places not whole.
    Form { len: usize },
    /// It extends another answer than the one the answer's root named in this command.
    OtherAnswer,
}

impl fmt::Display for Ignored {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Ignored::Names { bytes } => write!(f, "its root names {bytes} bytes, not one key"),
            Ignored::Missing { key } => write!(f, "{key:?}, which its root names, is not in the store"),
            Ignored::Form { len } => write!(f, "a record of {len} bytes not of the `BJC1` form"),
            Ignored::OtherAnswer => f.write_str(
                "it extends another stored answer than this command's (stale: an answer stored since, whose own places are as late, or a concurrent command's)",
            ),
        }
    }
}

/// A checkpoint as read: the sealed pointer it extends and the places its certificate took.
#[derive(Clone, Copy)]
pub(crate) struct Checkpoint<'m> {
    pub(crate) answer: Key,
    pub(crate) places: Stored<'m>,
}

/// The record of a checkpoint extending `answer` at `places`: its key and bytes.
pub(crate) fn encode<'f>(answer: Key, places: Stored<'_>, frame: &'f Frame) -> (Key, Vec<u8, &'f Frame>) {
    let places = places.bytes();
    let mut bytes = Vec::with_capacity_in(HEADER + places.len(), frame);
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&answer.0.0);
    // Places are at most two places' units of under 64 KiB names each: under 4 GiB.
    bytes.extend_from_slice(&u32::try_from(places.len()).unwrap_or(u32::MAX).to_le_bytes());
    bytes.extend_from_slice(places);
    (Key(ContentId::of(DOMAIN, &bytes)), bytes)
}

/// The checkpoint in `bytes`, used only when it extends `answer` (the module's first rule).
pub(crate) fn decode(bytes: &[u8], answer: Key, plant: Plant) -> Result<Checkpoint<'_>, Ignored> {
    let form = Ignored::Form { len: bytes.len() };
    if bytes.len() < HEADER || !bytes.starts_with(MAGIC) {
        return Err(form);
    }
    let len = u32::from_le_bytes([bytes[36], bytes[37], bytes[38], bytes[39]]);
    if usize::try_from(len).ok() != Some(bytes.len() - HEADER) {
        return Err(form);
    }
    let Some(places) = Stored::of(&bytes[HEADER..]) else { return Err(form) };
    let mut id = [0; 32];
    id.copy_from_slice(&bytes[4..36]);
    let extends = Key(ContentId(id));
    if extends != answer && plant != Plant::AnyAnswer {
        return Err(Ignored::OtherAnswer);
    }
    Ok(Checkpoint { answer: extends, places })
}

/// Places in the [`Stored`] form, in the command's frame.
pub(crate) struct Places<'f> {
    bytes: Vec<u8, &'f Frame>,
}

impl<'f> Places<'f> {
    /// The places of `taken`'s replay, as the pointer would save them.
    pub(crate) fn of_taken(taken: &Taken<'_>, frame: &'f Frame) -> Places<'f> {
        let mut bytes = Vec::new_in(frame);
        taken.encode(&mut bytes);
        Places { bytes }
    }

    /// `stored`'s places, copied.
    pub(crate) fn copy(stored: Stored<'_>, frame: &'f Frame) -> Places<'f> {
        let mut bytes = Vec::with_capacity_in(stored.bytes().len(), frame);
        bytes.extend_from_slice(stored.bytes());
        Places { bytes }
    }

    /// Per unit the later of `older`'s place and `newer`'s ([`later`]), in `older`'s order,
    /// then `newer`'s units `older` has not.
    pub(crate) fn merged<'s>(older: Stored<'s>, newer: Stored<'s>, frame: &'f Frame) -> Places<'f> {
        let mut bytes = Vec::new_in(frame);
        let kept = older.units().map(|(at, name, cursor)| {
            let place = match newer.units().find(|(it, its, _)| *it == at && *its == name) {
                Some((_, _, newer)) => later(cursor, newer),
                None => cursor,
            };
            (at, name, place)
        });
        let added = newer.units().filter(|(at, name, _)| !older.units().any(|(it, its, _)| it == *at && its == *name));
        // The later places' time where kept: what the `replays:` line reads the age from.
        let taken = newer.taken_wall_ns().or(older.taken_wall_ns());
        Stored::encode_units(kept.chain(added), taken, &mut bytes);
        // The helper's place is one moment with ITS side's checkout place (journal_sweep.rs
        // `Stored`'s `helper`): kept only from the side whose checkout place was kept, so the
        // pair is never split. A helper's place is kept only after a `taken`, so `taken` is
        // here wherever a helper's place is.
        if let (Some(helper), Some(_)) = (helper_of_kept(older, newer), taken) {
            Stored::encode_helper(helper, &mut bytes);
        }
        Places { bytes }
    }

    pub(crate) fn stored(&self) -> Stored<'_> {
        Stored::of(&self.bytes).expect("places made by Stored::encode_units or copied whole are whole")
    }
}

/// Which places a command replays from, and why.
pub(crate) struct Chosen<'f> {
    places: Places<'f>,
    pub(crate) status: Status,
}

/// What became of the checkout's checkpoint.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Status {
    /// No stored answer, a fresh request, or no checkpoint written for this checkout.
    Absent,
    Ignored(Ignored),
    Used,
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Status::Absent => f.write_str("absent"),
            Status::Ignored(why) => write!(f, "ignored: {why}; the answer's own places replayed"),
            Status::Used => f.write_str("used"),
        }
    }
}

impl<'f> Chosen<'f> {
    /// No stored answer: no place.
    pub(crate) fn none(frame: &'f Frame) -> Chosen<'f> {
        Chosen { places: Places::copy(Stored::NONE, frame), status: Status::Absent }
    }

    pub(crate) fn places(&self) -> Stored<'_> {
        self.places.stored()
    }
}

/// The places to replay from: the pointer's (`answer`), each made later by the checkpoint
/// `found` where it is used.
pub(crate) fn choose<'f>(answer: Stored<'_>, found: Option<Result<Checkpoint<'_>, Ignored>>, frame: &'f Frame) -> Chosen<'f> {
    match found {
        None => Chosen { places: Places::copy(answer, frame), status: Status::Absent },
        Some(Err(why)) => Chosen { places: Places::copy(answer, frame), status: Status::Ignored(why) },
        Some(Ok(checkpoint)) => Chosen { places: Places::merged(answer, checkpoint.places, frame), status: Status::Used },
    }
}

/// A cursor's journal: two places compare only on one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Journal {
    FsEvents([u8; 16]),
    Usn { volume: u64, journal: u64 },
    Sim(u64),
}

/// A cursor's journal and its place in it; `None` for no place.
fn position(cursor: Cursor) -> Option<(Journal, u64)> {
    match cursor {
        Cursor::None => None,
        Cursor::FsEvents { event, volume } => Some((Journal::FsEvents(volume), event)),
        // A USN is never negative; one that is reads as the journal's start.
        Cursor::Usn { volume, journal, next } => Some((Journal::Usn { volume, journal }, u64::try_from(next).unwrap_or(0))),
        Cursor::Sim { journal, next } => Some((Journal::Sim(journal), next)),
    }
}

/// The event IDs from `from` to `to` on one journal; `None` when they are of two journals (or
/// either is no place), or `to` is before `from`.
fn distance(from: Cursor, to: Cursor) -> Option<u64> {
    match (position(from), position(to)) {
        (Some((a, from)), Some((b, to))) if a == b => to.checked_sub(from),
        _ => None,
    }
}

/// The helper's place to keep with [`Places::merged`]'s places: that of the side whose checkout
/// unit's place `merged` keeps ([`later`]), and none where that side kept none. Where the two
/// checkout places are equal, either pair is one moment with it: the newer's is taken.
fn helper_of_kept(older: Stored<'_>, newer: Stored<'_>) -> Option<[u8; crate::journal_sweep::HELPER_PLACE]> {
    let checkout = |stored: Stored<'_>| stored.units().find(|(at, name, _)| *at == crate::journal_sweep::Where::Checkout as u8 && name.is_empty()).map(|(_, _, cursor)| cursor);
    match (checkout(older), checkout(newer)) {
        (_, None) => older.helper_place(),
        (None, Some(_)) => newer.helper_place(),
        (Some(old), Some(new)) if later(old, new) == new => match old == new {
            true => newer.helper_place().or(older.helper_place()),
            false => newer.helper_place(),
        },
        (Some(_), Some(_)) => older.helper_place(),
    }
}

/// Of the answer's place and a checkpoint's for one unit, the one to replay from: the later on
/// one journal, the checkpoint's across journals (it was taken after the answer's, on the
/// journal the system keeps now), the answer's when the checkpoint holds no place.
fn later(answer: Cursor, checkpoint: Cursor) -> Cursor {
    match (position(answer), position(checkpoint)) {
        (_, None) => answer,
        (Some((a, at)), Some((c, ct))) if a == c && ct < at => answer,
        _ => checkpoint,
    }
}

/// Why a certificate writes a checkpoint.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Due {
    /// Its replay covered this many event IDs, at least the threshold.
    Replayed { ids: u64 },
    /// This many units had no place on the journal they were replayed on.
    NewPlace { units: u32 },
}

impl fmt::Display for Due {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Due::Replayed { ids } => write!(f, "written: the replay covered {ids} event ids, at least {DUE_EVENTS} (journal_checkpoint.rs `DUE_EVENTS`)"),
            Due::NewPlace { units } => write!(f, "written: {units} unit(s) had no place on their journal"),
        }
    }
}

/// Whether a certificate that replayed from `used` to `now` writes a checkpoint, with
/// `every` the threshold of event IDs ([`DUE_EVENTS`] in a command).
pub(crate) fn due(used: Stored<'_>, now: Stored<'_>, every: u64) -> Option<Due> {
    let mut ids: u64 = 0;
    let mut unplaced: u32 = 0;
    for (at, name, cursor) in now.units() {
        let from = used.units().find(|(it, its, _)| *it == at && *its == name).map(|(_, _, from)| from);
        match from.and_then(|from| distance(from, cursor)) {
            Some(d) => ids = ids.saturating_add(d),
            None => unplaced = unplaced.saturating_add(1),
        }
    }
    // A newly acquired helper place (none saved before, or a new helper instance after a restart
    // or reboot) is checkpointed even when zero journal events fired since the previous command,
    // so the next check gets an instant helper vouch. When the same live helper instance vouched,
    // raise the event-ID threshold to `HELPER_DUE_EVENTS` so volume-wide FSEvents ticks do not
    // trigger a checkpoint write + fsync on every warm check.
    let (new_helper, same_helper) = match (
        used.helper_place().and_then(|bytes| blaze_watch::since::Place::decode(&bytes)),
        now.helper_place().and_then(|bytes| blaze_watch::since::Place::decode(&bytes)),
    ) {
        (_, None) => (false, false),
        (None, Some(_)) => (true, false),
        (Some(from), Some(to)) => (from.instance != to.instance, from.instance == to.instance),
    };
    if unplaced > 0 || new_helper {
        return Some(Due::NewPlace { units: unplaced.max(u32::from(new_helper)) });
    }
    let threshold = if same_helper && every == DUE_EVENTS {
        HELPER_DUE_EVENTS
    } else {
        every
    };
    (ids >= threshold).then_some(Due::Replayed { ids })
}

/// The profile's `journal:` line: what this command replayed against the whole suffix since
/// its answer was stored.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Counts {
    /// Event IDs replayed: from the places chosen to the places now, summed over units.
    pub(crate) replayed: u64,
    /// Event IDs from the answer's own places to now: what a replay with no checkpoint covers.
    pub(crate) whole: u64,
    /// Units replayed from no place on their journal (every row of each statted).
    pub(crate) unplaced: u32,
    /// The checkpoint's place of the first unit, when it was used.
    pub(crate) at: Option<u64>,
    pub(crate) status: Status,
    pub(crate) written: Option<Due>,
}

impl Counts {
    /// Of a replay from `chosen` (for the answer whose places were `answer`) to `now`.
    pub(crate) fn of(answer: Stored<'_>, chosen: &Chosen<'_>, now: Stored<'_>) -> Counts {
        let used = chosen.places();
        let mut counts = Counts { replayed: 0, whole: 0, unplaced: 0, at: None, status: chosen.status, written: None };
        for (at, name, cursor) in now.units() {
            let find = |stored: Stored<'_>| stored.units().find(|(it, its, _)| *it == at && *its == name).map(|(_, _, from)| from);
            match find(used).and_then(|from| distance(from, cursor)) {
                Some(d) => counts.replayed = counts.replayed.saturating_add(d),
                None => counts.unplaced = counts.unplaced.saturating_add(1),
            }
            if let Some(d) = find(answer).and_then(|from| distance(from, cursor)) {
                counts.whole = counts.whole.saturating_add(d);
            }
        }
        if chosen.status == Status::Used {
            counts.at = used.units().next().and_then(|(_, _, cursor)| position(cursor)).map(|(_, at)| at);
        }
        counts
    }
}

impl fmt::Display for Counts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "journal: replayed {} of {} event ids (checkpoint at ", self.replayed, self.whole)?;
        match self.at {
            Some(at) => write!(f, "{at}")?,
            None => f.write_str("none")?,
        }
        write!(f, "; floor: the ids appended since the last command); units from no place {}; checkpoint {}", self.unplaced, self.status)?;
        if let Some(due) = self.written {
            write!(f, "; {due}")?;
        }
        Ok(())
    }
}

/// The places to replay from for the stored answer whose sealed pointer is `answer` with the
/// places `stored`: its checkout's checkpoint read and checked (the module's rules), before any
/// file of the checkout is read.
pub(crate) fn read<'f, D: Driver, S: Sharing>(cx: &Cx, store: &mut CommandStore<'_, '_, D, S>, answer_root: &str, answer: Key, stored: Stored<'_>, frame: &'f Frame) -> Result<Chosen<'f>, CliError> {
    let root = name(answer_root, frame);
    let found = match store.named(root.as_str())? {
        None => None,
        Some(keys) if keys.len() != 32 => Some(Err(Ignored::Names { bytes: keys.len() })),
        Some(keys) => {
            let mut id = [0; 32];
            id.copy_from_slice(keys);
            let key = Key(ContentId(id));
            // `engine_answer::save` resets `<answer_root>.journal` to `[pointer.key]` (reusing
            // the same 32-byte root record as `answer_root` in the newest segment): no checkpoint
            // has been written past this answer yet, so do not probe older segments.
            if key == answer {
                None
            } else {
                match store.get(cx, key)? {
                    None => Some(Err(Ignored::Missing { key })),
                    Some(held) => Some(decode(held.bytes, answer, Plant::None)),
                }
            }
        }
    };
    Ok(choose(stored, found, frame))
}

/// After a certificate stood on the pointer `answer`: when [`due`] (with `every`), the
/// checkpoint of `chosen`'s places made later by `now` (this command's replay) is put and named,
/// riding the commit after the answer. Called only on the certificate's path.
#[expect(clippy::too_many_arguments, reason = "the write's inputs, each one handle")]
pub(crate) fn after_certificate<D: Driver, S: Sharing>(cx: &Cx, store: &mut CommandStore<'_, '_, D, S>, answer_root: &str, answer: Key, chosen: &Chosen<'_>, now: Stored<'_>, every: u64, frame: &Frame) -> Result<Option<Due>, CliError> {
    let Some(why) = due(chosen.places(), now, every) else {
        // If `<answer_root>.journal` still points to a stale/ignored checkpoint from an older
        // answer or a concurrent certifier, reset it to `[answer]` so later warm runs do not
        // probe ancient segments only to ignore the same record again.
        if matches!(chosen.status, Status::Ignored(_)) {
            store.pin(name(answer_root, frame).as_str(), &[answer])?;
        }
        return Ok(None);
    };
    let places = Places::merged(chosen.places(), now, frame);
    let (key, bytes) = encode(answer, places.stored(), frame);
    store.put(cx, key, &bytes)?;
    store.pin(name(answer_root, frame).as_str(), &[key])?;
    Ok(Some(why))
}

#[cfg(test)]
#[path = "tests/journal_checkpoint_tests.rs"]
mod tests;
