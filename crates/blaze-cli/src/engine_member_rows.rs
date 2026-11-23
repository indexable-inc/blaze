//! A MEMBER'S LISTING ROW (engine.md "An edit costs what it moved", D149 piece I1; rule 51: what
//! outlives a task is a record; rule 77: never rebuild what a record holds).
//!
//! THE CAUSE IT REMOVES (tempo, one body edited, tree 9cec6e9546): the Bodies phase opened every
//! member's records and listed its whole crate (`list_with` 142 of 951 main-thread samples,
//! with `written_by` and `in_blocks` over every body a macro wrote) to publish one note, the
//! bodies a macro wrote that no entry covers, and to place the bodies an owed file's entries
//! check. Both are a function of the member's records and the heads of its files, which a body
//! edit does not move.
//!
//! THE ROW holds what the Bodies phase took from a member's WHOLE listing (engine_bodies.rs
//! `groups`):
//! - its uncovered bodies: the count and the first [`MOST_UNCOVERED_NAMED`] by label, in the
//!   order the walk finds them (so the note over every member is the same bytes whether each
//!   member's part was read or made);
//! - each body a macro wrote at module level whose call's walk ends at a file entry
//!   ([`Written`]): that entry (file, outline index), the body's key, the macro as written and
//!   the body's label, in `written_by`'s order (by call, then body);
//! - the bodies of blocks' modules ([`Block`]): each with the body that owns it, its key and its
//!   name, sorted by (owner, key) as `in_blocks` gives them; or why they could not be told apart
//!   ([`InBlocksError`]).
//! An owed file's own bodies are not in the row: they are listed from that file alone
//! (front2 project_records.rs `list_files_with`) and keyed by chain, as before.
//!
//! THE KEY ([`row_key`]) is everything the row is a function of:
//! - the unit's `CrateScope` name (its crate key, so its cfg and its unit);
//! - its crate's names record as read: its name and its `out`, the digest every reader of the
//!   record keys its read by (engine_names.rs `input_cell`), so what a body task may take from
//!   the record: the items rows (places, member rows, the call each member was written by), the
//!   expansion texts and their origins (each walk up to a file entry), the block modules, the
//!   cfg the crate was lowered under, every name a label says;
//! - each file row of its items record, by index ([`FileHead`]): the file's HEAD, its outline's
//!   `Surface` digest (engine_tasks.rs `surface_of`); the whole-text digest under its edition
//!   and module form (`outline_source`) for a file whose outline failed; or a mark when the
//!   file is not outlined;
//! - the tool (the `BodyCheck` kind's, engine_store.rs `tools`): the code that derives the row;
//! - [`VERSION`], in the key and in the bytes: an old form is another key, never read
//!   (rule 26: an old home is rebuilt, nothing migrated).
//! Why the head and not the whole text (piece A, 2026-11-21; it was the whole text until then):
//! every field the row stores is read from the crate's names record. `written` and `named` are
//! the bodies placed in an expansion (`Records::text_at`) and each call's walk up the
//! expansion texts to a file entry (covered.rs `written_by`); `blocks` are the bodies of block
//! modules, which resolve.rs `make_blocks` writes into the names record, and their walk up
//! (covered.rs `in_blocks`). A body's text enters only as whether a file-placed body is listed
//! at all, and that enters only `in_blocks`'s set of the bodies whose blocks may hold
//! modules. A block written as a module (an `impl`, a `mod`, a type, a `fn` whose body writes
//! one, a macro call in statement position: blaze-scope blocks.rs) and every outline entry are
//! in the file's `Surface` (blaze-file-items nested.rs `NestedItem::surface`, the body calls'
//! surface pass), so an edit that moves any of them moves the head, the names task runs again
//! and the names record's `out` moves too. An edit inside a body that writes none of them moves
//! neither, and the row it would make is the stored one. What that saves: the edited member's
//! row is read, not made from its whole listing (tempo, one body edited in
//! `precompiles`: 69.6 ms of the setup's 130.6 ms, `records opened and row`, made 2 rows). The
//! world test is engine_ready_edit_tests.rs `a_body_edit_reads_its_members_row`: the row a
//! ready edit reads equals the row made fresh from today's whole listing, and its control is a
//! key without the names record's `out` or the heads.
//!
//! THE FORM, little-endian, every length checked once at [`open`] (no read past it): `MAGIC`,
//! `VERSION` u32; the uncovered count u32, the named count u32 and each label (u32 length,
//! UTF-8); the written count u32 and each (file u32, entry u32, key tag u8 [0 none, 1 a key]
//! and 32 key bytes, macro text, label text); the blocks tag u8 (0: count u32 and each owner 32,
//! key 32, name text; 1: the cause's code u8); nothing after. Two builders of one key write
//! equal bytes (rule 25): nothing in it is the machine's or a map's order.
//!
//! ONE PATH: a row made this command is encoded and then OPENED from its own bytes, as a stored
//! one is; the phase reads only opened rows, so a restore equals a fresh build in bytes.

