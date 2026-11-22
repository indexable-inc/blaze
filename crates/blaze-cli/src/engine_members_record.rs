//! THE MEMBERS RECORD (D187 piece 3, the prepare seam; rule 78: one seam, two strategies; rule
//! 77: never rebuild what a record holds; rule 51: what outlives a task is a record).
//!
//! THE CAUSE IT REMOVES: the Bodies phase's setup (engine_bodies.rs `groups`) visits EVERY member
//! of the crate list each request: its names record asked by key, its environment made from its
//! script's record, its items record opened, each file row matched to the plan's outlines (a
//! scan of the outlines a file), its module tree walked, its member row probed. On tempo's
//! ready edit that setup is most of the engine's 264 ms (the profile: "names records opened 913
//! (1123156656 bytes)", "member rows read 150 (floor: the owed members, 2)"). For a member none
//! of whose files moved, every one of those reads gives what the last request's gave: its unit,
//! its head subjects, its word on the files its tree reaches and does not hold, its row's
//! uncovered bodies, its row's key.
//!
//! THE RECORD keeps exactly that, per member the last request read whole and kept a row of
//! ([`Member`]). The setup's strategy is a type parameter ([`SetupInputs`], rule 78): the phase
//! path's ([`EveryMember`]) visits every member; the ready path's ([`StandingMembers`]) takes a
//! member that stands from the record and visits only the members a moved file is in. Both
//! write the record from the same per-member outputs (a standing member's entry is its last
//! one, unchanged): what one driver writes the other reads (rule 26).
//!
//! VALID BY ITS READ LIST (the design's third line), never by hand:
//! - the crate list it was made over (the `UnitGraph` record's out) and the tool (the
//!   `BodyCheck` kind's: the code that derived each part), in its header: either moved, no
//!   member stands;
//! - the answer it belongs to: made at serial `s` (this request's answer, the last one's plus
//!   one), it is read only by a request starting from the answer at `s` ([`Header::made_at`],
//!   as engine_scope_reads.rs `GrainsIn`: a request that saved no answer leaves a record that no
//!   request takes);
//! - per member: its unit as the crate list makes it today and its names input (name and out:
//!   every record of the member is its names task's), compared by the caller; and the outlines
//!   of the files it holds and of those its module tree reaches but it does not hold: it stands
//!   only when none is among the request's moved outlines ([`View::standing`]: the outlines
//!   entered and the files whose bodies are owed). A member with a file row no outline matched
//!   (`FileHead::NotOutlined`: a file that may appear) is never written, so never stands.
//!
//! THE FORM, little-endian, every length checked once at [`View::open`]: `MAGIC` (its last byte
//! the version: an old form is refused as `Magic`, by name, and made again, D160); the graph's
//! out (32); the tool (32); made at (u64); the member count (u32); then each member sorted by
//! its place in the crate list: place (u32), its unit's name (kind u8, subject 32), its crate
//! prefix (8), test (u8), its names input's name (33) and out (32), its row's key (32), its
//! row's uncovered count (u32), its row's named labels (u32 count, each a u32 length and UTF-8),
//! its held files (u32 count, each: file index u32, outline name 33, outline path as a u32
//! length and bytes, entries digest 32), its reached files it does not hold, in the order its
//! claims were made (u32 count, each an outline name 33).

use core::fmt;

use blaze_drive::{Key, Name, Tool};
use blaze_ids::ContentId;
use blaze_own::{AVec, Alloc};

use crate::engine_body_plan::Unit;
use crate::engine_heads::Subject;

/// The form's first bytes; the last is its version.
const MAGIC: [u8; 8] = *b"BLZMEMB\x01";

/// The setup's strategy (rule 78: a type parameter at one seam, engine_bodies.rs `groups`).
pub(crate) trait SetupInputs {
    /// Whether a member that stands is taken from the last request's record instead of visited.
    const STANDING: bool;
}

/// The phase path's: every member visited, as before the record.
pub(crate) struct EveryMember;

/// The ready path's: a member that stands taken from the record.
pub(crate) struct StandingMembers;

impl SetupInputs for EveryMember {
    const STANDING: bool = false;
}

impl SetupInputs for StandingMembers {
    const STANDING: bool = true;
}

/// One member as a request read it whole and kept its row.
pub(crate) struct Member<'r> {
    /// Its place in the crate list (`CrateGraph` index).
    pub(crate) at: u32,
    pub(crate) unit: Unit,
    /// Its crate's names input (`groups`' `members`): its name and the digest of its record.
    pub(crate) names: Name,
    pub(crate) names_out: ContentId,
    /// Its member listing row's key (engine_member_rows.rs `row_key`), which the answer's root
    /// names beside the rows (`member_rows`).
    pub(crate) row: Key,
    /// Its row's uncovered bodies: the count and the labels the row names.
    pub(crate) uncovered: u32,
    pub(crate) named: AVec<'r, &'r str>,
    /// Each file it holds: its index in the items record and the outline it matched.
    pub(crate) held: AVec<'r, Held<'r>>,
    /// Each file its module tree reaches that it does not hold (its claim: not held), in the
    /// order the claims were made.
    pub(crate) not_held: AVec<'r, Name>,
}

/// A held file: what [`Subject::new`] reads of its outline, kept.
#[derive(Clone, Copy)]
pub(crate) struct Held<'r> {
    pub(crate) file: u32,
    pub(crate) slot: Name,
    pub(crate) path: &'r [u8],
    pub(crate) entries: ContentId,
}

impl Held<'_> {
    /// The head subject of the file in the unit of crate `prefix`: [`Subject::new`]'s, from the
    /// kept parts.
    pub(crate) fn subject(&self, prefix: [u8; 8]) -> Subject {
        Subject { prefix, file: self.file, slot: self.slot, signature_file: crate::signature::SignatureFile::new(self.path), entries: self.entries }
    }
}

/// What the record was made over: a request reads it only where each is today's.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Header {
    pub(crate) graph_out: ContentId,
    pub(crate) tool: Tool,
    /// The serial of the answer of the request that made it.
    pub(crate) made_at: u64,
}

/// Why a stored members record does not open: each names the byte it is about (rule 19).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MembersRefused {
    /// Not this form or not this version: made again, never read (D160).
    Magic,
    /// The bytes end at `at` where `what` needs `need` more.
    Short { at: usize, what: &'static str, need: usize },
    /// A count at `at` larger than the bytes after it can hold (rule 18).
    Count { at: usize, what: &'static str, count: u32 },
    /// A label at `at` that is not UTF-8.
    Utf8 { at: usize },
    /// A name's kind byte at `at` that no kind has.
    Kind { at: usize, byte: u8 },
    /// The members out of the crate list's order at `at`.
    Order { at: usize },
    /// Bytes after the last member.
    Trailing { at: usize },
}

impl fmt::Display for MembersRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MembersRefused::Magic => f.write_str("the members record is not this form or version (engine_members_record.rs `MAGIC`)"),
            MembersRefused::Short { at, what, need } => write!(f, "the members record ends at byte {at}, where {what} needs {need} more (engine_members_record.rs)"),
            MembersRefused::Count { at, what, count } => write!(f, "the members record's count of {what} at byte {at} is {count}, more than its bytes hold (engine_members_record.rs)"),
            MembersRefused::Utf8 { at } => write!(f, "the members record's label at byte {at} is not UTF-8 (engine_members_record.rs)"),
            MembersRefused::Kind { at, byte } => write!(f, "the members record's name at byte {at} has the kind byte {byte}, which no kind has (engine_members_record.rs)"),
            MembersRefused::Order { at } => write!(f, "the members record's member at byte {at} is out of the crate list's order (engine_members_record.rs)"),
            MembersRefused::Trailing { at } => write!(f, "the members record has bytes after its last member, from byte {at} (engine_members_record.rs)"),
        }
    }
}

impl core::error::Error for MembersRefused {}

/// The record written one member at a time, in the crate list's order; [`Writer::finish`] sets
/// the count.
pub(crate) struct Writer<'w> {
    bytes: AVec<'w, u8>,
    count: u32,
    last: Option<u32>,
}

/// Where the member count is in the form: after the magic, the graph's out, the tool and the
/// serial.
const COUNT_AT: usize = 8 + 32 + 32 + 8;