use core::fmt;

use blaze_drive::{Input, Key, Name, Tool};
use blaze_id::ItemKey;
use blaze_ids::{ContentId, Hasher};
use blaze_ladder::front2::project::BodyRef;
use blaze_ladder::front2::project_records::{Listing, list_with};
use blaze_ladder::front2::source::{NamesSource, Records};
use blaze_own::{AVec, Alloc};
use blaze_surface::ItemIndex;

use crate::body_types::covered::{self, InBlocksError};
use crate::engine_tasks::Outlined;
use crate::names::expansion_texts::ExpansionTexts;

/// The form's version: in the key's hash and in the bytes.
pub(crate) const VERSION: u32 = 2;

/// The key's domain.
const KEY_DOMAIN: &str = "blaze-cli engine member listing row 1";

/// The bytes a row begins with.
const MAGIC: [u8; 8] = *b"BLZMROW\x01";

/// At most this many uncovered bodies are named in the note, the first found (the rest
/// counted): the one home of the bound (engine_bodies.rs's note and this row's form).
pub(crate) const MOST_UNCOVERED_NAMED: usize = 16;

/// What one file row of a member's items record contributes to its row's key ([`row_key`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FileHead {
    /// The row names no file the planner outlined (no row, no path in the package, or a path no
    /// outline has): no text of it is read through the records.
    NotOutlined,
    /// A file whose outline task failed, so it has no head: its text digest under its edition
    /// and module form (engine_tasks.rs `outline_source`), which moves with every byte.
    Text(ContentId),
    /// The file's head: its outline's `Surface` digest (engine_tasks.rs `surface_of`), which
    /// moves with every item, every block that writes a module and every statement macro call,
    /// and not with an edit inside a body that adds none (module doc, THE KEY).
    Head(ContentId),
}

impl FileHead {
    /// The key part of the outlined file `one`: its head, or its text when it has no outline.
    pub(crate) fn of(one: &Outlined<'_>) -> FileHead {
        match one.made.as_ref().ok().and_then(crate::engine_tasks::surface_of) {
            Some(surface) => FileHead::Head(surface),
            None => FileHead::Text(one.outline_source),
        }
    }
}

/// Everything a member's row is a function of (module doc, THE KEY).
#[derive(Clone, Copy, Debug)]
pub(crate) struct RowKey<'p> {
    /// The unit's `CrateScope` name.
    pub(crate) unit: Name,
    /// Its crate's names record, as the Bodies phase reads it (engine_names.rs `input_of`).
    pub(crate) names: Input,
    /// Each file row of its items record, by index.
    pub(crate) files: &'p [FileHead],
    /// The tool that derives the row.
    pub(crate) tool: Tool,
}