impl<'w> Writer<'w> {
    pub(crate) fn new(header: &Header, alloc: Alloc<'w>) -> Writer<'w> {
        let mut bytes = AVec::new_in(alloc);
        bytes.extend_from_slice(&MAGIC);
        bytes.extend_from_slice(&header.graph_out.0);
        bytes.extend_from_slice(&header.tool.0.0);
        bytes.extend_from_slice(&header.made_at.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        Writer { bytes, count: 0, last: None }
    }

    /// One member, after every member before it in the crate list.
    pub(crate) fn member(&mut self, m: &Member<'_>) {
        debug_assert!(self.last.is_none_or(|last| last < m.at), "members are written in the crate list's order");
        self.last = Some(m.at);
        self.count = self.count.saturating_add(1);
        let into = &mut self.bytes;
        fn count(into: &mut AVec<'_, u8>, n: usize) {
            into.extend_from_slice(&u32::try_from(n).unwrap_or(u32::MAX).to_le_bytes());
        }
        fn bytes(into: &mut AVec<'_, u8>, b: &[u8]) {
            count(into, b.len());
            into.extend_from_slice(b);
        }
        fn name(into: &mut AVec<'_, u8>, name: Name) {
            into.push(name.kind as u8);
            into.extend_from_slice(&name.subject.0);
        }
        into.extend_from_slice(&m.at.to_le_bytes());
        name(into, m.unit.name);
        into.extend_from_slice(&m.unit.prefix);
        into.push(u8::from(m.unit.test));
        name(into, m.names);
        into.extend_from_slice(&m.names_out.0);
        into.extend_from_slice(&m.row.0.0);
        into.extend_from_slice(&m.uncovered.to_le_bytes());
        count(into, m.named.len());
        for label in m.named.iter() {
            bytes(into, label.as_bytes());
        }
        count(into, m.held.len());
        for h in m.held.iter() {
            into.extend_from_slice(&h.file.to_le_bytes());
            name(into, h.slot);
            bytes(into, h.path);
            into.extend_from_slice(&h.entries.0);
        }
        count(into, m.not_held.len());
        for n in m.not_held.iter() {
            name(into, *n);
        }
    }

    /// The record's bytes.
    pub(crate) fn finish(mut self) -> AVec<'w, u8> {
        self.bytes[COUNT_AT..COUNT_AT + 4].copy_from_slice(&self.count.to_le_bytes());
        self.bytes
    }
}

/// A cursor over the record's bytes: every read checks its length once.
struct Cursor<'r> {
    bytes: &'r [u8],
    at: usize,
}

impl<'r> Cursor<'r> {
    fn take(&mut self, n: usize, what: &'static str) -> Result<&'r [u8], MembersRefused> {
        let left = self.bytes.len() - self.at;
        if left < n {
            return Err(MembersRefused::Short { at: self.at, what, need: n - left });
        }
        let out = &self.bytes[self.at..self.at + n];
        self.at += n;
        Ok(out)
    }

    fn u8(&mut self, what: &'static str) -> Result<u8, MembersRefused> {
        Ok(self.take(1, what)?[0])
    }

    fn u32(&mut self, what: &'static str) -> Result<u32, MembersRefused> {
        let b = self.take(4, what)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn u64(&mut self, what: &'static str) -> Result<u64, MembersRefused> {
        let mut b = [0u8; 8];
        b.copy_from_slice(self.take(8, what)?);
        Ok(u64::from_le_bytes(b))
    }

    fn id(&mut self, what: &'static str) -> Result<ContentId, MembersRefused> {
        let mut b = [0u8; 32];
        b.copy_from_slice(self.take(32, what)?);
        Ok(ContentId(b))
    }

    fn name(&mut self, what: &'static str) -> Result<Name, MembersRefused> {
        let at = self.at;
        let byte = self.u8(what)?;
        let kind = blaze_drive::Kind::from_u8(byte).ok_or(MembersRefused::Kind { at, byte })?;
        Ok(Name::new(kind, self.id(what)?))
    }

    fn bytes(&mut self, what: &'static str) -> Result<&'r [u8], MembersRefused> {
        let len = self.u32(what)?;
        self.take(usize::try_from(len).unwrap_or(usize::MAX), what)
    }

    /// A count of entries each at least `least` bytes long, checked against what is left.
    fn count(&mut self, least: usize, what: &'static str) -> Result<usize, MembersRefused> {
        let at = self.at;
        let count = self.u32(what)?;
        let n = usize::try_from(count).unwrap_or(usize::MAX);
        if n.saturating_mul(least) > self.bytes.len() - self.at {
            return Err(MembersRefused::Count { at, what, count });
        }
        Ok(n)
    }
}

/// The least bytes of one member, one held file and one name: what a count is checked against.
const MEMBER_LEAST: usize = 4 + 33 + 8 + 1 + 33 + 32 + 32 + 4 + 4 + 4 + 4;
const HELD_LEAST: usize = 4 + 33 + 4 + 32;
const NAME_LEN: usize = 33;

/// A stored members record, opened: every member decoded once, its texts in place.
pub(crate) struct View<'r> {
    pub(crate) header: Header,
    /// Sorted by `at`, each once.
    members: AVec<'r, Member<'r>>,
}

impl<'r> View<'r> {
    /// The record in `bytes`, its lists in `alloc`: every length, kind byte and the order
    /// checked here once (rule 74), nothing read after the end.
    pub(crate) fn open(bytes: &'r [u8], alloc: Alloc<'r>) -> Result<View<'r>, MembersRefused> {
        let mut c = Cursor { bytes, at: 0 };
        if c.take(MAGIC.len(), "the magic").map_err(|_| MembersRefused::Magic)? != &MAGIC[..] {
            return Err(MembersRefused::Magic);
        }
        let graph_out = c.id("the graph's out")?;
        let tool = Tool(c.id("the tool")?);
        let made_at = c.u64("the serial it was made at")?;
        let n = c.count(MEMBER_LEAST, "members")?;
        let mut members = AVec::with_capacity_in(n, alloc);
        let mut last: Option<u32> = None;
        for _ in 0..n {
            let starts = c.at;
            let at = c.u32("a member's place")?;
            if last.is_some_and(|before| before >= at) {
                return Err(MembersRefused::Order { at: starts });
            }
            last = Some(at);
            let unit_name = c.name("a member's unit")?;
            let mut prefix = [0u8; 8];
            prefix.copy_from_slice(c.take(8, "a member's crate prefix")?);
            let test = c.u8("a member's test mark")? != 0;
            let names = c.name("a member's names input")?;
            let names_out = c.id("a member's names record digest")?;
            let row = Key(c.id("a member's row key")?);
            let uncovered = c.u32("a member's uncovered count")?;
            let labels = c.count(4, "a member's named labels")?;
            let mut named = AVec::with_capacity_in(labels, alloc);
            for _ in 0..labels {
                let starts = c.at;
                let b = c.bytes("a label")?;
                named.push(core::str::from_utf8(b).map_err(|_| MembersRefused::Utf8 { at: starts })?);
            }
            let held_n = c.count(HELD_LEAST, "a member's held files")?;
            let mut held = AVec::with_capacity_in(held_n, alloc);
            for _ in 0..held_n {
                let file = c.u32("a held file's index")?;
                let slot = c.name("a held file's outline")?;
                let path = c.bytes("a held file's path")?;
                let entries = c.id("a held file's entries")?;
                held.push(Held { file, slot, path, entries });
            }
            let not_n = c.count(NAME_LEN, "a member's reached files it does not hold")?;
            let mut not_held = AVec::with_capacity_in(not_n, alloc);
            for _ in 0..not_n {
                not_held.push(c.name("a reached file it does not hold")?);
            }
            members.push(Member { at, unit: Unit { name: unit_name, prefix, test }, names, names_out, row, uncovered, named, held, not_held });
        }
        if c.at != bytes.len() {
            return Err(MembersRefused::Trailing { at: c.at });
        }
        Ok(View { header: Header { graph_out, tool, made_at }, members })
    }

    /// The member at crate list place `at` as the last request left it, when the record was
    /// made over `today` (graph, tool, and the answer this request starts from) and no outline
    /// in `moved` (sorted) is a file it holds or reaches. `None`: visit it. The caller still
    /// compares the unit and the names input with today's.
    pub(crate) fn standing(&self, at: u32, today: &Header, moved: &[Name]) -> Option<&Member<'r>> {
        if self.header != *today {
            return None;
        }
        let place = self.members.binary_search_by_key(&at, |m| m.at).ok()?;
        let member = &self.members[place];
        let hit = |name: &Name| moved.binary_search(name).is_ok();
        if member.held.iter().any(|h| hit(&h.slot)) || member.not_held.iter().any(hit) {
            return None;
        }
        Some(member)
    }