/// The store key of the row of `parts`.
pub(crate) fn row_key(parts: &RowKey<'_>) -> Key {
    let mut h = Hasher::new(KEY_DOMAIN);
    h.update(&VERSION.to_le_bytes());
    h.update(&u64::try_from(parts.unit.kind.index()).unwrap_or(u64::MAX).to_le_bytes()).update(&parts.unit.subject.0);
    h.update(&u64::try_from(parts.names.name.kind.index()).unwrap_or(u64::MAX).to_le_bytes()).update(&parts.names.name.subject.0);
    // Its `out`, the digest every reader of the record records (a body task's read of it):
    // not its `at`, whose task key may move with an input that leaves the record's bytes as
    // they are, which would make the row again for nothing.
    h.update(&parts.names.out.0);
    h.update(&parts.tool.0.0);
    h.update(&u64::try_from(parts.files.len()).unwrap_or(u64::MAX).to_le_bytes());
    for head in parts.files {
        match head {
            FileHead::NotOutlined => {
                h.update(&[0]);
            }
            FileHead::Text(digest) => {
                h.update(&[2]).update(&digest.0);
            }
            FileHead::Head(digest) => {
                h.update(&[3]).update(&digest.0);
            }
        }
    }
    Key(h.finish())
}

/// A body a macro wrote at module level whose call's walk ends at a file entry: the entry's task
/// checks it (body_types/covered.rs `WrittenBy` with an `Ok` top).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Written<'r> {
    /// The file (its items record index) and the outline entry the walk ends at.
    pub(crate) top: (u32, ItemIndex),
    /// Its key (covered.rs `body_key`); `None`: the records list no such member.
    pub(crate) key: Option<ItemKey>,
    /// The call's macro as written (covered.rs `WrittenBy::by`).
    pub(crate) by: &'r str,
    /// Its label (covered.rs `written_label`).
    pub(crate) label: &'r str,
}

/// A body of a block's module (covered.rs `InBlock`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Block<'r> {
    /// The body whose task checks it (`InBlock::owner`).
    pub(crate) owner: ItemKey,
    /// Its own key.
    pub(crate) key: ItemKey,
    /// Its name: a free fn's, a member's (the `::name` of its block label).
    pub(crate) name: &'r str,
}

/// A member's listing row, opened ([`open`]) or about to be encoded ([`encode`]). `'r`: its
/// texts (the row's bytes, read in place); `'a`: its lists.
#[derive(Debug, PartialEq)]
pub(crate) struct MemberRow<'r, 'a> {
    /// Bodies a macro wrote that no entry covers.
    pub(crate) uncovered: u32,
    /// The first of them by label, at most [`MOST_UNCOVERED_NAMED`].
    pub(crate) named: AVec<'a, &'r str>,
    /// Written bodies placed at a file entry, in `written_by`'s order.
    pub(crate) written: AVec<'a, Written<'r>>,
    /// Bodies of blocks' modules, sorted by (owner, key); or why they could not be told apart.
    pub(crate) blocks: Result<AVec<'a, Block<'r>>, InBlocksError>,
}

impl<'r> MemberRow<'r, '_> {
    /// The written bodies whose walk ends at an entry of file `file`, in order.
    pub(crate) fn written_in<'s>(&'s self, file: u32) -> impl Iterator<Item = &'s Written<'r>> + 's {
        self.written.iter().filter(move |w| w.top.0 == file)
    }
}

/// The name a block label says of `body` (engine_bodies.rs `BlockLabel`).
fn body_name<'n>(records: &'n Records<'n>, body: BodyRef<'n>) -> &'n str {
    match body {
        BodyRef::Free(key) => NamesSource::name_of(records, key).unwrap_or("_"),
        BodyRef::Member { name, .. } => name,
    }
}

/// `args` written once into `frame`, kept for the frame's life (the row's labels live as long as
/// the phase's other labels).
fn kept_text<'a>(frame: &'a bstd::mem::Frame, args: fmt::Arguments<'_>) -> &'a str {
    blaze_own::text::format(frame, args).into_bump_str()
}

/// The row of the member whose own crate's records are `records` (crate 0) and whose expansion
/// texts are `texts` (`None`: its crate has none), from its WHOLE listing: what the Bodies phase
/// took from that listing each command (module doc, THE ROW), derived exactly as it was there.
pub(crate) fn make<'n>(records: &'n Records<'n>, texts: Option<&ExpansionTexts<'n>>, frame: &'n bstd::mem::Frame, alloc: Alloc<'n>) -> MemberRow<'n, 'n> {
    let listed = list_with(records, Listing::FnsAndInitializers, alloc);
    let (written, blocks) = match texts {
        Some(texts) => (covered::written_by(records, texts, &listed, alloc), covered::in_blocks(records, texts, &listed, alloc)),
        None => (AVec::new_in(alloc), Ok(AVec::new_in(alloc))),
    };
    // The bodies a macro wrote that no entry covers: neither at a file entry nor in a block's
    // module (counted only when the blocks were told apart, as before).
    let mut uncovered = 0u32;
    let mut named: AVec<'n, &'n str> = AVec::new_in(alloc);
    if let Ok(blocks) = blocks.as_ref() {
        for w in written.iter().filter(|w| w.top.is_err()) {
            let key = covered::body_key(records, w.listed.body);
            if !key.is_some_and(|key| blocks.iter().any(|one| one.key == key)) {
                uncovered = uncovered.saturating_add(1);
                if named.len() < MOST_UNCOVERED_NAMED {
                    named.push(kept_text(frame, format_args!("{}", covered::written_label(records, w))));
                }
            }
        }
    }
    let mut placed: AVec<'n, Written<'n>> = AVec::with_capacity_in(written.len(), alloc);
    for w in written.iter() {
        let Ok(top) = w.top else { continue };
        placed.push(Written { top, key: covered::body_key(records, w.listed.body), by: w.by, label: kept_text(frame, format_args!("{}", covered::written_label(records, w))) });
    }
    let blocks = blocks.map(|blocks| {
        let mut out: AVec<'n, Block<'n>> = AVec::with_capacity_in(blocks.len(), alloc);
        out.extend(blocks.iter().map(|b| Block { owner: b.owner, key: b.key, name: body_name(records, b.listed.body) }));
        out
    });
    MemberRow { uncovered, named, written: placed, blocks }
}

/// `row`'s bytes appended to `into` (module doc, THE FORM).
pub(crate) fn encode(row: &MemberRow<'_, '_>, into: &mut AVec<'_, u8>) {
    fn text(into: &mut AVec<'_, u8>, text: &str) {
        into.extend_from_slice(&count(text.len()));
        into.extend_from_slice(text.as_bytes());
    }
    fn count(n: usize) -> [u8; 4] {
        u32::try_from(n).unwrap_or(u32::MAX).to_le_bytes()
    }
    into.extend_from_slice(&MAGIC);
    into.extend_from_slice(&VERSION.to_le_bytes());
    into.extend_from_slice(&row.uncovered.to_le_bytes());
    into.extend_from_slice(&count(row.named.len()));
    for one in row.named.iter() {
        text(into, one);
    }
    into.extend_from_slice(&count(row.written.len()));
    for w in row.written.iter() {
        into.extend_from_slice(&w.top.0.to_le_bytes());
        into.extend_from_slice(&w.top.1.0.to_le_bytes());
        match w.key {
            Some(key) => {
                into.push(1);
                into.extend_from_slice(&key.0.0);
            }
            None => into.push(0),
        }
        text(into, w.by);
        text(into, w.label);
    }
    match &row.blocks {
        Ok(blocks) => {
            into.push(0);
            into.extend_from_slice(&count(blocks.len()));
            for b in blocks.iter() {
                into.extend_from_slice(&b.owner.0.0);
                into.extend_from_slice(&b.key.0.0);
                text(into, b.name);
            }
        }
        Err(why) => {
            into.push(1);
            into.push(why.code());
        }
    }
}