    /// THE STAND BELOW THE HEADS (piece A, engine_bodies.rs `groups`): the member at place `at`
    /// when the record was made over `today`, no file it reaches and does not hold is in `moved`,
    /// at least one file it holds is, and each such held file's listing facts today
    /// (`entries_now` of its outline name: engine_listing_record.rs `entries_of`, which hold its
    /// surface and every entry's head and no body) are the ones the record kept. Everything the
    /// record keeps of the member is then today's (its row is keyed by its files' heads,
    /// engine_member_rows.rs THE KEY; its head subjects are its files' entries), and what a visit
    /// would add is the owed files' claims alone: the caller visits it only when a decision
    /// needs them. `None`: it stands whole ([`View::standing`]) or is visited.
    pub(crate) fn standing_below_heads(&self, at: u32, today: &Header, moved: &[Name], entries_now: impl Fn(Name) -> Option<ContentId>) -> Option<&Member<'r>> {
        if self.header != *today {
            return None;
        }
        let member = self.member(at)?;
        let hit = |name: &Name| moved.binary_search(name).is_ok();
        if member.not_held.iter().any(hit) {
            return None;
        }
        let mut below = false;
        for held in member.held.iter().filter(|held| hit(&held.slot)) {
            if entries_now(held.slot) != Some(held.entries) {
                return None;
            }
            below = true;
        }
        below.then_some(member)
    }

    pub(crate) fn len(&self) -> usize {
        self.members.len()
    }

    /// The member at crate list place `at` as the last record holds it, whatever the header:
    /// what the ready setup reads back for a member it will visit (its held and not-held files,
    /// engine_bodies.rs `BodiesStage::ready_reads`), never what a member stands on ([`View::standing`]).
    pub(crate) fn member(&self, at: u32) -> Option<&Member<'r>> {
        let place = self.members.binary_search_by_key(&at, |m| m.at).ok()?;
        Some(&self.members[place])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(kind: blaze_drive::Kind, b: u8) -> Name {
        Name::new(kind, ContentId([b; 32]))
    }

    fn header(made_at: u64) -> Header {
        Header { graph_out: ContentId([1; 32]), tool: Tool(ContentId([2; 32])), made_at }
    }

    /// THE ROUND TRIP (rule 26): a record opens to what was written, and a member stands only
    /// over today's header and when no moved outline is its file. CONTROLS (rule 23): a moved
    /// held file, a moved reached file, another serial, a cut byte, an old version.
    #[test]
    fn a_members_record_round_trips_and_stands_by_its_read_list() {
        let alloc = blaze_own::testing::heap().alloc();
        let kind = blaze_drive::Kind::FileItems;
        let (held_slot, reached, other) = (name(kind, 10), name(kind, 11), name(kind, 12));
        let mut named = AVec::new_in(alloc);
        named.push("a body a macro wrote");
        let mut held = AVec::new_in(alloc);
        held.push(Held { file: 0, slot: held_slot, path: b"c5/src/lib.rs", entries: ContentId([4; 32]) });
        let mut not_held = AVec::new_in(alloc);
        not_held.push(reached);
        let unit = Unit { name: name(blaze_drive::Kind::CrateScope, 5), prefix: [6; 8], test: false };
        let member = Member { at: 3, unit, names: name(blaze_drive::Kind::CrateScope, 7), names_out: ContentId([9; 32]), row: Key(ContentId([8; 32])), uncovered: 1, named, held, not_held };
        let mut writer = Writer::new(&header(9), alloc);
        writer.member(&member);
        let bytes = writer.finish();
        let view = View::open(&bytes, alloc).expect("the record opens");
        assert_eq!(view.header, header(9));
        assert_eq!(view.len(), 1);
        let got = view.standing(3, &header(9), &[other]).expect("a member none of whose files moved stands");
        assert_eq!((got.unit, got.names, got.names_out, got.row, got.uncovered), (unit, member.names, member.names_out, member.row, 1));
        assert_eq!(&got.named[..], &["a body a macro wrote"]);
        assert_eq!(got.held[0].path, b"c5/src/lib.rs");
        assert_eq!(&got.not_held[..], &[reached]);
        // Written again from what was read: the same bytes (a standing member is copied whole).
        let mut again = Writer::new(&header(9), alloc);
        again.member(got);
        assert_eq!(&again.finish()[..], &bytes[..], "a member read and written again is the same bytes");
        assert!(view.standing(3, &header(9), &[held_slot]).is_none(), "CONTROL: a moved held file");
        assert!(view.standing(3, &header(9), &[reached]).is_none(), "CONTROL: a moved reached file");
        assert!(view.standing(3, &header(10), &[]).is_none(), "CONTROL: another answer's record");
        assert!(view.standing(4, &header(9), &[]).is_none(), "a member the record does not hold");
        // The last byte is in the last reached name: its list's count is checked against the
        // bytes left before the name is read, so the cut is refused as that count.
        assert!(matches!(View::open(&bytes[..bytes.len() - 1], alloc), Err(MembersRefused::Count { what: "a member's reached files it does not hold", count: 1, .. })), "CONTROL: a cut byte");
        // A cut in the header is short of its field.
        assert!(matches!(View::open(&bytes[..COUNT_AT + 2], alloc), Err(MembersRefused::Short { what: "members", need: 2, .. })), "CONTROL: a cut header");
        let mut old = AVec::new_in(alloc);
        old.extend_from_slice(&bytes);
        old[7] = 0;
        assert!(matches!(View::open(&old, alloc), Err(MembersRefused::Magic)), "CONTROL: another version");
    }

    /// THE STAND BELOW THE HEADS (`View::standing_below_heads`): a member whose moved held file
    /// keeps its listing facts stands below its heads; CONTROLS (rule 23): the facts moved, no
    /// facts today (no outline), a moved reached file, another answer's record, and no held file
    /// moved (a whole stand, `View::standing`'s, not this one's).
    #[test]
    fn a_member_stands_below_its_heads_only_when_its_moved_files_keep_their_entries() {
        let alloc = blaze_own::testing::heap().alloc();
        let kind = blaze_drive::Kind::FileItems;
        let (held_slot, reached, other) = (name(kind, 10), name(kind, 11), name(kind, 12));
        let entries = ContentId([4; 32]);
        let mut held = AVec::new_in(alloc);
        held.push(Held { file: 0, slot: held_slot, path: b"c5/src/lib.rs", entries });
        let mut not_held = AVec::new_in(alloc);
        not_held.push(reached);
        let unit = Unit { name: name(blaze_drive::Kind::CrateScope, 5), prefix: [6; 8], test: true };
        let member = Member { at: 3, unit, names: name(blaze_drive::Kind::CrateScope, 7), names_out: ContentId([9; 32]), row: Key(ContentId([8; 32])), uncovered: 0, named: AVec::new_in(alloc), held, not_held };
        let mut writer = Writer::new(&header(9), alloc);
        writer.member(&member);
        let bytes = writer.finish();
        let view = View::open(&bytes, alloc).expect("the record opens");
        let same = |_: Name| Some(entries);
        assert!(view.standing_below_heads(3, &header(9), &[held_slot], same).is_some(), "a held file moved below its heads");
        assert!(view.standing_below_heads(3, &header(9), &[held_slot], |_| Some(ContentId([5; 32]))).is_none(), "CONTROL: its entries moved");
        assert!(view.standing_below_heads(3, &header(9), &[held_slot], |_| None).is_none(), "CONTROL: no facts today");
        let mut both = [held_slot, reached];
        both.sort_unstable();
        assert!(view.standing_below_heads(3, &header(9), &both, same).is_none(), "CONTROL: a moved reached file");
        assert!(view.standing_below_heads(3, &header(10), &[held_slot], same).is_none(), "CONTROL: another answer's record");
        assert!(view.standing_below_heads(3, &header(9), &[other], same).is_none(), "no held file moved: a whole stand, not this one");
    }
}