/// Why a row's bytes do not open: each names the byte offset it is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RowRefused {
    /// The bytes do not begin with the form's magic.
    Magic,
    /// Another version of the form (its key is another: never read, so a store fault).
    Version { found: u32 },
    /// The bytes end at `at` where `what` needs `need` more.
    Short { at: usize, what: &'static str, need: usize },
    /// A count at `at` larger than the bytes after it can hold (rule 18).
    Count { at: usize, what: &'static str, count: u32 },
    /// A text at `at` that is not UTF-8.
    Utf8 { at: usize },
    /// A tag byte at `at` that no form has.
    Tag { at: usize, what: &'static str, found: u8 },
    /// More named than counted, or more than [`MOST_UNCOVERED_NAMED`].
    Named { named: u32, uncovered: u32 },
    /// Bytes left after the row's end.
    Trailing { at: usize, left: usize },
}

impl fmt::Display for RowRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RowRefused::Magic => write!(f, "a member's listing row does not begin with its form's magic (engine_member_rows.rs)"),
            RowRefused::Version { found } => write!(f, "a member's listing row is of form {found}, not {VERSION} (engine_member_rows.rs)"),
            RowRefused::Short { at, what, need } => write!(f, "a member's listing row ends at byte {at}, where its {what} needs {need} more (engine_member_rows.rs)"),
            RowRefused::Count { at, what, count } => write!(f, "a member's listing row counts {count} {what} at byte {at}, more than its bytes hold (engine_member_rows.rs)"),
            RowRefused::Utf8 { at } => write!(f, "a member's listing row's text at byte {at} is not UTF-8 (engine_member_rows.rs)"),
            RowRefused::Tag { at, what, found } => write!(f, "a member's listing row's {what} tag at byte {at} is {found}, which no form has (engine_member_rows.rs)"),
            RowRefused::Named { named, uncovered } => write!(f, "a member's listing row names {named} uncovered bodies of {uncovered} (at most {MOST_UNCOVERED_NAMED}) (engine_member_rows.rs)"),
            RowRefused::Trailing { at, left } => write!(f, "a member's listing row has {left} bytes after its end at byte {at} (engine_member_rows.rs)"),
        }
    }
}

/// A cursor over a row's bytes: every read checks its length once.
struct Cursor<'r> {
    bytes: &'r [u8],
    at: usize,
}

impl<'r> Cursor<'r> {
    fn take(&mut self, n: usize, what: &'static str) -> Result<&'r [u8], RowRefused> {
        let left = self.bytes.len() - self.at;
        if left < n {
            return Err(RowRefused::Short { at: self.at, what, need: n - left });
        }
        let out = &self.bytes[self.at..self.at + n];
        self.at += n;
        Ok(out)
    }

    fn u8(&mut self, what: &'static str) -> Result<u8, RowRefused> {
        Ok(self.take(1, what)?[0])
    }

    fn u32(&mut self, what: &'static str) -> Result<u32, RowRefused> {
        let b = self.take(4, what)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn key(&mut self, what: &'static str) -> Result<ItemKey, RowRefused> {
        let b = self.take(32, what)?;
        let mut words = [0u8; 32];
        words.copy_from_slice(b);
        Ok(ItemKey(ContentId(words)))
    }

    fn text(&mut self, what: &'static str) -> Result<&'r str, RowRefused> {
        let len = self.u32(what)?;
        let at = self.at;
        let bytes = self.take(usize::try_from(len).unwrap_or(usize::MAX), what)?;
        core::str::from_utf8(bytes).map_err(|_| RowRefused::Utf8 { at })
    }

    /// A count of entries each at least `least` bytes long, checked against what is left.
    fn count(&mut self, least: usize, what: &'static str) -> Result<usize, RowRefused> {
        let at = self.at;
        let count = self.u32(what)?;
        let n = usize::try_from(count).unwrap_or(usize::MAX);
        if n.saturating_mul(least) > self.bytes.len() - self.at {
            return Err(RowRefused::Count { at, what, count });
        }
        Ok(n)
    }
}

/// The row in `bytes`, its texts read in place and its lists in `alloc`: every length and tag
/// checked here once, nothing after the end (module doc, THE FORM).
pub(crate) fn open<'r, 'a>(bytes: &'r [u8], alloc: Alloc<'a>) -> Result<MemberRow<'r, 'a>, RowRefused> {
    let mut c = Cursor { bytes, at: 0 };
    if c.take(MAGIC.len(), "magic").map_err(|_| RowRefused::Magic)? != &MAGIC[..] {
        return Err(RowRefused::Magic);
    }
    let version = c.u32("version")?;
    if version != VERSION {
        return Err(RowRefused::Version { found: version });
    }
    let uncovered = c.u32("uncovered count")?;
    let n = c.count(4, "named bodies")?;
    if n > MOST_UNCOVERED_NAMED || u32::try_from(n).map_or(true, |n| n > uncovered) {
        return Err(RowRefused::Named { named: u32::try_from(n).unwrap_or(u32::MAX), uncovered });
    }
    let mut named: AVec<'a, &'r str> = AVec::with_capacity_in(n, alloc);
    for _ in 0..n {
        named.push(c.text("named body")?);
    }
    // file 4, entry 4, tag 1, two texts' lengths 8.
    let n = c.count(17, "written bodies")?;
    let mut written: AVec<'a, Written<'r>> = AVec::with_capacity_in(n, alloc);
    for _ in 0..n {
        let file = c.u32("written body's file")?;
        let entry = c.u32("written body's entry")?;
        let at = c.at;
        let key = match c.u8("written body's key")? {
            0 => None,
            1 => Some(c.key("written body's key")?),
            found => return Err(RowRefused::Tag { at, what: "written body's key", found }),
        };
        let by = c.text("written body's macro")?;
        let label = c.text("written body's label")?;
        written.push(Written { top: (file, ItemIndex(entry)), key, by, label });
    }
    let at = c.at;
    let blocks = match c.u8("blocks")? {
        0 => {
            // owner 32, key 32, a text's length 4.
            let n = c.count(68, "bodies of blocks")?;
            let mut blocks: AVec<'a, Block<'r>> = AVec::with_capacity_in(n, alloc);
            for _ in 0..n {
                let owner = c.key("block body's owner")?;
                let key = c.key("block body's key")?;
                let name = c.text("block body's name")?;
                blocks.push(Block { owner, key, name });
            }
            Ok(blocks)
        }
        1 => {
            let at = c.at;
            let code = c.u8("blocks' cause")?;
            Err(InBlocksError::of_code(code).ok_or(RowRefused::Tag { at, what: "blocks' cause", found: code })?)
        }
        found => return Err(RowRefused::Tag { at, what: "blocks", found }),
    };
    if c.at != bytes.len() {
        return Err(RowRefused::Trailing { at: c.at, left: bytes.len() - c.at });
    }
    Ok(MemberRow { uncovered, named, written, blocks })
}

/// What the Bodies phase did with members' rows, a request's phases summed (the profile's
/// listing line): each member's row read by key, made from its whole listing, or made because
/// the bytes under its key did not open (counted, never put over: the store refuses other
/// bytes under a key, rule 21).
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub(crate) struct RowCounts {
    pub(crate) read: u32,
    pub(crate) made: u32,
    pub(crate) damaged: u32,
    /// THE ORACLE (tests only, engine_bodies.rs `groups`): rows read by key whose bytes are not
    /// the row made from today's whole listing of an opened member: a key that misses an input.
    #[cfg(test)]
    pub(crate) unequal: u32,
}

impl RowCounts {
    pub(crate) fn add(&mut self, other: RowCounts) {
        #[cfg(test)]
        {
            self.unequal = self.unequal.saturating_add(other.unequal);
        }
        self.read = self.read.saturating_add(other.read);
        self.made = self.made.saturating_add(other.made);
        self.damaged = self.damaged.saturating_add(other.damaged);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use blaze_drive::Kind;

    fn key(v: u8) -> ItemKey {
        ItemKey(ContentId([v; 32]))
    }

    /// A row with every part: uncovered bodies named, written bodies with and without a key, and
    /// bodies of blocks.
    fn full<'a>(alloc: Alloc<'a>) -> MemberRow<'static, 'a> {
        let mut named = AVec::new_in(alloc);
        named.push("{the expansion of `m!`}::a");
        named.push("{the expansion of `m!`}::b");
        let mut written = AVec::new_in(alloc);
        written.push(Written { top: (0, ItemIndex(3)), key: Some(key(1)), by: "m", label: "{the expansion of `m!`}::f" });
        written.push(Written { top: (1, ItemIndex(0)), key: None, by: "crate::n", label: "{the expansion of `crate::n!`}::clone" });
        let mut blocks = AVec::new_in(alloc);
        blocks.push(Block { owner: key(1), key: key(2), name: "serialize" });
        blocks.push(Block { owner: key(4), key: key(3), name: "_" });
        MemberRow { uncovered: 3, named, written, blocks: Ok(blocks) }
    }

    fn bytes_of<'a>(row: &MemberRow<'_, '_>, alloc: Alloc<'a>) -> AVec<'a, u8> {
        let mut out = AVec::new_in(alloc);
        encode(row, &mut out);
        out
    }

    /// Rule 26, restore equals fresh: a row with every part opens to the row it was made from,
    /// and encodes again to the same bytes; a row whose blocks were refused keeps its cause.
    /// CONTROL: a row differing in one written body's key encodes to other bytes, so the
    /// comparison sees a change in the part most easily lost.
    #[test]
    fn a_row_with_every_part_round_trips_in_bytes() {
        let heap = blaze_own::testing::heap();
        let g = heap.alloc();
        let row = full(g);
        let bytes = bytes_of(&row, g);
        let back = open(&bytes, g).expect("the row opens");
        assert_eq!(back, row, "the opened row is the row encoded");
        assert_eq!(&bytes_of(&back, g)[..], &bytes[..], "restore equals fresh in bytes");
        for why in InBlocksError::ALL {
            let refused = MemberRow { uncovered: 0, named: AVec::new_in(g), written: AVec::new_in(g), blocks: Err(why) };
            let bytes = bytes_of(&refused, g);
            assert_eq!(open(&bytes, g).expect("a refused row opens").blocks, Err(why), "the cause is kept");
        }
        // CONTROL.
        let mut other = full(g);
        other.written[1].key = Some(key(9));
        assert_ne!(&bytes_of(&other, g)[..], &bytes[..], "a moved key is another row's bytes");
    }

    /// Every length is checked once at open: a row one byte short of each of its ends, a count
    /// past its bytes, a stray tag and a trailing byte are each refused by name. CONTROL: the
    /// whole row opens, so each refusal is the cut's.
    #[test]
    fn a_row_short_or_long_is_refused_at_open() {
        let heap = blaze_own::testing::heap();
        let g = heap.alloc();
        let bytes = bytes_of(&full(g), g);
        assert!(open(&bytes, g).is_ok(), "CONTROL: the whole row opens");
        for cut in 0..bytes.len() {
            assert!(open(&bytes[..cut], g).is_err(), "a row cut at byte {cut} of {} is refused", bytes.len());
        }
        let mut long = AVec::new_in(g);
        long.extend_from_slice(&bytes);
        long.push(0);
        assert!(matches!(open(&long, g), Err(RowRefused::Trailing { left: 1, .. })), "a trailing byte is refused");
        // The written count (after magic, version, uncovered, named count and two named texts)
        // set past what the bytes hold.
        let named_end = 8 + 4 + 4 + 4 + (4 + full(g).named[0].len()) + (4 + full(g).named[1].len());
        let mut counted = AVec::new_in(g);
        counted.extend_from_slice(&bytes);
        counted[named_end..named_end + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(matches!(open(&counted, g), Err(RowRefused::Count { what: "written bodies", .. })), "a count past the bytes is refused before any list is reserved");
        // Another form on either side of today's, derived from the constant (never a literal: a
        // bump of `VERSION` must not make the planted form the current one).
        for found in [VERSION.wrapping_sub(1), VERSION.wrapping_add(1)] {
            let mut version = AVec::new_in(g);
            version.extend_from_slice(&bytes);
            version[8..12].copy_from_slice(&found.to_le_bytes());
            assert_eq!(open(&version, g), Err(RowRefused::Version { found }), "another form ({found}, today's {VERSION}) is refused, never read");
        }
    }

    /// A key's parts over `files`, every other part fixed.
    fn parts(files: &[FileHead]) -> RowKey<'_> {
        let unit = Name::new(Kind::CrateScope, ContentId([1; 32]));
        RowKey { unit, names: Input { name: unit, at: Key(ContentId([2; 32])), out: ContentId([3; 32]) }, files, tool: Tool(ContentId([4; 32])) }
    }

    /// The input of `key_of` that does not move it, or `None` when every one does.
    fn unmoved(key_of: &dyn Fn(&RowKey<'_>) -> Key) -> Option<&'static str> {
        let files = [FileHead::Text(ContentId([5; 32])), FileHead::NotOutlined];
        let base = key_of(&parts(&files));
        let mut unit = parts(&files);
        unit.unit.subject = ContentId([11; 32]);
        let mut at = parts(&files);
        at.names.name.subject = ContentId([12; 32]);
        let mut out = parts(&files);
        out.names.out = ContentId([13; 32]);
        let mut tool = parts(&files);
        tool.tool = Tool(ContentId([14; 32]));
        // An edit of the first file (failed outline): its text digest moves.
        let body_edit = [FileHead::Text(ContentId([15; 32])), FileHead::NotOutlined];
        // A head edit of the first file (an item, a block that writes a module): its surface.
        let head_edit = [FileHead::Head(ContentId([15; 32])), FileHead::NotOutlined];
        let outlined = [FileHead::Text(ContentId([5; 32])), FileHead::Text(ContentId([5; 32]))];
        let fewer = [FileHead::Text(ContentId([5; 32]))];
        let moved: [(&'static str, RowKey<'_>); 8] = [
            ("the unit", unit),
            ("the names record's name", at),
            ("the names record's out", out),
            ("the tool", tool),
            ("a file's text", parts(&body_edit)),
            ("a file's head", parts(&head_edit)),
            ("a file outlined", parts(&outlined)),
            ("the file count", parts(&fewer)),
        ];
        moved.iter().find(|(_, parts)| key_of(parts) == base).map(|(what, _)| *what)
    }

    /// Rule 21: the key moves with each input the row is a function of, a body-only edit of a
    /// file included. PLANTED: a key that leaves out the files' digests is caught (a nested item
    /// written in a body would read the old row: a wrong note, silently).
    #[test]
    fn the_key_moves_with_every_input() {
        assert_eq!(unmoved(&row_key), None, "every input moves the row's key");
        let planted = |p: &RowKey<'_>| row_key(&RowKey { files: &[], ..*p });
        assert_eq!(unmoved(&planted), Some("a file's text"), "PLANTED: a key without the files' digests is caught");
    }
}
